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

/// Who said a Hobbes-tab message, as the voice is shown it, and its text
/// when that differs from the stored one: "Person" (heard or typed), "You
/// (voice)" (what the voice said, and the requests it handed over, without
/// their frame), "Hobbes (brain)" (the brain's replies and tool calls), a
/// Heartbeat wake as one line (its briefing is long; its outcome is in the
/// reply after it).
fn voice_view(m: &Message) -> (String, Option<String>) {
    let text = m.content.get_text_content();
    let text = text.as_deref().map(str::trim);
    let strip = |t: &str, prefix: &str| t.strip_prefix(prefix.trim_end()).unwrap_or(t).trim().to_string();
    match (m.author.as_str(), text) {
        ("User", Some(t)) if t.starts_with(crate::heartbeat::wake::WAKE_HEADER) => {
            ("Heartbeat".into(), Some("(woke to check on the person's day)".into()))
        }
        ("User", Some(t)) if crate::voice::brain::is_voice_request(t) => (
            "You (voice), asking the brain".into(),
            Some(strip(crate::voice::display::display_text(t), HEARD_PREFIX)),
        ),
        ("User", Some(t)) if crate::voice::transcript::is_recovery_prompt(m) => ("System".into(), Some(t.to_string())),
        ("User", Some(t)) => ("Person".into(), Some(strip(t, HEARD_PREFIX))),
        ("Hobbes", Some(t)) if t.starts_with(SPOKE_PREFIX.trim_end()) => ("You (voice)".into(), Some(strip(t, SPOKE_PREFIX))),
        ("User", None) => ("Person".into(), None),
        ("Hobbes", _) => ("Hobbes (brain)".into(), None),
        (other, _) => (other.to_string(), None),
    }
}

/// The recent conversation in the Hobbes session (read-only) for the voice
/// tier's instructions: the last `limit` messages as "Speaker: text" lines
/// (see [`voice_view`] for the speakers), capped in size.
pub fn session_tail(session_state: Signal<SessionState>, session_id: &str, limit: usize) -> String {
    let state = session_state.peek();
    let Some(s) = state.sessions.get(session_id) else { return String::new() };
    let start = s.messages.len().saturating_sub(limit.clamp(1, 40));
    let tail: Vec<Message> = s.messages[start..]
        .iter()
        .map(|m| {
            let (author, text) = voice_view(m);
            let mut v = m.clone();
            v.author = author;
            if let Some(content) = text {
                v.content = MessageContent::Text {
                    content,
                    thought_signature: None,
                    thought_summary: None,
                    signature_provider: None,
                };
            }
            v
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
            let mut request = transcript_message(true, "x");
            request.content = MessageContent::Text {
                content: crate::voice::brain::framed_request("fix CI"),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            };
            s.messages.push(request);
            let mut reply = transcript_message(false, "x");
            reply.content = MessageContent::Text {
                content: "Dispatched a fix.".into(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            };
            s.messages.push(reply);
            let sig = use_signal(|| state);
            let tail = session_tail(sig, &id, 6);
            let lines: Vec<&str> = tail.lines().collect();
            assert_eq!(
                lines,
                [
                    "Person: what's blocked?",
                    "Heartbeat: (woke to check on the person's day)",
                    "You (voice): CI is red on main.",
                    "You (voice), asking the brain: fix CI",
                    "Hobbes (brain): Dispatched a fix.",
                ]
            );
            // The voice request's frame is never shown to the voice.
            assert!(!tail.contains("[Voice request") && !tail.contains("Details:"));
            assert!(!tail.contains("detail detail"));
            assert_eq!(session_tail(sig, "missing", 6), "");
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
}
