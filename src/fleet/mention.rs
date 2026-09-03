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

/// The unique typeable handle for a session: its cwd-derived name, suffixed
/// with a short id (`name~3f2a`) whenever another live session shares the
/// name — so every window is addressable individually.
pub fn mention_handle(s: &FleetSession, live: &FleetState) -> String {
    let duplicated = live
        .sessions
        .values()
        .any(|o| o.id != s.id && o.name.eq_ignore_ascii_case(&s.name));
    if duplicated {
        let short: String = s.id.chars().filter(|c| c.is_ascii_alphanumeric()).take(4).collect();
        format!("{}~{}", s.name, short)
    } else {
        s.name.clone()
    }
}

/// (handle, session id) pairs for every live session.
pub fn mention_handles(live: &FleetState) -> Vec<(String, String)> {
    live.sessions
        .values()
        .map(|s| (mention_handle(s, live), s.id.clone()))
        .collect()
}

/// Session ids mentioned in `text` (case-insensitive exact handle match
/// after punctuation trimming), deduped in order of first appearance. A
/// plain name shared by several windows matches nothing — ambiguity never
/// attaches the wrong window's status; the autocomplete inserts unique
/// handles.
pub fn detect_mentions(text: &str, handles: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        let raw = trim_token(raw);
        if raw.is_empty() {
            continue;
        }
        if let Some((_, id)) = handles.iter().find(|(h, _)| h.eq_ignore_ascii_case(raw)) {
            if !out.contains(id) {
                out.push(id.clone());
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
        s.display_name(),
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
    // Mentions match unique handles (name, or name~id when windows share a
    // repo) — titles have spaces, so they ride the autocomplete instead.
    let handles = mention_handles(live);
    let mentioned = detect_mentions(message, &handles);
    if mentioned.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    for id in &mentioned {
        if let Some(s) = live.sessions.get(id) {
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
        let handles = vec![
            ("cai-hobbes".to_string(), "id-1".to_string()),
            ("clearmirror.ai-2025".to_string(), "id-2".to_string()),
        ];
        let found = detect_mentions(
            "How is @CAI-HOBBES, and @clearmirror.ai-2025? Also @unknown and plain@text.",
            &handles,
        );
        assert_eq!(found, vec!["id-1".to_string(), "id-2".to_string()]);
        // Deduped.
        assert_eq!(detect_mentions("@cai-hobbes @cai-hobbes", &handles).len(), 1);
    }

    #[test]
    fn duplicate_window_names_get_unique_handles() {
        // Two terminals in the same repo + one unique.
        let mut state = state_with(&["puget-bench", "solo"]);
        crate::fleet::reduce(
            &mut state,
            &FleetEvent::SessionStart {
                session_id: "s9".into(),
                cwd: "/Users/x/Sites/puget-bench".into(),
                reason: "startup".into(),
            },
            Utc::now(),
        );
        let handles = mention_handles(&state);
        // Unique name stays plain; duplicated names carry ~id suffixes.
        assert!(handles.iter().any(|(h, _)| h == "solo"));
        let dup: Vec<&(String, String)> =
            handles.iter().filter(|(h, _)| h.starts_with("puget-bench~")).collect();
        assert_eq!(dup.len(), 2, "each window individually addressable: {handles:?}");
        // Handles are unique.
        let mut hs: Vec<&str> = handles.iter().map(|(h, _)| h.as_str()).collect();
        hs.sort_unstable();
        hs.dedup();
        assert_eq!(hs.len(), handles.len());

        // A plain ambiguous name matches nothing (never the wrong window);
        // its unique handle matches exactly one.
        assert!(detect_mentions("check @puget-bench", &handles).is_empty());
        let (h, id) = dup[0];
        assert_eq!(detect_mentions(&format!("check @{h}"), &handles), vec![id.clone()]);
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
