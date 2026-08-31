//! Append-only session event journal (Phase 1: dual-write).
//!
//! Every model-visible or user-authored session mutation is mirrored into the
//! `session_events` table alongside the existing `SessionState` model. In this
//! phase the journal is **write-only** — nothing reads it at runtime — so a
//! logging failure must never affect app behavior (all append paths swallow
//! errors after logging them).
//!
//! Design rules:
//! - Serde-tagged enum (`kind` tag). Rows are deserialized **individually** on
//!   read; a row whose kind is unknown to this build (written by a newer
//!   version) is skipped with a warning, never fails the whole read.
//! - Only final messages are journaled — never streaming chunks. Assistant
//!   messages are logged once, at end-of-turn finalization.
//! - Timestamps live in the row (`ts` column, captured at append time) so a
//!   future replay never needs to call `Utc::now()`.
//! - Appends follow P-009: the event is serialized to a row buffer on the
//!   calling thread; only the prepared buffers move to the background writer
//!   (see `session_store::append_events`).

use serde::{Deserialize, Serialize};

use crate::components::chat::Message;
use crate::timers::ScheduledTimer;

/// One journaled session mutation. The `kind` serde tag doubles as the
/// `session_events.kind` column value (see [`SessionEvent::kind`]).
///
/// Message-shaped variants embed the full `Message` as it exists in the model:
/// tool calls and their results are folded into a single message
/// (`MessageContent::ToolCall` mutated in place when the result arrives), so
/// `ToolCall` carries the message at dispatch time (status `Running`) and
/// `ToolResult` carries the same message id again with the final
/// status/response. Skill invocations ride the same pair: a completed
/// `MessageContent::SkillCall` update is journaled as `ToolResult`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    /// A user-authored message (typed sends, /skill bubbles, /unload
    /// confirmations, synthetic recovery prompts authored as "User").
    UserMessage { message: Message },
    /// A finalized assistant message (end-of-turn text, error bubbles,
    /// turn-limit warnings, memory-optimization notices).
    AssistantMessage { message: Message },
    /// A tool/skill call message at dispatch time (status Running/Pending).
    ToolCall { message: Message },
    /// The same tool/skill call message after its result was written in place.
    ToolResult { message: Message },
    /// HOBBES_UPDATE_SCRATCHPAD overwrite.
    ScratchpadSet { content: String },
    /// A skill payload was inserted into `session.loaded_skills`.
    SkillLoaded { name: String, payload: String },
    /// A skill was removed from `session.loaded_skills`.
    SkillUnloaded { name: String },
    /// A background summarizer wrote `active_context.conversation_summary`.
    /// Stored as JSON to mirror `ConversationSummary` without pinning its shape.
    SummaryComputed { summary: serde_json::Value },
    /// HOBBES_SET_TIMER scheduled a timer.
    TimerCreated { timer: ScheduledTimer },
    /// HOBBES_CANCEL_TIMER cancelled a pending timer.
    TimerCancelled { timer_id: String },
    /// The poll scheduler marked a due timer as fired.
    TimerFired { timer: ScheduledTimer },
    /// The session was pinned to a connector/model (per-tab picker).
    ConnectorPinned {
        connector_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    },
    /// The session was renamed.
    SessionRenamed { name: String },
    /// `delete_message_and_after` truncated history. `seq` is the journal seq
    /// of the first event referencing the deleted message (everything at or
    /// after it is undone); 0 when the message predates the journal.
    /// `message_id` is carried so Phase 2 can re-anchor even without a seq.
    RewoundTo {
        seq: i64,
        #[serde(default)]
        message_id: String,
    },
}

impl SessionEvent {
    /// The serde tag, mirrored into the `kind` column for cheap querying.
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEvent::UserMessage { .. } => "UserMessage",
            SessionEvent::AssistantMessage { .. } => "AssistantMessage",
            SessionEvent::ToolCall { .. } => "ToolCall",
            SessionEvent::ToolResult { .. } => "ToolResult",
            SessionEvent::ScratchpadSet { .. } => "ScratchpadSet",
            SessionEvent::SkillLoaded { .. } => "SkillLoaded",
            SessionEvent::SkillUnloaded { .. } => "SkillUnloaded",
            SessionEvent::SummaryComputed { .. } => "SummaryComputed",
            SessionEvent::TimerCreated { .. } => "TimerCreated",
            SessionEvent::TimerCancelled { .. } => "TimerCancelled",
            SessionEvent::TimerFired { .. } => "TimerFired",
            SessionEvent::ConnectorPinned { .. } => "ConnectorPinned",
            SessionEvent::SessionRenamed { .. } => "SessionRenamed",
            SessionEvent::RewoundTo { .. } => "RewoundTo",
        }
    }
}

/// Append one event to a session's journal. Fire-and-forget: serializes on
/// the calling thread, hands the prepared row to the background writer, and
/// no-ops (with a debug log) when the store isn't initialized (tests).
pub fn log_event(session_id: &str, event: SessionEvent) {
    crate::session_store::append_events(session_id, vec![event]);
}

/// Batch variant of [`log_event`] — one background hop for a whole turn.
pub fn log_events(session_id: &str, events: Vec<SessionEvent>) {
    crate::session_store::append_events(session_id, events);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_serde_tag() {
        let ev = SessionEvent::ScratchpadSet {
            content: "notes".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("kind").and_then(|k| k.as_str()), Some(ev.kind()));
    }

    #[test]
    fn rewound_to_tolerates_missing_message_id() {
        // Older rows (or trimmed writers) without message_id still parse.
        let ev: SessionEvent =
            serde_json::from_str(r#"{"kind":"RewoundTo","seq":42}"#).unwrap();
        assert_eq!(
            ev,
            SessionEvent::RewoundTo {
                seq: 42,
                message_id: String::new()
            }
        );
    }

    #[test]
    fn unknown_kind_fails_single_row_only() {
        // The enum itself must reject unknown kinds so load_events can skip
        // them row-by-row instead of poisoning a whole read.
        let res = serde_json::from_str::<SessionEvent>(
            r#"{"kind":"FromTheFuture","payload":123}"#,
        );
        assert!(res.is_err());
    }
}
