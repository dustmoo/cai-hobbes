//! Project merge and per-project stats — pure, over `PlannerState`.
//!
//! The persistence half (store writes, session retagging through
//! `session_store`, trust rules) lives with its caller in
//! `components::planner_view::manage_projects`; this layer only computes.

use chrono::{DateTime, Utc};

use super::model::Project;
use super::project_identity;
use super::PlannerState;

/// What a merge changed, so the caller persists exactly that.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutcome {
    pub keep_id: String,
    /// The projects folded in (and removed from state).
    pub merged_ids: Vec<String>,
    /// To-dos whose `project_id` moved to the keeper.
    pub moved_todos: Vec<String>,
    /// Names added to the keeper's aliases.
    pub aliases_added: Vec<String>,
    /// Other projects whose `distinct_from` was rewritten.
    pub touched_projects: Vec<String>,
    /// The agent-time scope named a merged project.
    pub grant_changed: bool,
}

/// One project's activity, for `HOBBES_PROJECT_LIST` and Manage projects.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectStats {
    pub open_todos: usize,
    /// Latest of the project's own `updated_at` and its to-dos'.
    pub last_activity: DateTime<Utc>,
}

pub fn project_stats(state: &PlannerState, p: &Project) -> ProjectStats {
    let mut open_todos = 0;
    let mut last_activity = p.updated_at;
    for t in state.todos.iter().filter(|t| t.project_id.as_deref() == Some(p.id.as_str())) {
        if !t.status.is_closed() {
            open_todos += 1;
        }
        last_activity = last_activity.max(t.updated_at);
    }
    ProjectStats { open_todos, last_activity }
}

