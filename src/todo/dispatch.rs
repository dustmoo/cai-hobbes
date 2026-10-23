//! HOBBES_DISPATCH: turn an assignment (a planner todo) into a running
//! worker — a seeded Hobbes chat tab, or a headless `claude -p` run in the
//! todo's project directory.
//!
//! Auto-approval rationale: dispatch itself is metadata-only (it creates a
//! session/row and a prompt). The dispatched WORK stays fully gated — a chat
//! tab runs the normal in-app approval flow (with trust rules), and a
//! headless run uses the default permission mode so its PermissionRequests
//! hold in the Fleet UI for Approve/Deny. Never `--bare` (it skips hooks and
//! would make the run invisible to the fleet) and never bypass modes.

use dioxus::prelude::{Readable, Writable};

use crate::components::builtin_tools::BuiltinToolCtx;
use crate::settings::ModelChoice;
use crate::todo::model::{AgentState, Lane, Project, Todo, TodoStatus};

/// Dispatching IS handing the to-do to an agent: it joins the agent lane and
/// starts `Working` (its brief-driven state takes over from there).
fn mark_dispatched(t: &mut Todo, worker_session: &str) {
    t.linked_fleet_session = Some(worker_session.to_string());
    t.set_lane(Lane::Agent);
    t.agent_state = Some(AgentState::Working);
    t.updated_at = chrono::Utc::now();
}

/// The extra line a Hobbes chat worker gets: how to raise a decision so it
/// lands in the person's decision queue. With the decision queue hidden
/// (`Settings::agent_time_enabled` off) the worker asks in its own chat.
fn chat_worker_note(todo_id: &str, decisions_on: bool) -> String {
    if !decisions_on {
        return "\nIf you need a decision from the user, ask it here in this chat as a one-line \
                question with the options, then stop; the user will answer here."
            .to_string();
    }
    format!(
        "\nIf you need a decision from the user, call HOBBES_DECISIONS with action 'ask' \
         (todo_id: {todo_id}) giving a one-line question, the options, and an estimate, then stop; \
         the answer will arrive here as a message."
    )
}

/// The line a chat worker linked to its parent chat gets (`todo::relay`).
const RELAY_WORKER_NOTE: &str = "\nYou were dispatched from another chat. To ask it a question or hand it a \
     result, call HOBBES_MESSAGE_PARENT; a short completion report goes to it automatically \
     when you finish.";

/// What the spawner is asked to launch. Separated behind a trait so the
/// handler is testable without running `claude`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRequest {
    pub prompt: String,
    pub session_id: String,
    pub model: Option<String>,
    pub cwd: String,
}

pub trait ClaudeSpawner: Send + Sync {
    /// Fire the run; the receiver reports launch success/failure (NOT run
    /// completion — the fleet observes the run itself via hooks).
    fn spawn(&self, req: SpawnRequest) -> tokio::sync::oneshot::Receiver<Result<(), String>>;
}

/// Production spawner: `claude -p <prompt> --session-id <uuid> [--model m]`
/// in the project directory, with the same sane PATH/env the MCP stdio
/// spawns use (a bundled .app's environment won't find `claude` otherwise).
/// The awaiting task reaps the child; runs die with the app — the fleet row
/// survives restarts via hydration, and briefs already landed stay.
pub struct RealClaudeSpawner;

impl ClaudeSpawner for RealClaudeSpawner {
    fn spawn(&self, req: SpawnRequest) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        dioxus::prelude::spawn(async move {
            let mut cmd = tokio::process::Command::new("claude");
            cmd.arg("-p")
                .arg(&req.prompt)
                .arg("--session-id")
                .arg(&req.session_id)
                .current_dir(&req.cwd)
                .env("PATH", crate::mcp::manager::McpManager::get_sane_path())
                .envs(crate::mcp::manager::McpManager::get_critical_env_vars())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(model) = &req.model {
                cmd.arg("--model").arg(model);
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    let _ = tx.send(Ok(()));
                    // Await to reap; the run's progress is observed via hooks.
                    match child.wait().await {
                        Ok(status) => tracing::info!(
                            "dispatched claude run {} exited: {status}",
                            req.session_id
                        ),
                        Err(e) => tracing::warn!(
                            "dispatched claude run {} wait failed: {e}",
                            req.session_id
                        ),
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("failed to launch claude: {e}")));
                }
            }
        });
        rx
    }
}

