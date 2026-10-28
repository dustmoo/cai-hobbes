//! Prompt history for the chat input: Up/Down walk back through what you've
//! sent, like a terminal (or Claude Code). One list across all tabs, oldest
//! first, persisted in the session store's `meta` table. Also decides which
//! turn Esc may take back ([`rewindable_prompt`]).

use crate::components::chat::Message;
use crate::components::shared::MessageContent;

/// Meta key holding the history as a JSON array of strings.
const META_PROMPT_HISTORY: &str = "prompt_history";
/// At most this many prompts are kept.
pub const MAX_PROMPTS: usize = 500;

/// The stored history, oldest first. One indexed row, fine on the UI thread.
pub fn load() -> Vec<String> {
    crate::session_store::meta_get(META_PROMPT_HISTORY)
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Write the history in the background.
pub fn save(history: Vec<String>) {
    let Ok(json) = serde_json::to_string(&history) else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::session_store::meta_set(META_PROMPT_HISTORY, &json) {
            tracing::error!("prompt history: failed to save: {e}");
        }
    });
}

/// Record a sent prompt. Blank prompts and a repeat of the newest are
/// skipped. Returns whether the history changed.
pub fn push(history: &mut Vec<String>, prompt: &str) -> bool {
    if prompt.trim().is_empty() || history.last().is_some_and(|p| p == prompt) {
        return false;
    }
    history.push(prompt.to_string());
    if history.len() > MAX_PROMPTS {
        let excess = history.len() - MAX_PROMPTS;
        history.drain(..excess);
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Older,
    Newer,
}

/// Where a history step lands: `cursor` counts back from the newest prompt
/// (`None` = back at an empty draft), `text` is the new draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub cursor: Option<usize>,
    pub text: String,
}

/// One Up/Down press. Browsing starts from an empty draft and continues
/// while the draft still holds the recalled prompt; once it is edited, the
/// arrows move the caret again. `None` leaves the key to the textarea.
pub fn step(history: &[String], cursor: Option<usize>, draft: &str, dir: Direction) -> Option<Step> {
    let at = |i: usize| history.len().checked_sub(i + 1).map(|idx| history[idx].as_str());
    let browsing = cursor.filter(|&i| at(i) == Some(draft));
    match (dir, browsing) {
        (Direction::Older, Some(i)) => {
            let next = if at(i + 1).is_some() { i + 1 } else { i };
            Some(Step { cursor: Some(next), text: at(next)?.to_string() })
        }
        (Direction::Older, None) if draft.is_empty() => {
            Some(Step { cursor: Some(0), text: at(0)?.to_string() })
        }
        (Direction::Newer, Some(0)) => Some(Step { cursor: None, text: String::new() }),
        (Direction::Newer, Some(i)) => Some(Step { cursor: Some(i - 1), text: at(i - 1)?.to_string() }),
        _ => None,
    }
}

/// Prompts matching `query` (case-insensitive, anywhere in the text),
/// newest first, without repeats, at most `limit`. An empty query lists the
/// newest prompts.
pub fn search<'a>(history: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<&str> = Vec::new();
    for p in history.iter().rev() {
        if out.len() >= limit {
            break;
        }
        if (q.is_empty() || p.to_lowercase().contains(&q)) && !out.contains(&p.as_str()) {
            out.push(p);
        }
    }
    out
}

/// How many recent prompts [`rewindable_prompt`] checks against.
const RECENT_FOR_REWIND: usize = 20;

