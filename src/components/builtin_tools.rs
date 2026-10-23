// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

//! Single dispatch point for Hobbes' built-in ("virtual server") tools.
//!
//! These tools are intercepted *before* MCP dispatch because they operate on app
//! state — `SessionState`, the skill registry, the permission manager — that
//! `McpManager` does not own. `McpClientType::NativeCore` therefore has no
//! executor: reaching it means interception was missed, and it returns an
//! explicit "this is a Hobbes bug" error.
//!
//! Two call sites dispatch built-ins:
//!
//! 1. `stream_manager.rs` — the normal streaming path.
//! 2. `chat.rs` — the permission-approval resume path, which re-runs tool calls
//!    left in `Running` after the user approves them.
//!
//! Both must agree on the complete tool list. When they drifted, `HOBBES_SET_TIMER`
//! and `HOBBES_INVOKE_SKILL` worked on the streaming path but hit the NativeCore
//! bug error whenever the same turn happened to resume from an approval. Routing
//! both sites through [`dispatch_builtin_tool`] is what keeps them in sync — add
//! new built-ins here, never at a call site.

use crate::components::shared::{ToolCall, ToolCallStatus};
use crate::context::permissions::{PermissionManager, PermissionStatus, SkillInvoker};
use crate::mcp::manager::McpContext;
use crate::session::SessionState;
use crate::settings::Settings;
use crate::skills::SkillRegistry;
use dioxus::prelude::*;

/// Every tool name handled by [`dispatch_builtin_tool`].
///
/// Mirrors the match arms below, and must stay in sync with the schemas
/// advertised by `CoreClient::list_tools` (`src/mcp/core_client.rs`) — a tool
/// declared there but missing here is advertised to the model and then fails as
/// an unintercepted NativeCore call.
///
/// Deliberately *not* consulted by `dispatch_builtin_tool` as an early-out: a
/// guard would mean a new match arm whose name was omitted here silently fell
/// through to MCP, trading a loud failure for a quiet one. This exists to
/// document the contract and to let the test below catch drift.
#[allow(dead_code)]
pub const BUILTIN_TOOLS: &[&str] = &[
    "HOBBES_PAGE_RESULT",
    "HOBBES_UPDATE_SCRATCHPAD",
    "HOBBES_WRITE_HANDOFF",
    "HOBBES_INVOKE_SKILL",
    "HOBBES_UNLOAD_SKILL",
    "HOBBES_SET_TIMER",
    "HOBBES_LIST_TIMERS",
    "HOBBES_CANCEL_TIMER",
    "HOBBES_FLEET_STATUS",
    "HOBBES_MESSAGE_PARENT",
    "HOBBES_MESSAGE_WORKER",
    "HOBBES_TODO_CREATE",
    "HOBBES_TODO_UPDATE",
    "HOBBES_TODO_LIST",
    "HOBBES_PLAN_DAY",
    "HOBBES_TIME_BLOCK",
    "HOBBES_PROJECT_UPSERT",
    "HOBBES_PROJECT_LIST",
    "HOBBES_PROJECT_MERGE",
    "HOBBES_CALENDAR_LIST",
    "HOBBES_DISPATCH",
    "HOBBES_DECISIONS",
    "HOBBES_TIME_GOALS",
    "HOBBES_AGENT_TIME",
];

#[allow(dead_code)]
pub fn is_builtin_tool(name: &str) -> bool {
    BUILTIN_TOOLS.contains(&name)
}

/// Whether a tool belongs to the `hobbes-planner` virtual server. Used both to
/// gate dispatch on `settings.planner_enabled` and to withhold the tool
/// definitions from the prompt when the planner is off (system_context.rs).
/// Rewrite any non-null `linked_session` values in todo create/update args
/// from name-or-id references to resolved fleet session ids. Returns
/// `Ok(None)` when nothing needed resolving (avoids a clone on the hot
/// path), `Err` with a live-session listing on failure.
fn resolve_linked_sessions(
    args: &serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    let has_refs = ["todos", "updates"].iter().any(|key| {
        args.get(key).and_then(|v| v.as_array()).is_some_and(|items| {
            items.iter().any(|i| {
                i.get("linked_session").is_some_and(|v| v.is_string())
            })
        })
    });
    if !has_refs {
        return Ok(None);
    }
    let live = crate::fleet::shared().snapshot();
    let mut out = args.clone();
    for key in ["todos", "updates"] {
        if let Some(items) = out.get_mut(key).and_then(|v| v.as_array_mut()) {
            for item in items {
                let Some(reference) = item.get("linked_session").and_then(|v| v.as_str())
                else {
                    continue;
                };
                let id = crate::fleet::resolve_session_ref(&live, reference)?;
                item["linked_session"] = serde_json::Value::String(id);
            }
        }
    }
    Ok(Some(out))
}

/// Whether a tool belongs to the fleet family — gated on
/// `fleet_enabled && pro_active()` both at dispatch and when withholding
/// definitions from the prompt (system_context.rs).
pub fn is_fleet_tool(name: &str) -> bool {
    name == "HOBBES_FLEET_STATUS"
}

pub fn is_planner_tool(name: &str) -> bool {
    name.starts_with("HOBBES_TODO_")
        || matches!(
            name,
            "HOBBES_PLAN_DAY" | "HOBBES_TIME_BLOCK" | "HOBBES_PROJECT_UPSERT"
                | "HOBBES_PROJECT_LIST" | "HOBBES_PROJECT_MERGE" | "HOBBES_CALENDAR_LIST" | "HOBBES_DISPATCH" | "HOBBES_DECISIONS"
                | "HOBBES_TIME_GOALS" | "HOBBES_AGENT_TIME"
        )
}

/// The tools behind the `experimental_agent_time` release gate
/// ([`Settings::agent_time_enabled`]): withheld from every prompt
/// (system_context.rs) and refused at dispatch while it is off.
pub fn is_experimental_agent_tool(name: &str) -> bool {
    matches!(name, "HOBBES_AGENT_TIME" | "HOBBES_DECISIONS")
}

