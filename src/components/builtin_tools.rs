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
use crate::context::permissions::{PermissionManager, PermissionStatus};
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
    "HOBBES_INVOKE_SKILL",
    "HOBBES_SET_TIMER",
    "HOBBES_LIST_TIMERS",
    "HOBBES_CANCEL_TIMER",
    "HOBBES_FLEET_STATUS",
    "HOBBES_TODO_CREATE",
    "HOBBES_TODO_UPDATE",
    "HOBBES_TODO_LIST",
    "HOBBES_PLAN_DAY",
    "HOBBES_TIME_BLOCK",
    "HOBBES_PROJECT_UPSERT",
    "HOBBES_CALENDAR_LIST",
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
                | "HOBBES_CALENDAR_LIST"
        )
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
}

pub struct BuiltinOutcome {
    pub status: ToolCallStatus,
    pub response: String,
    /// Whether the caller should persist `SessionState` after writing the result
    /// back into the message. Pagination is turn-local and skips the write; the
    /// scratchpad, skills, and timers must survive a restart.
    pub persist: bool,
}

/// Run `tool_call` if it is a built-in, otherwise return `None` so the caller
/// falls through to normal MCP dispatch.
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

        "HOBBES_SET_TIMER" => {
            let (status, response) = session_state.write().handle_set_timer(args_json, session_id);
            Some(BuiltinOutcome {
                status,
                response,
                persist: true,
            })
        }

        "HOBBES_LIST_TIMERS" => {
            let (status, response) = session_state.read().handle_list_timers(session_id);
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
                invoke_skill(deps, args_json, session_id, profile_id).await;
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
            handlers::handle_time_block(&mut planner.write(), args_json, today, true)
        }
        "HOBBES_PROJECT_UPSERT" => {
            handlers::handle_project_upsert(&mut planner.write(), args_json, today, true)
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

/// The model-invocation path for skills: execute the skill (capability
/// registration only), persist its payload into `session.loaded_skills` so it
/// stays in system context for later turns, and return the instruction manual as
/// the tool result.
async fn invoke_skill(
    deps: BuiltinToolCtx,
    args_json: &serde_json::Value,
    session_id: &str,
    profile_id: Option<&String>,
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

    let permission = deps
        .permission_manager
        .read()
        .check_skill_permission(&skill.metadata.name);
    if permission != PermissionStatus::Allowed {
        return (
            ToolCallStatus::Error,
            format!(
                "The user has disabled auto-approval for skill '{}'. Ask them to run /{} themselves or enable the skill in Settings → Permissions.",
                skill_name, skill_name
            ),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the drift that caused the original bug: a tool advertised by
    /// `CoreClient` but missing from `BUILTIN_TOOLS` reaches `NativeCore` and
    /// fails as an unintercepted call.
    #[test]
    fn every_core_client_tool_is_dispatchable() {
        for tool in crate::mcp::core_client::CoreClient::new().list_tools() {
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

    #[test]
    fn unknown_tools_are_not_builtin() {
        assert!(!is_builtin_tool("some_mcp_server_tool"));
        assert!(!is_builtin_tool("MCP_LOAD_SERVER_TOOLS"));
        assert!(!is_planner_tool("HOBBES_UPDATE_SCRATCHPAD"));
        assert!(!is_planner_tool("MCP_LOAD_SERVER_TOOLS"));
    }
}
