//! The Heartbeat's trust rules: every tool call from the dedicated Heartbeat
//! session passes the agent-time scope check
//! (`todo::agent_time::agent_time_scope`) before the normal permission flow,
//! run by `builtin_tools::heartbeat_gate` at both tool-call sites.
//!
//! Two rows differ from agent time, because the Heartbeat's job is to route
//! work rather than do it, and it holds no agent-time lease (so it cannot
//! file proposals through `HOBBES_AGENT_TIME`):
//! - `HOBBES_DISPATCH` → NeedsApproval (agent time: Deny). Launching a
//!   worker is held for the person, like any outside effect.
//! - `HOBBES_DECISIONS action=ask` → Allow (agent time: Deny). Raising a
//!   decision only adds to the person's queue; answering one stays denied.
//!
//! `HOBBES_AGENT_TIME` is denied outright: the Heartbeat is not agent time.

use crate::todo::agent_time::{agent_time_scope, ScopeVerdict};

/// The scope verdict for a call from the Heartbeat session.
pub fn heartbeat_scope(tool: &str, args: &serde_json::Value) -> ScopeVerdict {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match tool {
        "HOBBES_DISPATCH" => ScopeVerdict::NeedsApproval,
        "HOBBES_DECISIONS" if action == "ask" => ScopeVerdict::Allow,
        "HOBBES_AGENT_TIME" => ScopeVerdict::Deny,
        _ => agent_time_scope(tool, args).0,
    }
}

/// What the gate tells a call site to do (mirrors `AgentTimeGate`, which the
/// call sites already handle).
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatVerdict {
    /// Not the Heartbeat session, or in scope: the normal flow.
    Proceed,
    /// Hold as a `PermissionRequest`, bypassing trust rules and
    /// always-allow (streaming path).
    ForceApproval,
    /// Refuse with this tool error.
    Refuse(String),
}

/// The gate decision. `user_approved` is the approval-resume path: a call
/// held for approval was approved by the person and runs; Deny never runs,
/// approved or not (a call can't have been held for a Deny, but a resumed
/// call is re-checked anyway).
pub fn gate_verdict(
    is_heartbeat_session: bool,
    tool: &str,
    args: &serde_json::Value,
    user_approved: bool,
) -> HeartbeatVerdict {
    if !is_heartbeat_session {
        return HeartbeatVerdict::Proceed;
    }
    match heartbeat_scope(tool, args) {
        ScopeVerdict::Allow => HeartbeatVerdict::Proceed,
        ScopeVerdict::NeedsApproval if user_approved => HeartbeatVerdict::Proceed,
        ScopeVerdict::NeedsApproval => HeartbeatVerdict::ForceApproval,
        ScopeVerdict::Deny => HeartbeatVerdict::Refuse(format!(
            "'{tool}' is out of scope for the Heartbeat and was refused. The Heartbeat may read \
             what the user has shared, write its scratchpad and handoff note, raise a decision \
             (HOBBES_DECISIONS ask) and ask to dispatch a worker (held for the user's approval); \
             the user's to-dos, plan, timers and anything destructive are off limits."
        )),
    }
}

/// "What you may do" for the wake message. `planner_on` gates dispatch,
/// `decisions_on` the decision queue (`Settings::agent_time_enabled`).
pub fn trust_summary(planner_on: bool, decisions_on: bool) -> Vec<String> {
    let mut out = vec![
        "Read what the user has shared (to-dos, calendar, projects, Fleet status, timers) and use read-only tools freely.".to_string(),
        "Write your own scratchpad and handoff note (HOBBES_WRITE_HANDOFF).".to_string(),
    ];
    if decisions_on {
        out.push("Raise a decision for the user with HOBBES_DECISIONS action \"ask\".".to_string());
    } else {
        out.push("If you need the user, say so in one line here; they read this chat.".to_string());
    }
    if planner_on {
        out.push("Ask to dispatch a worker with HOBBES_DISPATCH; it is held for the user's approval.".to_string());
    }
    out.push("Any outside effect (sending, posting, changing a repo, running a command) is held for the user's approval.".to_string());
    out.push("Refused outright: changing the user's to-dos, plan, time blocks, projects or timers, answering decisions, and anything destructive.".to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn not_the_heartbeat_session_is_untouched() {
        assert_eq!(gate_verdict(false, "HOBBES_TODO_CREATE", &json!({}), false), HeartbeatVerdict::Proceed);
        assert_eq!(gate_verdict(false, "delete_repo", &json!({}), false), HeartbeatVerdict::Proceed);
    }

    #[test]
    fn verdicts_follow_agent_time_scope() {
        let v = |tool: &str, args: serde_json::Value, ok: bool| gate_verdict(true, tool, &args, ok);
        // Allow runs.
        assert_eq!(v("HOBBES_TODO_LIST", json!({}), false), HeartbeatVerdict::Proceed);
        assert_eq!(v("HOBBES_WRITE_HANDOFF", json!({}), false), HeartbeatVerdict::Proceed);
        assert_eq!(v("github_list_issues", json!({}), false), HeartbeatVerdict::Proceed);
        // NeedsApproval is held, and runs once approved.
        assert_eq!(v("gmail_send_email", json!({}), false), HeartbeatVerdict::ForceApproval);
        assert_eq!(v("gmail_send_email", json!({}), true), HeartbeatVerdict::Proceed);
        // Deny is refused, approved or not.
        assert!(matches!(v("HOBBES_TODO_CREATE", json!({}), false), HeartbeatVerdict::Refuse(_)));
        assert!(matches!(v("HOBBES_SET_TIMER", json!({}), true), HeartbeatVerdict::Refuse(_)));
        assert!(matches!(v("github_delete_repo", json!({}), false), HeartbeatVerdict::Refuse(_)));
        assert!(matches!(v("HOBBES_DECISIONS", json!({"action": "resolve"}), false), HeartbeatVerdict::Refuse(_)));
    }

    #[test]
    fn heartbeat_rows_that_differ_from_agent_time() {
        assert_eq!(heartbeat_scope("HOBBES_DISPATCH", &json!({"todo_id": "t"})), ScopeVerdict::NeedsApproval);
        assert_eq!(gate_verdict(true, "HOBBES_DISPATCH", &json!({}), false), HeartbeatVerdict::ForceApproval);
        assert_eq!(heartbeat_scope("HOBBES_DECISIONS", &json!({"action": "ask"})), ScopeVerdict::Allow);
        assert_eq!(heartbeat_scope("HOBBES_DECISIONS", &json!({"action": "list"})), ScopeVerdict::Allow);
        assert_eq!(heartbeat_scope("HOBBES_AGENT_TIME", &json!({"action": "start"})), ScopeVerdict::Deny);
    }

    #[test]
    fn summary_reflects_what_is_available() {
        let all = trust_summary(true, true).join("\n");
        assert!(all.contains("HOBBES_DISPATCH") && all.contains("HOBBES_DECISIONS"));
        let none = trust_summary(false, false).join("\n");
        assert!(!none.contains("HOBBES_DISPATCH") && !none.contains("HOBBES_DECISIONS"));
        assert!(none.contains("Refused outright"));
    }
}
