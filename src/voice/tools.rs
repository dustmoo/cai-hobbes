//! The tools the realtime voice model may call, and their dispatch.
//!
//! The voice tier gets a small, mostly read-only set. Anything that needs
//! thought or changes things goes to the brain (`VOICE_ASK_BRAIN`), whose
//! turn runs in the Hobbes session with the full permission system. Built-in
//! calls go through `scope_gates` + `dispatch_builtin_tool` exactly like a
//! chat turn's (P-015) — never a parallel dispatch path.

use serde_json::{json, Value};

use crate::components::builtin_tools::{self, AgentTimeGate, BuiltinToolCtx};
use crate::components::shared::{ToolCall, ToolCallStatus};
use crate::voice::realtime::ToolDecl;

pub const ASK_BRAIN: &str = "VOICE_ASK_BRAIN";
pub const CANCEL_BRAIN: &str = "VOICE_CANCEL_BRAIN";
pub const END_CONVERSATION: &str = "VOICE_END_CONVERSATION";

/// Read-only (or harmless) built-ins the voice tier may call directly.
const CORE_TOOLS: &[&str] = &["HOBBES_FLEET_STATUS", "HOBBES_LIST_TIMERS", "HOBBES_SET_TIMER"];
const PLANNER_TOOLS: &[&str] = &["HOBBES_TODO_LIST", "HOBBES_CALENDAR_LIST"];

/// The voice tier's own tools.
fn voice_only_decls() -> Vec<ToolDecl> {
    vec![
        ToolDecl {
            name: ASK_BRAIN.into(),
            description: "Hand a request to Hobbes' main model (the brain), which has the full \
                toolset: email, files, the terminal, MCP servers, dispatching work to coding \
                agents, editing the planner, and careful reasoning. Use it for anything you \
                can't answer from your own quick tools, anything that changes something, and \
                anything that needs real thought. It runs in the background: tell the person \
                you're on it, keep talking with them, and relay the result when it arrives. \
                Call it ONCE per request and wait for the result, even if it takes a minute or \
                more; never repeat or reword a request that is still running."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "request": {"type": "string", "description": "What the person wants, in full sentences, with every detail they gave, and whether they want a quick answer or the full detail. The brain can't hear the conversation."},
                },
                "required": ["request"]
            }),
            non_blocking: true,
        },
        ToolDecl {
            name: CANCEL_BRAIN.into(),
            description: "Stop the brain's current request (when the person changes their mind)."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
            non_blocking: false,
        },
        ToolDecl {
            name: END_CONVERSATION.into(),
            description: "End the voice conversation when the person says goodbye or asks you \
                to stop listening. Say a brief goodbye first."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
            non_blocking: false,
        },
    ]
}

/// Gemini accepts an OpenAPI subset; run schemas through the existing
/// sanitizer (falls back to the raw schema).
fn sanitize(schema: &Value) -> Value {
    crate::gemini::convert::convert_schema(schema)
        .ok()
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or_else(|| schema.clone())
}

/// Every tool the voice tier is offered, given the settings.
pub fn voice_tool_decls(planner_enabled: bool) -> Vec<ToolDecl> {
    let mut decls = voice_only_decls();
    let core = crate::mcp::core_client::CoreClient::new().list_tools();
    let planner = crate::mcp::planner_client::PlannerClient::new().list_tools();
    let wanted = |name: &str| CORE_TOOLS.contains(&name) || (planner_enabled && PLANNER_TOOLS.contains(&name));
    for t in core.iter().chain(planner.iter()) {
        if !wanted(t.name.as_ref()) {
            continue;
        }
        decls.push(ToolDecl {
            name: t.name.to_string(),
            description: t.description.as_deref().unwrap_or_default().to_string(),
            parameters: Value::Object((*t.input_schema).clone()),
            non_blocking: false,
        });
    }
    for d in &mut decls {
        d.parameters = sanitize(&d.parameters);
    }
    decls
}

/// Which hobbes-* server owns a built-in (for the synthetic ToolCall).
fn server_for(name: &str) -> &'static str {
    if PLANNER_TOOLS.contains(&name) {
        "hobbes-planner"
    } else {
        "hobbes-core"
    }
}

/// Run a built-in the voice tier called, through the same gates and dispatch
/// a chat turn uses. Returns the JSON response for the model.
pub async fn dispatch(ctx: BuiltinToolCtx, session_id: &str, name: &str, args: &Value) -> Value {
    if !CORE_TOOLS.contains(&name) && !PLANNER_TOOLS.contains(&name) {
        return json!({"error": format!("{name} isn't available in voice; ask the brain instead.")});
    }
    let tool_call = ToolCall {
        execution_id: uuid::Uuid::new_v4().to_string(),
        server_name: server_for(name).to_string(),
        tool_name: name.to_string(),
        arguments: args.to_string(),
        status: ToolCallStatus::Running,
        // The voice tier is the person talking: interactive rules (the
        // cross-company hold still applies to what the brain does).
        origin: Some(crate::session::TurnOrigin::User),
        ..Default::default()
    };
    match builtin_tools::scope_gates(ctx, &tool_call, args, session_id, false) {
        AgentTimeGate::Proceed => {}
        AgentTimeGate::ForceApproval => {
            return json!({"error": "That needs the person's approval; ask the brain to do it instead."})
        }
        AgentTimeGate::Refuse(msg) => return json!({"error": msg}),
    }
    match builtin_tools::dispatch_builtin_tool(ctx, &tool_call, args, session_id, None, false).await {
        Some(outcome) => {
            if outcome.persist {
                crate::session::SessionState::save_signal(&ctx.session_state, None);
            }
            match outcome.status {
                ToolCallStatus::Error => json!({"error": outcome.response}),
                _ => json!({"result": outcome.response}),
            }
        }
        None => json!({"error": format!("{name} isn't available right now.")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_tools_are_curated() {
        let names: Vec<String> = voice_tool_decls(true).into_iter().map(|d| d.name).collect();
        for n in [ASK_BRAIN, CANCEL_BRAIN, END_CONVERSATION, "HOBBES_FLEET_STATUS", "HOBBES_TODO_LIST"] {
            assert!(names.iter().any(|x| x == n), "missing {n}: {names:?}");
        }
        assert!(!names.iter().any(|x| x == "HOBBES_DISPATCH" || x == "HOBBES_TODO_UPDATE" || x == "HOBBES_PEER_READ"));
        let no_planner: Vec<String> = voice_tool_decls(false).into_iter().map(|d| d.name).collect();
        assert!(!no_planner.iter().any(|x| x == "HOBBES_TODO_LIST"));
    }

    #[test]
    fn only_the_brain_runs_in_the_background() {
        for d in voice_tool_decls(true) {
            assert_eq!(d.non_blocking, d.name == ASK_BRAIN, "{}", d.name);
        }
    }
}
