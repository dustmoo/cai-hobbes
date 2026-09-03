//! Background, knowledge-preserving summarization of large tool results.
//!
//! When a turn completes, its tool results are still the "current turn" working
//! set and are kept in full. On the *next* user turn they become historical and,
//! if large, would otherwise be paginated (chopped behind a HOBBES_PAGE_RESULT
//! footer the model may ignore — the failure mode that produced hallucinated
//! data). To avoid that, we proactively summarize the just-completed turn's large
//! tool results in the background, storing a dense fact-preserving summary on
//! `ToolCall.result_summary`. Pass 2 then substitutes that summary instead of
//! hard-truncating once the result is historical.
//!
//! This mirrors the proactive conversation-summary pattern: fire-and-forget,
//! never blocking the turn, and persisted via the serialize-then-move pattern.

use crate::components::shared::MessageContent;
use crate::session::Session;

/// Floor for the "large enough to summarize" threshold. The caller passes a
/// context-scaled threshold (a result is only worth summarizing if it would
/// later exceed its historical budget), but never below this — there's no point
/// summarizing trivially small results on any provider.
pub const SUMMARY_THRESHOLD_CHARS: usize = 8_000;

/// Upper bound on results summarized per turn, to keep background LLM cost
/// bounded even if a turn produced many large tool calls.
const MAX_SUMMARIES_PER_TURN: usize = 4;

/// A tool result awaiting summarization, identified by its owning message.
#[derive(Clone, Debug)]
pub struct PendingSummary {
    pub message_id: uuid::Uuid,
    pub tool_name: String,
    pub response: String,
}

/// Collect large, not-yet-summarized tool results from the current turn (every
/// message at or after the last user message). `threshold_chars` is the minimum
/// raw size worth summarizing — typically the provider's historical per-result
/// budget so we skip results that would never need compression (e.g. on a 1M
/// window). Bounded by `MAX_SUMMARIES_PER_TURN`.
pub fn collect_pending(session: &Session, threshold_chars: usize) -> Vec<PendingSummary> {
    let threshold = threshold_chars.max(SUMMARY_THRESHOLD_CHARS);
    let start = session
        .messages
        .iter()
        .rposition(|m| m.author == "User" && matches!(m.content, MessageContent::Text { .. }))
        .map(|i| i + 1)
        .unwrap_or(0);

    session
        .messages
        .iter()
        .skip(start)
        .filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc)
                if tc.result_summary.is_none() && tc.response.len() > threshold =>
            {
                Some(PendingSummary {
                    message_id: m.id,
                    tool_name: tc.tool_name.clone(),
                    response: tc.response.clone(),
                })
            }
            _ => None,
        })
        .take(MAX_SUMMARIES_PER_TURN)
        .collect()
}

/// Write a generated summary back onto the owning tool call. Returns true if the
/// target message was found and updated.
pub fn apply_summary(
    session: &mut Session,
    message_id: uuid::Uuid,
    summary: String,
) -> bool {
    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == message_id) {
        if let MessageContent::ToolCall(tc) = &mut msg.content {
            tc.result_summary = Some(summary);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::chat::Message;
    use crate::components::shared::{ToolCall, ToolCallStatus};
    use chrono::Utc;

    fn user_msg() -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: "User".to_string(),
            content: MessageContent::Text {
                content: "hi".to_string(),
                thought_signature: None,
                thought_summary: None,
            },
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn tool_msg(response: String, summary: Option<String>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(ToolCall {
                execution_id: uuid::Uuid::new_v4().to_string(),
                server_name: "s".to_string(),
                tool_name: "T".to_string(),
                arguments: "{}".to_string(),
                status: ToolCallStatus::Completed,
                response,
                thought_signature: None,
                thought_summary: None,
                cached_image_path: None,
                result_summary: summary,
            }),
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn session_with(messages: Vec<Message>) -> Session {
        Session {
            id: "s".to_string(),
            name: "s".to_string(),
            messages,
            active_context: crate::session::ActiveContext::default(),
            last_updated: Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: None,
            llm_connector_id: None,
            llm_provider: None,
            chat_model: None,
            project_id: None,
            project_tag_user_set: false,
            loaded_skills: std::collections::HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
        }
    }

    #[test]
    fn collects_only_large_unsummarized_current_turn_results() {
        let big = "x".repeat(SUMMARY_THRESHOLD_CHARS + 1);
        let small = "x".repeat(100);
        let session = session_with(vec![
            // Previous turn: large but should be ignored (before last user msg).
            user_msg(),
            tool_msg(big.clone(), None),
            // Current turn.
            user_msg(),
            tool_msg(big.clone(), None),          // collected
            tool_msg(small, None),                // too small
            tool_msg(big.clone(), Some("done".into())), // already summarized
        ]);
        let pending = collect_pending(&session, SUMMARY_THRESHOLD_CHARS);
        assert_eq!(pending.len(), 1, "only the large, unsummarized current-turn result");
    }

    #[test]
    fn apply_summary_sets_field() {
        let big = "x".repeat(SUMMARY_THRESHOLD_CHARS + 1);
        let mut session = session_with(vec![user_msg(), tool_msg(big, None)]);
        let id = session.messages[1].id;
        assert!(apply_summary(&mut session, id, "S".to_string()));
        if let MessageContent::ToolCall(tc) = &session.messages[1].content {
            assert_eq!(tc.result_summary.as_deref(), Some("S"));
        } else {
            panic!("expected tool call");
        }
    }
}
