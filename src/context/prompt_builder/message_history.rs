use crate::components::shared::MessageContent;
use crate::llm::types::{ChatMessage, ChatRole, ContentBlock, ToolDefinition};
use serde_json::json;
use super::PromptBuilder;
use super::types::{ToolResultPosition, unwrap_json_strings};

/// Result of the message linearisation pass (Pass 1).
pub(crate) struct LinearisedMessages {
    pub messages: Vec<ChatMessage>,
    pub tool_result_positions: Vec<ToolResultPosition>,
    pub pages_to_store: Vec<(String, crate::session::PagedResult)>,
}

impl<'a> PromptBuilder<'a> {
    /// Pass 1: Linearise the session message history into a flat `Vec<ChatMessage>`
    /// suitable for LLM consumption, with tool results tracked for Pass 2 budget
    /// allocation.
    ///
    /// This handles:
    /// - First user message preservation
    /// - Dynamic history length capping for finite context windows
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

        // Start with the user-configured (or default) history length.
        // If we know the provider's context window size, dynamically cap
        // the history to what can actually fit after system + tool definitions
        // consume their budget. This ensures a 32K model doesn't try to squeeze
        // 75 messages into a window that can only hold ~8.
        let history_len = if let Some(ctx_tokens) = provider_context {
            let system_chars = system.len();
            let tool_def_chars: usize = tools.iter()
                .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
                .sum();
            let chars_per_tok = tuning.chars_per_token.max(1.0);
            let total_chars = (ctx_tokens as f64 * chars_per_tok) as usize;
            let safety = (total_chars as f64 * tuning.context_safety_margin) as usize;
            let overhead = system_chars + tool_def_chars + safety;
            let history_budget_chars = total_chars.saturating_sub(overhead);
            // Estimate average message size from actual recent messages;
            // fall back to 500 chars/message if the session is empty.
            let avg_msg_chars = if session_messages.is_empty() {
                500
            } else {
                let sample: usize = session_messages.iter().rev().take(10)
                    .map(|m| m.content.display_summary().len().max(100))
                    .sum();
                (sample / session_messages.len().min(10)).max(100)
            };
            let dynamic_max = (history_budget_chars / avg_msg_chars).max(1);
            // Never exceed the user-configured value, but cap if the window is too small.
            let capped = tuning.chat_history_length.min(dynamic_max);
            tracing::debug!(
                ctx_tokens,
                system_chars,
                tool_def_chars,
                overhead_chars = overhead,
                history_budget_chars,
                avg_msg_chars,
                dynamic_max,
                configured = tuning.chat_history_length,
                effective = capped,
                "Dynamic history length resolved"
            );
            capped
        } else {
            // No known context window (e.g. Gemini with unlimited context) —
            // use the configured value as-is.
            tuning.chat_history_length
        };

        // 1. Add the first user message to preserve the original intent.
        if let Some(first_message) = session_messages.iter().find(|m| m.author == "User") {
            if let MessageContent::Text { .. } = &first_message.content {
                messages.push(first_message.clone().into());
                first_message_id = Some(first_message.id);
            }
        }

        // 2. Add the last `history_len` messages.
        let start_index = session_messages.len().saturating_sub(history_len);

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

        for (offset, message) in session_messages.iter().skip(start_index).enumerate() {
            let abs_idx = start_index + offset;
            // Avoid duplicating the first message
            if Some(message.id) != first_message_id {
                // Skip placeholder
                if is_continuation_placeholder && last_message.is_some_and(|lm| message.id == lm.id) {
                    continue;
                }

                match &message.content {
                    MessageContent::Text {
                        thought_signature, ..
                    } => {
                        // Track the latest thought_signature for backfilling onto
                        // subsequent tool calls that may lack their own.
                        if let Some(sig) = thought_signature {
                            last_thought_signature = Some(sig.clone());
                        }

                        let chat_msg: ChatMessage = message.clone().into();

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
                        // Backfill thought_signature from the last known one if this
                        // tool call doesn't have its own (critical for Gemini thinking mode).
                        let current_thought_signature = tc
                            .thought_signature
                            .clone()
                            .or(last_thought_signature.clone());
                        if let Some(sig) = &tc.thought_signature {
                            last_thought_signature = Some(sig.clone());
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
        }
    }
}
