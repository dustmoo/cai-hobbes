//! System sessions: the chat sessions Hobbes runs for itself — the
//! Heartbeat and Voice tabs. They are a fixed part of the app, not topics:
//! they keep their name and icon (drawn from the session id, so a lost or
//! changed emoji can't hide them), are never filed under a project, and are
//! never retitled from a summary.
//!
//! A plain registry (no Signals) so background code and tests can ask too.
//! Seeded on first use from the ids the two features persist, and updated
//! whenever either creates its session.

use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemSession {
    Heartbeat,
    Voice,
}

#[derive(Default)]
struct Ids {
    heartbeat: Option<String>,
    voice: Option<String>,
    /// The persisted ids have been read (the store was open).
    seeded: bool,
}

fn registry() -> &'static RwLock<Ids> {
    static REG: OnceLock<RwLock<Ids>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Ids::default()))
}

/// Fill unknown ids from the store, once it is open.
fn seed() {
    if registry().read().map(|ids| ids.seeded).unwrap_or(true) || !crate::session_store::is_ready() {
        return;
    }
    let heartbeat = crate::session_store::meta_get(crate::heartbeat::META_KEY)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    let voice = crate::session_store::meta_get(crate::voice::session::META_KEY).filter(|s| !s.is_empty());
    if let Ok(mut ids) = registry().write() {
        if ids.heartbeat.is_none() {
            ids.heartbeat = heartbeat;
        }
        if ids.voice.is_none() {
            ids.voice = voice;
        }
        ids.seeded = true;
    }
}

/// Record the session a system feature uses (on creation or adoption).
pub fn register(kind: SystemSession, id: &str) {
    if let Ok(mut ids) = registry().write() {
        let slot = match kind {
            SystemSession::Heartbeat => &mut ids.heartbeat,
            SystemSession::Voice => &mut ids.voice,
        };
        *slot = Some(id.to_string());
    }
}

/// Which system session `id` is, if any.
pub fn kind(id: &str) -> Option<SystemSession> {
    seed();
    let ids = registry().read().ok()?;
    if ids.heartbeat.as_deref() == Some(id) {
        Some(SystemSession::Heartbeat)
    } else if ids.voice.as_deref() == Some(id) {
        Some(SystemSession::Voice)
    } else {
        None
    }
}

pub fn is_system(id: &str) -> bool {
    kind(id).is_some()
}

/// Put a hydrated system session back in shape: its icon emoji (journaled,
/// so a rebuild from the event journal keeps it) and no project tag.
/// Returns whether anything changed (the caller saves).
pub fn repair(
    session_state: &mut dioxus::prelude::Signal<crate::session::SessionState>,
    id: &str,
    emoji: &str,
) -> bool {
    use dioxus::prelude::{Readable, Writable};
    use crate::session_events::{log_event, SessionEvent};
    let (needs_emoji, tagged) = match session_state.peek().sessions.get(id) {
        Some(s) => (s.tab_emoji.as_deref() != Some(emoji), s.project_id.is_some()),
        None => return false,
    };
    if !needs_emoji && !tagged {
        return false;
    }
    {
        let mut state = session_state.write();
        if let Some(s) = state.sessions.get_mut(id) {
            if needs_emoji {
                s.tab_emoji = Some(emoji.to_string());
            }
            if tagged {
                s.project_id = None;
                s.project_tag_user_set = false;
            }
        }
    }
    if needs_emoji {
        log_event(id, SessionEvent::TabTitled { emoji: Some(emoji.to_string()), title: None });
    }
    if tagged {
        tracing::info!(session_id = id, "system session: removed its project tag");
        log_event(id, SessionEvent::ProjectTagged { project_id: None, user_set: false });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_sessions_are_recognised() {
        register(SystemSession::Voice, "test-voice-session");
        register(SystemSession::Heartbeat, "test-heartbeat-session");
        assert_eq!(kind("test-voice-session"), Some(SystemSession::Voice));
        assert_eq!(kind("test-heartbeat-session"), Some(SystemSession::Heartbeat));
        assert!(!is_system("an-ordinary-chat"));
    }
}