/// The assignment prompt a worker receives.
pub fn build_assignment_prompt(
    todo: &Todo,
    project: Option<&Project>,
    instructions: Option<&str>,
) -> String {
    let mut out = format!("Assignment: {}\n", todo.title);
    if let Some(o) = todo.outcome.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        out.push_str(&format!("Definition of done: {}\n", o));
    }
    if let Some(p) = project {
        out.push_str(&format!("Project: {}\n", p.title));
    }
    if !todo.notes.trim().is_empty() {
        out.push_str(&format!("\nNotes:\n{}\n", todo.notes.trim()));
    }
    if !todo.checklist.is_empty() {
        out.push_str("\nChecklist:\n");
        for item in &todo.checklist {
            out.push_str(&format!(
                "- [{}] {}\n",
                if item.done { "x" } else { " " },
                item.title
            ));
        }
    }
    if let Some(progress) = todo.latest_progress.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!("\nLatest progress so far: {progress}\n"));
    }
    if let Some(extra) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        out.push_str(&format!("\nAdditional instructions: {extra}\n"));
    }
    out.push_str(
        "\nWork autonomously. When blocked, stop and summarize where you got stuck.",
    );
    out
}

/// The open to-do `todo_id` and its project, or why it can't be dispatched.
fn dispatchable(deps: &BuiltinToolCtx, todo_id: &str) -> Result<(Todo, Option<Project>), String> {
    let planner = deps.planner.read();
    let todo = planner
        .todo(todo_id)
        .ok_or_else(|| format!("unknown todo id '{todo_id}'"))?
        .clone();
    if matches!(todo.status, TodoStatus::Completed | TodoStatus::Cancelled) {
        return Err(format!("todo '{}' is already closed", todo.title));
    }
    let project = todo
        .project_id
        .as_deref()
        .and_then(|pid| planner.projects.iter().find(|p| p.id == pid))
        .cloned();
    Ok((todo, project))
}

/// Pin `session` to a validated connector/model choice, exactly as the chat
/// bar's model switch does (`ChatCommand::SwitchModel` / `SwitchConnector`
/// in main.rs): the connector id, the legacy provider mirror, and the model
/// (`None` = the connector's own model).
pub fn pin_session_model(session: &mut crate::session::Session, choice: &ModelChoice) {
    session.llm_connector_id = Some(choice.connector_id.clone());
    session.llm_provider = Some(choice.provider);
    session.chat_model = choice.model.clone();
}

/// Clear a session's connector/model pin (back to the user's default).
pub fn unpin_session_model(session: &mut crate::session::Session) {
    session.llm_connector_id = None;
    session.llm_provider = None;
    session.chat_model = None;
}

/// Journal a pin so the session's event log replays it.
pub fn log_pin(session_id: &str, choice: Option<&ModelChoice>) {
    crate::session_events::log_event(
        session_id,
        crate::session_events::SessionEvent::ConnectorPinned {
            connector_id: choice.map(|c| c.connector_id.clone()),
            provider: choice.map(|c| format!("{:?}", c.provider)),
            model: choice.and_then(|c| c.model.clone()),
        },
    );
}

