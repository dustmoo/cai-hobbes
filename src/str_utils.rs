//! Shared string utilities used across the context and session modules.
//!
//! Centralised here to prevent duplication — both `session.rs` (delivery-time
//! pagination) and `context/prompt_builder.rs` (build-time condensation) need
//! the same UTF-8-safe boundary logic.

/// Snap a byte index DOWN to the nearest valid UTF-8 char boundary.
///
/// Prevents panics when slicing multi-byte characters (emojis, CJK, etc.).
/// Always returns a value in `0..=s.len()`.
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find a clean split point within `content` at approximately `budget` chars.
///
/// Tries to split at semantic boundaries in this priority order, searching
/// within the last 20% of the budget window:
/// 1. JSON object boundary (`},`)
/// 2. JSON array boundary (`],`)
/// 3. Paragraph break (`\n\n`)
/// 4. Line break (`\n`)
/// 5. Fallback: first valid char boundary at or before `budget`
///
/// Returns a byte offset guaranteed to be a valid UTF-8 char boundary.
/// The caller splits `content` as `&content[..split_at]` / `&content[split_at..]`.
pub fn find_split_point(content: &str, budget: usize) -> usize {
    let safe_end = floor_char_boundary(content, budget);
    let mut split_at = safe_end;

    let search_start = floor_char_boundary(content, (budget as f64 * 0.8) as usize);
    if search_start < safe_end {
        let search_slice = &content[search_start..safe_end];
        if let Some(pos) = search_slice.rfind("},") {
            split_at = search_start + pos + 2;
        } else if let Some(pos) = search_slice.rfind("],") {
            split_at = search_start + pos + 2;
        } else if let Some(pos) = search_slice.rfind("\n\n") {
            split_at = search_start + pos + 2;
        } else if let Some(pos) = search_slice.rfind('\n') {
            split_at = search_start + pos + 1;
        }
    }

    if split_at == 0 {
        // Absolute fallback: take at least one character so callers never
        // produce an infinite loop on a non-empty input.
        split_at = content
            .char_indices()
            .nth(1)
            .map(|(i, _)| i)
            .unwrap_or(content.len());
    }

    split_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_ascii() {
        assert_eq!(floor_char_boundary("hello", 3), 3);
        assert_eq!(floor_char_boundary("hello", 10), 5); // clamped to len
    }

    #[test]
    fn floor_char_boundary_multibyte() {
        let s = "héllo"; // 'é' is 2 bytes at index 1
        // index 2 is inside 'é' — should snap back to 1
        assert_eq!(floor_char_boundary(s, 2), 1);
        // index 3 is start of 'l' — valid boundary
        assert_eq!(floor_char_boundary(s, 3), 3);
    }

    #[test]
    fn find_split_point_line_boundary() {
        let content = "line one\nline two\nline three";
        // Budget that falls mid-word in "two" — should snap to after "line one\n"
        let split = find_split_point(content, 12);
        assert!(content.is_char_boundary(split));
        assert!(split <= 12);
    }

    #[test]
    fn find_split_point_json_boundary() {
        let content = r#"{"a":1},{"b":2},{"c":3}"#;
        let split = find_split_point(content, 16);
        assert!(content.is_char_boundary(split));
        // Should split after a `},`
        assert!(split > 0);
    }

    #[test]
    fn find_split_point_empty() {
        // Empty string → split at 0 with no panic
        let split = find_split_point("", 100);
        assert_eq!(split, 0);
    }

    #[test]
    fn find_split_point_shorter_than_budget() {
        let content = "short";
        let split = find_split_point(content, 1000);
        assert_eq!(split, content.len());
    }
}
