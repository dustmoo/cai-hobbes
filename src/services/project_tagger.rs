//! Auto-tagging chat sessions to planner projects, and mapping fleet
//! terminal sessions to projects by cwd.
//!
//! Policy (user decisions, 2026-09-02): auto-apply with one-click correction,
//! and match EXISTING projects only — the tagger never creates a project and
//! never guesses on ambiguity. A session the user has manually tagged or
//! cleared (`project_tag_user_set`) is never touched.

use crate::session::ConversationSummary;
use crate::todo::model::Project;

/// Candidates shorter than this never match by containment — "AI" must not
/// tag everything to an "AI Strategy" project.
const MIN_CONTAINMENT_LEN: usize = 4;

/// Does `candidate` name a project with these normalized names (title and
/// aliases — `project_identity::project_keys`)? Equality, or containment
/// either way when both sides are long enough.
fn names_match(keys: &[String], candidate: &str) -> bool {
    let c = crate::todo::project_identity::normalize(candidate);
    if c.is_empty() {
        return false;
    }
    keys.iter().any(|t| {
        t == &c
            || (t.len() >= MIN_CONTAINMENT_LEN && c.len() >= MIN_CONTAINMENT_LEN
                && (t.contains(&c) || c.contains(t.as_str())))
    })
}

/// The project this session's content points at — `Some(id)` only when
/// exactly one open project matches any candidate (ambiguity → None).
/// Candidates: the summary's `project_name` entity, its `key_topics`, and
/// the session name.
pub fn suggest_project(
    projects: &[Project],
    session_name: &str,
    summary: &ConversationSummary,
) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = summary
        .entities
        .other_entities
        .get("project_name")
        .and_then(|v| v.as_str())
    {
        candidates.push(p.to_string());
    }
    if let Some(topics) = summary
        .entities
        .other_entities
        .get("key_topics")
        .and_then(|v| v.as_array())
    {
        candidates.extend(topics.iter().filter_map(|t| t.as_str()).map(str::to_string));
    }
    candidates.push(session_name.to_string());

    let mut matched: Option<&Project> = None;
    for project in projects
        .iter()
        .filter(|p| p.status == crate::todo::model::TodoStatus::Open
            || p.status == crate::todo::model::TodoStatus::InProgress)
    {
        let keys = crate::todo::project_identity::project_keys(project);
        if candidates.iter().any(|c| names_match(&keys, c)) {
            match matched {
                None => matched = Some(project),
                // Two different projects match → ambiguous, tag nothing.
                Some(prev) if prev.id != project.id => return None,
                Some(_) => {}
            }
        }
    }
    matched.map(|p| p.id.clone())
}

/// Normalize a project path for prefix matching: `~` expanded, trailing
/// slashes trimmed.
pub(crate) fn norm_path(p: &str) -> Option<String> {
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir()?.join(rest).to_string_lossy().into_owned()
    } else if p == "~" {
        dirs::home_dir()?.to_string_lossy().into_owned()
    } else {
        p.to_string()
    };
    Some(expanded.trim_end_matches('/').to_string())
}

