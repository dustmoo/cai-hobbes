// Pure skill-invocation detection: finds `/name` command tokens anywhere in a
// message (not just at the start), and powers cursor-aware autocomplete.
// Kept free of Dioxus/UI types so it's testable headlessly.

/// A skill invocation detected inside a message.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInvocation {
    pub skill_name: String,
    /// Text after the command token on the SAME line, trimmed. Everything else
    /// in the message stays conversational context (it reaches the skill turn
    /// through normal message history).
    pub arguments: String,
    /// Byte range of the raw `/name` token within the message.
    pub token_range: (usize, usize),
}

/// Punctuation that may trail a command token in prose ("try /research.").
const TRAILING_PUNCT: &[char] = &['.', ',', '!', '?', ':', ';'];

/// Scan `message` for the first whitespace-delimited token of the form
/// `/name` where `name` matches a known skill. Rules that kill false
/// positives on paths (`/usr/bin`), URLs, and fractions:
/// - the token must START with `/` (so `https://x.co/foo` and `a/b` never match)
/// - the remainder must be non-empty and contain no further `/` (so `/usr/bin` never matches)
/// - after stripping at most one trailing punctuation char, the remainder must
///   exactly match a known skill name (case-sensitive, same as `get_skill`)
///
/// The first qualifying token wins; later ones are ignored.
pub fn detect_skill_invocation(
    message: &str,
    is_known: impl Fn(&str) -> bool,
) -> Option<SkillInvocation> {
    for (start, token) in tokens_with_offsets(message) {
        let Some(rest) = token.strip_prefix('/') else {
            continue;
        };
        if rest.is_empty() || rest.contains('/') {
            continue;
        }
        let candidate = rest.strip_suffix(TRAILING_PUNCT).unwrap_or(rest);
        if candidate.is_empty() || !is_known(candidate) {
            continue;
        }

        let end = start + token.len();
        // Arguments: rest of the same line after the token
        let line_end = message[end..]
            .find('\n')
            .map(|i| end + i)
            .unwrap_or(message.len());
        let arguments = message[end..line_end].trim().to_string();

        return Some(SkillInvocation {
            skill_name: candidate.to_string(),
            arguments,
            token_range: (start, end),
        });
    }
    None
}

/// Whitespace-delimited tokens with their byte offsets.
fn tokens_with_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_whitespace()
        .scan(0usize, move |search_from, token| {
            // split_whitespace yields tokens in order; find each from the last
            // position to recover byte offsets without re-tokenizing.
            let start = text[*search_from..]
                .find(token)
                .map(|i| *search_from + i)
                .expect("token must exist in source text");
            *search_from = start + token.len();
            Some((start, token))
        })
}

/// An in-progress `/command` token at the cursor, for autocomplete.
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteQuery {
    /// The partial skill name typed so far (may be empty right after `/`).
    pub query: String,
    /// Byte range of the token in the text: from the `/` to the end of the
    /// full token (including any characters after the cursor).
    pub token_range: (usize, usize),
}

/// Given the textarea content and the cursor position in UTF-16 code units
/// (JS `selectionStart`), return the `/query` token the cursor is inside, if
/// any. The query is the text between `/` and the cursor.
pub fn autocomplete_query_at(text: &str, cursor_utf16: usize) -> Option<AutocompleteQuery> {
    let cursor = utf16_to_byte_offset(text, cursor_utf16);

    // Scan back from the cursor to the previous whitespace (or start of text)
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map(|i| i + text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1))
        .unwrap_or(0);

    let before_cursor = &text[token_start..cursor];
    let rest = before_cursor.strip_prefix('/')?;
    if rest.contains('/') || rest.chars().any(char::is_whitespace) {
        return None;
    }

    // Extend to the end of the full token (cursor may sit mid-token)
    let token_end = text[cursor..]
        .find(char::is_whitespace)
        .map(|i| cursor + i)
        .unwrap_or(text.len());

    Some(AutocompleteQuery {
        query: rest.to_string(),
        token_range: (token_start, token_end),
    })
}

/// Convert a UTF-16 code-unit offset (JS `selectionStart`) to a byte offset.
/// Clamps to the end of the string, and snaps to char boundaries.
pub fn utf16_to_byte_offset(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if units >= utf16_offset {
            return byte_idx;
        }
        units += ch.len_utf16();
    }
    text.len()
}