/// The prompt Esc may take back from an interrupted turn: the session's
/// last user message, when it is one the person typed (it is among their
/// recent prompts, so not a timer or relay) and the turn after it produced
/// only text. Once a tool or skill ran, the turn had effects and stays in
/// the transcript.
pub fn rewindable_prompt<'a>(messages: &'a [Message], history: &[String]) -> Option<&'a Message> {
    let at = messages.iter().rposition(|m| m.author == "User")?;
    let prompt = &messages[at];
    let MessageContent::Text { content, .. } = &prompt.content else {
        return None;
    };
    if !history.iter().rev().take(RECENT_FOR_REWIND).any(|p| p == content) {
        return None;
    }
    messages[at + 1..]
        .iter()
        .all(|m| matches!(m.content, MessageContent::Text { .. }))
        .then_some(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(author: &str, content: MessageContent) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content,
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: chrono::Utc::now(),
            usage: None,
        }
    }

    fn text(author: &str, t: &str) -> Message {
        message(
            author,
            MessageContent::Text {
                content: t.into(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            },
        )
    }

    #[test]
    fn esc_takes_back_a_typed_prompt_whose_turn_only_wrote_text() {
        let history = vec!["fix the build".to_string()];
        let mut msgs = vec![text("User", "earlier"), text("Hobbes", "ok"), text("User", "fix the build")];
        assert_eq!(rewindable_prompt(&msgs, &history).map(|m| m.id), Some(msgs[2].id), "nothing streamed yet");
        msgs.push(text("Hobbes", "Looking at"));
        assert_eq!(rewindable_prompt(&msgs, &history).map(|m| m.id), Some(msgs[2].id), "partial text");

        // A prompt the person didn't type here (a timer, a relay) stays.
        assert!(rewindable_prompt(&msgs, &["something else".to_string()]).is_none());
        assert!(rewindable_prompt(&[text("Hobbes", "hi")], &history).is_none());
    }

    #[test]
    fn esc_keeps_a_turn_that_ran_a_tool() {
        let history = vec!["fix the build".to_string()];
        let call = crate::components::shared::ToolCall::default();
        let msgs = vec![text("User", "fix the build"), message("Hobbes", MessageContent::ToolCall(call))];
        assert!(rewindable_prompt(&msgs, &history).is_none());
    }

    fn hist() -> Vec<String> {
        vec!["first".into(), "second".into(), "third".into()]
    }

    #[test]
    fn search_is_newest_first_case_insensitive_and_deduped() {
        let h: Vec<String> = ["Fix the BUILD", "deploy", "fix the build", "Fix the BUILD", "notes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(search(&h, "build", 10), ["Fix the BUILD", "fix the build"]);
        assert_eq!(search(&h, "", 2), ["notes", "Fix the BUILD"]);
        assert!(search(&h, "nothing", 10).is_empty());
    }

    #[test]
    fn push_skips_blanks_and_repeats_and_caps() {
        let mut h = hist();
        assert!(!push(&mut h, "  "));
        assert!(!push(&mut h, "third"));
        assert!(push(&mut h, "second"));
        assert_eq!(h.last().map(String::as_str), Some("second"));

        let mut big: Vec<String> = (0..MAX_PROMPTS).map(|i| i.to_string()).collect();
        assert!(push(&mut big, "new"));
        assert_eq!(big.len(), MAX_PROMPTS);
        assert_eq!(big.first().map(String::as_str), Some("1"));
        assert_eq!(big.last().map(String::as_str), Some("new"));
    }

    #[test]
    fn up_and_down_walk_the_history_from_an_empty_draft() {
        let h = hist();
        let up = |c, d: &str| step(&h, c, d, Direction::Older);
        let down = |c, d: &str| step(&h, c, d, Direction::Newer);

        let s = up(None, "").unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (Some(0), "third"));
        let s = up(s.cursor, &s.text).unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (Some(1), "second"));
        let s = up(s.cursor, &s.text).unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (Some(2), "first"));
        let s = up(s.cursor, &s.text).unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (Some(2), "first"), "stops at the oldest");

        let s = down(Some(1), "second").unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (Some(0), "third"));
        let s = down(s.cursor, &s.text).unwrap();
        assert_eq!((s.cursor, s.text.as_str()), (None, ""), "past the newest: back to empty");
        assert_eq!(down(None, ""), None);
    }

    #[test]
    fn a_draft_or_an_edited_recall_keeps_the_arrows_for_the_caret() {
        let h = hist();
        assert_eq!(step(&h, None, "typing", Direction::Older), None);
        assert_eq!(step(&h, Some(0), "third, edited", Direction::Older), None);
        assert_eq!(step(&h, Some(0), "third, edited", Direction::Newer), None);
        assert_eq!(step(&[], None, "", Direction::Older), None);
    }
}
