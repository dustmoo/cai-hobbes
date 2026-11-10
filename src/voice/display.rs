//! Pure helpers for how voice transcript lines are shown to the person and
//! presented to the brain. The stored messages never change; only these two
//! views do.

use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::voice::session::{HEARD_PREFIX, SPOKE_PREFIX};

/// Where the long bracketed frame starts in a voice request.
const FRAME_MARKER: &str = "[Voice request, relayed by Hobbes' voice layer";

/// The text of a message as the person sees it: a voice request loses its
/// bracketed frame (the 🎙 request line(s) stay); anything else is returned
/// untouched.
pub fn display_text(text: &str) -> &str {
    if !crate::voice::brain::is_voice_request(text) {
        return text;
    }
    match text.find(FRAME_MARKER) {
        Some(i) => text[..i].trim_end(),
        None => text,
    }
}

/// After the person edits the displayed text of `original`, the text to
/// store: a voice request keeps its frame around the edited request.
pub fn restore_frame(original: &str, edited: String) -> String {
    if !crate::voice::brain::is_voice_request(original) {
        return edited;
    }
    let request = edited.trim();
    let request = request.strip_prefix(HEARD_PREFIX.trim_end()).unwrap_or(request);
    crate::voice::brain::framed_request(request)
}

/// How the prompt presents one session message that may be a spoken line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpokenLine {
    /// Not a spoken line: present as usual.
    Normal,
    /// A spoken restatement of the brain reply it follows: omit.
    Drop,
    /// The voice speaking on its own: present this text as context, not as
    /// the assistant's words.
    Context(String),
}

fn spoken_text(m: &Message) -> Option<&str> {
    if m.author != "Hobbes" {
        return None;
    }
    match &m.content {
        MessageContent::Text { content, .. } => content.strip_prefix(SPOKE_PREFIX),
        _ => None,
    }
}

/// Classify `messages[idx]`. A spoken line that follows a brain reply (a
/// non-user text message, looking back over other spoken lines) is a
/// restatement of it; any other is context. Depends only on the stored
/// messages before `idx`, so the rendering of old history never changes
/// when later messages are appended.
pub fn classify_spoken(messages: &[Message], idx: usize) -> SpokenLine {
    let Some(text) = messages.get(idx).and_then(spoken_text) else {
        return SpokenLine::Normal;
    };
    let prev = messages[..idx].iter().rev().find(|m| spoken_text(m).is_none());
    let after_reply =
        prev.is_some_and(|m| m.author != "User" && matches!(m.content, MessageContent::Text { .. }));
    if after_reply {
        SpokenLine::Drop
    } else {
        SpokenLine::Context(format!("[Hobbes' voice said aloud: {}]", text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::session::transcript_message;

    fn msg(author: &str, text: &str) -> Message {
        let mut m = transcript_message(true, "x");
        m.author = author.to_string();
        m.content = MessageContent::Text {
            content: text.to_string(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        };
        m
    }

    #[test]
    fn display_hides_frame_only_for_voice_requests() {
        let framed = crate::voice::brain::framed_request("what time is it");
        assert_eq!(display_text(&framed), "🎙 what time is it");
        assert_eq!(display_text("hello [Voice request, relayed by Hobbes' voice layer"), "hello [Voice request, relayed by Hobbes' voice layer");
        assert_eq!(display_text("plain"), "plain");
        assert_eq!(display_text("🎙 just heard"), "🎙 just heard");
    }

    #[test]
    fn edit_keeps_frame() {
        let framed = crate::voice::brain::framed_request("a");
        let out = restore_frame(&framed, "🎙 b".to_string());
        assert_eq!(out, crate::voice::brain::framed_request("b"));
        assert_eq!(restore_frame("hi", "yo".into()), "yo");
    }

    #[test]
    fn spoken_after_reply_is_dropped() {
        let ms = vec![
            msg("User", &crate::voice::brain::framed_request("q")),
            msg("Hobbes", "The answer."),
            msg("Hobbes", "🔊 Short answer."),
            msg("Hobbes", "🔊 Details are in your tab."),
        ];
        assert_eq!(classify_spoken(&ms, 1), SpokenLine::Normal);
        assert_eq!(classify_spoken(&ms, 2), SpokenLine::Drop);
        assert_eq!(classify_spoken(&ms, 3), SpokenLine::Drop);
    }

    #[test]
    fn standalone_spoken_is_context() {
        let ms = vec![
            msg("User", "🎙 hi"),
            msg("Hobbes", "🔊 One moment."),
            msg("User", "typed"),
            msg("Hobbes", "🔊 Which one?"),
        ];
        assert_eq!(classify_spoken(&ms, 1), SpokenLine::Context("[Hobbes' voice said aloud: One moment.]".into()));
        assert_eq!(classify_spoken(&ms, 3), SpokenLine::Context("[Hobbes' voice said aloud: Which one?]".into()));
        let first = vec![msg("Hobbes", "🔊 Hello.")];
        assert!(matches!(classify_spoken(&first, 0), SpokenLine::Context(_)));
    }
}