/// Convert a byte offset in `text` to UTF-16 code units (for restoring the JS
/// cursor after splicing).
pub fn byte_to_utf16_offset(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())]
        .chars()
        .map(char::len_utf16)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |n| names.contains(&n)
    }

    // ── detect_skill_invocation ────────────────────────────────────────────

    #[test]
    fn detects_at_start_of_message() {
        let inv = detect_skill_invocation("/research topic one", known(&["research"])).unwrap();
        assert_eq!(inv.skill_name, "research");
        assert_eq!(inv.arguments, "topic one");
        assert_eq!(inv.token_range, (0, 9));
    }

    #[test]
    fn detects_mid_message() {
        let msg = "Here is some context about the task. Now /research rust runtimes";
        let inv = detect_skill_invocation(msg, known(&["research"])).unwrap();
        assert_eq!(inv.skill_name, "research");
        assert_eq!(inv.arguments, "rust runtimes");
        assert_eq!(&msg[inv.token_range.0..inv.token_range.1], "/research");
    }

    #[test]
    fn detects_at_end_of_message_with_empty_args() {
        let inv =
            detect_skill_invocation("please summarize then /digest", known(&["digest"])).unwrap();
        assert_eq!(inv.skill_name, "digest");
        assert_eq!(inv.arguments, "");
    }

    #[test]
    fn arguments_are_same_line_only() {
        let msg = "context first\n/research rust async\nand this line is more context";
        let inv = detect_skill_invocation(msg, known(&["research"])).unwrap();
        assert_eq!(inv.arguments, "rust async");
    }

    #[test]
    fn rejects_paths_and_urls() {
        let is_known = known(&["research", "usr", "bin", "foo"]);
        assert!(detect_skill_invocation("run /usr/bin/env please", &is_known).is_none());
        assert!(detect_skill_invocation("see https://x.co/foo for info", &is_known).is_none());
        assert!(detect_skill_invocation("the ratio a/b is fine", &is_known).is_none());
        assert!(detect_skill_invocation("path/to/foo", &is_known).is_none());
    }

    #[test]
    fn rejects_unknown_names_and_bare_slash() {
        assert!(detect_skill_invocation("/unknown args", known(&["research"])).is_none());
        assert!(detect_skill_invocation("a / b", known(&["research"])).is_none());
        assert!(detect_skill_invocation("nothing here", known(&["research"])).is_none());
    }

    #[test]
    fn rejects_slash_inside_longer_token() {
        assert!(detect_skill_invocation("foo/research", known(&["research"])).is_none());
    }

    #[test]
    fn strips_one_trailing_punctuation() {
        let inv = detect_skill_invocation("you could try /research.", known(&["research"])).unwrap();
        assert_eq!(inv.skill_name, "research");
        assert_eq!(inv.arguments, "");
        // Only ONE trailing punct is stripped — "/research.." stays unknown
        assert!(detect_skill_invocation("try /research..", known(&["research"])).is_none());
    }

    #[test]
    fn first_qualifying_token_wins() {
        let inv = detect_skill_invocation(
            "/digest then also /research things",
            known(&["digest", "research"]),
        )
        .unwrap();
        assert_eq!(inv.skill_name, "digest");
        assert_eq!(inv.arguments, "then also /research things");
    }

    #[test]
    fn unicode_context_offsets_are_correct() {
        let msg = "résumé context 🚀 then /research crème brûlée";
        let inv = detect_skill_invocation(msg, known(&["research"])).unwrap();
        assert_eq!(&msg[inv.token_range.0..inv.token_range.1], "/research");
        assert_eq!(inv.arguments, "crème brûlée");
    }

    // ── autocomplete_query_at ──────────────────────────────────────────────

    #[test]
    fn autocomplete_at_start() {
        let q = autocomplete_query_at("/res", 4).unwrap();
        assert_eq!(q.query, "res");
        assert_eq!(q.token_range, (0, 4));
    }

    #[test]
    fn autocomplete_bare_slash_mid_message() {
        let text = "some context /";
        let q = autocomplete_query_at(text, 14).unwrap();
        assert_eq!(q.query, "");
        assert_eq!(q.token_range, (13, 14));
    }

    #[test]
    fn autocomplete_mid_message_token() {
        let text = "explain this then /rese and continue";
        // cursor right after "/rese" (byte & utf16 offset 23 here)
        let q = autocomplete_query_at(text, 23).unwrap();
        assert_eq!(q.query, "rese");
        assert_eq!(&text[q.token_range.0..q.token_range.1], "/rese");
    }

    #[test]
    fn autocomplete_cursor_mid_token_extends_range_to_token_end() {
        let text = "try /research now";
        // cursor after "/rese" (offset 9); full token is "/research"
        let q = autocomplete_query_at(text, 9).unwrap();
        assert_eq!(q.query, "rese");
        assert_eq!(&text[q.token_range.0..q.token_range.1], "/research");
    }

    #[test]
    fn autocomplete_rejects_paths_and_plain_text() {
        assert!(autocomplete_query_at("see /usr/bi", 11).is_none());
        assert!(autocomplete_query_at("hello world", 11).is_none());
        assert!(autocomplete_query_at("", 0).is_none());
    }

    #[test]
    fn autocomplete_handles_utf16_cursor_after_emoji() {
        // "🚀 /re" — the rocket is 2 UTF-16 units / 4 bytes.
        let text = "🚀 /re";
        // JS selectionStart at end = 2 (emoji) + 1 (space) + 3 = 6
        let q = autocomplete_query_at(text, 6).unwrap();
        assert_eq!(q.query, "re");
        assert_eq!(&text[q.token_range.0..q.token_range.1], "/re");
    }

    #[test]
    fn utf16_conversions_round_trip() {
        let text = "a🚀b héllo";
        for (byte_idx, _) in text.char_indices() {
            let utf16 = byte_to_utf16_offset(text, byte_idx);
            assert_eq!(utf16_to_byte_offset(text, utf16), byte_idx);
        }
        assert_eq!(
            utf16_to_byte_offset(text, byte_to_utf16_offset(text, text.len())),
            text.len()
        );
        // Clamping beyond the end
        assert_eq!(utf16_to_byte_offset(text, 999), text.len());
    }
}