/// The refusal a gated tool gets while Agent time / Decisions are hidden.
pub const EXPERIMENTAL_AGENT_TOOL_REFUSAL: &str =
    "Agent time and the decision queue aren't available in this version of Hobbes. \
     If you need the user's decision, ask them directly in this chat.";

/// The agent-time tool family. Never offered to an ordinary chat: it is not
/// in `PlannerClient::list_tools`, and the agent-time runtime adds
/// `PlannerClient::agent_time_tools()` only to a session for which
/// [`agent_time_tool_visible`] holds. Dispatch refuses it from any other
/// session regardless (belt and braces).
pub fn is_agent_time_tool(name: &str) -> bool {
    name == "HOBBES_AGENT_TIME"
}

/// Whether `session_id` is the leased agent-time session — the condition
/// for offering (and dispatching) `HOBBES_AGENT_TIME`. For the prompt-side
/// tool list (`PromptBuilder::with_agent_time_tools`, set by chat.rs).
pub fn agent_time_tool_visible(planner: &crate::todo::PlannerState, session_id: &str) -> bool {
    planner.is_agent_time_session(session_id)
}

/// What the agent-time gate tells a tool-call site to do.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentTimeGate {
    /// Continue through the normal flow (built-in dispatch / MCP permission).
    Proceed,
    /// Hold the call as a `PermissionRequest` without consulting trust rules
    /// or always-allow (streaming path only).
    ForceApproval,
    /// Refuse: write this as the call's error result.
    Refuse(String),
}

/// **The agent-time scope gate** (SYMBIOTIC_PLANNER.md §3), run by BOTH
/// tool-call sites — the streaming path (`stream_manager.rs`) and the
/// approval-resume path (`chat.rs`) — before built-in dispatch and before
/// `McpManager::use_mcp_tool`'s permission check, for every tool call.
/// A no-op (`Proceed`) for chats that are not agent-time chats. Records
/// every gated call on the run's log and persists it (the decision itself
/// is the pure `agent_time_runtime::gate_decision`).
pub fn agent_time_gate(
    deps: BuiltinToolCtx,
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
    user_approved: bool,
) -> AgentTimeGate {
    use crate::agent_time_runtime as rt;
    // Budget is checked at every call too, not only on usage events and the
    // tick: an over-budget run is stopped here, and the call below is then
    // refused as an ended run.
    if deps.planner.peek().is_agent_time_session(session_id) {
        rt::enforce_now(deps.planner, deps.session_state);
    }
    let decision = {
        let planner = deps.planner.peek();
        rt::gate_decision(&rt::GateInput {
            is_agent_time_chat: rt::is_agent_time_chat_now(&planner, session_id),
            leased: planner.is_agent_time_session(session_id),
            intent_declared: rt::intent_declared(&planner, session_id),
            tool: &tool_call.tool_name,
            args: args_json,
            user_approved,
        })
    };
    let summary = || crate::context::trust_store::arg_summary(args_json);
    match decision {
        rt::GateDecision::NotAgentTime => AgentTimeGate::Proceed,
        rt::GateDecision::Proceed { record } => {
            if let Some(verdict) = record {
                rt::record_call(deps.planner, session_id, &tool_call.tool_name, &summary(), verdict);
            }
            AgentTimeGate::Proceed
        }
        rt::GateDecision::ApprovedByUser => {
            rt::note(
                deps.planner,
                session_id,
                &tool_call.tool_name,
                &format!("{} [approved by the user]", summary()),
            );
            AgentTimeGate::Proceed
        }
        rt::GateDecision::ForceApproval => {
            rt::record_call(
                deps.planner,
                session_id,
                &tool_call.tool_name,
                &summary(),
                crate::todo::agent_time::ScopeVerdict::NeedsApproval,
            );
            AgentTimeGate::ForceApproval
        }
        rt::GateDecision::Refuse { message, record } => {
            if let Some(verdict) = record {
                rt::record_call(deps.planner, session_id, &tool_call.tool_name, &summary(), verdict);
            }
            tracing::info!(
                session_id,
                tool = %tool_call.tool_name,
                "Agent-time gate refused a tool call"
            );
            AgentTimeGate::Refuse(message)
        }
    }
}

/// **The Heartbeat scope gate**, run by BOTH tool-call sites right after
/// [`agent_time_gate`] (when that says `Proceed`) and before built-in
/// dispatch and `use_mcp_tool`'s permission check. A no-op for every
/// session but the dedicated Heartbeat session; there, the agent-time scope
/// rules apply (`heartbeat::trust`): Allow proceeds, NeedsApproval is held
/// as a `PermissionRequest` without consulting trust rules or always-allow
/// (`ForceApproval`; runs once the user approves), Deny is refused.
pub fn heartbeat_gate(
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
    user_approved: bool,
) -> AgentTimeGate {
    let is_heartbeat = crate::heartbeat::HEARTBEAT.peek().session_id.as_deref() == Some(session_id);
    match crate::heartbeat::trust::gate_verdict(is_heartbeat, &tool_call.tool_name, args_json, user_approved) {
        crate::heartbeat::trust::HeartbeatVerdict::Proceed => AgentTimeGate::Proceed,
        crate::heartbeat::trust::HeartbeatVerdict::ForceApproval => AgentTimeGate::ForceApproval,
        crate::heartbeat::trust::HeartbeatVerdict::Refuse(message) => {
            tracing::info!(session_id, tool = %tool_call.tool_name, "Heartbeat gate refused a tool call");
            AgentTimeGate::Refuse(message)
        }
    }
}

/// Both scope gates in order: agent time first, then (when it proceeds) the
/// Heartbeat's. The one entry point the two tool-call sites use, so neither
/// can run one gate without the other.
pub fn scope_gates(
    deps: BuiltinToolCtx,
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
    user_approved: bool,
) -> AgentTimeGate {
    match agent_time_gate(deps, tool_call, args_json, session_id, user_approved) {
        AgentTimeGate::Proceed => heartbeat_gate(tool_call, args_json, session_id, user_approved),
        other => other,
    }
}

