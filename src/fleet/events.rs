//! Hook event payload parsing.
//!
//! Claude Code `type: "http"` hooks POST the event's JSON input as the
//! request body (verified against the hooks reference, 2026-08): every event
//! carries `session_id`, `hook_event_name`, and usually `cwd` /
//! `transcript_path`; event-specific fields ride alongside. Parsing is
//! deliberately lenient about extras — the contract grows fields over time —
//! and strict only about what the reducer needs.

use serde_json::Value;

/// A parsed hook event. Only the five events we register hooks for; anything
/// else is a parse error (and an ingest 400).
#[derive(Debug, Clone, PartialEq)]
pub enum FleetEvent {
    SessionStart {
        session_id: String,
        cwd: String,
        /// `startup` | `resume` | `clear` | `compact` | `fork`
        reason: String,
    },
    SessionEnd {
        session_id: String,
        cwd: String,
        /// `clear` | `resume` | `logout` | `prompt_input_exit` | `other`
        reason: String,
    },
    Stop {
        session_id: String,
        cwd: String,
        /// Background agents/tasks still running when the main turn ended
        /// (the payload's `background_tasks` array length). Nonzero means
        /// "idle for input, still working".
        background_tasks: usize,
    },
    /// `SubagentStop` — a subagent (Task tool) finished: background-work
    /// heartbeat.
    SubagentStop {
        session_id: String,
        cwd: String,
    },
    Notification {
        session_id: String,
        cwd: String,
        /// The payload's `type` field: `permission_prompt`, `idle_prompt`, …
        kind: String,
        message: String,
    },
    PermissionRequest {
        session_id: String,
        cwd: String,
        /// Hobbes-generated (the payload has no `tool_use_id` for this
        /// event) — keys the held response.
        request_id: String,
        tool_name: String,
        tool_input: Value,
    },
    /// `UserPromptSubmit` — the user submitted a prompt: the turn is starting
    /// and the user is demonstrably handling this session.
    PromptSubmit {
        session_id: String,
        cwd: String,
    },
    /// `PostToolUse` — a tool call finished: liveness heartbeat mid-turn.
    /// High-volume; not appended to `fleet_events` (state-only).
    ToolActivity {
        session_id: String,
        cwd: String,
        tool_name: String,
    },
}

impl FleetEvent {
    pub fn session_id(&self) -> &str {
        match self {
            FleetEvent::SessionStart { session_id, .. }
            | FleetEvent::SessionEnd { session_id, .. }
            | FleetEvent::Stop { session_id, .. }
            | FleetEvent::Notification { session_id, .. }
            | FleetEvent::PermissionRequest { session_id, .. }
            | FleetEvent::PromptSubmit { session_id, .. }
            | FleetEvent::ToolActivity { session_id, .. }
            | FleetEvent::SubagentStop { session_id, .. } => session_id,
        }
    }

    pub fn cwd(&self) -> &str {
        match self {
            FleetEvent::SessionStart { cwd, .. }
            | FleetEvent::SessionEnd { cwd, .. }
            | FleetEvent::Stop { cwd, .. }
            | FleetEvent::Notification { cwd, .. }
            | FleetEvent::PermissionRequest { cwd, .. }
            | FleetEvent::PromptSubmit { cwd, .. }
            | FleetEvent::ToolActivity { cwd, .. }
            | FleetEvent::SubagentStop { cwd, .. } => cwd,
        }
    }

    pub fn event_name(&self) -> &'static str {
        match self {
            FleetEvent::SessionStart { .. } => "SessionStart",
            FleetEvent::SessionEnd { .. } => "SessionEnd",
            FleetEvent::Stop { .. } => "Stop",
            FleetEvent::Notification { .. } => "Notification",
            FleetEvent::PermissionRequest { .. } => "PermissionRequest",
            FleetEvent::PromptSubmit { .. } => "UserPromptSubmit",
            FleetEvent::ToolActivity { .. } => "PostToolUse",
            FleetEvent::SubagentStop { .. } => "SubagentStop",
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The payload's `transcript_path`, when present and non-empty. Read
/// straight off the raw body — [`FleetEvent`] deliberately doesn't carry it
/// (the enum stays frozen; hook bodies are small, the re-parse is cheap).
pub fn transcript_path_from_body(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("transcript_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// Parse a hook POST body. Dispatches on the payload's own
/// `hook_event_name` — the URL path segment is routing sugar, the payload is
/// authoritative.
pub fn parse_event(body: &[u8]) -> Result<FleetEvent, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let session_id = str_field(&v, "session_id");
    if session_id.is_empty() {
        return Err("missing session_id".to_string());
    }
    let cwd = str_field(&v, "cwd");
    let name = v
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or("missing hook_event_name")?;

    match name {
        "SessionStart" => Ok(FleetEvent::SessionStart {
            session_id,
            cwd,
            reason: str_field(&v, "reason"),
        }),
        "SessionEnd" => Ok(FleetEvent::SessionEnd {
            session_id,
            cwd,
            reason: str_field(&v, "reason"),
        }),
        "Stop" => Ok(FleetEvent::Stop {
            session_id,
            cwd,
            background_tasks: v
                .get("background_tasks")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
        }),
        "SubagentStop" => Ok(FleetEvent::SubagentStop { session_id, cwd }),
        "Notification" => Ok(FleetEvent::Notification {
            session_id,
            cwd,
            kind: str_field(&v, "type"),
            message: str_field(&v, "message"),
        }),
        "PermissionRequest" => Ok(FleetEvent::PermissionRequest {
            session_id,
            cwd,
            request_id: uuid::Uuid::new_v4().to_string(),
            tool_name: str_field(&v, "tool_name"),
            tool_input: v.get("tool_input").cloned().unwrap_or(Value::Null),
        }),
        "UserPromptSubmit" => Ok(FleetEvent::PromptSubmit { session_id, cwd }),
        "PostToolUse" => Ok(FleetEvent::ToolActivity {
            session_id,
            cwd,
            tool_name: str_field(&v, "tool_name"),
        }),
        other => Err(format!("unsupported hook event: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures mirror the documented payload shapes (hooks reference).

    #[test]
    fn parses_session_start() {
        let body = serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/Users/x/.claude/projects/p/t.jsonl",
            "cwd": "/Users/x/dev/hobbes",
            "permission_mode": "default",
            "hook_event_name": "SessionStart",
            "reason": "startup",
            "model": "claude-fable-5"
        });
        let ev = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            ev,
            FleetEvent::SessionStart {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
                reason: "startup".into(),
            }
        );
    }

    #[test]
    fn parses_session_end() {
        let body = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "SessionEnd",
            "reason": "logout"
        });
        let ev = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            ev,
            FleetEvent::SessionEnd {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
                reason: "logout".into(),
            }
        );
    }

