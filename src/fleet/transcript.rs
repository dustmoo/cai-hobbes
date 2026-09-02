//! Claude Code transcript tail-reading and digestion.
//!
//! Transcripts are append-ordered JSONL (`~/.claude/projects/<slug>/<id>.jsonl`,
//! up to a few MB). Only `user` and `assistant` lines matter for a brief;
//! everything else (`mode`, `ai-title`, `attachment`, `file-history-*`, …) is
//! UI/undo bookkeeping. The digest is built pure so it's fully testable on
//! fixture strings; only [`read_tail`] touches the filesystem, and it is only
//! ever called from the background brief task — never on the hook hot path.

use serde_json::Value;

/// How much of the file tail to read. 400KB comfortably covers the recent
/// turns of even tool-heavy sessions (~3KB per line average).
pub const TAIL_MAX_BYTES: u64 = 400 * 1024;
/// Typed-user turn boundaries to walk back.
pub const DIGEST_MAX_TURNS: usize = 6;
/// Hard cap on digest text handed to the LLM.
pub const DIGEST_MAX_CHARS: usize = 24_000;
/// Per-tool-call input clip in the rendered digest.
pub const TOOL_INPUT_CLIP: usize = 200;

/// Read the last `max_bytes` of the file. When the read starts mid-file the
/// first (possibly partial) line is discarded.
pub fn read_tail(path: &str, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Mid-file cut: drop everything up to and including the first newline.
        match text.find('\n') {
            Some(nl) => text.drain(..=nl),
            None => text.drain(..),
        };
    }
    Ok(text)
}

/// A transcript tail rendered for the brief prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptDigest {
    /// "USER: …" / "ASSISTANT: …" / "[tool: name {input…}]" lines.
    pub text: String,
    /// Typed-user turn boundaries covered.
    pub turn_count: usize,
}

/// Is this a typed human prompt (vs. a tool result or synthetic message)?
fn is_typed_user(line: &Value) -> bool {
    if line.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return false;
    }
    if line.get("toolUseResult").is_some() {
        return false;
    }
    // promptSource "system" marks synthetic user lines (task notifications
    // etc.). Absent promptSource on a non-tool-result user line is treated as
    // typed — older transcript versions may lack the field.
    !matches!(
        line.get("promptSource").and_then(Value::as_str),
        Some(s) if s != "typed"
    )
}

/// Extract the user's prompt text from a typed user line.
fn user_text(line: &Value) -> String {
    let content = line.pointer("/message/content");
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Render an assistant line's content blocks: text kept, tool calls named
/// with clipped input, thinking dropped.
fn assistant_text(line: &Value) -> String {
    let Some(Value::Array(blocks)) = line.pointer("/message/content") else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        out.push(t.to_string());
                    }
                }
            }
            Some("tool_use") => {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                let input = b
                    .get("input")
                    .map(|i| serde_json::to_string(i).unwrap_or_default())
                    .unwrap_or_default();
                out.push(format!(
                    "[tool: {} {}]",
                    name,
                    crate::fleet::truncate_summary(&input, TOOL_INPUT_CLIP)
                ));
            }
            _ => {} // thinking, images, …
        }
    }
    out.join("\n")
}

