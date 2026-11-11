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

/// Lowercased words only, so punctuation and spacing don't count.
/// Whether `messages[idx]` is a voice request that only repeats what the
/// person was just heard saying: the nearest earlier User message (past
/// anything the voice said aloud) is a heard line with the same words. The
/// chat shows such a request as a hand-off marker rather than a second
/// copy of the person's bubble. Depends only on messages up to `idx`.
/// Returns the heard line's index, so the caller can check it is rendered
/// (the marker points at it). Texts compare as the voice controller does
/// (`controller::normalize_request`).
pub fn echoes_heard_line(messages: &[Message], idx: usize) -> Option<usize> {
    let Some(MessageContent::Text { content, .. }) = messages.get(idx).map(|m| &m.content) else {
        return None;
    };
    if messages[idx].author != "User" || !crate::voice::brain::is_voice_request(content) {
        return None;
    }
    let request = display_text(content);
    let request = request.strip_prefix(HEARD_PREFIX.trim_end()).unwrap_or(request);
    let normalize = crate::voice::controller::normalize_request;
    let (at, m) = messages[..idx].iter().enumerate().rev().find(|(_, m)| spoken_text(m).is_none())?;
    let heard = Some(m)
        .filter(|m| m.author == "User")
        .and_then(|m| m.content.get_text_content())
        .filter(|t| t.starts_with(HEARD_PREFIX) && !crate::voice::brain::is_voice_request(t))?;
    let n = normalize(&heard);
    (!n.is_empty() && n == normalize(request)).then_some(at)
}

/// Whether the hand-off marker may stand in for a voice request: the heard
/// line it points at (`heard`, an index into the sorted messages) is inside
/// the rendered window, which starts at `first_visible` (the Load More
/// cut-off). Otherwise the request renders as its own bubble.
pub fn marker_shown(heard: usize, first_visible: usize) -> bool {
    heard >= first_visible
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
    fn request_repeating_heard_line_is_an_echo() {
        let req = crate::voice::brain::framed_request("Please dispatch a cab to Gemini Carbon.");
        let ms = vec![
            msg("User", "🎙 please dispatch a cab to Gemini Carbon"),
            msg("Hobbes", "🔊 On it."),
            msg("User", &req),
        ];
        assert_eq!(echoes_heard_line(&ms, 2), Some(0));
        // Directly after the heard line, too.
        assert_eq!(echoes_heard_line(&[ms[0].clone(), ms[2].clone()], 1), Some(0));
    }

    #[test]
    fn echoes_fold_case_beyond_ascii_and_need_the_heard_line_rendered() {
        let req = crate::voice::brain::framed_request("ÄRGER mit dem Ölpreis");
        let ms = vec![msg("User", "🎙 Ärger mit dem ölpreis"), msg("User", &req)];
        assert_eq!(echoes_heard_line(&ms, 1), Some(0));
        // The marker stands in only while the heard line is rendered.
        assert!(marker_shown(0, 0));
        assert!(!marker_shown(0, 1), "above the Load More cut-off: render the bubble");
    }

    #[test]
    fn paraphrase_or_no_heard_line_is_not_an_echo() {
        let req = crate::voice::brain::framed_request("Send Sam the Q3 numbers");
        let paraphrase = vec![msg("User", "🎙 can you get Sam those numbers"), msg("User", &req)];
        assert_eq!(echoes_heard_line(&paraphrase, 1), None);
        let typed = vec![msg("User", "Send Sam the Q3 numbers"), msg("User", &req)];
        assert_eq!(echoes_heard_line(&typed, 1), None);
        // A brain reply in between ends the match.
        let replied = vec![msg("User", "🎙 Send Sam the Q3 numbers"), msg("Hobbes", "Done."), msg("User", &req)];
        assert_eq!(echoes_heard_line(&replied, 2), None);
        // Not a voice request at all.
        let plain = vec![msg("User", "🎙 hi"), msg("User", "🎙 hi")];
        assert_eq!(echoes_heard_line(&plain, 1), None);
        assert_eq!(echoes_heard_line(&[msg("User", &req)], 0), None);
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
