//! Diagnosing stuck workers: when a wake carries a worker that is stuck,
//! needs the person, or finished with an open item, the wake message
//! includes a short tail of that worker's own record — the Claude Code
//! transcript for a dispatched run (through `fleet::transcript`'s digest),
//! or the last few messages of a Hobbes chat worker — so the Heartbeat can
//! name the actual blocker instead of "it's stuck".
//!
//! Everything here is pure except the runtime's file read; the tail goes
//! only into the wake message, to the person's own provider, like the rest
//! of the wake.

use std::collections::HashSet;

use super::perception::*;
use super::HeartbeatEvent;
use crate::components::chat::Message;
use crate::components::shared::MessageContent;

/// A tail is capped at this many characters (cut from the front, so the
/// most recent activity survives).
pub const DIAGNOSIS_MAX_CHARS: usize = 1_500;
/// At most this many workers are diagnosed per wake.
pub const MAX_DIAGNOSES: usize = 3;
/// Bytes of a transcript's end read for a diagnosis.
pub const TRANSCRIPT_TAIL_BYTES: u64 = 96 * 1024;
/// Typed-user turns a transcript tail walks back.
const TRANSCRIPT_TURNS: usize = 2;
/// Chat messages a Hobbes worker's tail shows.
const CHAT_MESSAGES: usize = 6;
/// One chat message is clipped to this before the overall cap.
const CHAT_MESSAGE_CLIP: usize = 600;
const TOOL_RESPONSE_CLIP: usize = 200;

/// A worker the wake should diagnose.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosisTarget {
    pub session_id: String,
    pub todo_id: Option<String>,
    /// "stuck", "needs you", "finished with an open item", …
    pub state: &'static str,
}

/// A rendered diagnosis for the wake message.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnosis {
    /// The worker's to-do title, or its session's name.
    pub subject: String,
    pub state: &'static str,
    /// "Claude Code transcript" or "chat".
    pub source: &'static str,
    pub tail: String,
}

/// The workers a wake's events point at: stuck, needing the person, raising
/// a decision, finished with an open item, or a dispatched session needing
/// attention. Newest first, one per session, at most [`MAX_DIAGNOSES`].
pub fn diagnosis_targets(events: &[HeartbeatEvent]) -> Vec<DiagnosisTarget> {
    let mut evs: Vec<&HeartbeatEvent> = events.iter().collect();
    evs.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in evs {
        let state = match e.kind.as_str() {
            KIND_WORKER_STUCK => "stuck",
            KIND_WORKER_NEEDS_YOU => "needs you",
            KIND_DECISION_RAISED => "raised a decision",
            KIND_WORKER_REVIEW => "finished with an open item",
            KIND_SESSION_NEEDS_ATTENTION if !is_fleet_noise(e) => "needs attention",
            _ => continue,
        };
        let Some(sid) = e.session_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !seen.insert(sid.to_string()) {
            continue;
        }
        out.push(DiagnosisTarget { session_id: sid.to_string(), todo_id: e.todo_id.clone(), state });
        if out.len() >= MAX_DIAGNOSES {
            break;
        }
    }
    out
}

