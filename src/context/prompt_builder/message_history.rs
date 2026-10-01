use crate::components::shared::MessageContent;
use crate::llm::types::{ChatMessage, ChatRole, ContentBlock, ToolDefinition};
use serde_json::json;
use super::PromptBuilder;
use super::types::{ToolResultPosition, unwrap_json_strings};

/// Decide whether a stored `thought_signature` may be replayed to the provider
/// this prompt is being built for.
///
/// `thought_signature` is an opaque, provider-specific blob. Gemini expects it
/// back as `thoughtSignature` on a function-call part; Claude expects it as
/// `signature` on a `thinking` block. Replaying one provider's blob to the other
/// is a hard 400, and a mid-chat model switch only re-pins the session — the
/// history keeps whatever signatures were minted before the switch.
///
/// Rules:
/// - stamped with the effective provider → keep.
/// - stamped with a different provider → drop.
/// - **unstamped (legacy row)** → keep only when the message belongs to the
///   most recent turn.
///
/// Why that legacy rule: `signature_provider` is new, but signature *storage*
/// is not. `git log -S thought_signature -- src/llm/claude.rs` shows the field
/// has been written by the Claude connector since the provider landed
/// (`99e3672 feat(llm): implement Claude (Anthropic Messages API) provider`,
/// via its `signature_delta` handling), just as Gemini has written it since
/// `fdc2575`. So an unstamped signature could have been minted by *either*
/// provider and there is no per-message evidence to tell them apart. The only
/// safe rule is therefore positional: signatures on older turns are dropped
/// outright (losing them costs nothing but a little cache reuse), while the
/// most recent turn — the in-flight continuation loop, where Gemini's Baton
/// Pattern actually needs the signature back — is preserved, since a switch
/// mid-turn is not possible (the pin changes between turns).
pub(crate) fn signature_for_provider(
    sig: Option<&str>,
    minted_by: Option<crate::settings::LlmProvider>,
    effective: crate::settings::LlmProvider,
    in_latest_turn: bool,
) -> Option<String> {
    let sig = sig?;
    match minted_by {
        Some(p) if p == effective => Some(sig.to_string()),
        Some(_) => None,
        None => in_latest_turn.then(|| sig.to_string()),
    }
}

/// After a trim, the history is brought down to this fraction of each limit
/// (message count, and the char budget on finite windows). The gap between
/// the limit (high water) and this mark is what makes trims rare: with the
/// default 75-message limit a trim drops to ≤ 50 messages, so the next one is
/// ~25 messages away instead of firing on every append.
const HISTORY_LOW_WATER_NUM: usize = 2;
const HISTORY_LOW_WATER_DEN: usize = 3;

/// Outcome of `resolve_history_window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryWindow {
    /// First session-message index included in the prompt (the pinned first
    /// user message is added separately, wherever it sits).
    pub start_index: usize,
    /// Anchor to persist on the session: the id of the message at
    /// `start_index`, or `None` when the window starts at the beginning.
    pub anchor: Option<uuid::Uuid>,
}

