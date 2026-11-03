//! Voice in the Hobbes session (`hobbes_session`): voice conversations are
//! transcribed there and the brain's turns run there (on the global
//! connector — the session is never pinned), alongside typed turns and
//! Heartbeat wakes.

use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::session::SessionState;

/// Prefix on transcript lines of what the person said.
pub const HEARD_PREFIX: &str = "🎙 ";
/// Prefix on transcript lines of what the voice said.
pub const SPOKE_PREFIX: &str = "🔊 ";

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

/// The recent conversation in the Hobbes session (read-only) for the voice
/// tier's instructions: the last `limit` messages as "Author: text" lines,
/// a Heartbeat wake briefing reduced to one line (it is long, and its
/// outcome is in the reply after it).
pub fn session_tail(session_state: Signal<SessionState>, session_id: &str, limit: usize) -> String {
    let state = session_state.peek();
    let Some(s) = state.sessions.get(session_id) else { return String::new() };
    let start = s.messages.len().saturating_sub(limit.clamp(1, 40));
    let tail: Vec<Message> = s.messages[start..]
        .iter()
        .map(|m| {
            let wake = m.author == "User"
                && m.content.get_text_content().is_some_and(|t| t.starts_with(crate::heartbeat::wake::WAKE_HEADER));
            if wake {
                let mut short = m.clone();
                short.author = "Heartbeat".to_string();
                short.content = MessageContent::Text {
                    content: "(woke to check on the person's day)".to_string(),
                    thought_signature: None,
                    thought_summary: None,
                    signature_provider: None,
                };
                short
            } else {
                m.clone()
            }
        })
        .collect();
    tail_text(&tail, limit, 6000)
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
    fn the_session_tail_shortens_wake_briefings() {
        let mut dom = VirtualDom::new(|| {
            let mut state = SessionState::new();
            let id = state.create_session_raw(None);
            let s = state.sessions.get_mut(&id).unwrap();
            s.messages.push(transcript_message(true, "what's blocked?"));
            let mut wake = transcript_message(true, "x");
            wake.content = MessageContent::Text {
                content: format!("{}\n\nWhy: CI failed\n{}", crate::heartbeat::wake::WAKE_HEADER, "detail ".repeat(500)),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            };
            s.messages.push(wake);
            s.messages.push(transcript_message(false, "CI is red on main."));
            let sig = use_signal(|| state);
            let tail = session_tail(sig, &id, 6);
            assert!(tail.contains("🎙 what's blocked?") && tail.contains("CI is red on main."));
            assert!(tail.contains("Heartbeat: (woke"));
            assert!(!tail.contains("detail detail"));
            assert_eq!(session_tail(sig, "missing", 6), "");
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
}
