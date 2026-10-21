//! Handoff notes: what the model writes for its future self.
//!
//! A summary says what happened; a handoff says what the model was doing,
//! what it expects next and what would change its mind. One per chat, kept
//! on the `Session`, written with `HOBBES_WRITE_HANDOFF` and shown back in
//! the turn briefing with its age (Heartbeat Phase 1).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HandoffNote {
    /// What the model was doing when it wrote the note.
    pub doing: String,
    /// What it expects to happen next, and what it plans to do about it.
    pub expect_next: String,
    /// What would change its plan, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would_change_mind: Option<String>,
    pub written_at: DateTime<Utc>,
}

/// Longest a single handoff field may be, in chars. A handoff is a few
/// sentences; anything longer is cut, not refused, so the note still lands.
pub const MAX_FIELD_CHARS: usize = 600;

/// A trimmed, capped string argument; `None` when missing or blank.
fn field(args: &serde_json::Value, key: &str) -> Option<String> {
    let text = args.get(key).and_then(|v| v.as_str())?.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_FIELD_CHARS {
        return Some(text.to_string());
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

/// HOBBES_WRITE_HANDOFF: replace this chat's handoff note.
///
/// `doing` and `expect_next` are required; `would_change_mind` is optional.
/// The note replaces any earlier one and is journaled as `HandoffWritten`;
/// the dispatch arm persists the session.
pub fn handle_write_handoff(
    state: &mut crate::session::SessionState,
    args: &serde_json::Value,
    session_id: &str,
) -> (crate::components::shared::ToolCallStatus, String) {
    use crate::components::shared::ToolCallStatus;

    let (Some(doing), Some(expect_next)) = (field(args, "doing"), field(args, "expect_next")) else {
        return (
            ToolCallStatus::Error,
            "HOBBES_WRITE_HANDOFF needs non-empty 'doing' and 'expect_next'.".to_string(),
        );
    };
    let note = HandoffNote {
        doing,
        expect_next,
        would_change_mind: field(args, "would_change_mind"),
        written_at: Utc::now(),
    };
    let Some(session) = state.sessions.get_mut(session_id) else {
        tracing::warn!("HOBBES_WRITE_HANDOFF: Session '{}' not found", session_id);
        return (
            ToolCallStatus::Error,
            format!("Session '{}' not found", session_id),
        );
    };
    let replaced = session.handoff.is_some();
    session.handoff = Some(note.clone());
    crate::session_events::log_event(
        session_id,
        crate::session_events::SessionEvent::HandoffWritten { note },
    );
    tracing::debug!("HOBBES_WRITE_HANDOFF: note written (replaced earlier: {replaced})");
    (
        ToolCallStatus::Completed,
        "Handoff note saved. It replaces any earlier note and is shown back to you, with its age, when you next pick up this chat after a gap.".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::ToolCallStatus;
    use crate::session::SessionState;
    use serde_json::json;

    fn state_with_session() -> (SessionState, String) {
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        (state, id)
    }

    #[test]
    fn writes_trimmed_note_and_replaces_earlier_one() {
        let (mut state, id) = state_with_session();
        let (status, msg) = handle_write_handoff(
            &mut state,
            &json!({"doing": "  running the suite ", "expect_next": "a green build"}),
            &id,
        );
        assert_eq!(status, ToolCallStatus::Completed, "{msg}");
        let note = state.sessions[&id].handoff.clone().unwrap();
        assert_eq!(note.doing, "running the suite");
        assert_eq!(note.expect_next, "a green build");
        assert_eq!(note.would_change_mind, None);
        assert!((Utc::now() - note.written_at).num_seconds() < 5);

        let (status, _) = handle_write_handoff(
            &mut state,
            &json!({"doing": "fixing a flaky test", "expect_next": "rerun", "would_change_mind": "a second failure"}),
            &id,
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let note = state.sessions[&id].handoff.clone().unwrap();
        assert_eq!(note.doing, "fixing a flaky test");
        assert_eq!(note.would_change_mind.as_deref(), Some("a second failure"));
    }

    #[test]
    fn requires_doing_and_expect_next() {
        let (mut state, id) = state_with_session();
        for args in [
            json!({"expect_next": "x"}),
            json!({"doing": "x"}),
            json!({"doing": "   ", "expect_next": "x"}),
            json!({"doing": "x", "expect_next": 3}),
        ] {
            let (status, _) = handle_write_handoff(&mut state, &args, &id);
            assert_eq!(status, ToolCallStatus::Error, "{args}");
        }
        assert!(state.sessions[&id].handoff.is_none());
    }

    #[test]
    fn blank_would_change_mind_is_dropped() {
        let (mut state, id) = state_with_session();
        handle_write_handoff(
            &mut state,
            &json!({"doing": "a", "expect_next": "b", "would_change_mind": "  "}),
            &id,
        );
        assert_eq!(state.sessions[&id].handoff.as_ref().unwrap().would_change_mind, None);
    }

    #[test]
    fn long_fields_are_capped() {
        let (mut state, id) = state_with_session();
        let long = "é".repeat(MAX_FIELD_CHARS * 2);
        handle_write_handoff(&mut state, &json!({"doing": long, "expect_next": "b"}), &id);
        let doing = state.sessions[&id].handoff.as_ref().unwrap().doing.clone();
        assert_eq!(doing.chars().count(), MAX_FIELD_CHARS);
        assert!(doing.ends_with('…'));
    }

    #[test]
    fn unknown_session_is_an_error() {
        let (mut state, _) = state_with_session();
        let (status, _) =
            handle_write_handoff(&mut state, &json!({"doing": "a", "expect_next": "b"}), "nope");
        assert_eq!(status, ToolCallStatus::Error);
    }
}
