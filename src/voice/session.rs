//! The dedicated **Voice** chat session: where voice conversations are
//! transcribed and where the brain's turns run (on the global connector —
//! the session is never pinned). Separate from the Heartbeat's session; each
//! can read the other through [`peer_tail`].

use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::session::SessionState;

/// Title of the dedicated session.
pub const SESSION_TITLE: &str = "Voice";
/// Its tab emoji (the tab components draw a metal mouth in its place).
pub const SESSION_EMOJI: &str = "👄";
/// `session_store` meta key holding the session id across launches.
pub const META_KEY: &str = "voice_session_id";

/// The Voice session's id once known (cached from the store at first use).
pub static VOICE_SESSION_ID: GlobalSignal<Option<String>> = Signal::global(|| {
    crate::session_store::meta_get(META_KEY)
        .filter(|s| !s.is_empty())
        // Past any rotation the pointer missed (session_rotation).
        .map(|id| crate::system_sessions::resolve_successor(&id))
});

/// Prefix on transcript lines of what the person said.
pub const HEARD_PREFIX: &str = "🎙 ";
/// Prefix on transcript lines of what the voice said.
pub const SPOKE_PREFIX: &str = "🔊 ";

/// The Voice session's id, hydrating it if it is only on disk and creating
/// it if it never existed or was deleted. Never touches the active session
/// or the open tabs. `None` when the store is unwritable.
/// `profile`: the Composio profile a new session starts with; `home`:
/// Hobbes' home profile, which an existing session is moved to when set.
pub fn ensure_session(mut session_state: Signal<SessionState>, profile: Option<String>, home: Option<&str>) -> Option<String> {
    if session_state.peek().save_disabled {
        return None;
    }
    let known = VOICE_SESSION_ID.peek().clone();
    if let Some(id) = known {
        let mut hydrated = session_state.peek().sessions.contains_key(&id);
        if !hydrated {
            if let Some(session) = crate::session_store::load_session(&id) {
                session_state.write().sessions.insert(id.clone(), session);
                hydrated = true;
            }
        }
        if hydrated {
            adopt(&mut session_state, &id, home);
            return Some(id);
        }
        tracing::info!("voice: session {id} is gone; creating a new one");
    }
    let id = {
        let mut state = session_state.write();
        let previous_active = state.active_session_id.clone();
        let id = state.create_session_raw(profile);
        if !previous_active.is_empty() {
            state.active_session_id = previous_active;
        }
        // A user-set name, so auto-titling never renames it.
        state.update_session_name_raw(&id, SESSION_TITLE.to_string());
        id
    };
    adopt(&mut session_state, &id, home);
    if let Err(e) = crate::session_store::meta_set(META_KEY, &id) {
        tracing::warn!("voice: could not persist the session id: {e}");
    }
    *VOICE_SESSION_ID.write() = Some(id.clone());
    tracing::info!("voice: created session {id}");
    Some(id)
}

/// Mark `id` as the Voice system session and repair its icon / project tag.
fn adopt(session_state: &mut Signal<SessionState>, id: &str, home: Option<&str>) {
    crate::system_sessions::register(crate::system_sessions::SystemSession::Voice, id);
    if crate::system_sessions::repair(session_state, id, SESSION_EMOJI, home) {
        SessionState::save_signal(session_state, None);
    }
}

/// Startup: repair the Voice session if its tab is open (hydrated).
pub fn repair_on_startup(mut session_state: Signal<SessionState>, home: Option<String>) {
    let Some(id) = VOICE_SESSION_ID.peek().clone() else { return };
    if session_state.peek().sessions.contains_key(&id) {
        adopt(&mut session_state, &id, home.as_deref());
    }
}

/// A transcript line as a chat message.
pub fn transcript_message(heard: bool, text: &str) -> Message {
    let (author, prefix) = if heard { ("User", HEARD_PREFIX) } else { ("Hobbes", SPOKE_PREFIX) };
    Message {
        id: uuid::Uuid::new_v4(),
        author: author.to_string(),
        content: MessageContent::Text {
            content: format!("{prefix}{}", text.trim()),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        },
        attachments: Vec::new(),
        comments: Vec::new(),
        created_at: chrono::Utc::now(),
        usage: None,
    }
}

/// Append a transcript line to the Voice session and persist.
pub fn append_transcript(mut session_state: Signal<SessionState>, session_id: &str, heard: bool, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let appended = {
        let mut state = session_state.write();
        match state.sessions.get_mut(session_id) {
            Some(s) => {
                s.messages.push(transcript_message(heard, text));
                true
            }
            None => false,
        }
    };
    if appended {
        SessionState::save_signal(&session_state, None);
    }
}

