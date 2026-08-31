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
}

impl FleetEvent {
    pub fn session_id(&self) -> &str {
        match self {
            FleetEvent::SessionStart { session_id, .. }
            | FleetEvent::SessionEnd { session_id, .. }
            | FleetEvent::Stop { session_id, .. }
            | FleetEvent::Notification { session_id, .. }
            | FleetEvent::PermissionRequest { session_id, .. } => session_id,
        }
    }

    pub fn cwd(&self) -> &str {
        match self {
            FleetEvent::SessionStart { cwd, .. }
            | FleetEvent::SessionEnd { cwd, .. }
            | FleetEvent::Stop { cwd, .. }
            | FleetEvent::Notification { cwd, .. }
            | FleetEvent::PermissionRequest { cwd, .. } => cwd,
        }
    }

    pub fn event_name(&self) -> &'static str {
        match self {
            FleetEvent::SessionStart { .. } => "SessionStart",
            FleetEvent::SessionEnd { .. } => "SessionEnd",
            FleetEvent::Stop { .. } => "Stop",
            FleetEvent::Notification { .. } => "Notification",
            FleetEvent::PermissionRequest { .. } => "PermissionRequest",
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
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
        "Stop" => Ok(FleetEvent::Stop { session_id, cwd }),
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
