use crate::llm::types::{ChatMessage, ContentBlock, ToolDefinition};
use crate::str_utils::find_split_point;
use serde_json::json;
use super::PromptBuilder;
use super::types::ToolResultPosition;

impl<'a> PromptBuilder<'a> {
    /// Pass 2: Apply dynamic per-tool-result budgets and paginate results that
    /// exceed their allocation. Edits `messages` elements in-place (never changes
    /// their count) and appends any new PageQueue entries to `pages_to_store`.
    // Cohesive single-purpose budgeting pass; each argument is a distinct input.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_pass2_budget(
        &self,
        messages: &mut [ChatMessage],
        tool_result_positions: &[ToolResultPosition],
        pages_to_store: &mut Vec<(String, crate::session::PagedResult)>,
        tools: &[ToolDefinition],
        system: &str,
        provider_context: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
    ) {
        if tool_result_positions.is_empty() {
            return;
        }

        let system_chars = system.len();
        let tool_def_chars: usize = tools.iter()
            .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
            .sum();

        let non_result_chars: usize = messages.iter()
            .enumerate()
            .filter(|(idx, _)| !tool_result_positions.iter().any(|trp| trp.msg_idx == *idx))
            .map(|(_, m)| m.content.iter().map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::Thinking { text, .. } => text.len(),
                ContentBlock::ToolCall { name, arguments, .. } => name.len() + arguments.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.to_string().len(),
                _ => 0, // Image, etc.
            }).sum::<usize>())
            .sum();

        // All tool results that belong to the current turn are protected (full
        // fidelity); only history may be compressed. Collect their positions so
        // the budget split can give the working set the lion's share.
        let active_indices: Vec<usize> = tool_result_positions
            .iter()
            .enumerate()
            .filter(|(_, trp)| trp.is_active)
            .map(|(i, _)| i)
            .collect();

        // Resolve per-result budgets. With a known context window we split it
        // proportionally; with an unknown window (an unconfigured OpenAI-compatible
        // endpoint) we fall back to fixed caps so we never ship an unbounded
        // payload to a server whose limit we can't see.
        let budgets: Vec<usize> = match Self::compute_tool_result_budget(
            system_chars,
            tool_def_chars,
            non_result_chars,
            tool_result_positions.len(),
            &active_indices,
            provider_context,
            tuning,
        ) {
            Some(b) => b,
            None => tool_result_positions
                .iter()
                .map(|trp| {
                    if trp.is_active {
                        tuning.max_active_tool_output_length
                    } else {
                        tuning.max_tool_output_length
                    }
                })
                .collect(),
        };

        for (pos_idx, trp) in tool_result_positions.iter().enumerate() {
            let budget_chars = budgets[pos_idx];
            let ToolResultPosition {
                msg_idx,
                tool_name,
                execution_id,
                is_active,
                result_summary,
            } = trp;

            let Some(msg) = messages.get_mut(*msg_idx) else {
                continue;
            };
            let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first() else {
                continue;
            };

            // Extract raw content. If the value is a string (e.g. TOON markdown
            // from compact_tool_results), use it directly to avoid JSON-escaping
            // (\n → \\n). For objects/arrays, serialize preserving structure.
            let serialized = content
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string())
                });

            if serialized.len() <= budget_chars {
                continue; // Fits — leave it in full.
            }

            let short_suffix: String = execution_id
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(6)
                .collect();
            let tool_call_id = format!("page-{}-{}", tool_name, short_suffix);

            // HISTORICAL + over budget: prefer a knowledge-preserving summary if one
            // exists. This keeps the salient facts (IDs, names, numbers) in context
            // instead of chopping the data behind pagination the model may ignore.
            // The full payload is still stashed for explicit retrieval.
            if !is_active {
                if let Some(summary) = result_summary {
                    pages_to_store.push((
                        tool_call_id.clone(),
                        crate::session::PagedResult {
                            remaining_content: serialized.clone(),
                            tool_name: tool_name.clone(),
                        },
                    ));
                    let replaced = format!(
                        "[Summary of an earlier '{}' result — key facts preserved. Full data: call HOBBES_PAGE_RESULT with tool_call_id \"{}\"]\n\n{}",
                        tool_name, tool_call_id, summary
                    );
                    tracing::info!(
                        "Pass 2: summarised historical '{}' ({} bytes → {} chars summary, id={})",
                        tool_name,
                        serialized.len(),
                        replaced.len(),
                        tool_call_id
                    );
                    if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first_mut() {
                        *content = json!(replaced);
                    }
                    continue;
                }
            }

            // No summary available (or this is an active result we must not
            // summarise): paginate. Page 1 is served inline; the remainder is
            // stored as a raw string so future turns can re-split it at their own
            // budget. The model can always fetch the rest via HOBBES_PAGE_RESULT —
            // never a silent hard truncation.
            let split_at = find_split_point(&serialized, budget_chars);
            if split_at < serialized.len() {
                pages_to_store.push((
                    tool_call_id.clone(),
                    crate::session::PagedResult {
                        remaining_content: serialized[split_at..].to_string(),
                        tool_name: tool_name.clone(),
                    },
                ));
                let page1_with_footer = format!(
                    "{}\n\n[More content available. To view the next page, use the HOBBES_PAGE_RESULT tool with tool_call_id \"{}\"]",
                    &serialized[..split_at], tool_call_id
                );
                tracing::info!(
                    "Pass 2: paginated '{}' ({} bytes → {} chars budget, id={})",
                    tool_name,
                    serialized.len(),
                    budget_chars,
                    tool_call_id
                );
                if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first_mut() {
                    *content = json!(page1_with_footer);
                }
            }
            // else: find_split_point found no boundary and returned full length —
            // content effectively fits, no change.
        }
    }

    /// Compute per-tool-result budgets for fitting results within the context window.
    /// Returns `None` when the context window is unknown (caller applies fixed caps).
    ///
    /// Budget split: the current turn's tool results (`active_indices`) collectively
    /// receive `active_result_budget_ratio` of the remaining budget, divided equally
    /// among them; historical results share the rest equally. When everything is
    /// active (or nothing is), the budget is split evenly within that group.
    pub(crate) fn compute_tool_result_budget(
        system_chars: usize,
        tool_def_chars: usize,
        non_result_message_chars: usize,
        num_tool_results: usize,
        active_indices: &[usize],
        max_context_tokens: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
    ) -> Option<Vec<usize>> {
        let max_tokens = max_context_tokens?;

        // Convert tokens to chars using the configurable ratio
        let total_chars =
            (max_tokens as f64 * tuning.chars_per_token * (1.0 - tuning.context_safety_margin)) as usize;
        let overhead = system_chars + tool_def_chars + non_result_message_chars;

        if overhead >= total_chars {
            // No room for tool results at all
            return Some(vec![1024; num_tool_results]); // minimal fallback
        }

        let remaining = total_chars - overhead;

        if num_tool_results == 0 {
            return Some(vec![]);
        }

        let active_count = active_indices.len();
        let historical_count = num_tool_results - active_count;
        let mut budgets = vec![0usize; num_tool_results];

        if active_count == 0 || historical_count == 0 {
            // All results in one bucket — split the whole budget evenly.
            let per = remaining / num_tool_results;
            budgets.iter_mut().for_each(|b| *b = per);
        } else {
            let active_share = (remaining as f64 * tuning.active_result_budget_ratio) as usize;
            let historical_share = remaining.saturating_sub(active_share);
            let active_per = active_share / active_count;
            let historical_per = historical_share / historical_count;
            for (i, b) in budgets.iter_mut().enumerate() {
                *b = if active_indices.contains(&i) {
                    active_per
                } else {
                    historical_per
                };
            }
        }

        tracing::debug!(
            "Tool result budgets: {} chars remaining, {} results ({} active), budgets={:?}",
            remaining,
            num_tool_results,
            active_count,
            budgets
        );

        Some(budgets)
    }

    /// Split `content` into a `Vec<String>` of pages that each fit within `page_size` chars.
    ///
    /// **Note:** Production code no longer calls this function — Pass 2 uses
    /// `crate::str_utils::find_split_point` directly so page 1 is served inline and
    /// the remainder is stored in the `PageQueue` for dynamic re-segmentation at
    /// delivery time. This function is preserved for unit-test coverage only.
    ///
    /// The split strategy (JSON boundaries → paragraph → line → char fallback) matches
    /// `str_utils::find_split_point` exactly because it delegates to it.
    #[cfg(test)]
    pub(crate) fn segment_into_pages(content: &str, page_size: usize) -> Vec<String> {
        if content.len() <= page_size {
            return vec![content.to_string()];
        }

        let mut pages = Vec::new();
        let mut remaining = content;

        while !remaining.is_empty() {
            if remaining.len() <= page_size {
                pages.push(remaining.to_string());
                break;
            }
            let split_at = crate::str_utils::find_split_point(remaining, page_size);
            pages.push(remaining[..split_at].to_string());
            remaining = &remaining[split_at..];
        }

        pages
    }

}