/// The plain text of the last `limit` messages of `messages`, oldest first,
/// as "Author: text" lines (tool calls summarised), capped at `max_chars`
/// (keeping the newest).
pub fn tail_text(messages: &[Message], limit: usize, max_chars: usize) -> String {
    let start = messages.len().saturating_sub(limit);
    let lines: Vec<String> = messages[start..]
        .iter()
        .filter_map(|m| {
            let body = match &m.content {
                MessageContent::Text { content, .. } => content.trim().to_string(),
                other => other.display_summary(),
            };
            (!body.is_empty()).then(|| format!("{}: {}", m.author, body))
        })
        .collect();
    let mut out = lines.join("\n");
    if out.chars().count() > max_chars {
        let skip = out.chars().count() - max_chars;
        out = format!("…{}", out.chars().skip(skip).collect::<String>());
    }
    out
}

/// Which peer session to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    Voice,
    Heartbeat,
}

impl Peer {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "voice" => Some(Peer::Voice),
            "heartbeat" => Some(Peer::Heartbeat),
            _ => None,
        }
    }

    fn session_id(self) -> Option<String> {
        match self {
            Peer::Voice => VOICE_SESSION_ID.peek().clone(),
            Peer::Heartbeat => crate::heartbeat::HEARTBEAT.peek().session_id.clone(),
        }
    }
}

/// The recent conversation of the Voice or Heartbeat session (read-only),
/// hydrated from disk if needed without inserting it into memory.
pub fn peer_tail(session_state: Signal<SessionState>, peer: Peer, limit: usize) -> Result<String, String> {
    let name = match peer {
        Peer::Voice => "Voice",
        Peer::Heartbeat => "Heartbeat",
    };
    let id = peer.session_id().ok_or_else(|| format!("The {name} session doesn't exist yet."))?;
    let limit = limit.clamp(1, 40);
    let messages = |id: &str| -> Option<Vec<Message>> {
        let state = session_state.peek();
        match state.sessions.get(id) {
            Some(s) => Some(s.messages[s.messages.len().saturating_sub(limit)..].to_vec()),
            None => {
                drop(state);
                crate::session_store::load_session(id).map(|s| {
                    let start = s.messages.len().saturating_sub(limit);
                    s.messages[start..].to_vec()
                })
            }
        }
    };
    let mut tail = messages(&id).ok_or_else(|| format!("The {name} session couldn't be read."))?;
    // Just after a rotation the session is nearly empty: top the tail up
    // from the archive it replaced (session_rotation).
    if tail.len() < limit {
        let kind = match peer {
            Peer::Voice => crate::system_sessions::SystemSession::Voice,
            Peer::Heartbeat => crate::system_sessions::SystemSession::Heartbeat,
        };
        if let Some(mut earlier) = crate::system_sessions::latest_archive(kind).and_then(|a| messages(&a)) {
            earlier.append(&mut tail);
            tail = earlier;
        }
    }
    let text = tail_text(&tail, limit, 6000);
    if text.is_empty() {
        Ok(format!("The {name} session has no messages yet."))
    } else {
        Ok(format!("Recent messages in the {name} session (oldest first):\n{text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_lines_are_prefixed_by_side() {
        let heard = transcript_message(true, " what's blocked? ");
        assert_eq!(heard.author, "User");
        assert_eq!(heard.content.get_text_content().as_deref(), Some("🎙 what's blocked?"));
        let spoke = transcript_message(false, "Two sessions.");
        assert_eq!(spoke.author, "Hobbes");
        assert_eq!(spoke.content.get_text_content().as_deref(), Some("🔊 Two sessions."));
    }

    #[test]
    fn tail_keeps_the_newest_within_limits() {
        let msgs: Vec<Message> = (0..10).map(|i| transcript_message(i % 2 == 0, &format!("line {i}"))).collect();
        let t = tail_text(&msgs, 3, 10_000);
        assert_eq!(t.lines().count(), 3);
        assert!(t.ends_with("line 9"));
        let capped = tail_text(&msgs, 10, 20);
        assert!(capped.starts_with('…'));
        assert!(capped.ends_with("line 9"));
    }

    #[test]
    fn peer_names_parse() {
        assert_eq!(Peer::parse("Heartbeat"), Some(Peer::Heartbeat));
        assert_eq!(Peer::parse(" voice "), Some(Peer::Voice));
        assert_eq!(Peer::parse("other"), None);
    }
}