/// Decide where the outgoing history starts, so trimming changes the request
/// prefix rarely and in large steps.
///
/// Every message after the pinned first user message is a cache-relevant
/// prefix byte: a window that slides by one message per request (the old
/// `len - history_len` rule) invalidates the provider's prompt cache for the
/// whole history on every request once a chat passes the limit. Instead:
///
/// - The window starts at the persisted `anchor` (or the start of the chat
///   when it is absent or no longer exists — e.g. after a rewind — which then
///   recomputes with the same rule).
/// - It only moves when the included history exceeds a limit: more than
///   `max_messages` messages, or (finite windows) more than `char_budget`
///   estimated chars. That is the high-water mark.
/// - It then jumps forward to the earliest turn boundary that brings both
///   measures under the low-water mark (`HISTORY_LOW_WATER_*` of each limit),
///   or as far as it may go when none does.
/// - A cut is only ever made at a user text message (a turn start), so a
///   tool_use/tool_result pair or a turn's thinking chain is never split, and
///   never at or after the latest user text message (the in-progress turn).
///   If no such boundary is ahead of the anchor, the window is kept as is.
///
/// Pure in its inputs: the same messages, anchor and limits always resolve
/// the same window.
pub(crate) fn resolve_history_window(
    messages: &[crate::components::chat::Message],
    anchor: Option<uuid::Uuid>,
    first_pinned_idx: Option<usize>,
    max_messages: usize,
    char_budget: Option<usize>,
) -> HistoryWindow {
    let len = messages.len();
    // A stale anchor (deleted message) falls back to the start of the chat.
    let start = anchor
        .and_then(|id| messages.iter().position(|m| m.id == id))
        .unwrap_or(0);

    // Per-message size estimate. Tool results are deliberately not counted at
    // full size — Pass 2 fits those against the real window.
    let msg_chars = |m: &crate::components::chat::Message| {
        m.content.display_summary().len().max(100)
    };
    // suffix[i] = (messages, chars) included when the window starts at `i`,
    // not counting the pinned first message (it is sent either way).
    let mut suffix = vec![(0usize, 0usize); len + 1];
    for i in (0..len).rev() {
        let (count, chars) = suffix[i + 1];
        suffix[i] = if Some(i) == first_pinned_idx {
            (count, chars)
        } else {
            (count + 1, chars + msg_chars(&messages[i]))
        };
    }
    let over = |i: usize, count_limit: usize, char_limit: Option<usize>| {
        let (count, chars) = suffix[i];
        count > count_limit || char_limit.is_some_and(|b| chars > b)
    };

    let max_messages = max_messages.max(1);
    let keep = HistoryWindow {
        start_index: start,
        anchor: (start > 0).then(|| messages[start].id),
    };
    if start >= len || !over(start, max_messages, char_budget) {
        return keep;
    }

    // Candidate cuts: user text messages strictly after the current start and
    // strictly before the in-progress turn's user message.
    let is_user_text = |m: &crate::components::chat::Message| {
        m.author == "User" && matches!(m.content, MessageContent::Text { .. })
    };
    let current_turn_user_idx = messages.iter().rposition(is_user_text).unwrap_or(0);
    let candidates: Vec<usize> = (start + 1..current_turn_user_idx)
        .filter(|&i| Some(i) != first_pinned_idx && is_user_text(&messages[i]))
        .collect();
    let Some(&furthest) = candidates.last() else {
        return keep;
    };

    let low_count = (max_messages * HISTORY_LOW_WATER_NUM / HISTORY_LOW_WATER_DEN).max(1);
    let low_chars = char_budget.map(|b| b * HISTORY_LOW_WATER_NUM / HISTORY_LOW_WATER_DEN);
    let cut = candidates
        .iter()
        .copied()
        .find(|&i| !over(i, low_count, low_chars))
        .unwrap_or(furthest);

    tracing::info!(
        from = start,
        to = cut,
        messages = len,
        included_before = suffix[start].0,
        included_after = suffix[cut].0,
        max_messages,
        ?char_budget,
        "History window trimmed: anchor moved forward to a turn boundary"
    );
    HistoryWindow {
        start_index: cut,
        anchor: Some(messages[cut].id),
    }
}

/// Result of the message linearisation pass (Pass 1).
pub(crate) struct LinearisedMessages {
    pub messages: Vec<ChatMessage>,
    pub tool_result_positions: Vec<ToolResultPosition>,
    pub pages_to_store: Vec<(String, crate::session::PagedResult)>,
    /// Trim anchor this build used — persisted by the caller via
    /// `PromptBuildResult::history_anchor`.
    pub history_anchor: Option<uuid::Uuid>,
}

