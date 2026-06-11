use crate::llm::types::{ChatMessage, ContentBlock, ToolDefinition};
use crate::str_utils::find_split_point;
use serde_json::json;
use super::PromptBuilder;
use super::types::ToolResultPosition;

impl<'a> PromptBuilder<'a> {
    /// Pass 2: Apply dynamic per-tool-result budgets and paginate results that
    /// exceed their allocation. Mutates `messages` in-place and appends any
    /// new PageQueue entries to `pages_to_store`.
    pub(crate) fn apply_pass2_budget(
        &self,
        messages: &mut Vec<ChatMessage>,
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

        let active_idx = tool_result_positions.iter().position(|trp| trp.is_active);

        if let Some(budgets) = Self::compute_tool_result_budget(
            system_chars,
            tool_def_chars,
            non_result_chars,
            tool_result_positions.len(),
            active_idx,
            provider_context,
            tuning,
        ) {
            // Apply budgets: paginate results that exceed their allocation.
            // Uses find_split_point directly (instead of the old segment_into_pages)
            // so all content beyond page 1 is stored in the PageQueue as a raw
            // string for dynamic re-segmentation at delivery time. This guarantees
            // the model can always fetch content via HOBBES_PAGE_RESULT rather than
            // hitting a silent hard truncation.
            for (pos_idx, trp) in tool_result_positions.iter().enumerate() {
                let budget_chars = budgets[pos_idx];
                let ToolResultPosition { msg_idx, tool_name, execution_id, already_condensed, .. } = trp;

                // Skip entries already condensed by Pass 1 TOON stashing.
                // Their HOBBES_PAGE_RESULT footer must not be overwritten.
                if *already_condensed {
                    tracing::debug!(
                        "Pass 2 skipping '{}' — already condensed by Pass 1",
                        tool_name
                    );
                    continue;
                }

                if let Some(msg) = messages.get_mut(*msg_idx) {
                    if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first() {
                        // Extract raw content for pagination. If the value is a string
                        // (e.g., TOON markdown from compact_tool_results), use it directly
                        // to avoid JSON-escaping (\n → \\n). For objects/arrays, fall back
                        // to JSON serialization which preserves structure.
                        let serialized = content.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string()));

                        if serialized.len() > budget_chars {
                            // Find where page 1 ends using a single smart split.
                            // Store everything after it as a raw string so delivery
                            // can re-split dynamically at the budget of each future turn.
                            let split_at = find_split_point(&serialized, budget_chars);

                            if split_at < serialized.len() {
                                let short_suffix: String = execution_id.chars()
                                    .filter(|c| c.is_alphanumeric())
                                    .take(6)
                                    .collect();
                                let tool_call_id = format!("page-{}-{}", tool_name, short_suffix);
                                pages_to_store.push((tool_call_id.clone(), crate::session::PagedResult {
                                    remaining_content: serialized[split_at..].to_string(),
                                    tool_name: tool_name.clone(),
                                }));
                                let page1_with_footer = format!(
                                    "{}\n\n[More content available. To view the next page, use the HOBBES_PAGE_RESULT tool with tool_call_id \"{}\"]",
                                    &serialized[..split_at], tool_call_id
                                );
                                tracing::info!(
                                    "Pass 2: paginated '{}' ({} bytes → {} chars budget, id={})",
                                    tool_name, serialized.len(), budget_chars, tool_call_id
                                );
                                if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first_mut() {
                                    *content = json!(page1_with_footer);
                                }
                            }
                            // else: split_at == serialized.len() means find_split_point
                            // found no good boundary and returned the full length —
                            // content fits as-is, no truncation needed.
                        }
                    }
                }
            }
        }
        // else: compute_tool_result_budget returned None → no context limit, skip
    }

    /// Compute per-tool-result budgets for fitting results within the context window.
    /// Returns `None` if the provider has unlimited context.
    ///
    /// Budget split: the active tool result receives 60% of the remaining budget,
    /// historical results share the remaining 40% equally.
    pub(crate) fn compute_tool_result_budget(
        system_chars: usize,
        tool_def_chars: usize,
        non_result_message_chars: usize,
        num_tool_results: usize,
        active_index: Option<usize>,
        max_context_tokens: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
    ) -> Option<Vec<usize>> {
        let max_tokens = max_context_tokens?;

        // Convert tokens to chars using the configurable ratio
        let total_chars = (max_tokens as f64 * tuning.chars_per_token * (1.0 - tuning.context_safety_margin)) as usize;
        let overhead = system_chars + tool_def_chars + non_result_message_chars;

        if overhead >= total_chars {
            // No room for tool results at all
            return Some(vec![1024; num_tool_results]); // minimal fallback
        }

        let remaining = total_chars - overhead;

        if num_tool_results == 0 {
            return Some(vec![]);
        }

        let mut budgets = Vec::with_capacity(num_tool_results);

        if let Some(active_idx) = active_index {
            let active_budget = (remaining as f64 * tuning.active_result_budget_ratio) as usize;
            let historical_count = num_tool_results - 1;
            let historical_per = if historical_count > 0 {
                ((remaining as f64 * (1.0 - tuning.active_result_budget_ratio)) / historical_count as f64) as usize
            } else {
                0
            };

            for i in 0..num_tool_results {
                if i == active_idx {
                    budgets.push(active_budget);
                } else {
                    budgets.push(historical_per);
                }
            }
        } else {
            // No active result identified → split equally
            let per_result = remaining / num_tool_results;
            budgets.resize(num_tool_results, per_result);
        }

        tracing::debug!(
            "Tool result budgets: {} chars remaining, {} results, active_idx={:?}, budgets={:?}",
            remaining, num_tool_results, active_index, budgets
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

    /// Calculate a context-aware cap for tool result length.
    /// For providers with finite context windows (OpenAI-compat, Claude),
    /// caps to ~30% of the context budget (in chars, assuming ~4 chars/token).
    /// For Gemini or unconfigured providers, falls back to the resolved setting.
    pub(crate) fn effective_tool_result_limit(&self, tuning: &crate::settings::ResolvedContextTuning) -> usize {
        let user_max = tuning.max_active_tool_output_length;

        let provider_context_tokens = self.effective_context_window();

        if let Some(max_tokens) = provider_context_tokens {
            let ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(tuning.tool_result_budget_ratio);
            let context_cap = (max_tokens as f64 * ratio * tuning.chars_per_token) as usize;
            let effective = context_cap.min(user_max);
            tracing::debug!(
                "Tool result limit: {} chars (ratio: {:.0}%, provider context: {} tokens, user max: {})",
                effective, ratio * 100.0, max_tokens, user_max
            );
            effective
        } else {
            user_max
        }
    }
}