/// The app-state signals a built-in handler may need.
#[derive(Clone, Copy)]
pub struct BuiltinToolCtx {
    pub session_state: Signal<SessionState>,
    pub settings: Signal<Settings>,
    pub skill_registry: Signal<SkillRegistry>,
    pub permission_manager: Signal<PermissionManager>,
    pub mcp_context: Signal<McpContext>,
    /// The global planner (to-dos, day plans, time blocks) — shared across all
    /// chat tabs, unlike the per-session state above.
    pub planner: Signal<crate::todo::PlannerState>,
    /// The UI command bus — dispatch uses it to focus a freshly created
    /// assignment tab (SwitchToSession opens unknown ids as new tabs).
    pub chat_command: Signal<Option<crate::components::chat_input::ChatCommand>>,
}

/// Hand a planner todo to a headless Claude Code run — the exact code path
/// `HOBBES_DISPATCH { target: "claude_code" }` takes, so the planner's
/// "Dispatch to a background agent" and the model's tool call cannot drift.
///
/// `parent_session_id` is the review chat (if any) that gets the result
/// handed back when the run finishes — `None` when there is no conversation
/// to resume (e.g. a bare planner-row dispatch with no chat opened for it).
pub(crate) fn dispatch_todo_to_background(
    deps: BuiltinToolCtx,
    todo_id: &str,
    model: Option<String>,
    parent_session_id: Option<String>,
) -> Result<String, String> {
    let mut args = serde_json::json!({ "todo_id": todo_id, "target": "claude_code" });
    if let Some(m) = model {
        args["model"] = serde_json::Value::String(m);
    }
    crate::todo::dispatch::handle_dispatch(
        deps,
        &args,
        &crate::todo::dispatch::RealClaudeSpawner,
        parent_session_id.as_deref(),
    )
}

pub struct BuiltinOutcome {
    pub status: ToolCallStatus,
    pub response: String,
    /// Whether the caller should persist `SessionState` after writing the result
    /// back into the message. Pagination is turn-local and skips the write; the
    /// scratchpad, skills, and timers must survive a restart.
    pub persist: bool,
}

/// Whether this tool call is a model `HOBBES_INVOKE_SKILL` that must wait for
/// the user's approval before it runs. The streaming path asks this first and,
/// when true, parks the call as a `PermissionRequest` (the same hold-and-resume
/// mechanism MCP tool calls use) instead of dispatching it; the approval-resume
/// path then dispatches with `user_approved = true`.
///
/// Unknown skills and `disable_model_invocation` skills return `false` — they
/// are hard errors inside [`invoke_skill`], not something approval can unlock.
/// A skill that is already loaded in this session was approved when it loaded,
/// so re-invoking it (e.g. to reload an idle-stubbed manual) needs no new prompt.
pub fn skill_invoke_needs_approval(
    deps: BuiltinToolCtx,
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
) -> bool {
    if tool_call.tool_name != "HOBBES_INVOKE_SKILL" {
        return false;
    }
    let name = args_json.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let Some(skill) = deps.skill_registry.read().get_skill(name) else {
        return false;
    };
    if skill.metadata.disable_model_invocation {
        return false;
    }
    let already_loaded = deps
        .session_state
        .read()
        .sessions
        .get(session_id)
        .is_some_and(|s| s.loaded_skills.contains_key(&skill.metadata.name));
    if already_loaded {
        return false;
    }
    // Only Ask raises the bubble. Off is refused by `invoke_skill` with an error.
    deps.permission_manager
        .read()
        .check_skill_permission(&skill.metadata.name, SkillInvoker::Model)
        == PermissionStatus::RequiresPrompt
}

/// Built-ins that ask the person every single time, whatever their trust
/// rules or always-allow lists say: irreversible changes to their own
/// records. Trust rules never apply to built-ins (they are evaluated only in
/// `McpManager::use_mcp_tool`), and there is no "always allow" for these —
/// the approval prompt hides the trust-rule option for them.
pub fn always_asks(tool_name: &str) -> bool {
    tool_name == "HOBBES_PROJECT_MERGE"
}

/// Whether the streaming path must hold this built-in call as a
/// `PermissionRequest` (the same hold-and-resume mechanism MCP calls use)
/// instead of dispatching it: an [`always_asks`] tool (unless its feature
/// is off, so dispatch refuses it straight away), or a model skill load
/// that needs approval ([`skill_invoke_needs_approval`]). The approval-resume
/// path in chat.rs dispatches with `user_approved = true`.
pub fn builtin_needs_approval(
    deps: BuiltinToolCtx,
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
) -> bool {
    if always_asks(&tool_call.tool_name) {
        return !is_planner_tool(&tool_call.tool_name) || deps.settings.peek().planner_enabled;
    }
    skill_invoke_needs_approval(deps, tool_call, args_json, session_id)
}

/// `HOBBES_PROJECT_MERGE`: fold `merge_ids` into `keep_id` through the same
/// path Manage projects uses (`merge_projects_persisted` + `finish_merge`,
/// whose disk half runs on a blocking thread). Refused unless the person
/// approved this very call (`user_approved` — only the approval-resume path
/// sets it; the streaming path always holds the call first).
async fn project_merge_tool(
    deps: BuiltinToolCtx,
    args_json: &serde_json::Value,
    user_approved: bool,
) -> (ToolCallStatus, String) {
    if !deps.settings.peek().planner_enabled {
        return (ToolCallStatus::Error, "The planner is disabled in Settings.".to_string());
    }
    if !user_approved {
        return (
            ToolCallStatus::Error,
            "HOBBES_PROJECT_MERGE needs the user's approval every time, and this call wasn't approved. Nothing was merged."
                .to_string(),
        );
    }
    let validated = crate::todo::handlers::validate_project_merge(&deps.planner.peek(), args_json);
    let (keep_id, merge_ids) = match validated {
        Ok(v) => v,
        Err(e) => return (ToolCallStatus::Error, e),
    };
    let (keep_title, merged_titles) = {
        let planner = deps.planner.peek();
        let title = |id: &str| {
            planner
                .projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.title.clone())
                .unwrap_or_else(|| id.to_string())
        };
        (title(&keep_id), merge_ids.iter().map(|id| title(id)).collect::<Vec<_>>())
    };
    let manager = crate::settings::SettingsManager::new(crate::get_settings_path());
    let merged = crate::components::planner_view::manage_projects::merge_projects_persisted(
        deps.planner,
        deps.session_state,
        deps.settings,
        &manager,
        &keep_id,
        &merge_ids,
    );
    match merged {
        Ok((outcome, job)) => {
            let chats =
                crate::components::planner_view::manage_projects::finish_merge(job, deps.session_state).await;
            (
                ToolCallStatus::Completed,
                format!(
                    "Merged {} into '{}' [{}]: {} to-do(s) and {} chat(s) moved; the merged names are now its aliases.",
                    merged_titles.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", "),
                    keep_title,
                    keep_id,
                    outcome.moved_todos.len(),
                    chats
                ),
            )
        }
        Err(e) => (ToolCallStatus::Error, format!("Nothing was merged: {e}")),
    }
}

