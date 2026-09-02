//! `@session` mentions in chat: the user references a fleet session by name
//! and its current status/brief is appended to the outgoing message as a
//! frozen-at-send context block. Message-scoped by design — the status at
//! send time is what the conversation was about; a durable "follow this
//! project" pin is a separate future mechanism.

use chrono::{DateTime, NaiveDate, Utc};

use super::{truncate_summary, FleetSession, FleetState};
use crate::skills::invocation::utf16_to_byte_offset;

/// The `@query` token under the cursor, for the autocomplete popover.
/// Mirrors `skills::invocation::autocomplete_query_at` with an `@` prefix and
/// no line-start rule — mentions can appear mid-sentence.
pub struct MentionQuery {
    pub query: String,
    /// Byte range of the full token (including the `@`).
    pub token_range: (usize, usize),
}

pub fn mention_query_at(text: &str, cursor_utf16: usize) -> Option<MentionQuery> {
    let cursor = utf16_to_byte_offset(text, cursor_utf16);
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map(|i| i + text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1))
        .unwrap_or(0);
    let before_cursor = &text[token_start..cursor];
    let rest = before_cursor.strip_prefix('@')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    let token_end = text[cursor..]
        .find(char::is_whitespace)
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    Some(MentionQuery {
        query: rest.to_string(),
        token_range: (token_start, token_end),
    })
}

/// Trailing sentence punctuation a mention token sheds before matching
/// (session names themselves contain dots and hyphens, so only obvious
/// sentence-enders are trimmed).
fn trim_token(token: &str) -> &str {
    token.trim_end_matches([',', ';', ':', '!', '?', ')', '"', '\''])
}

/// Canonical session names mentioned in `text` (case-insensitive exact match
/// after punctuation trimming), deduped in order of first appearance.
pub fn detect_mentions(text: &str, names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        let raw = trim_token(raw);
        if raw.is_empty() {
            continue;
        }
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(raw)) {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// One session's status rendered for the appended block.
fn render_session(s: &FleetSession, now: DateTime<Utc>, today: NaiveDate) -> String {
    let status = match &s.status {
        super::FleetStatus::Working => "working".to_string(),
        super::FleetStatus::WorkingBackground => "working (background agents)".to_string(),
        super::FleetStatus::Idle => "idle".to_string(),
        super::FleetStatus::NeedsAttention(super::AttentionKind::Gate) => {
            "waiting on a permission approval".to_string()
        }
        super::FleetStatus::NeedsAttention(super::AttentionKind::Notification {
            message, ..
        }) => format!("needs attention — {}", truncate_summary(message, 120)),
    };
    let mut lines = vec![format!(
        "{} — {} · {} today",
        s.name,
        status,
        crate::todo::model::format_minutes(s.minutes_on(today, now))
    )];
    if let Some(b) = &s.brief {
        lines.push(format!("  brief: {}", truncate_summary(&b.headline, 200)));
        for bullet in b.bullets.iter().take(3) {
            lines.push(format!("  - {}", truncate_summary(bullet, 120)));
        }
        if let Some(blocked) = &b.blocked_on {
            lines.push(format!("  blocked: {}", truncate_summary(blocked, 160)));
        }
    }
    if let Some(g) = &s.pending_gate {
        lines.push(format!(
            "  pending approval: {} {}",
            g.tool_name,
            truncate_summary(&g.input_summary, 120)
        ));
    }
    lines.join("\n")
}

/// Separator between the user's text and the appended status block. Stored
/// with the message (the model sees it; history stays honest); the bubble
/// splits on it to render the block as a collapsed disclosure instead.
pub const EXPANSION_MARKER: &str = "\n\n---\n[fleet status at send time]\n";

/// Split a message into (user text, fleet block) when it carries an
/// expansion. Display-side only — the stored text is never rewritten.
pub fn split_expansion(text: &str) -> Option<(&str, &str)> {
    text.split_once(EXPANSION_MARKER)
}

/// Expand `@name` mentions against the live fleet: returns the message with a
/// status block appended, or `None` when nothing matched (send unchanged).
pub fn expand(
    message: &str,
    live: &FleetState,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> Option<String> {
    let names: Vec<String> = live.sessions.values().map(|s| s.name.clone()).collect();
    let mentioned = detect_mentions(message, &names);
    if mentioned.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    for name in &mentioned {
        if let Some(s) = live.sessions.values().find(|s| &s.name == name) {
            blocks.push(render_session(s, now, today));
        }
    }
    Some(format!(
        "{}{}{}",
        message,
        EXPANSION_MARKER,
        blocks.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::events::FleetEvent;

    fn state_with(names: &[&str]) -> FleetState {
        let mut state = FleetState::default();
        for (i, n) in names.iter().enumerate() {
            crate::fleet::reduce(
                &mut state,
                &FleetEvent::SessionStart {
                    session_id: format!("s{i}"),
                    cwd: format!("/Users/x/Sites/{n}"),
                    reason: "startup".into(),
                },
                Utc::now(),
            );
        }
        state
    }

    #[test]
    fn mention_query_mid_sentence_and_prefix_rules() {
        let text = "check @cai-h please";
        // Cursor right after "@cai-h" (12 utf16 units in ASCII).
        let q = mention_query_at(text, 12).unwrap();
        assert_eq!(q.query, "cai-h");
        assert_eq!(&text[q.token_range.0..q.token_range.1], "@cai-h");
        // No @ token under cursor → None.
        assert!(mention_query_at("plain text", 5).is_none());
        // Email-ish text: cursor inside "x@y" — token starts at 'x', no @ prefix.
        assert!(mention_query_at("mail x@y now", 8).is_none());
    }

    #[test]
    fn detect_matches_case_insensitively_and_trims_punctuation() {
        let names = vec!["cai-hobbes".to_string(), "clearmirror.ai-2025".to_string()];
        let found = detect_mentions(
            "How is @CAI-HOBBES, and @clearmirror.ai-2025? Also @unknown and plain@text.",
            &names,
        );
        assert_eq!(found, vec!["cai-hobbes".to_string(), "clearmirror.ai-2025".to_string()]);
        // Deduped.
        assert_eq!(
            detect_mentions("@cai-hobbes @cai-hobbes", &names).len(),
            1
        );
    }

    #[test]
    fn expand_appends_status_block_only_on_match() {
        let state = state_with(&["cai-hobbes", "puget-bench"]);
        let now = Utc::now();
        let today = chrono::Local::now().date_naive();
        let out = expand("what's @cai-hobbes doing?", &state, now, today).unwrap();
        assert!(out.starts_with("what's @cai-hobbes doing?"));
        assert!(out.contains("[fleet status at send time]"));
        assert!(out.contains("cai-hobbes — working"));
        assert!(!out.contains("puget-bench"), "unmentioned sessions stay out");
        assert!(expand("no mentions here", &state, now, today).is_none());

        // Display split recovers the user text and the block.
        let (main, block) = split_expansion(&out).unwrap();
        assert_eq!(main, "what's @cai-hobbes doing?");
        assert!(block.contains("cai-hobbes — working"));
        assert!(split_expansion("plain message").is_none());
    }
}