/// How [`dispatch_to_chat`] opens the worker.
#[derive(Debug, Clone, Default)]
pub struct ChatDispatchOpts<'a> {
    /// Extra guidance appended to the assignment.
    pub instructions: Option<&'a str>,
    /// A validated model choice to pin the new tab to (`None` = the user's
    /// default).
    pub choice: Option<ModelChoice>,
    /// The chat that asked: its Composio profile is inherited, and the new
    /// worker is linked to it for the dispatch relay (`todo::relay`).
    pub parent_session_id: Option<&'a str>,
    /// Switch to the new tab (the model's dispatch does). `false` leaves the
    /// user where they are; the caller opens the tab in the background.
    pub focus: bool,
}

/// What [`dispatch_to_chat`] started.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDispatched {
    pub session_id: String,
    pub title: String,
    /// The model the new chat runs on (its pin, else the default).
    pub model: String,
}

/// **The chat branch of `HOBBES_DISPATCH`, reusable**: open a new tab
/// (project-tagged, pinned to `opts.choice` when given), mark the to-do
/// dispatched (agent lane, `Working`, linked, `worker_model` recorded) and
/// queue the assignment, which the background drain sends.
pub fn dispatch_to_chat(
    mut deps: BuiltinToolCtx,
    todo_id: &str,
    opts: ChatDispatchOpts,
) -> Result<ChatDispatched, String> {
    let (todo, project) = dispatchable(&deps, todo_id)?;
    let mut prompt = build_assignment_prompt(&todo, project.as_ref(), opts.instructions);
    // Inherit the dispatching tab's Composio profile (else the global
    // default), as every UI path that opens a tab does. A tab with no
    // profile would see every profile's Composio tools, which collide by
    // name ("Duplicate function declaration").
    let profile = opts
        .parent_session_id
        .and_then(|pid| {
            deps.session_state
                .peek()
                .sessions
                .get(pid)
                .and_then(|s| s.composio_profile.clone())
        })
        .or_else(|| deps.settings.peek().get_active_profile().map(|p| p.id.clone()));
    let new_id = {
        let mut state = deps.session_state.write();
        // create_session makes the new tab the active session; an unfocused
        // dispatch keeps the user where they are.
        let previous_active = state.active_session_id.clone();
        let id = state.create_session(profile);
        if let Some(s) = state.sessions.get_mut(&id) {
            s.project_id = todo.project_id.clone();
            if let Some(c) = &opts.choice {
                pin_session_model(s, c);
            }
        }
        if !opts.focus && !previous_active.is_empty() {
            state.active_session_id = previous_active;
        }
        id
    };
    crate::session_events::log_event(
        &new_id,
        crate::session_events::SessionEvent::ProjectTagged {
            project_id: todo.project_id.clone(),
            user_set: false,
        },
    );
    if let Some(c) = &opts.choice {
        log_pin(&new_id, Some(c));
    }
    // The parent ↔ worker link the dispatch relay travels along.
    let linked = opts
        .parent_session_id
        .is_some_and(|parent| crate::todo::relay::record_link(deps.session_state, parent, &new_id, todo_id));
    crate::session::SessionState::save_signal(&deps.session_state, None);
    let model = {
        let settings = deps.settings.peek();
        let state = deps.session_state.peek();
        state
            .sessions
            .get(&new_id)
            .map(|s| settings.chat_model_for_session(s))
            .unwrap_or_else(|| settings.active_chat_model())
    };
    {
        let mut planner = deps.planner.write();
        if let Some(t) = planner.todo_mut(todo_id) {
            mark_dispatched(t, &new_id);
            t.worker_model = Some(model.clone()).filter(|m| !m.trim().is_empty());
        }
    }
    let decisions_on = deps.settings.peek().agent_time_enabled();
    prompt.push_str(&chat_worker_note(todo_id, decisions_on));
    if linked && deps.settings.peek().dispatch_relay_mode != crate::settings::DispatchRelayMode::Off {
        prompt.push_str(RELAY_WORKER_NOTE);
    }
    if let Some(t) = deps.planner.peek().todo(todo_id) {
        let _ = crate::todo::store::save_todo(t);
    }
    crate::components::chat_queue::queue_push(
        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
        &new_id,
        crate::components::chat_queue::QueuedMessage::new(prompt, vec![]),
    );
    if opts.focus {
        deps.chat_command.set(Some(
            crate::components::chat_input::ChatCommand::SwitchToSession(new_id.clone()),
        ));
    }
    crate::components::chat_queue::request_drain();
    Ok(ChatDispatched { session_id: new_id, title: todo.title, model })
}