impl<'a> PromptBuilder<'a> {
    /// Pass 1: Linearise the session message history into a flat `Vec<ChatMessage>`
    /// suitable for LLM consumption, with tool results tracked for Pass 2 budget
    /// allocation.
    ///
    /// This handles:
    /// - First user message preservation
    /// - The forward-only history window (`resolve_history_window`), capped by
    ///   message count and, for finite context windows, a char budget
    /// - Text → ChatMessage conversion
    /// - ToolCall → functionCall + functionResponse with TOON condensation
    /// - SkillCall → brief stub
    /// - Vision injection from cached screenshots
    pub(crate) fn linearise_messages(
        &self,
        system: &str,
        tools: &[ToolDefinition],
        provider_context: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
        is_continuation_placeholder: bool,
        last_message: Option<&crate::components::chat::Message>,
    ) -> LinearisedMessages {
        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut last_thought_signature: Option<String> = None;
        // Track positions of ToolResult messages for Pass 2 budget allocation
        let mut tool_result_positions: Vec<ToolResultPosition> = Vec::new();
        // Pages to store in PageQueue. Pass 1 no longer paginates anything;
        // Pass 2 (apply_pass2_budget) is the single authority that fills this.
        let pages_to_store: Vec<(String, crate::session::PagedResult)> = Vec::new();

        let session_messages = &self.session.messages;
        let mut first_message_id = None;

        // 1. Add the first user message to preserve the original intent.
        let mut first_pinned_idx = None;
        if let Some((idx, first_message)) =
            session_messages.iter().enumerate().find(|(_, m)| m.author == "User")
        {
            if let MessageContent::Text { .. } = &first_message.content {
                messages.push(first_message.clone().into());
                first_message_id = Some(first_message.id);
                first_pinned_idx = Some(idx);
            }
        }

        // 2. Add the history window: everything from the persisted trim anchor
        // on. If we know the provider's context window size, the history must
        // also fit what is left after system + tool definitions + safety margin
        // consume their budget (a 32K model can't hold 75 messages). Both limits
        // only decide *when* the anchor jumps forward (see
        // `resolve_history_window`); they never re-derive the start on every
        // build, which is what kept the outgoing prefix (and the provider's
        // prompt cache) changing on every request.
        let history_char_budget = provider_context.map(|ctx_tokens| {
            // `system` includes the per-turn TURN_CONTEXT, so this budget moves a
            // little between requests. That only shifts the moment a trim fires:
            // the anchor never moves back, and a trim lands well under the
            // budget, so a jittering system size can't flip the window.
            let system_chars = system.len();
            let tool_def_chars: usize = tools.iter()
                .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
                .sum();
            let chars_per_tok = tuning.chars_per_token.max(1.0);
            let total_chars = (ctx_tokens as f64 * chars_per_tok) as usize;
            let safety = (total_chars as f64 * tuning.context_safety_margin) as usize;
            let overhead = system_chars + tool_def_chars + safety;
            let history_budget_chars = total_chars.saturating_sub(overhead);
            tracing::debug!(
                ctx_tokens,
                system_chars,
                tool_def_chars,
                overhead_chars = overhead,
                history_budget_chars,
                "History char budget resolved"
            );
            history_budget_chars
        });
        let window = resolve_history_window(
            session_messages,
            self.session.history_anchor,
            first_pinned_idx,
            tuning.chat_history_length,
            history_char_budget,
        );
        let start_index = window.start_index;

        // Identify the current turn's boundary: every message at or after the
        // most recent user text message belongs to the in-progress turn, and its
        // tool results are protected from lossy compression (Flaw A fix). Using
        // "last user message" rather than "literally the last message" keeps a
        // tool result full even when later tool calls or text follow it within
        // the same turn — the Gmail-fetch-then-summarise case that previously
        // collapsed the fetch to a fixed cap mid-turn.
        let current_turn_start_idx = session_messages
            .iter()
            .rposition(|m| {
                m.author == "User" && matches!(m.content, MessageContent::Text { .. })
            })
            .map(|i| i + 1)
            .unwrap_or(0);

        // Provider this prompt targets — stored thought signatures minted by any
        // other provider must not be replayed (see `signature_for_provider`).
        let effective_provider = self.effective_provider();

        for (offset, message) in session_messages.iter().skip(start_index).enumerate() {
            let abs_idx = start_index + offset;
            // Avoid duplicating the first message
            if Some(message.id) != first_message_id {
                // Skip placeholder
                if is_continuation_placeholder && last_message.is_some_and(|lm| message.id == lm.id) {
                    continue;
                }

                // A message belongs to the "latest turn" when it sits at or after
                // the most recent user text message. Used by the signature
                // provenance filter for legacy (unstamped) rows.
                let in_latest_turn = abs_idx >= current_turn_start_idx;

                match &message.content {
                    MessageContent::Text {
                        thought_signature,
                        signature_provider,
                        ..
                    } => {
                        // Drop signatures minted by a different provider than the
                        // one this prompt targets (mid-chat model switch).
                        let filtered = signature_for_provider(
                            thought_signature.as_deref(),
                            *signature_provider,
                            effective_provider,
                            in_latest_turn,
                        );

                        // Track the latest thought_signature for backfilling onto
                        // subsequent tool calls that may lack their own. A rejected
                        // (foreign) signature resets the baton to `None` rather
                        // than leaving a stale one to be backfilled across the
                        // provider boundary.
                        if thought_signature.is_some() {
                            last_thought_signature = filtered.clone();
                        }

                        let mut chat_msg: ChatMessage = message.clone().into();
                        // `From<Message>` copies the raw signature into the
                        // Thinking block; overwrite it with the filtered value.
                        for block in chat_msg.content.iter_mut() {
                            if let ContentBlock::Thinking { signature, .. } = block {
                                *signature = filtered.clone();
                            }
                        }

                        // Handle comments
                        if !message.comments.is_empty() {
                            let mut comment_text =
                                String::from("[User comments on the above message:");
                            for comment in &message.comments {
                                comment_text.push_str(&format!(
                                    "\n- On \"{}\": {}",
                                    comment.text_selection, comment.comment
                                ));
                            }
                            comment_text.push(']');

                            messages.push(chat_msg);
                            messages.push(ChatMessage {
                                role: ChatRole::User,
                                content: vec![ContentBlock::Text { text: comment_text }],
                            });
                        } else {
                            messages.push(chat_msg);
                        }
                    }
                    MessageContent::ToolCall(tc) => {
                        // Drop a signature minted by a different provider before
                        // it can be replayed (or backfilled onto a later call).
                        let own_signature = signature_for_provider(
                            tc.thought_signature.as_deref(),
                            tc.signature_provider,
                            effective_provider,
                            in_latest_turn,
                        );
                        // Backfill thought_signature from the last known one if this
                        // tool call doesn't have its own (critical for Gemini thinking mode).
                        // Only signatures that themselves passed the filter are ever
                        // carried forward, so a backfill can never cross a provider
                        // boundary.
                        let current_thought_signature =
                            own_signature.clone().or_else(|| last_thought_signature.clone());
                        if tc.thought_signature.is_some() {
                            last_thought_signature = own_signature;
                        }

                        // Sanitize tool name — MUST match the name used in function declarations.
                        let sanitized_tool_name = crate::gemini::convert::get_prefixed_tool_name(
                            &tc.server_name,
                            &tc.tool_name,
                        );

                        // 1. Add the Assistant's ToolCall message (with thinking + function call)
                        let mut assistant_content = Vec::new();

                        // Include thinking summary BEFORE the function call (critical for Gemini)
                        if let Some(summary) = &tc.thought_summary {
                            if !summary.is_empty() {
                                assistant_content.push(ContentBlock::Thinking {
                                    text: summary.clone(),
                                    signature: current_thought_signature.clone(),
                                });
                            }
                        }

                        let args_value: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                        assistant_content.push(ContentBlock::ToolCall {
                            id: tc.execution_id.to_string(),
                            name: sanitized_tool_name.clone(),
                            arguments: args_value,
                            signature: current_thought_signature,
                        });

                        messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: assistant_content,
                        });

                        // 2. Add the Tool Result message.
                        // A tool result is "active" (protected, full fidelity) when
                        // it belongs to the current turn — its position is at or
                        // after the last user message (see `current_turn_start_idx`).
                        let is_active_tool_call = abs_idx >= current_turn_start_idx;
                        
                        // Why clone here?
                        // We clone `tc.response` so we can build a non-destructive
                        // prompt copy (TOON conversion, summary substitution, or
                        // pagination) for this turn only. The historical
                        // `SessionState::tc.response` is left untouched, so the full
                        // (potentially massive) payload is always preserved for
                        // future re-budgeting as the window shifts.
                        let result_string = tc.response.clone();

                        // Pass 1 is REPRESENTATION-ONLY: convert to compact TOON
                        // markdown when enabled, otherwise keep raw JSON. All
                        // budget-based work — truncation, pagination, and
                        // knowledge-preserving summary substitution — happens in
                        // Pass 2 (`apply_pass2_budget`), the single context-aware
                        // authority. Crucially, a large result is NOT chopped to a
                        // fixed cap here: on a large-context provider it stays in
                        // full; on a small one Pass 2 trims it against the real
                        // window.
                        let result_value: serde_json::Value = if tuning.compact_tool_results {
                            // Parse to JSON for markdown conversion.
                            let mut json_val: serde_json::Value =
                                serde_json::from_str(&result_string).unwrap_or(json!(result_string));
                            // Recursively unwrap stringified JSON (e.g. Composio's
                            // result.text) so TOON can render the full structure.
                            unwrap_json_strings(&mut json_val);
                            json!(crate::formatters::toon::to_toon(&json_val))
                        } else {
                            // Raw JSON preserved for fidelity; Pass 2 paginates if needed.
                            match serde_json::from_str::<serde_json::Value>(&result_string) {
                                Ok(val) => val,
                                Err(_) => json!(result_string),
                            }
                        };

                        messages.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: vec![ContentBlock::ToolResult {
                                call_id: tc.execution_id.to_string(),
                                name: sanitized_tool_name.clone(),
                                content: result_value,
                            }],
                        });

                        // Capture the ToolResult index BEFORE any vision message is pushed
                        // so Pass 2 budget allocation always targets the correct message.
                        let tool_result_msg_idx = messages.len() - 1;

                        // ── Vision injection ─────────────────────────────────────────────
                        // If this tool call produced a screenshot, inject it as a vision
                        // message so the model can see the image on the next turn.
                        // The image was already resized to max 768px and saved as JPEG by
                        // stream_manager::save_tool_image — we just need to load and re-encode.
                        if let Some(ref img_path) = tc.cached_image_path {
                            let fs_path = img_path.strip_prefix("file://").unwrap_or(img_path);
                            match std::fs::read(fs_path) {
                                Ok(bytes) => {
                                    use base64::Engine;
                                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    messages.push(ChatMessage {
                                        role: ChatRole::User,
                                        content: vec![
                                            ContentBlock::Text {
                                                text: "[Vision context from previous tool result — screenshot captured by browser:]".to_string(),
                                            },
                                            ContentBlock::Image {
                                                mime_type: "image/jpeg".to_string(),
                                                data,
                                            },
                                        ],
                                    });
                                    tracing::debug!(
                                        "Injected cached screenshot '{}' as vision context for '{}'",
                                        fs_path, tc.tool_name
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Could not load cached tool image '{}': {}",
                                        fs_path, e
                                    );
                                }
                            }
                        }

                        // Track position for Pass 2 budget allocation.
                        // Uses the pre-captured index so vision messages don't shift it.
                        // `result_summary` is populated from the tool call's
                        // background-generated summary (Part 4); Pass 2 uses it to
                        // compress over-budget history without losing facts.
                        tool_result_positions.push(ToolResultPosition {
                            msg_idx: tool_result_msg_idx,
                            tool_name: sanitized_tool_name,
                            execution_id: tc.execution_id.to_string(),
                            is_active: is_active_tool_call,
                            result_summary: tc.result_summary.clone(),
                        });

                    }
                    MessageContent::SkillCall(sc) => {
                        if let crate::components::shared::SkillCallStatus::Completed = sc.status {
                            // Only include a brief marker — the full skill instructions are
                            // already in the system context via the `active_skill` injection
                            // (lines 260-289). Including sc.response here was DOUBLE injecting
                            // the instruction manual, resolved tools, and arguments — adding
                            // thousands of tokens redundantly.
                            let context_text = format!(
                                "[Skill '{}' activated with arguments: {}. See active_skill in system context for instructions.]",
                                sc.skill_name, sc.arguments
                            );

                            messages.push(ChatMessage {
                                role: ChatRole::User,
                                content: vec![ContentBlock::Text { text: context_text }],
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        LinearisedMessages {
            messages,
            tool_result_positions,
            pages_to_store,
            history_anchor: window.anchor,
        }
    }
}