    #[test]
    fn parses_stop_ignoring_extras() {
        let body = serde_json::json!({
            "session_id": "abc123",
            "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
            "transcript_path": "/t.jsonl",
            "cwd": "/Users/x/dev/hobbes",
            "permission_mode": "default",
            "hook_event_name": "Stop",
            "last_assistant_message": "All done."
        });
        let ev = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            ev,
            FleetEvent::Stop {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
                background_tasks: 0,
            }
        );
    }

    #[test]
    fn stop_counts_background_tasks_and_subagent_stop_parses() {
        let body = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/x",
            "hook_event_name": "Stop",
            "background_tasks": [{"id": "a"}, {"id": "b"}]
        });
        assert_eq!(
            parse_event(body.to_string().as_bytes()).unwrap(),
            FleetEvent::Stop {
                session_id: "abc123".into(),
                cwd: "/x".into(),
                background_tasks: 2,
            }
        );
        let sub = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/x",
            "hook_event_name": "SubagentStop"
        });
        assert_eq!(
            parse_event(sub.to_string().as_bytes()).unwrap(),
            FleetEvent::SubagentStop {
                session_id: "abc123".into(),
                cwd: "/x".into(),
            }
        );
    }

    #[test]
    fn transcript_path_from_body_shapes() {
        let with = serde_json::json!({"session_id": "a", "transcript_path": "/t.jsonl"});
        assert_eq!(
            transcript_path_from_body(with.to_string().as_bytes()).as_deref(),
            Some("/t.jsonl")
        );
        let empty = serde_json::json!({"session_id": "a", "transcript_path": ""});
        assert!(transcript_path_from_body(empty.to_string().as_bytes()).is_none());
        let absent = serde_json::json!({"session_id": "a"});
        assert!(transcript_path_from_body(absent.to_string().as_bytes()).is_none());
        assert!(transcript_path_from_body(b"not json").is_none());
        let non_string = serde_json::json!({"transcript_path": 42});
        assert!(transcript_path_from_body(non_string.to_string().as_bytes()).is_none());
    }

    #[test]
    fn parses_prompt_submit_and_post_tool_use() {
        let prompt = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "fix the tests"
        });
        assert_eq!(
            parse_event(prompt.to_string().as_bytes()).unwrap(),
            FleetEvent::PromptSubmit {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
            }
        );

        let tool = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_response": {"stdout": "…"}
        });
        assert_eq!(
            parse_event(tool.to_string().as_bytes()).unwrap(),
            FleetEvent::ToolActivity {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
                tool_name: "Bash".into(),
            }
        );
    }

    #[test]
    fn parses_notification_type_and_message() {
        let body = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "Notification",
            "type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"
        });
        let ev = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            ev,
            FleetEvent::Notification {
                session_id: "abc123".into(),
                cwd: "/Users/x/dev/hobbes".into(),
                kind: "permission_prompt".into(),
                message: "Claude needs your permission to use Bash".into(),
            }
        );
    }

    #[test]
    fn parses_permission_request_with_generated_request_id() {
        // Documented shape: tool_name + tool_input, NO tool_use_id, optional
        // permission_suggestions.
        let body = serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/t.jsonl",
            "cwd": "/Users/x/dev/hobbes",
            "permission_mode": "default",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "rm -rf node_modules", "description": "Remove node_modules" },
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "rm -rf node_modules" }],
                    "behavior": "allow",
                    "destination": "localSettings"
                }
            ]
        });
        let ev = parse_event(body.to_string().as_bytes()).unwrap();
        match ev {
            FleetEvent::PermissionRequest {
                session_id,
                cwd,
                request_id,
                tool_name,
                tool_input,
            } => {
                assert_eq!(session_id, "abc123");
                assert_eq!(cwd, "/Users/x/dev/hobbes");
                assert!(!request_id.is_empty(), "must self-assign an id");
                assert_eq!(tool_name, "Bash");
                assert_eq!(
                    tool_input.get("command").and_then(Value::as_str),
                    Some("rm -rf node_modules")
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_garbage_and_unknown_events() {
        assert!(parse_event(b"not json").is_err());
        assert!(parse_event(b"{}").is_err(), "no session_id");
        let no_name = serde_json::json!({"session_id": "s"});
        assert!(parse_event(no_name.to_string().as_bytes()).is_err());
        let unknown = serde_json::json!({
            "session_id": "s",
            "hook_event_name": "PreToolUse"
        });
        assert!(parse_event(unknown.to_string().as_bytes()).is_err());
    }
}