/// The HOBBES_DISPATCH handler. Returns (status-ok?, response) like the
/// other planner handlers; the dispatch layer persists on success.
/// `parent_session_id` is the chat session the tool call came from — the
/// conversation that gets handed the result when the run finishes.
///
/// `target: "chat"` takes an optional `connector` / `model`, validated
/// against `Settings::allowed_models` (an invalid choice is an error listing
/// what is allowed, never a silent fallback). `target: "claude_code"` passes
/// `model` straight to `claude --model`.
pub fn handle_dispatch(
    mut deps: BuiltinToolCtx,
    args: &serde_json::Value,
    spawner: &dyn ClaudeSpawner,
    parent_session_id: Option<&str>,
) -> Result<String, String> {
    let todo_id = args
        .get("todo_id")
        .and_then(|v| v.as_str())
        .ok_or("missing required 'todo_id'")?;
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or("missing required 'target' ('chat' or 'claude_code')")?;
    let instructions = args.get("instructions").and_then(|v| v.as_str());
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let (todo, project) = dispatchable(&deps, todo_id)?;

    match target {
        "chat" => {
            let connector = args.get("connector").and_then(|v| v.as_str());
            let choice = deps
                .settings
                .peek()
                .resolve_model_choice(connector, model.as_deref())
                .map_err(|e| format!("Not dispatched: {e}. Omit connector and model for the user's default."))?;
            // New tab, project-tagged, pinned, linked, seeded, focused.
            let d = dispatch_to_chat(
                deps,
                todo_id,
                ChatDispatchOpts { instructions, choice, parent_session_id, focus: true },
            )?;
            Ok(format!(
                "Dispatched '{}' to a new chat tab (session {}) on {}. The assignment \
                 sends automatically; progress will flow back onto the todo.",
                d.title,
                d.session_id,
                crate::settings::short_model_label(&d.model)
            ))
        }
        "claude_code" => {
            let prompt = build_assignment_prompt(&todo, project.as_ref(), instructions);
            let project =
                project.ok_or_else(|| {
                    format!(
                        "todo '{}' has no project — tag it to a project with a path first",
                        todo.title
                    )
                })?;
            let path = project
                .path
                .as_deref()
                .and_then(crate::services::project_tagger::norm_path)
                .filter(|p| std::path::Path::new(p).is_dir())
                .ok_or_else(|| {
                    format!(
                        "project '{}' has no valid path — set one with HOBBES_PROJECT_UPSERT \
                         (e.g. path: \"~/Sites/{}\"), then retry",
                        project.title,
                        project.title.to_lowercase().replace(' ', "-")
                    )
                })?;

            let run_id = uuid::Uuid::new_v4().to_string();
            {
                let mut planner = deps.planner.write();
                if let Some(t) = planner.todo_mut(todo_id) {
                    mark_dispatched(t, &run_id);
                    t.worker_model = model.clone().filter(|m| !m.trim().is_empty());
                }
            }
            if let Some(t) = deps.planner.peek().todo(todo_id) {
                let _ = crate::todo::store::save_todo(t);
            }
            crate::fleet::bridge::precreate_dispatched_session(
                &run_id,
                &path,
                &todo.title,
                todo_id,
                parent_session_id,
            );

            let launch_rx = spawner.spawn(SpawnRequest {
                prompt,
                session_id: run_id.clone(),
                model,
                cwd: path.clone(),
            });
            // Launch-failure watchdog: mark the row ended and surface the
            // error on the todo so a dead dispatch never sits "running".
            {
                let mut planner = deps.planner;
                let run_id = run_id.clone();
                let todo_id = todo_id.to_string();
                dioxus::prelude::spawn(async move {
                    if let Ok(Err(e)) = launch_rx.await {
                        tracing::error!("dispatch launch failed: {e}");
                        crate::fleet::store::merge_session(&run_id, |row| {
                            row.ended_at = Some(chrono::Utc::now());
                        });
                        crate::fleet::shared().poke();
                        let changed = {
                            let mut p = planner.write();
                            if let Some(t) = p.todo_mut(&todo_id) {
                                t.latest_progress =
                                    Some(format!("Dispatch failed to launch claude: {e}"));
                                // Explicit: the worker never started.
                                t.agent_state = Some(AgentState::Stuck);
                                t.updated_at = chrono::Utc::now();
                                true
                            } else {
                                false
                            }
                        };
                        if changed {
                            let p = planner.peek();
                            if let Some(t) = p.todo(&todo_id) {
                                let _ = crate::todo::store::save_todo(t);
                            }
                        }
                    }
                });
            }
            Ok(format!(
                "Dispatched '{}' to a headless Claude Code run in {path} (fleet session \
                 {run_id}). Its permission requests will appear in the Fleet for approval; \
                 progress lands on the todo via briefs.",
                todo.title
            ))
        }
        other => Err(format!("unknown target '{other}' — use 'chat' or 'claude_code'")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::todo::model::ChecklistItem;
    use dioxus::prelude::*;

    fn item(title: &str, done: bool) -> ChecklistItem {
        ChecklistItem {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            done,
        }
    }

    struct RecordingSpawner(std::sync::Mutex<Vec<SpawnRequest>>);
    impl ClaudeSpawner for RecordingSpawner {
        fn spawn(&self, req: SpawnRequest) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
            self.0.lock().unwrap().push(req);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(Ok(()));
            rx
        }
    }

    fn test_ctx(planner: crate::todo::PlannerState) -> BuiltinToolCtx {
        test_ctx_with(planner, crate::settings::Settings::default())
    }

    pub(crate) fn test_ctx_with(planner: crate::todo::PlannerState, settings: crate::settings::Settings) -> BuiltinToolCtx {
        use dioxus::prelude::Signal;
        let sessions = crate::session::SessionState { save_disabled: true, ..Default::default() };
        BuiltinToolCtx {
            session_state: Signal::new(sessions),
            settings: Signal::new(settings),
            skill_registry: Signal::new(crate::skills::SkillRegistry::default()),
            permission_manager: Signal::new(crate::context::permissions::PermissionManager::new(
                Signal::new(crate::settings::Settings::default()),
            )),
            mcp_context: Signal::new(crate::mcp::manager::McpContext {
                servers: Vec::new(),
                connected_toolkit_slugs: Vec::new(),
            }),
            planner: Signal::new(planner),
            chat_command: Signal::new(None),
        }
    }

    #[test]
    fn dispatch_validation_errors_name_the_fix() {
        // Signals need a Dioxus runtime; run the assertions inside a
        // one-shot component (the stream_manager test harness idiom).
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let spawner = RecordingSpawner(Default::default());
            let mut planner = crate::todo::PlannerState::default();
            let todo = Todo::new("Orphan task", 1.0);
            let todo_id = todo.id.clone();
            planner.todos.push(todo);

            // Unknown todo.
            let err = handle_dispatch(
                test_ctx(planner.clone()),
                &serde_json::json!({"todo_id": "nope", "target": "claude_code"}),
                &spawner,
                None,
            )
            .unwrap_err();
            assert!(err.contains("unknown todo"));

            // No project → names the fix.
            let err = handle_dispatch(
                test_ctx(planner.clone()),
                &serde_json::json!({"todo_id": todo_id, "target": "claude_code"}),
                &spawner,
                None,
            )
            .unwrap_err();
            assert!(err.contains("no project"), "got: {err}");

            // Bad target.
            let err = handle_dispatch(
                test_ctx(planner),
                &serde_json::json!({"todo_id": todo_id, "target": "carrier_pigeon"}),
                &spawner,
                None,
            )
            .unwrap_err();
            assert!(err.contains("carrier_pigeon"));
            assert!(spawner.0.lock().unwrap().is_empty(), "nothing spawned on errors");

            // A dispatch carrying a parent chat records `dispatched_from` on
            // the precreated fleet row, so the run's continuation later knows
            // which chat to hand the result back to.
            let mut planner_with_project = crate::todo::PlannerState::default();
            let mut project = Project {
                id: "proj-1".into(),
                title: "Puget Bench".into(),
                notes: String::new(),
                area_id: None,
                status: TodoStatus::Open,
                deadline: None,
                sort_order: 0.0,
                path: Some(std::env::temp_dir().to_string_lossy().to_string()),
                aliases: Vec::new(),
                distinct_from: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            project.path = Some(std::env::temp_dir().to_string_lossy().to_string());
            let mut linked_todo = Todo::new("Ship CSV export", 1.0);
            linked_todo.project_id = Some(project.id.clone());
            let linked_todo_id = linked_todo.id.clone();
            planner_with_project.projects.push(project);
            planner_with_project.todos.push(linked_todo);

            let ok = handle_dispatch(
                test_ctx(planner_with_project),
                &serde_json::json!({"todo_id": linked_todo_id, "target": "claude_code"}),
                &spawner,
                Some("parent-chat-1"),
            )
            .unwrap();
            let req = spawner
                .0
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("claude_code dispatch spawns a run");
            assert!(ok.contains(&req.session_id));
            let shared = crate::fleet::shared();
            let row = shared
                .state
                .lock()
                .expect("fleet state lock poisoned")
                .sessions
                .get(&req.session_id)
                .cloned()
                .expect("precreate_dispatched_session inserts the row");
            assert_eq!(row.dispatched_from.as_deref(), Some("parent-chat-1"));
            assert!(!req.prompt.contains("HOBBES_DECISIONS"), "headless runs have no Hobbes tools");

            dioxus::prelude::rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn dispatch_moves_the_todo_into_the_agent_lane() {
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let spawner = RecordingSpawner(Default::default());
            let mut planner = crate::todo::PlannerState::default();
            let project = Project {
                id: "proj-2".into(),
                title: "Bench".into(),
                notes: String::new(),
                area_id: None,
                status: TodoStatus::Open,
                deadline: None,
                sort_order: 0.0,
                path: Some(std::env::temp_dir().to_string_lossy().to_string()),
                aliases: Vec::new(),
                distinct_from: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            let mut todo = Todo::new("Run the 70B suite", 1.0);
            todo.project_id = Some(project.id.clone());
            todo.kind = Some(crate::todo::model::HumanKind::Chore);
            todo.outcome = Some("Results table in Notes".into());
            let id = todo.id.clone();
            planner.projects.push(project);
            planner.todos.push(todo);
            let ctx = test_ctx(planner);
            handle_dispatch(
                ctx,
                &serde_json::json!({"todo_id": id, "target": "claude_code"}),
                &spawner,
                None,
            )
            .unwrap();
            let t = ctx.planner.peek().todo(&id).cloned().unwrap();
            assert_eq!(t.lane, Lane::Agent);
            assert_eq!(t.agent_state, Some(AgentState::Working));
            assert_eq!(t.kind, None, "kinds belong to human to-dos");
            let req = spawner.0.lock().unwrap().last().cloned().unwrap();
            assert!(req.prompt.contains("Definition of done: Results table in Notes"));

            dioxus::prelude::rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    fn planner_with(title: &str) -> (crate::todo::PlannerState, String) {
        let mut planner = crate::todo::PlannerState::default();
        let todo = Todo::new(title, 1.0);
        let id = todo.id.clone();
        planner.todos.push(todo);
        (planner, id)
    }

    #[test]
    fn a_chat_dispatch_validates_and_pins_the_chosen_model() {
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let spawner = RecordingSpawner(Default::default());
            let settings = crate::settings::test_delegation_settings();
            let claude_id = settings.llm_connectors.iter().find(|c| c.name == "Claude").unwrap().id.clone();

            // Allowed: the new tab is pinned like the chat bar pins one.
            let (planner, id) = planner_with("Summarise the thread");
            let ctx = test_ctx_with(planner, settings.clone());
            let ok = handle_dispatch(
                ctx,
                &serde_json::json!({"todo_id": id, "target": "chat", "connector": "Claude", "model": "claude-haiku-5-5"}),
                &spawner,
                None,
            )
            .unwrap();
            assert!(ok.contains("on Haiku 5.5"), "{ok}");
            let t = ctx.planner.peek().todo(&id).cloned().unwrap();
            assert_eq!(t.worker_model.as_deref(), Some("claude-haiku-5-5"));
            assert_eq!((t.lane, t.agent_state), (Lane::Agent, Some(AgentState::Working)));
            let sid = t.linked_fleet_session.clone().unwrap();
            let state = ctx.session_state.peek();
            let s = state.sessions.get(&sid).expect("new tab");
            assert_eq!(s.llm_connector_id.as_deref(), Some(claude_id.as_str()));
            assert_eq!(s.llm_provider, Some(crate::settings::LlmProvider::Claude));
            assert_eq!(s.chat_model.as_deref(), Some("claude-haiku-5-5"));
            drop(state);
            assert!(crate::components::chat_queue::CHAT_QUEUE.peek().get(&sid).is_some_and(|q| q.len() == 1));

            // Unknown model / unknown connector: refused with the allowed
            // list, nothing opened, the to-do untouched.
            for (args, needle) in [
                (serde_json::json!({"target": "chat", "model": "gpt-9"}), "gpt-9"),
                (serde_json::json!({"target": "chat", "connector": "Bedrock"}), "unknown connector 'Bedrock'"),
                (serde_json::json!({"target": "chat", "connector": "Local", "model": "claude-haiku-5-5"}), "isn't available on connector 'Local'"),
            ] {
                let (planner, id) = planner_with("Look something up");
                let ctx = test_ctx_with(planner, settings.clone());
                let mut args = args;
                args["todo_id"] = serde_json::Value::String(id.clone());
                let err = handle_dispatch(ctx, &args, &spawner, None).unwrap_err();
                assert!(err.contains(needle) && err.contains("Claude: claude-fable-5-1"), "{err}");
                assert!(ctx.session_state.peek().sessions.is_empty(), "no tab opened");
                let t = ctx.planner.peek().todo(&id).cloned().unwrap();
                assert_eq!((t.lane, t.linked_fleet_session), (Lane::Human, None));
            }

            // No choice: the user's default, no pin.
            let (planner, id) = planner_with("Format the table");
            let ctx = test_ctx_with(planner, settings.clone());
            handle_dispatch(ctx, &serde_json::json!({"todo_id": id, "target": "chat"}), &spawner, None).unwrap();
            let t = ctx.planner.peek().todo(&id).cloned().unwrap();
            let sid = t.linked_fleet_session.clone().unwrap();
            assert_eq!(ctx.session_state.peek().sessions.get(&sid).unwrap().llm_connector_id, None);
            assert_eq!(t.worker_model.as_deref(), Some("claude-opus-5-5"), "the default model is recorded");

            // claude_code keeps its own model param: no validation.
            let mut planner = crate::todo::PlannerState::default();
            let project = Project {
                id: "proj-m".into(),
                title: "Bench".into(),
                notes: String::new(),
                area_id: None,
                status: TodoStatus::Open,
                deadline: None,
                sort_order: 0.0,
                path: Some(std::env::temp_dir().to_string_lossy().to_string()),
                aliases: Vec::new(),
                distinct_from: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            let mut todo = Todo::new("Run the suite", 1.0);
            todo.project_id = Some(project.id.clone());
            let id = todo.id.clone();
            planner.projects.push(project);
            planner.todos.push(todo);
            let ctx = test_ctx_with(planner, settings);
            handle_dispatch(ctx, &serde_json::json!({"todo_id": id, "target": "claude_code", "model": "sonnet"}), &spawner, None)
                .unwrap();
            let req = spawner.0.lock().unwrap().last().cloned().unwrap();
            assert_eq!(req.model.as_deref(), Some("sonnet"));
            assert_eq!(ctx.planner.peek().todo(&id).unwrap().worker_model.as_deref(), Some("sonnet"));

            dioxus::prelude::rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn an_unfocused_chat_dispatch_keeps_the_user_where_they_are() {
        // The Planner's "Accept & start" when no agent time runs: a new chat
        // worker on the grant's model, opened in the background.
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let settings = crate::settings::test_delegation_settings();
            let choice = settings.resolve_model_choice(Some("Claude"), Some("claude-haiku-5-5")).unwrap();
            let mut planner = crate::todo::PlannerState::default();
            let mut todo = Todo::new("Draft the release notes", 1.0);
            todo.set_lane(Lane::Agent);
            let id = todo.id.clone();
            planner.todos.push(todo);
            let mut ctx = test_ctx_with(planner, settings);
            let here = ctx.session_state.write().create_session_raw(None);
            let d = dispatch_to_chat(ctx, &id, ChatDispatchOpts { choice, focus: false, ..Default::default() }).unwrap();
            assert_eq!(d.model, "claude-haiku-5-5");
            assert_eq!(ctx.session_state.peek().active_session_id, here, "focus stays put");
            assert_eq!(*ctx.chat_command.peek(), None, "no tab switch");
            let t = ctx.planner.peek().todo(&id).cloned().unwrap();
            assert_eq!(t.agent_state, Some(AgentState::Working));
            assert_eq!(t.linked_fleet_session.as_deref(), Some(d.session_id.as_str()));

            dioxus::prelude::rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn chat_worker_note_names_the_decision_queue_only_when_it_is_on() {
        let hidden = chat_worker_note("t1", false);
        assert!(!hidden.contains("HOBBES_DECISIONS"), "{hidden}");
        assert!(hidden.contains("ask it here in this chat"), "{hidden}");
        let on = chat_worker_note("t1", true);
        assert!(on.contains("HOBBES_DECISIONS") && on.contains("todo_id: t1"), "{on}");
    }

    #[test]
    fn assignment_prompt_carries_the_work() {
        let mut todo = Todo::new("Ship CSV export", 1.0);
        todo.notes = "Schema per use-case grouping".into();
        todo.checklist = vec![item("draft schema", false), item("review with team", true)];
        todo.latest_progress = Some("Schema drafted".into());
        let mut project = crate::todo::model::Project {
            id: "p1".into(),
            title: "Puget Bench".into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::Open,
            deadline: None,
            sort_order: 0.0,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        project.path = Some("~/Sites/puget".into());
        let p = build_assignment_prompt(&todo, Some(&project), Some("prioritize tests"));
        assert!(p.contains("Assignment: Ship CSV export"));
        assert!(p.contains("Project: Puget Bench"));
        assert!(p.contains("Schema per use-case grouping"));
        assert!(p.contains("- [ ] draft schema"));
        assert!(p.contains("- [x] review with team"));
        assert!(p.contains("Latest progress so far: Schema drafted"));
        assert!(p.contains("prioritize tests"));
        assert!(p.contains("Work autonomously"));
    }
}