/// Digest the tail: keep the last `max_turns` typed-user turns (or whatever
/// the tail holds), tolerating unparseable lines, capped at `max_chars`
/// (truncated from the front — the most recent activity survives).
pub fn digest_transcript(tail: &str, max_turns: usize, max_chars: usize) -> TranscriptDigest {
    // Collect rendered lines in file order, remembering typed-user positions.
    let mut rendered: Vec<(bool, String)> = Vec::new(); // (is_typed_user_turn, text)
    for raw in tail.lines() {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if is_typed_user(&v) {
                    let t = user_text(&v);
                    if !t.trim().is_empty() {
                        rendered.push((true, format!("USER: {}", t.trim())));
                    }
                }
            }
            Some("assistant") => {
                let t = assistant_text(&v);
                if !t.is_empty() {
                    rendered.push((false, format!("ASSISTANT: {}", t)));
                }
            }
            _ => {}
        }
    }

    // Walk back to the Nth typed-user boundary.
    let mut turn_count = 0usize;
    let mut start = 0usize;
    for (i, (is_turn, _)) in rendered.iter().enumerate().rev() {
        if *is_turn {
            turn_count += 1;
            if turn_count >= max_turns {
                start = i;
                break;
            }
        }
    }
    let mut text = rendered[start..]
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Cap from the front, keeping the end (most recent), on a char boundary.
    let total = text.chars().count();
    if total > max_chars {
        text = format!(
            "…{}",
            text.chars()
                .skip(total - max_chars.saturating_sub(1))
                .collect::<String>()
        );
    }
    TranscriptDigest { text, turn_count }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_line(text: &str, source: &str) -> String {
        serde_json::json!({
            "type": "user", "promptSource": source,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn tool_result_line() -> String {
        serde_json::json!({
            "type": "user",
            "toolUseResult": {"stdout": "big output"},
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "big"}]}
        })
        .to_string()
    }

    fn assistant_line(blocks: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": blocks}
        })
        .to_string()
    }

    #[test]
    fn digest_keeps_typed_users_and_assistant_text_drops_the_rest() {
        let tail = [
            user_line("fix the bug", "typed"),
            assistant_line(serde_json::json!([
                {"type": "thinking", "thinking": "secret reasoning"},
                {"type": "text", "text": "Found it in foo.rs"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
            ])),
            tool_result_line(),
            user_line("<task-notification>done</task-notification>", "system"),
            serde_json::json!({"type": "mode", "mode": "plan"}).to_string(),
            "not json at all".to_string(),
        ]
        .join("\n");
        let d = digest_transcript(&tail, 6, 10_000);
        assert!(d.text.contains("USER: fix the bug"));
        assert!(d.text.contains("Found it in foo.rs"));
        assert!(d.text.contains("[tool: Bash"));
        assert!(d.text.contains("cargo test"));
        assert!(!d.text.contains("secret reasoning"), "thinking dropped");
        assert!(!d.text.contains("big output"), "tool results dropped");
        assert!(!d.text.contains("task-notification"), "synthetic user dropped");
        assert_eq!(d.turn_count, 1);
    }

    #[test]
    fn digest_walks_back_max_turns_boundaries() {
        let mut lines = Vec::new();
        for i in 0..10 {
            lines.push(user_line(&format!("prompt {i}"), "typed"));
            lines.push(assistant_line(
                serde_json::json!([{"type": "text", "text": format!("answer {i}")}]),
            ));
        }
        let d = digest_transcript(&lines.join("\n"), 3, 100_000);
        assert_eq!(d.turn_count, 3);
        assert!(!d.text.contains("prompt 6"));
        assert!(d.text.contains("prompt 7"));
        assert!(d.text.contains("answer 9"));
    }

    #[test]
    fn is_meta_user_lines_are_dropped() {
        let line = serde_json::json!({
            "type": "user", "isMeta": true,
            "message": {"role": "user", "content": "<local-command-caveat>x</local-command-caveat>"}
        })
        .to_string();
        let d = digest_transcript(&line, 6, 10_000);
        assert!(d.text.is_empty());
        assert_eq!(d.turn_count, 0);
    }

    #[test]
    fn missing_prompt_source_counts_as_typed() {
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "older format prompt"}
        })
        .to_string();
        let d = digest_transcript(&line, 6, 10_000);
        assert!(d.text.contains("older format prompt"));
        assert_eq!(d.turn_count, 1);
    }

    #[test]
    fn tool_input_is_clipped() {
        let tail = assistant_line(serde_json::json!([
            {"type": "tool_use", "name": "Write", "input": {"content": "x".repeat(5000)}},
        ]));
        let d = digest_transcript(&tail, 6, 100_000);
        let tool_line = d.text.lines().find(|l| l.contains("[tool:")).unwrap();
        assert!(tool_line.chars().count() < TOOL_INPUT_CLIP + 40);
        assert!(tool_line.contains('…'));
    }

    #[test]
    fn char_cap_keeps_the_end() {
        let tail = [
            user_line(&format!("early {}", "a".repeat(500)), "typed"),
            user_line(&format!("late {}", "b".repeat(100)), "typed"),
        ]
        .join("\n");
        let d = digest_transcript(&tail, 6, 200);
        assert!(d.text.chars().count() <= 200);
        assert!(d.text.contains("bbb"), "most recent content survives");
        assert!(d.text.starts_with('…'));
    }

    #[test]
    fn read_tail_drops_partial_first_line() {
        let dir = std::env::temp_dir().join(format!("hobbes_tail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let content = format!("{}\n{}\n", "x".repeat(100), "{\"type\":\"mode\"}");
        std::fs::write(&path, &content).unwrap();
        // Read fewer bytes than the file: the cut lands inside line 1.
        let tail = read_tail(path.to_str().unwrap(), 30).unwrap();
        assert_eq!(tail, "{\"type\":\"mode\"}\n");
        // Reading the whole file keeps line 1.
        let full = read_tail(path.to_str().unwrap(), 10_000).unwrap();
        assert_eq!(full, content);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