/// The project whose `path` is the longest prefix of `cwd` (component-aligned
/// — `/a/bc` is not under `/a/b`). With no path match, the deepest folder
/// in `cwd` that exactly names one open project (title or alias, normalized,
/// at least 4 characters) — so a repo folder maps to the project named
/// after it, or to one carrying the repo name as an alias.
pub fn project_for_cwd(projects: &[Project], cwd: &str) -> Option<String> {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return None;
    }
    let by_path = projects
        .iter()
        .filter_map(|p| {
            let root = norm_path(p.path.as_deref()?)?;
            let under = cwd == root
                || (cwd.starts_with(&root) && cwd.as_bytes().get(root.len()) == Some(&b'/'));
            under.then_some((root.len(), p.id.clone()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, id)| id);
    if by_path.is_some() {
        return by_path;
    }
    let open: Vec<Project> = projects.iter().filter(|p| !p.status.is_closed()).cloned().collect();
    for folder in cwd.rsplit('/') {
        let key = crate::todo::project_identity::normalize(folder);
        if key.chars().filter(|c| !c.is_whitespace()).count() < MIN_CONTAINMENT_LEN {
            continue;
        }
        if let crate::todo::project_identity::ExactLookup::One(id) =
            crate::todo::project_identity::lookup_exact(&open, folder)
        {
            return Some(id);
        }
    }
    None
}

/// Auto-tag after a summary write (both summarizer paths call this).
/// Respects the policy guards: never overrides an existing tag or a manual
/// decision, and only fires on a unique match. Dioxus-side only (touches
/// Signals).
pub fn maybe_auto_tag(
    session_id: &str,
    mut session_state: dioxus::prelude::Signal<crate::session::SessionState>,
    planner: dioxus::prelude::Signal<crate::todo::PlannerState>,
) {
    use dioxus::prelude::{Readable, Writable};
    let (name, summary) = {
        let state = session_state.peek();
        match state.sessions.get(session_id) {
            Some(s) if s.project_id.is_none() && !s.project_tag_user_set => (
                s.name.clone(),
                s.active_context.conversation_summary.clone(),
            ),
            _ => return,
        }
    };
    let suggestion = suggest_project(&planner.peek().projects, &name, &summary);
    let Some(project_id) = suggestion else { return };
    {
        let mut state = session_state.write();
        match state.sessions.get_mut(session_id) {
            // Re-check under the write guard (a manual tag may have raced in).
            Some(s) if s.project_id.is_none() && !s.project_tag_user_set => {
                s.project_id = Some(project_id.clone());
            }
            _ => return,
        }
    }
    tracing::info!(session_id, project_id, "auto-tagged session to project");
    crate::session_events::log_event(
        session_id,
        crate::session_events::SessionEvent::ProjectTagged {
            project_id: Some(project_id),
            user_set: false,
        },
    );
    crate::session::SessionState::save_signal(&session_state, None);
}

/// How many chat sessions are tagged to any of `project_ids`: hydrated ones
/// from `SessionState` (authoritative over their rows), the rest through a
/// `session_store` query — never by hydrating.
pub fn count_sessions_tagged(state: &crate::session::SessionState, project_ids: &[String]) -> usize {
    let tagged = |s: &crate::session::Session| {
        s.project_id.as_ref().is_some_and(|p| project_ids.contains(p))
    };
    let hydrated = state.sessions.values().filter(|s| tagged(s)).count();
    let stored = crate::session_store::session_ids_tagged(project_ids)
        .into_iter()
        .filter(|id| !state.sessions.contains_key(id))
        .count();
    hydrated + stored
}

/// Retag every chat session tagged to one of `from_ids` (merged projects)
/// to `to_id`: hydrated sessions in `SessionState` (saved through P-009),
/// the rest in place in `session_store`. Each retag is journaled as a
/// `ProjectTagged` event carrying the session's existing `user_set`, so the
/// journal projection agrees with the row. Returns how many were retagged.
pub fn retag_merged_sessions(
    mut session_state: dioxus::prelude::Signal<crate::session::SessionState>,
    from_ids: &[String],
    to_id: &str,
) -> usize {
    use dioxus::prelude::{Readable, Writable};
    let hydrated: std::collections::HashSet<String> =
        session_state.peek().sessions.keys().cloned().collect();
    let mut retagged: Vec<(String, bool)> = Vec::new();
    {
        let mut state = session_state.write();
        for (id, s) in state.sessions.iter_mut() {
            if s.project_id.as_ref().is_some_and(|p| from_ids.contains(p)) {
                s.project_id = Some(to_id.to_string());
                retagged.push((id.clone(), s.project_tag_user_set));
            }
        }
    }
    let any_hydrated = !retagged.is_empty();
    retagged.extend(crate::session_store::retag_sessions(from_ids, to_id, &hydrated));
    for (id, user_set) in &retagged {
        crate::session_events::log_event(
            id,
            crate::session_events::SessionEvent::ProjectTagged {
                project_id: Some(to_id.to_string()),
                user_set: *user_set,
            },
        );
    }
    if any_hydrated {
        crate::session::SessionState::save_signal(&session_state, None);
    }
    if !retagged.is_empty() {
        tracing::info!(count = retagged.len(), to_id, "retagged sessions after a project merge");
    }
    retagged.len()
}

/// Display title for a project id.
pub fn project_title<'a>(projects: &'a [Project], id: &str) -> Option<&'a str> {
    projects
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.title.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ConversationSummary;
    use crate::todo::model::TodoStatus;
    use chrono::Utc;

    fn project(id: &str, title: &str, path: Option<&str>) -> Project {
        Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::Open,
            deadline: None,
            sort_order: 0.0,
            path: path.map(str::to_string),
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn summary_with(project_name: Option<&str>, topics: &[&str]) -> ConversationSummary {
        let mut s = ConversationSummary::default();
        if let Some(p) = project_name {
            s.entities
                .other_entities
                .insert("project_name".into(), serde_json::json!(p));
        }
        if !topics.is_empty() {
            s.entities
                .other_entities
                .insert("key_topics".into(), serde_json::json!(topics));
        }
        s
    }

    #[test]
    fn unique_match_wins_ambiguity_tags_nothing() {
        let projects = vec![project("p1", "Puget", None), project("p2", "Hobbes", None)];
        let s = summary_with(Some("puget"), &[]);
        assert_eq!(suggest_project(&projects, "Sep 02", &s), Some("p1".into()));

        // Two projects named in topics → ambiguous.
        let s = summary_with(None, &["puget benchmarks", "hobbes fleet"]);
        assert_eq!(suggest_project(&projects, "Sep 02", &s), None);

        // Nothing matches → None (never invents).
        let s = summary_with(Some("unrelated"), &[]);
        assert_eq!(suggest_project(&projects, "Sep 02", &s), None);
    }

    #[test]
    fn short_candidates_do_not_containment_match() {
        let projects = vec![project("p1", "AI Strategy", None)];
        // "AI" (len 2) must not tag via containment…
        let s = summary_with(Some("AI"), &[]);
        assert_eq!(suggest_project(&projects, "chat", &s), None);
        // …but a real mention does.
        let s = summary_with(None, &["ai strategy sync"]);
        assert_eq!(suggest_project(&projects, "chat", &s), Some("p1".into()));
    }

    #[test]
    fn session_name_is_a_candidate_and_closed_projects_are_skipped() {
        let mut done = project("p1", "Puget", None);
        done.status = TodoStatus::Completed;
        let projects = vec![done, project("p2", "Exceller", None)];
        let s = ConversationSummary::default();
        assert_eq!(
            suggest_project(&projects, "Exceller onboarding", &s),
            Some("p2".into())
        );
        assert_eq!(suggest_project(&projects, "Puget notes", &s), None);
    }

    #[test]
    fn cwd_maps_by_longest_component_aligned_prefix() {
        let projects = vec![
            project("root", "Sites", Some("/Users/x/Sites")),
            project("puget", "Puget", Some("/Users/x/Sites/puget-bench/")),
        ];
        assert_eq!(
            project_for_cwd(&projects, "/Users/x/Sites/puget-bench/sub"),
            Some("puget".into())
        );
        assert_eq!(
            project_for_cwd(&projects, "/Users/x/Sites/other"),
            Some("root".into())
        );
        // Component alignment: /Sites-x is not under /Sites.
        assert_eq!(project_for_cwd(&projects, "/Users/x/Sites-x"), None);
        assert_eq!(project_for_cwd(&[], "/anywhere"), None);
    }

    #[test]
    fn cwd_falls_back_to_a_folder_naming_a_project_or_alias() {
        let mut hobbes = project("h", "Hobbes", None);
        hobbes.aliases.push("cai-hobbes".into());
        let projects = vec![hobbes, project("x", "Exceller8", None)];
        assert_eq!(
            project_for_cwd(&projects, "/Users/x/Sites/cai-hobbes/src"),
            Some("h".into())
        );
        assert_eq!(project_for_cwd(&projects, "/Users/x/Sites/exceller8"), Some("x".into()));
        assert_eq!(project_for_cwd(&projects, "/Users/x/Sites/unrelated"), None);
        // A path match still wins.
        let with_path = vec![project("p", "Puget", Some("/Users/x/Sites/cai-hobbes"))];
        assert_eq!(project_for_cwd(&with_path, "/Users/x/Sites/cai-hobbes"), Some("p".into()));
    }

    #[test]
    fn aliases_tag_like_titles() {
        let mut p = project("h", "Hobbes", None);
        p.aliases.push("cai-hobbes".into());
        let projects = vec![p, project("x", "Exceller8", None)];
        let s = summary_with(Some("cai_hobbes"), &[]);
        assert_eq!(suggest_project(&projects, "Oct 09", &s), Some("h".into()));
        // Parenthetical title: its inner name tags too.
        let projects = vec![project("hp", "Hobbes (cai-hobbes)", None)];
        let s = summary_with(None, &["cai-hobbes release"]);
        assert_eq!(suggest_project(&projects, "Oct 09", &s), Some("hp".into()));
    }
}
