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

/// HOBBES_WRITE_HANDOFF: replace this chat's handoff note.
pub fn handle_write_handoff(
    state: &mut crate::session::SessionState,
    args: &serde_json::Value,
    session_id: &str,
) -> (crate::components::shared::ToolCallStatus, String) {
    let _ = (state, args, session_id);
    (
        crate::components::shared::ToolCallStatus::Error,
        "HOBBES_WRITE_HANDOFF is not implemented yet".to_string(),
    )
}