impl PlannerState {
    /// Fold `merge_ids` into `keep_id`: every to-do moves to the keeper, the
    /// merged titles and aliases become the keeper's aliases, a missing
    /// path / area / deadline is taken from the merged projects, their notes
    /// are appended, the agent-time scope is rewritten, and the merged
    /// projects are removed. Chat sessions are the caller's job (they live
    /// in `SessionState` and `session_store`).
    pub fn merge_projects(
        &mut self,
        keep_id: &str,
        merge_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<MergeOutcome, String> {
        if !self.projects.iter().any(|p| p.id == keep_id) {
            return Err(format!("unknown project id '{keep_id}'"));
        }
        let mut merged_ids: Vec<String> = Vec::new();
        for id in merge_ids {
            if id == keep_id || merged_ids.contains(id) {
                continue;
            }
            if !self.projects.iter().any(|p| &p.id == id) {
                return Err(format!("unknown project id '{id}'"));
            }
            merged_ids.push(id.clone());
        }
        if merged_ids.is_empty() {
            return Err("nothing to merge".to_string());
        }

        let merged: Vec<Project> = self
            .projects
            .iter()
            .filter(|p| merged_ids.contains(&p.id))
            .cloned()
            .collect();

        let mut moved_todos = Vec::new();
        for t in self.todos.iter_mut() {
            if t.project_id.as_ref().is_some_and(|pid| merged_ids.contains(pid)) {
                t.project_id = Some(keep_id.to_string());
                t.updated_at = now;
                moved_todos.push(t.id.clone());
            }
        }

        let mut grant_changed = false;
        let scope = &mut self.agent_time_grant.scope.project_ids;
        if scope.iter().any(|id| merged_ids.contains(id)) {
            let mut next: Vec<String> = Vec::new();
            for id in scope.iter() {
                let id = if merged_ids.contains(id) { keep_id.to_string() } else { id.clone() };
                if !next.contains(&id) {
                    next.push(id);
                }
            }
            *scope = next;
            grant_changed = true;
        }

        let mut aliases_added = Vec::new();
        {
            let keeper = self
                .projects
                .iter_mut()
                .find(|p| p.id == keep_id)
                .expect("checked above");
            for m in &merged {
                for name in std::iter::once(&m.title).chain(m.aliases.iter()) {
                    if project_identity::add_alias(&keeper.title, &mut keeper.aliases, name) {
                        aliases_added.push(name.trim().to_string());
                    }
                }
                if keeper.path.is_none() {
                    keeper.path = m.path.clone();
                }
                if keeper.area_id.is_none() {
                    keeper.area_id = m.area_id.clone();
                }
                if keeper.deadline.is_none() {
                    keeper.deadline = m.deadline;
                }
                if !m.notes.trim().is_empty() {
                    if !keeper.notes.trim().is_empty() {
                        keeper.notes.push_str("\n\n");
                    }
                    keeper.notes.push_str(&format!("From {}:\n{}", m.title, m.notes.trim()));
                }
                for d in &m.distinct_from {
                    if d != keep_id && !merged_ids.contains(d) && !keeper.distinct_from.contains(d) {
                        keeper.distinct_from.push(d.clone());
                    }
                }
            }
            keeper.distinct_from.retain(|d| !merged_ids.contains(d));
            keeper.updated_at = now;
        }

        // Others that said "not a duplicate of <merged>" now mean the keeper.
        let mut touched_projects = Vec::new();
        for p in self.projects.iter_mut() {
            if p.id == keep_id || merged_ids.contains(&p.id) {
                continue;
            }
            if p.distinct_from.iter().any(|d| merged_ids.contains(d)) {
                let mut next: Vec<String> = Vec::new();
                for d in &p.distinct_from {
                    let d = if merged_ids.contains(d) { keep_id.to_string() } else { d.clone() };
                    if !next.contains(&d) {
                        next.push(d);
                    }
                }
                p.distinct_from = next;
                p.updated_at = now;
                touched_projects.push(p.id.clone());
            }
        }

        self.projects.retain(|p| !merged_ids.contains(&p.id));

        Ok(MergeOutcome {
            keep_id: keep_id.to_string(),
            merged_ids,
            moved_todos,
            aliases_added,
            touched_projects,
            grant_changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{Todo, TodoStatus};

    fn project(id: &str, title: &str) -> Project {
        Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::Open,
            deadline: None,
            sort_order: 0.0,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn todo(id: &str, project: Option<&str>) -> Todo {
        let mut t = Todo::new(id, 0.0);
        t.id = id.into();
        t.project_id = project.map(str::to_string);
        t
    }

    #[test]
    fn merge_moves_everything_and_records_aliases() {
        let mut keep = project("k", "Hobbes");
        keep.aliases.push("cai-hobbes".into());
        let mut a = project("a", "cai-hobbes");
        a.path = Some("~/Sites/cai-hobbes".into());
        a.notes = "repo notes".into();
        let mut b = project("b", "Hobbes (cai-hobbes)");
        b.aliases.push("The Assistant".into());
        b.distinct_from.push("other".into());
        let mut other = project("other", "Taxes");
        other.distinct_from.push("b".into());
        let mut state = PlannerState {
            projects: vec![keep, a, b, other],
            todos: vec![todo("t1", Some("a")), todo("t2", Some("b")), todo("t3", Some("k")), todo("t4", None)],
            ..Default::default()
        };
        state.agent_time_grant.scope.project_ids = vec!["a".into(), "k".into(), "b".into()];

        let now = Utc::now();
        let out = state
            .merge_projects("k", &["a".into(), "b".into(), "k".into()], now)
            .unwrap();

        assert_eq!(out.merged_ids, vec!["a", "b"]);
        assert_eq!(out.moved_todos, vec!["t1", "t2"]);
        assert!(state.todos.iter().filter(|t| t.id != "t4").all(|t| t.project_id.as_deref() == Some("k")));
        assert_eq!(state.todos.iter().find(|t| t.id == "t4").unwrap().project_id, None);
        // "cai-hobbes" was already an alias; the others are new.
        assert_eq!(out.aliases_added, vec!["Hobbes (cai-hobbes)", "The Assistant"]);
        let k = state.projects.iter().find(|p| p.id == "k").unwrap();
        assert_eq!(k.aliases, vec!["cai-hobbes", "Hobbes (cai-hobbes)", "The Assistant"]);
        assert_eq!(k.path.as_deref(), Some("~/Sites/cai-hobbes"));
        assert!(k.notes.contains("From cai-hobbes:\nrepo notes"));
        assert_eq!(k.distinct_from, vec!["other"]);
        assert_eq!(state.projects.len(), 2);
        assert!(out.grant_changed);
        assert_eq!(state.agent_time_grant.scope.project_ids, vec!["k"]);
        assert_eq!(out.touched_projects, vec!["other"]);
        assert_eq!(state.projects.iter().find(|p| p.id == "other").unwrap().distinct_from, vec!["k"]);
    }

    #[test]
    fn merge_validates_ids() {
        let mut state = PlannerState {
            projects: vec![project("k", "Hobbes"), project("a", "cai-hobbes")],
            ..Default::default()
        };
        let now = Utc::now();
        assert!(state.merge_projects("nope", &["a".into()], now).is_err());
        assert!(state.merge_projects("k", &["ghost".into()], now).is_err());
        assert!(state.merge_projects("k", &["k".into()], now).is_err());
        assert_eq!(state.projects.len(), 2, "a refused merge changes nothing");
    }

    #[test]
    fn stats_count_open_todos_and_latest_activity() {
        let p = project("k", "Hobbes");
        let mut done = todo("d", Some("k"));
        done.status = TodoStatus::Completed;
        done.updated_at = p.updated_at + chrono::Duration::days(2);
        let state = PlannerState {
            projects: vec![p.clone()],
            todos: vec![todo("o", Some("k")), done.clone(), todo("x", None)],
            ..Default::default()
        };
        let s = project_stats(&state, &p);
        assert_eq!(s.open_todos, 1);
        assert_eq!(s.last_activity, done.updated_at);
    }
}