/// Keep the last `max` characters, marking the cut with "…".
fn cap_front(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    format!("…{}", text.chars().skip(total - max.saturating_sub(1)).collect::<String>())
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// The tail of a Claude Code transcript (JSONL text from
/// `fleet::transcript::read_tail`), digested to the last couple of turns
/// and capped at [`DIAGNOSIS_MAX_CHARS`]. `None` when nothing readable.
pub fn transcript_tail(jsonl: &str) -> Option<String> {
    let d = crate::fleet::transcript::digest_transcript(jsonl, TRANSCRIPT_TURNS, DIAGNOSIS_MAX_CHARS);
    let text = d.text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The last few messages of a Hobbes chat worker, capped at
/// [`DIAGNOSIS_MAX_CHARS`]. `None` when it has no messages to show.
pub fn chat_tail(messages: &[Message]) -> Option<String> {
    let lines: Vec<String> = messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Text { content, .. } if !content.trim().is_empty() => {
                let who = if m.author == "User" { "USER" } else { "HOBBES" };
                Some(format!("{who}: {}", clip(content, CHAT_MESSAGE_CLIP)))
            }
            MessageContent::ToolCall(tc) => {
                let resp = clip(&tc.response.replace('\n', " "), TOOL_RESPONSE_CLIP);
                Some(if resp.is_empty() {
                    format!("[tool: {} — {}]", tc.tool_name, tc.status)
                } else {
                    format!("[tool: {} — {}: {}]", tc.tool_name, tc.status, resp)
                })
            }
            MessageContent::PermissionRequest(tc) => {
                Some(format!("[held for the user's approval: {}]", tc.tool_name))
            }
            _ => None,
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let tail = lines[lines.len().saturating_sub(CHAT_MESSAGES)..].join("\n");
    Some(cap_front(&tail, DIAGNOSIS_MAX_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::{EventSource, Urgency};
    use chrono::{Duration, TimeZone, Utc};

    fn ev(kind: &str, sid: Option<&str>, mins_ago: i64) -> HeartbeatEvent {
        let at = Utc.with_ymd_and_hms(2026, 10, 22, 14, 0, 0).unwrap() - Duration::minutes(mins_ago);
        let mut e = HeartbeatEvent::new(EventSource::Planner, kind, "x", Urgency::High, at);
        e.session_id = sid.map(str::to_string);
        e.todo_id = Some("t".into());
        e
    }

    #[test]
    fn targets_are_worker_trouble_newest_first_one_per_session() {
        let mut noise = ev(KIND_SESSION_NEEDS_ATTENTION, Some("terminal"), 0);
        noise.todo_id = None;
        let events = vec![
            ev(KIND_WORKER_STUCK, Some("a"), 10),
            ev(KIND_DECISION_RAISED, Some("a"), 5),
            ev(KIND_WORKER_DONE, Some("b"), 1),
            ev(KIND_WORKER_REVIEW, Some("c"), 3),
            ev(KIND_WORKER_NEEDS_YOU, None, 2),
            noise,
        ];
        let t = diagnosis_targets(&events);
        assert_eq!(
            t.iter().map(|t| (t.session_id.as_str(), t.state)).collect::<Vec<_>>(),
            vec![("c", "finished with an open item"), ("a", "raised a decision")]
        );
        let many: Vec<_> = (0..6).map(|i| ev(KIND_WORKER_STUCK, Some(&format!("s{i}")), i)).collect();
        assert_eq!(diagnosis_targets(&many).len(), MAX_DIAGNOSES);
    }

    #[test]
    fn transcript_tail_is_digested_and_capped() {
        let user = |t: &str| {
            serde_json::json!({"type": "user", "promptSource": "typed", "message": {"role": "user", "content": t}})
                .to_string()
        };
        let asst = |t: &str| {
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}}).to_string()
        };
        let small = [user("prep the Clyde notes"), asst("I can't open the shared drive: 403.")].join("\n");
        let tail = transcript_tail(&small).unwrap();
        assert!(tail.contains("USER: prep the Clyde notes"));
        assert!(tail.ends_with("ASSISTANT: I can't open the shared drive: 403."));

        let big = [user("go"), asst(&"x".repeat(5000)), asst("blocked on the API key")].join("\n");
        let tail = transcript_tail(&big).unwrap();
        assert_eq!(tail.chars().count(), DIAGNOSIS_MAX_CHARS);
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with("blocked on the API key"), "the newest activity survives");

        assert_eq!(transcript_tail("not json\n{}"), None);
    }

    fn msg(author: &str, content: MessageContent) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn text(s: &str) -> MessageContent {
        MessageContent::Text { content: s.into(), thought_signature: None, thought_summary: None, signature_provider: None }
    }

    #[test]
    fn chat_tail_shows_the_last_messages_capped() {
        use crate::components::shared::{ToolCall, ToolCallStatus};
        let tool = ToolCall {
            tool_name: "github_get_pr".into(),
            status: ToolCallStatus::Error,
            response: "404 Not Found".into(),
            ..Default::default()
        };
        let held = ToolCall { tool_name: "gmail_send_email".into(), ..Default::default() };
        let mut msgs: Vec<Message> = (0..10).map(|i| msg("User", text(&format!("old {i}")))).collect();
        msgs.push(msg("Hobbes", text("Trying the PR.")));
        msgs.push(msg("Hobbes", MessageContent::ToolCall(tool)));
        msgs.push(msg("Hobbes", MessageContent::PermissionRequest(held)));
        let tail = chat_tail(&msgs).unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), CHAT_MESSAGES);
        assert_eq!(lines[3], "HOBBES: Trying the PR.");
        assert_eq!(lines[4], "[tool: github_get_pr — Error: 404 Not Found]");
        assert_eq!(lines[5], "[held for the user's approval: gmail_send_email]");
        assert!(!tail.contains("old 3"));

        let long: Vec<Message> = (0..6).map(|_| msg("Hobbes", text(&"y".repeat(900)))).collect();
        let tail = chat_tail(&long).unwrap();
        assert_eq!(tail.chars().count(), DIAGNOSIS_MAX_CHARS);
        assert!(tail.starts_with('…'));
        assert_eq!(chat_tail(&[]), None);
    }
}