/// Run `tool_call` if it is a built-in, otherwise return `None` so the caller
/// falls through to normal MCP dispatch.
///
/// `user_approved` is true only on the approval-resume path: the user just
/// clicked Approve on this call's permission bubble, which satisfies the skill
/// gate for this one invocation.
///
/// Only computes the result — writing it back into the message, persisting, and
/// recording it in tool-call history stay with the caller, since the two sites
/// differ there.
pub async fn dispatch_builtin_tool(
    deps: BuiltinToolCtx,
    tool_call: &ToolCall,
    args_json: &serde_json::Value,
    session_id: &str,
    profile_id: Option<&String>,
    user_approved: bool,
) -> Option<BuiltinOutcome> {
    let mut session_state = deps.session_state;

    match tool_call.tool_name.as_str() {
        // Needs SessionState.page_queue, which McpManager doesn't have.
        "HOBBES_PAGE_RESULT" => {
            let page_budget = crate::session::compute_page_budget(
                &deps.settings.read(),
                session_state.read().sessions.get(session_id),
            );
            let (status, response) =
                session_state
                    .write()
                    .handle_page_result(args_json, "", page_budget);
            Some(BuiltinOutcome {
                status,
                response,
                persist: false,
            })
        }

        "HOBBES_UPDATE_SCRATCHPAD" => {
            let (status, response) = session_state.write().handle_scratchpad_update(
                args_json,
                session_id,
                &deps.settings.read(),
            );
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_WRITE_HANDOFF" => {
            let (status, response) =
                crate::handoff::handle_write_handoff(&mut session_state.write(), args_json, session_id);
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_SET_TIMER" => {
            // An intention must name a to-do whose worker is running, or it
            // could only ever fire at its deadline. Checked here because the
            // planner lives outside SessionState.
            let planner = deps.planner.read();
            if let Some(todo_id) = args_json
                .get("until_worker_done")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                if let Err(msg) = crate::timers::validate_worker_target(todo_id, planner.todo(todo_id)) {
                    return Some(BuiltinOutcome {
                        status: ToolCallStatus::Error,
                        response: msg,
                        persist: false,
                    });
                }
            }
            let (status, response) = session_state.write().handle_set_timer(args_json, session_id, |id| {
                planner.todo(id).map(|t| t.title.clone())
            });
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_LIST_TIMERS" => {
            let planner = deps.planner.read();
            let (status, response) = session_state
                .read()
                .handle_list_timers(session_id, |id| planner.todo(id).map(|t| t.title.clone()));
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_FLEET_STATUS" => {
            // Belt + braces with the prompt-side withholding: a stale prompt
            // can still emit the call after the fleet is switched off.
            if !(deps.settings.read().fleet_enabled && crate::entitlement::pro_active()) {
                return Some(BuiltinOutcome {
                    status: ToolCallStatus::Error,
                    response: "Fleet observation is disabled in Settings.".to_string(),
                    persist: false,
                });
            }
            let now = chrono::Utc::now();
            let today = chrono::Local::now().date_naive();
            let live = crate::fleet::shared().snapshot();
            let ended_today: Vec<crate::fleet::FleetSession> =
                crate::fleet::store::sessions_active_on(today)
                    .into_iter()
                    .filter(|r| !live.sessions.contains_key(&r.id))
                    .collect();
            let filter = args_json.get("session").and_then(|v| v.as_str());
            let links = crate::todo::handlers::linked_session_pairs(&deps.planner.read());
            let report = crate::fleet::briefs::status_report(
                &live,
                &ended_today,
                filter,
                &links,
                now,
                today,
            );
            Some(BuiltinOutcome {
                status: ToolCallStatus::Completed,
                response: serde_json::to_string_pretty(&report)
                    .unwrap_or_else(|_| report.to_string()),
                persist: false,
            })
        }

        "HOBBES_CANCEL_TIMER" => {
            let (status, response) = session_state
                .write()
                .handle_cancel_timer(args_json, session_id);
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_INVOKE_SKILL" => {
            let (status, response) =
                invoke_skill(deps, args_json, session_id, profile_id, user_approved).await;
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_UNLOAD_SKILL" => {
            let (status, response) = unload_skill(deps, args_json, session_id);
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        // Dispatch relay (todo::relay): parent ↔ its own chat workers only,
        // refused by mode and link inside the handlers.
        "HOBBES_MESSAGE_PARENT" => {
            let (status, response) =
                crate::todo::relay::handle_message_parent(deps, args_json, session_id);
            Some(BuiltinOutcome { status, response, persist: true })
        }

        "HOBBES_MESSAGE_WORKER" => {
            let (status, response) =
                crate::todo::relay::handle_message_worker(deps, args_json, session_id);
            Some(BuiltinOutcome { status, response, persist: true })
        }

        "HOBBES_PROJECT_MERGE" => {
            let (status, response) = project_merge_tool(deps, args_json, user_approved).await;
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        name if is_planner_tool(name) => {
            let (status, response) = run_planner_tool(deps, name, args_json, session_id);
            // Planner mutations persist through todo::store inside the handler;
            // this flag persists the *session* so the tool response written into
            // the message survives a restart.
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        _ => None,
    }
}

/// Execute one of the `hobbes-planner` tools against the global planner state.
///
/// Planner days are user-local, so "today" is `Local::now()`, not UTC. The
/// handlers write through to `todo::store` themselves (`persist: true`).
fn run_planner_tool(
    deps: BuiltinToolCtx,
    tool_name: &str,
    args_json: &serde_json::Value,
    session_id: &str,
) -> (ToolCallStatus, String) {
    if !deps.settings.read().planner_enabled {
        return (
            ToolCallStatus::Error,
            "The planner is disabled in Settings.".to_string(),
        );
    }
    if is_experimental_agent_tool(tool_name) && !deps.settings.read().agent_time_enabled() {
        return (ToolCallStatus::Error, EXPERIMENTAL_AGENT_TOOL_REFUSAL.to_string());
    }

    use crate::todo::handlers;
    let mut planner = deps.planner;
    let today = chrono::Local::now().date_naive();

    // `linked_session` values may be session names; resolve them to fleet
    // session ids at this edge (the handlers stay fleet-free and receive
    // pre-resolved ids). Errors name what IS live.
    let resolved_args: serde_json::Value;
    let args_json: &serde_json::Value =
        if matches!(tool_name, "HOBBES_TODO_CREATE" | "HOBBES_TODO_UPDATE") {
            match resolve_linked_sessions(args_json) {
                Ok(Some(v)) => {
                    resolved_args = v;
                    &resolved_args
                }
                Ok(None) => args_json,
                Err(e) => return (ToolCallStatus::Error, e),
            }
        } else {
            args_json
        };

    match tool_name {
        "HOBBES_TODO_CREATE" => {
            handlers::handle_todo_create(&mut planner.write(), args_json, session_id, today, true)
        }
        "HOBBES_TODO_UPDATE" => {
            // session_id threads through so `status: in_progress` opens an
            // agent-actor focus session attributed to this chat (the same
            // provenance source as TodoOrigin::Ai in HOBBES_TODO_CREATE) and
            // auto-links the todo to this session when unlinked.
            handlers::handle_todo_update(&mut planner.write(), args_json, session_id, today, true)
        }
        "HOBBES_TODO_LIST" => handlers::handle_todo_list(&planner.read(), args_json, today),
        "HOBBES_DISPATCH" => {
            // Dispatch itself is metadata-only; the dispatched work stays
            // gated (chat = in-app approvals with trust rules; headless =
            // fleet-held PermissionRequests; never --bare/bypass).
            match crate::todo::dispatch::handle_dispatch(
                deps,
                args_json,
                &crate::todo::dispatch::RealClaudeSpawner,
                Some(session_id),
            ) {
                Ok(msg) => (ToolCallStatus::Completed, msg),
                Err(e) => (ToolCallStatus::Error, e),
            }
        }
        "HOBBES_PLAN_DAY" => {
            let (default_capacity, meetings_count) = {
                let s = deps.settings.read();
                (
                    s.planner_daily_capacity_minutes,
                    s.planner_calendar_counts_against_capacity,
                )
            };
            handlers::handle_plan_day(
                &mut planner.write(),
                args_json,
                default_capacity,
                meetings_count,
                today,
                true,
            )
        }
        "HOBBES_TIME_BLOCK" => {
            if args_json.get("action").and_then(|v| v.as_str()) == Some("list") {
                // Read-only: a read guard, so listing doesn't re-render the planner.
                handlers::time_block_list(&planner.read(), args_json, today)
            } else {
                handlers::handle_time_block(&mut planner.write(), args_json, today, true)
            }
        }
        "HOBBES_PROJECT_UPSERT" => {
            handlers::handle_project_upsert(&mut planner.write(), args_json, today, true)
        }
        "HOBBES_PROJECT_LIST" => handlers::handle_project_list(&planner.read(), args_json),
        "HOBBES_TIME_GOALS" => {
            handlers::handle_time_goals(&mut planner.write(), args_json, today, true)
        }
        "HOBBES_AGENT_TIME" => {
            // The handler itself refuses unless this session holds the
            // agent-time lease and the grant allows it today.
            let owner = deps
                .settings
                .peek()
                .user_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "the user".to_string());
            handlers::handle_agent_time_for(
                &mut planner.write(),
                args_json,
                session_id,
                today,
                true,
                &owner,
            )
        }
        "HOBBES_DECISIONS" => {
            let caller_name = deps
                .session_state
                .peek()
                .sessions
                .get(session_id)
                .map(|s| s.name.clone());
            // "N handled by your rules today": trust-rule hits from the gate
            // log (empty when the store is unavailable).
            let rule_hits = Some(crate::todo::measures::routing_rule_hits_on(
                &crate::context::trust_store::recent(500),
                today,
            ));
            let outcome = handlers::handle_decisions(
                &mut planner.write(),
                args_json,
                caller_name.as_deref(),
                rule_hits,
                true,
            );
            let mut response = outcome.response;
            if let Some(entry) = outcome.deliver {
                response.push('\n');
                response.push_str(&deliver_decision(deps, &entry));
            }
            (outcome.status, response)
        }
        "HOBBES_CALENDAR_LIST" => {
            // Read-only over the calendar cache — no planner mutation, so no
            // state/persist. Subscription roster and sync errors are resolved
            // here at the edge; the handler and its formatter stay pure over
            // what they're given.
            let subscriptions = deps.settings.read().planner_calendar_subscriptions.clone();
            let sync_errors: Vec<(String, String)> = subscriptions
                .iter()
                .filter(|s| s.enabled)
                .filter_map(|s| {
                    crate::todo::calendar_sync::load_sync_state(&s.id)
                        .last_error
                        .map(|e| (s.name.clone(), e))
                })
                .collect();
            handlers::handle_calendar_list(args_json, &subscriptions, &sync_errors, today)
        }
        other => (
            ToolCallStatus::Error,
            format!(
                "Planner tool '{}' matched is_planner_tool but has no handler. \
                This is a Hobbes bug — please report it.",
                other
            ),
        ),
    }
}

/// Hand a resolved decision back to its worker, through whatever channel
/// exists, and record how it went (`lanes::record_delivery`).
///
/// - A Hobbes chat worker (the linked session is a chat): the answer is
///   queued as a user message to that session (`CHAT_QUEUE`); it sends when
///   that tab is next active.
/// - A headless Claude Code run: the fleet has no input channel into a
///   running or finished `claude -p`, so the answer is recorded on the to-do
///   and surfaced in its progress line, and the to-do returns to `Queued`
///   for a resume.
fn deliver_decision(
    deps: BuiltinToolCtx,
    entry: &crate::todo::model::ResolvedDecision,
) -> String {
    use crate::todo::model::DecisionDelivery;
    let mut planner = deps.planner;
    let linked = planner
        .peek()
        .todo(&entry.todo_id)
        .and_then(|t| t.linked_fleet_session.clone());
    let is_chat = |sid: &str| {
        deps.session_state.peek().sessions.contains_key(sid)
            || crate::session_store::contains(sid)
            || crate::fleet::shared()
                .snapshot()
                .sessions
                .get(sid)
                .is_some_and(|r| r.origin == crate::fleet::FleetOrigin::Hobbes)
    };
    let (delivery, msg) = match linked.as_deref() {
        Some(sid) if is_chat(sid) => {
            crate::components::chat_queue::queue_push(
                &mut crate::components::chat_queue::CHAT_QUEUE.write(),
                sid,
                crate::components::chat_queue::QueuedMessage::new(
                    crate::todo::lanes::answer_message(entry),
                    vec![],
                ),
            );
            crate::components::chat_queue::request_drain();
            (
                DecisionDelivery::ChatQueued { session_id: sid.to_string() },
                "Handed back to the worker's chat as a message; it sends when that tab is next \
                 active."
                    .to_string(),
            )
        }
        Some(_) => (
            DecisionDelivery::NotDelivered {
                reason: "the worker is a headless Claude Code run; the fleet has no channel to \
                         send it input"
                    .into(),
            },
            "Not delivered: this worker is a headless Claude Code run and the fleet has no \
             channel to send it input. The answer is recorded on the to-do (shown in its \
             progress) and the to-do is back in the queue — re-dispatch it to continue with \
             the answer."
                .to_string(),
        ),
        None => (
            DecisionDelivery::NotDelivered { reason: "no worker is linked to this to-do".into() },
            "Not delivered: no worker is linked to this to-do. The answer is recorded on it; \
             dispatch it to continue."
                .to_string(),
        ),
    };
    let updated = crate::todo::lanes::record_delivery(
        &mut planner.write(),
        &entry.id,
        delivery,
        chrono::Utc::now(),
    );
    if let Some(d) = updated {
        if let Err(e) = crate::todo::store::save_resolved_decision(&d) {
            tracing::error!("Failed to persist decision delivery {}: {}", d.id, e);
        }
    }
    if let Some(t) = planner.peek().todo(&entry.todo_id) {
        if let Err(e) = crate::todo::store::save_todo(t) {
            tracing::error!("Failed to persist todo {}: {}", t.id, e);
        }
    }
    msg
}

/// The model-invocation path for skills: execute the skill (capability
/// registration only), persist its payload into `session.loaded_skills` so it
/// stays in system context for later turns, and return the instruction manual as
/// the tool result.
async fn invoke_skill(
    deps: BuiltinToolCtx,
    args_json: &serde_json::Value,
    session_id: &str,
    profile_id: Option<&String>,
    user_approved: bool,
) -> (ToolCallStatus, String) {
    let mut session_state = deps.session_state;

    let skill_name = args_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = args_json
        .get("arguments")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let skill_opt = deps.skill_registry.read().get_skill(&skill_name);
    let skill = match skill_opt {
        None => {
            return (
                ToolCallStatus::Error,
                format!(
                    "Skill '{}' not found. Only skills listed under available_skills in your system context can be invoked.",
                    skill_name
                ),
            );
        }
        Some(skill) if skill.metadata.disable_model_invocation => {
            return (
                ToolCallStatus::Error,
                format!(
                    "Skill '{}' has model invocation disabled. The user must invoke it themselves with /{}.",
                    skill_name, skill_name
                ),
            );
        }
        Some(skill) => skill,
    };

    // The streaming path parks calls that need approval before dispatching
    // (`skill_invoke_needs_approval`), so reaching a non-Allowed skill here
    // means a path skipped that step — refuse rather than load unapproved.
    // A skill already loaded this session stays reloadable even if it was
    // switched Off meanwhile; once unloaded, Off means it cannot come back.
    let already_loaded = session_state
        .read()
        .sessions
        .get(session_id)
        .is_some_and(|s| s.loaded_skills.contains_key(&skill.metadata.name));
    if !already_loaded {
        let status = deps
            .permission_manager
            .read()
            .check_skill_permission(&skill.metadata.name, SkillInvoker::Model);
        match status {
            PermissionStatus::Denied(reason) => {
                return (
                    ToolCallStatus::Error,
                    format!("Skill '{}': {}", skill_name, reason),
                );
            }
            PermissionStatus::RequiresPrompt if !user_approved => {
                return (
                    ToolCallStatus::Error,
                    format!(
                        "Skill '{}' needs the user's approval before it can be loaded. Ask them to run /{} themselves or enable the skill in Settings → Permissions.",
                        skill_name, skill_name
                    ),
                );
            }
            _ => {}
        }
    }

    let mut skill_call = crate::components::shared::SkillCall {
        execution_id: uuid::Uuid::new_v4().to_string(),
        skill_name: skill.metadata.name.clone(),
        arguments,
        status: crate::components::shared::SkillCallStatus::Running,
        response: String::new(),
        instructions: skill.instructions.clone(),
        path: skill.path.clone(),
        has_scripts: !skill.scripts.is_empty(),
        raw_output: None,
        profile_color: {
            let settings_read = deps.settings.read();
            crate::components::shared::resolve_profile_color(profile_id, &settings_read)
        },
    };

    // Use the reactively-synced McpContext signal
    // (P-001: never get_mcp_context().await mid-turn)
    let mcp_context = {
        let mut ctx = deps.mcp_context.read().clone();
        ctx.enrich_from_settings(&deps.settings.read());
        ctx
    };

    match crate::skills::execute_skill(&mut skill_call, Some(&mcp_context)).await {
        Ok(result) => {
            if result.status == crate::components::shared::SkillCallStatus::Completed {
                {
                    let mut state = session_state.write();
                    if let Some(session) = state.sessions.get_mut(session_id) {
                        session
                            .loaded_skills
                            .insert(skill_call.skill_name.clone(), result.output.clone());
                    }
                }
                crate::session_events::log_event(
                    session_id,
                    crate::session_events::SessionEvent::SkillLoaded {
                        name: skill_call.skill_name.clone(),
                        payload: result.output.clone(),
                    },
                );
                tracing::info!(
                    "Model invoked skill '{}' — persisted into session.loaded_skills",
                    skill_call.skill_name
                );
                (ToolCallStatus::Completed, result.output)
            } else {
                (ToolCallStatus::Error, result.output)
            }
        }
        Err(e) => (
            ToolCallStatus::Error,
            format!("Skill invocation failed: {}", e),
        ),
    }
}

/// Drop `name` from the session's loaded skills. `Err` carries the model-facing
/// explanation (with what *is* loaded, so it can correct itself).
fn remove_loaded_skill(
    state: &mut SessionState,
    session_id: &str,
    name: &str,
) -> Result<(), String> {
    let Some(session) = state.sessions.get_mut(session_id) else {
        return Err("Session not found.".to_string());
    };
    if session.loaded_skills.remove(name).is_some() {
        return Ok(());
    }
    let mut loaded: Vec<&str> = session.loaded_skills.keys().map(String::as_str).collect();
    loaded.sort_unstable();
    Err(format!(
        "Skill '{}' is not loaded. Currently loaded skills: [{}].",
        name,
        loaded.join(", ")
    ))
}

/// The model-unload path — same effect and journal event as the user's
/// `/unload` (SkillUnloaded, which session replay applies), minus the
/// confirmation chat bubble: the tool result is the confirmation.
fn unload_skill(
    deps: BuiltinToolCtx,
    args_json: &serde_json::Value,
    session_id: &str,
) -> (ToolCallStatus, String) {
    let mut session_state = deps.session_state;
    let name = args_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let result = remove_loaded_skill(&mut session_state.write(), session_id, &name);
    match result {
        Ok(()) => {
            crate::session_events::log_event(
                session_id,
                crate::session_events::SessionEvent::SkillUnloaded { name: name.clone() },
            );
            tracing::info!("Model unloaded skill '{}' from session.loaded_skills", name);
            (
                ToolCallStatus::Completed,
                format!("Unloaded skill '{}' from session context.", name),
            )
        }
        Err(msg) => (ToolCallStatus::Error, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unload_removes_only_the_named_skill() {
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        {
            let s = state.sessions.get_mut(&id).unwrap();
            s.loaded_skills.insert("news".into(), "{}".into());
            s.loaded_skills.insert("timestamp".into(), "{}".into());
        }
        assert!(remove_loaded_skill(&mut state, &id, "news").is_ok());
        let s = state.sessions.get(&id).unwrap();
        assert!(!s.loaded_skills.contains_key("news"));
        assert!(s.loaded_skills.contains_key("timestamp"));
    }

    #[test]
    fn unload_of_unloaded_skill_errors_and_lists_loaded() {
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        state
            .sessions
            .get_mut(&id)
            .unwrap()
            .loaded_skills
            .insert("timestamp".into(), "{}".into());
        let err = remove_loaded_skill(&mut state, &id, "news").unwrap_err();
        assert!(err.contains("'news' is not loaded"), "{err}");
        assert!(err.contains("[timestamp]"), "{err}");
        assert!(remove_loaded_skill(&mut state, "missing-session", "news").is_err());
    }

    /// Guards the drift that caused the original bug: a tool advertised by
    /// `CoreClient` but missing from `BUILTIN_TOOLS` reaches `NativeCore` and
    /// fails as an unintercepted call.
    #[test]
    fn every_core_client_tool_is_dispatchable() {
        let relay = crate::mcp::core_client::CoreClient::relay_tools();
        for tool in crate::mcp::core_client::CoreClient::new().list_tools().into_iter().chain(relay) {
            assert!(
                is_builtin_tool(&tool.name),
                "CoreClient advertises '{}' but BUILTIN_TOOLS does not list it — \
                 it would fail as an unintercepted NativeCore call",
                tool.name
            );
        }
    }

    /// Same drift guard for the planner: a tool advertised by `PlannerClient`
    /// but not dispatchable here reaches `NativePlanner` and fails.
    #[test]
    fn every_planner_client_tool_is_dispatchable() {
        for tool in crate::mcp::planner_client::PlannerClient::new().list_tools() {
            assert!(
                is_builtin_tool(&tool.name),
                "PlannerClient advertises '{}' but BUILTIN_TOOLS does not list it — \
                 it would fail as an unintercepted NativePlanner call",
                tool.name
            );
            assert!(
                is_planner_tool(&tool.name),
                "'{}' is advertised by PlannerClient but is_planner_tool() misses it — \
                 it would dodge the planner_enabled gate and the disabled-tool filter",
                tool.name
            );
        }
    }

    /// HOBBES_AGENT_TIME is dispatchable and planner-gated, but never
    /// advertised by `list_tools` — ordinary chats must not see it.
    #[test]
    fn agent_time_tool_is_dispatchable_but_withheld() {
        let listed: Vec<String> = crate::mcp::planner_client::PlannerClient::new()
            .list_tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for tool in crate::mcp::planner_client::PlannerClient::agent_time_tools() {
            assert!(is_builtin_tool(&tool.name));
            assert!(is_planner_tool(&tool.name));
            assert!(is_agent_time_tool(&tool.name));
            assert!(!listed.contains(&tool.name.to_string()), "{} must not be in list_tools", tool.name);
        }
        let mut planner = crate::todo::PlannerState::default();
        assert!(!agent_time_tool_visible(&planner, "chat"));
        planner.agent_time_lease = Some(crate::todo::agent_time::AgentTimeLease {
            chat_session_id: "chat".into(),
            opened_at: chrono::Utc::now(),
        });
        assert!(agent_time_tool_visible(&planner, "chat"));
        assert!(!agent_time_tool_visible(&planner, "other"));
    }

    fn two_projects() -> crate::todo::PlannerState {
        let mut planner = crate::todo::PlannerState::default();
        for (id, title) in [("keep", "Hobbes"), ("dup", "cai-hobbes"), ("other", "Taxes")] {
            planner.projects.push(crate::todo::model::Project {
                id: id.into(),
                title: title.into(),
                notes: String::new(),
                area_id: None,
                status: crate::todo::model::TodoStatus::Open,
                deadline: None,
                sort_order: 0.0,
                path: None,
                aliases: Vec::new(),
                distinct_from: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            });
        }
        let mut t = crate::todo::model::Todo::new("ship it", 0.0);
        t.project_id = Some("dup".into());
        planner.todos.push(t);
        planner
    }

    /// HOBBES_PROJECT_MERGE is an always-ask built-in: the streaming path
    /// holds it for approval every time (no trust rule or always-allow can
    /// skip that), and dispatch refuses it without the approval.
    #[test]
    fn project_merge_always_asks_and_refuses_unapproved() {
        assert!(always_asks("HOBBES_PROJECT_MERGE"));
        assert!(is_builtin_tool("HOBBES_PROJECT_MERGE"));
        assert!(is_planner_tool("HOBBES_PROJECT_MERGE"));
        for t in ["HOBBES_PROJECT_LIST", "HOBBES_PROJECT_UPSERT", "HOBBES_TODO_CREATE"] {
            assert!(!always_asks(t), "{t}");
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let ctx = crate::todo::dispatch::tests::test_ctx_with(two_projects(), Settings::default());
            let args = serde_json::json!({ "keep_id": "keep", "merge_ids": ["dup"] });
            let tc = ToolCall::new("hobbes-planner".into(), "HOBBES_PROJECT_MERGE".into(), args.clone(), None, None);
            assert!(builtin_needs_approval(ctx, &tc, &args, "chat"), "always held for approval");
            // A trust rule naming the tool changes nothing.
            ctx.settings.clone().write().permission_settings.trust_rules.push(
                crate::context::permissions::TrustRule {
                    id: "r".into(),
                    server: "hobbes-planner".into(),
                    tool: Some("HOBBES_PROJECT_MERGE".into()),
                    command_prefix: None,
                    project_id: None,
                    created_at: chrono::Utc::now(),
                },
            );
            assert!(builtin_needs_approval(ctx, &tc, &args, "chat"));

            let handle = tokio::runtime::Handle::current();
            let (status, msg) = handle.block_on(project_merge_tool(ctx, &args, false));
            assert_eq!(status, ToolCallStatus::Error);
            assert!(msg.contains("approval"), "{msg}");
            assert_eq!(ctx.planner.peek().projects.len(), 3, "nothing merged");

            let (status, msg) = handle.block_on(project_merge_tool(ctx, &args, true));
            assert_eq!(status, ToolCallStatus::Completed, "{msg}");
            assert!(msg.contains("1 to-do(s)"), "{msg}");
            let planner = ctx.planner.peek();
            assert!(!planner.projects.iter().any(|p| p.id == "dup"));
            assert_eq!(planner.todos[0].project_id.as_deref(), Some("keep"));
            drop(planner);

            // Planner off: not held (dispatch refuses it at once).
            let off = crate::todo::dispatch::tests::test_ctx_with(
                two_projects(),
                Settings { planner_enabled: false, ..Default::default() },
            );
            assert!(!builtin_needs_approval(off, &tc, &args, "chat"));
            let (status, _) = handle.block_on(project_merge_tool(off, &args, true));
            assert_eq!(status, ToolCallStatus::Error);
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    /// The release gate (`Settings::agent_time_enabled`, off by default):
    /// dispatch refuses HOBBES_DECISIONS and HOBBES_AGENT_TIME outright, and
    /// the decision queue answers again once the switch is on.
    #[test]
    fn hidden_agent_time_and_decisions_are_refused_at_dispatch() {
        assert!(is_experimental_agent_tool("HOBBES_AGENT_TIME"));
        assert!(is_experimental_agent_tool("HOBBES_DECISIONS"));
        assert!(!is_experimental_agent_tool("HOBBES_DISPATCH"));
        assert!(!is_experimental_agent_tool("HOBBES_TODO_LIST"));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let hidden = crate::todo::dispatch::tests::test_ctx_with(
                crate::todo::PlannerState::default(),
                Settings::default(),
            );
            let list = serde_json::json!({ "action": "list" });
            for tool in ["HOBBES_DECISIONS", "HOBBES_AGENT_TIME"] {
                let (status, msg) = run_planner_tool(hidden, tool, &list, "chat");
                assert_eq!(status, ToolCallStatus::Error, "{tool}: {msg}");
                assert_eq!(msg, EXPERIMENTAL_AGENT_TOOL_REFUSAL, "{tool}");
            }
            // The rest of the planner is untouched.
            let (status, msg) = run_planner_tool(hidden, "HOBBES_TODO_LIST", &serde_json::json!({}), "chat");
            assert_eq!(status, ToolCallStatus::Completed, "{msg}");

            let on = crate::todo::dispatch::tests::test_ctx_with(
                crate::todo::PlannerState::default(),
                Settings { experimental_agent_time: true, ..Default::default() },
            );
            let (status, msg) = run_planner_tool(on, "HOBBES_DECISIONS", &list, "chat");
            assert_eq!(status, ToolCallStatus::Completed, "{msg}");
            assert!(msg.starts_with("No decisions waiting."), "{msg}");
            // HOBBES_AGENT_TIME still has its own lease check behind the gate.
            let (status, msg) =
                run_planner_tool(on, "HOBBES_AGENT_TIME", &serde_json::json!({"action": "start", "intent": "x"}), "chat");
            assert_eq!(status, ToolCallStatus::Error);
            assert_ne!(msg, EXPERIMENTAL_AGENT_TOOL_REFUSAL);
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn unknown_tools_are_not_builtin() {
        assert!(!is_builtin_tool("some_mcp_server_tool"));
        assert!(!is_builtin_tool("MCP_LOAD_SERVER_TOOLS"));
        assert!(!is_planner_tool("HOBBES_UPDATE_SCRATCHPAD"));
        assert!(!is_planner_tool("MCP_LOAD_SERVER_TOOLS"));
    }
}
