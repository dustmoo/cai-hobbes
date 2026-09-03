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
use crate::todo::model::{Project, Todo, TodoStatus};

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

/// The HOBBES_DISPATCH handler. Returns (status-ok?, response) like the
/// other planner handlers; the dispatch layer persists on success.
pub fn handle_dispatch(
    mut deps: BuiltinToolCtx,
    args: &serde_json::Value,
    spawner: &dyn ClaudeSpawner,
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

    let (todo, project) = {
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
        (todo, project)
    };
    let prompt = build_assignment_prompt(&todo, project.as_ref(), instructions);

    match target {
        "chat" => {
            // New tab, project-tagged, linked, seeded, focused. Focus-steal
            // is v1-accepted; background drain is out of scope (the queue
            // drain only serves the active session).
            let new_id = deps.session_state.write().create_session(None);
            {
                let mut state = deps.session_state.write();
                if let Some(s) = state.sessions.get_mut(&new_id) {
                    s.project_id = todo.project_id.clone();
                }
            }
            crate::session_events::log_event(
                &new_id,
                crate::session_events::SessionEvent::ProjectTagged {
                    project_id: todo.project_id.clone(),
                    user_set: false,
                },
            );
            crate::session::SessionState::save_signal(&deps.session_state, None);
            {
                let mut planner = deps.planner.write();
                if let Some(t) = planner.todo_mut(todo_id) {
                    t.linked_fleet_session = Some(new_id.clone());
                    t.updated_at = chrono::Utc::now();
                }
            }
            if let Some(t) = deps.planner.peek().todo(todo_id) {
                let _ = crate::todo::store::save_todo(t);
            }
            crate::components::chat_queue::queue_push(
                &mut crate::components::chat_queue::CHAT_QUEUE.write(),
                &new_id,
                crate::components::chat_queue::QueuedMessage::new(prompt, vec![]),
            );
            deps.chat_command.set(Some(
                crate::components::chat_input::ChatCommand::SwitchToSession(new_id.clone()),
            ));
            crate::components::chat_queue::request_drain();
            Ok(format!(
                "Dispatched '{}' to a new chat tab (session {new_id}). The assignment \
                 sends automatically; progress will flow back onto the todo.",
                todo.title
            ))
        }
        "claude_code" => {
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
                    t.linked_fleet_session = Some(run_id.clone());
                    t.updated_at = chrono::Utc::now();
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
mod tests {
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
        use dioxus::prelude::Signal;
        BuiltinToolCtx {
            session_state: Signal::new(crate::session::SessionState::default()),
            settings: Signal::new(crate::settings::Settings::default()),
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
            )
            .unwrap_err();
            assert!(err.contains("unknown todo"));

            // No project → names the fix.
            let err = handle_dispatch(
                test_ctx(planner.clone()),
                &serde_json::json!({"todo_id": todo_id, "target": "claude_code"}),
                &spawner,
            )
            .unwrap_err();
            assert!(err.contains("no project"), "got: {err}");

            // Bad target.
            let err = handle_dispatch(
                test_ctx(planner),
                &serde_json::json!({"todo_id": todo_id, "target": "carrier_pigeon"}),
                &spawner,
            )
            .unwrap_err();
            assert!(err.contains("carrier_pigeon"));
            assert!(spawner.0.lock().unwrap().is_empty(), "nothing spawned on errors");
            dioxus::prelude::rsx! { div {} }
        });
        dom.rebuild_in_place();
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
