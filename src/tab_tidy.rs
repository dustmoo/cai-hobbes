//! "Tidy tabs": a pure pass over the open chat tabs that suggests what to
//! close or file, for the user to review. No Dioxus, no LLM, no I/O.
//!
//! Three lists, each tab in at most one of them:
//!  1. **Finished** — a to-do linked to the chat (`Todo::linked_fleet_session`
//!     holds the Hobbes chat session id for dispatched chats) is done, and no
//!     linked to-do is still open.
//!  2. **Idle** — untouched for `IDLE_DAYS`+ with no open linked to-do.
//!  3. **Unsorted** — no (live) planner project, with a suggested one from a
//!     cheap word match, or `None` for "keep unsorted".
//!
//! Finished and idle are *close* suggestions and never include the active
//! tab, a streaming tab, a tab with a pending timer, or a pinned tab (there
//! is no pin concept today; `pinned` is carried so it can start to count the
//! day one exists). Closing is never deleting: the session stays in History.

use chrono::{DateTime, Duration, Utc};

/// How long a tab must sit untouched to count as idle.
pub const IDLE_DAYS: i64 = 7;

/// What the pass needs to know about one open chat tab.
#[derive(Debug, Clone, PartialEq)]
pub struct TidyTab {
    pub session_id: String,
    /// `Session::display_title`.
    pub title: String,
    /// `Session::tab_topic` — the AI topic or the user's rename.
    pub topic: Option<String>,
    /// Display only (`Session::tab_emoji`); the pass never reads it.
    pub emoji: Option<String>,
    pub last_updated: DateTime<Utc>,
    pub project_id: Option<String>,
    pub is_active: bool,
    pub is_streaming: bool,
    pub has_pending_timer: bool,
    pub pinned: bool,
}

/// The slice of a planner to-do that matters here.
#[derive(Debug, Clone, PartialEq)]
pub struct TidyTodo {
    /// `Todo::linked_fleet_session`.
    pub linked_session: Option<String>,
    pub status: TidyTodoStatus,
    /// When it was completed (`completed_at`, else `updated_at`).
    pub done_at: DateTime<Utc>,
    pub project_id: Option<String>,
}

/// `TodoStatus` reduced to what tidying distinguishes. A cancelled to-do is
/// neither done (the work wasn't finished) nor open (it won't block idling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TidyTodoStatus {
    Open,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TidyProject {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FinishedTab {
    pub session_id: String,
    /// When the linked to-do was completed (latest, if several).
    pub done_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdleTab {
    pub session_id: String,
    pub idle_since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnsortedTab {
    pub session_id: String,
    /// Suggested planner project id; `None` = keep unsorted.
    pub suggested_project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TidySuggestions {
    pub finished: Vec<FinishedTab>,
    pub idle: Vec<IdleTab>,
    pub unsorted: Vec<UnsortedTab>,
}

#[cfg(test)]
impl TidySuggestions {
    pub fn is_empty(&self) -> bool {
        self.finished.is_empty() && self.idle.is_empty() && self.unsorted.is_empty()
    }
}

/// Never close what the user is looking at, what is working, what will fire,
/// or what they pinned.
fn protected(t: &TidyTab) -> bool {
    t.is_active || t.is_streaming || t.has_pending_timer || t.pinned
}

/// The suggestion pass. Output order follows `tabs` (open order).
pub fn suggest(
    tabs: &[TidyTab],
    todos: &[TidyTodo],
    projects: &[TidyProject],
    now: DateTime<Utc>,
) -> TidySuggestions {
    let mut out = TidySuggestions::default();
    let idle_cutoff = now - Duration::days(IDLE_DAYS);

    for t in tabs {
        let linked: Vec<&TidyTodo> = todos
            .iter()
            .filter(|td| td.linked_session.as_deref() == Some(t.session_id.as_str()))
            .collect();
        let has_open = linked.iter().any(|td| td.status == TidyTodoStatus::Open);
        let last_done = linked
            .iter()
            .filter(|td| td.status == TidyTodoStatus::Done)
            .map(|td| td.done_at)
            .max();

        let mut closing = false;
        if !protected(t) {
            if let (Some(done_at), false) = (last_done, has_open) {
                out.finished.push(FinishedTab {
                    session_id: t.session_id.clone(),
                    done_at,
                });
                closing = true;
            } else if !has_open && t.last_updated <= idle_cutoff {
                out.idle.push(IdleTab {
                    session_id: t.session_id.clone(),
                    idle_since: t.last_updated,
                });
                closing = true;
            }
        }
        if closing {
            continue;
        }

        let has_live_project = t
            .project_id
            .as_ref()
            .is_some_and(|pid| projects.iter().any(|p| &p.id == pid));
        if !has_live_project && !projects.is_empty() {
            out.unsorted.push(UnsortedTab {
                session_id: t.session_id.clone(),
                suggested_project: suggest_project(t, &linked, projects),
            });
        }
    }
    out
}

/// A linked to-do's project wins; else the project whose name shares the most
/// words with the tab's title/topic. Ties and no overlap give `None`.
fn suggest_project(
    tab: &TidyTab,
    linked: &[&TidyTodo],
    projects: &[TidyProject],
) -> Option<String> {
    // Most recently completed/updated linked to-do with a live project.
    let mut by_todo: Vec<&&TidyTodo> = linked.iter().collect();
    by_todo.sort_by_key(|td| std::cmp::Reverse(td.done_at));
    for td in by_todo {
        if let Some(pid) = &td.project_id {
            if projects.iter().any(|p| &p.id == pid) {
                return Some(pid.clone());
            }
        }
    }

    let mut text = tab.title.clone();
    if let Some(topic) = &tab.topic {
        text.push(' ');
        text.push_str(topic);
    }
    let words = keywords(&text);
    if words.is_empty() {
        return None;
    }

    let mut best: Option<(&TidyProject, usize)> = None;
    let mut tied = false;
    for p in projects {
        let score = keywords(&p.title)
            .iter()
            .filter(|pw| words.contains(pw))
            .count();
        if score == 0 {
            continue;
        }
        match best {
            Some((_, b)) if score < b => {}
            Some((_, b)) if score == b => tied = true,
            _ => {
                best = Some((p, score));
                tied = false;
            }
        }
    }
    if tied {
        return None;
    }
    best.map(|(p, _)| p.id.clone())
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "about", "your", "our", "new", "chat", "tab",
    "project", "notes", "review", "plan", "work", "setup", "check", "test",
];

/// Lowercased alphabetic words of 3+ letters, minus filler, with a trailing
/// plural "s" folded ("benchmarks" matches "benchmark"). Deduplicated.
fn keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.chars().count() < 3 || raw.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut w = raw.to_lowercase();
        if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
            w.pop();
        }
        if STOPWORDS.contains(&w.as_str()) || out.contains(&w) {
            continue;
        }
        out.push(w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
    }

    fn days_ago(d: i64) -> DateTime<Utc> {
        now() - Duration::days(d)
    }

    fn tab(id: &str, title: &str, age_days: i64) -> TidyTab {
        TidyTab {
            session_id: id.into(),
            title: title.into(),
            topic: None,
            emoji: None,
            last_updated: days_ago(age_days),
            project_id: None,
            is_active: false,
            is_streaming: false,
            has_pending_timer: false,
            pinned: false,
        }
    }

    fn todo(session: &str, status: TidyTodoStatus, done_days_ago: i64) -> TidyTodo {
        TidyTodo {
            linked_session: Some(session.into()),
            status,
            done_at: days_ago(done_days_ago),
            project_id: None,
        }
    }

    fn proj(id: &str, title: &str) -> TidyProject {
        TidyProject { id: id.into(), title: title.into() }
    }

    fn ids<T>(v: &[T], f: impl Fn(&T) -> &str) -> Vec<String> {
        v.iter().map(|x| f(x).to_string()).collect()
    }

    #[test]
    fn finished_when_linked_todo_is_done() {
        let tabs = [tab("a", "Build Status", 1), tab("b", "Other", 1)];
        let todos = [todo("a", TidyTodoStatus::Done, 3)];
        let s = suggest(&tabs, &todos, &[], now());
        assert_eq!(s.finished.len(), 1);
        assert_eq!(s.finished[0].session_id, "a");
        assert_eq!(s.finished[0].done_at, days_ago(3));
        assert!(s.idle.is_empty());
    }

    #[test]
    fn finished_uses_latest_done_date() {
        let tabs = [tab("a", "x", 1)];
        let todos = [todo("a", TidyTodoStatus::Done, 5), todo("a", TidyTodoStatus::Done, 2)];
        let s = suggest(&tabs, &todos, &[], now());
        assert_eq!(s.finished[0].done_at, days_ago(2));
    }

    #[test]
    fn a_still_open_linked_todo_blocks_finished_and_idle() {
        let tabs = [tab("a", "x", 30)];
        let todos = [todo("a", TidyTodoStatus::Done, 3), todo("a", TidyTodoStatus::Open, 0)];
        let s = suggest(&tabs, &todos, &[], now());
        assert!(s.finished.is_empty());
        assert!(s.idle.is_empty());
    }

    #[test]
    fn cancelled_todo_is_not_finished_and_does_not_block_idle() {
        let tabs = [tab("a", "x", 30)];
        let todos = [todo("a", TidyTodoStatus::Cancelled, 20)];
        let s = suggest(&tabs, &todos, &[], now());
        assert!(s.finished.is_empty());
        assert_eq!(ids(&s.idle, |i| &i.session_id), vec!["a"]);
    }

    #[test]
    fn todo_linked_to_another_session_is_ignored() {
        let tabs = [tab("a", "x", 1)];
        let todos = [todo("zzz", TidyTodoStatus::Done, 1)];
        assert!(suggest(&tabs, &todos, &[], now()).finished.is_empty());
        let unlinked = TidyTodo { linked_session: None, ..todo("a", TidyTodoStatus::Done, 1) };
        assert!(suggest(&tabs, &[unlinked], &[], now()).finished.is_empty());
    }

    #[test]
    fn idle_threshold_is_seven_days_inclusive() {
        let tabs = [tab("six", "x", 6), tab("seven", "x", 7), tab("eight", "x", 8)];
        let s = suggest(&tabs, &[], &[], now());
        assert_eq!(ids(&s.idle, |i| &i.session_id), vec!["seven", "eight"]);
        assert_eq!(s.idle[0].idle_since, days_ago(7));
    }

    #[test]
    fn done_and_idle_tab_is_listed_only_as_finished() {
        let tabs = [tab("a", "x", 40)];
        let todos = [todo("a", TidyTodoStatus::Done, 35)];
        let s = suggest(&tabs, &todos, &[], now());
        assert_eq!(s.finished.len(), 1);
        assert!(s.idle.is_empty());
    }

    #[test]
    fn protected_tabs_are_never_suggested_for_closing() {
        let mut active = tab("active", "x", 30);
        active.is_active = true;
        let mut streaming = tab("streaming", "x", 30);
        streaming.is_streaming = true;
        let mut timer = tab("timer", "x", 30);
        timer.has_pending_timer = true;
        let mut pinned = tab("pinned", "x", 30);
        pinned.pinned = true;
        let tabs = [active, streaming, timer, pinned];
        let todos = [
            todo("active", TidyTodoStatus::Done, 1),
            todo("streaming", TidyTodoStatus::Done, 1),
            todo("timer", TidyTodoStatus::Done, 1),
            todo("pinned", TidyTodoStatus::Done, 1),
        ];
        let s = suggest(&tabs, &todos, &[], now());
        assert!(s.finished.is_empty());
        assert!(s.idle.is_empty());
    }

    #[test]
    fn protected_untagged_tabs_can_still_be_filed() {
        let mut active = tab("a", "Meso drift ratio", 30);
        active.is_active = true;
        let projects = [proj("p1", "Meso drift measurement")];
        let s = suggest(&[active], &[], &projects, now());
        assert_eq!(s.unsorted.len(), 1);
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("p1"));
    }

    #[test]
    fn tabs_being_closed_are_not_also_offered_for_filing() {
        let tabs = [tab("a", "Meso drift", 30)];
        let projects = [proj("p1", "Meso drift")];
        let s = suggest(&tabs, &[], &projects, now());
        assert_eq!(s.idle.len(), 1);
        assert!(s.unsorted.is_empty());
    }

    #[test]
    fn unsorted_matches_title_words_to_project_names() {
        let projects = [
            proj("fam", "Family & Infrastructure"),
            proj("meso", "Meso — drift measurement"),
        ];
        let tabs = [
            tab("a", "Meso Drift Ratio", 1),
            tab("b", "News Search", 1),
            tab("c", "Shopping for family fitness", 1),
        ];
        let s = suggest(&tabs, &[], &projects, now());
        let got: Vec<_> = s
            .unsorted
            .iter()
            .map(|u| (u.session_id.as_str(), u.suggested_project.as_deref()))
            .collect();
        assert_eq!(got, vec![("a", Some("meso")), ("b", None), ("c", Some("fam"))]);
    }

    #[test]
    fn topic_counts_as_well_as_title() {
        let mut t = tab("a", "Oct 05 - 02:30 PM", 1);
        t.topic = Some("Puget order benchmark".into());
        let projects = [proj("pg", "Puget Systems"), proj("cm", "Clearmirror")];
        let s = suggest(&[t], &[], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("pg"));
    }

    #[test]
    fn plural_and_case_fold() {
        let projects = [proj("b", "Benchmark")];
        let s = suggest(&[tab("a", "BENCHMARKS again", 1)], &[], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("b"));
    }

    #[test]
    fn higher_overlap_wins_and_ties_give_none() {
        let projects = [proj("one", "Puget"), proj("two", "Puget Systems Agreement")];
        let s = suggest(&[tab("a", "Puget agreement draft", 1)], &[], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("two"));

        let tie = [proj("x", "Alpha"), proj("y", "Alpha")];
        let s = suggest(&[tab("a", "Alpha thing", 1)], &[], &tie, now());
        assert_eq!(s.unsorted[0].suggested_project, None);
    }

    #[test]
    fn stopwords_and_digits_do_not_match() {
        let projects = [proj("p", "Project Review 2026")];
        let s = suggest(&[tab("a", "Project review 2026 notes", 1)], &[], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project, None);
    }

    #[test]
    fn linked_todo_project_beats_word_match() {
        let projects = [proj("word", "Meso drift"), proj("todo", "Family")];
        let mut td = todo("a", TidyTodoStatus::Open, 0);
        td.project_id = Some("todo".into());
        let s = suggest(&[tab("a", "Meso drift", 1)], &[td], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("todo"));
    }

    #[test]
    fn linked_todo_with_dead_project_falls_back_to_words() {
        let projects = [proj("word", "Meso drift")];
        let mut td = todo("a", TidyTodoStatus::Open, 0);
        td.project_id = Some("gone".into());
        let s = suggest(&[tab("a", "Meso drift", 1)], &[td], &projects, now());
        assert_eq!(s.unsorted[0].suggested_project.as_deref(), Some("word"));
    }

    #[test]
    fn tagged_tabs_are_sorted_but_dangling_tags_are_not() {
        let projects = [proj("p1", "Alpha")];
        let mut ok = tab("ok", "x", 1);
        ok.project_id = Some("p1".into());
        let mut dangling = tab("dangling", "x", 1);
        dangling.project_id = Some("deleted".into());
        let s = suggest(&[ok, dangling], &[], &projects, now());
        assert_eq!(ids(&s.unsorted, |u| &u.session_id), vec!["dangling"]);
    }

    #[test]
    fn no_projects_means_no_unsorted_list() {
        let s = suggest(&[tab("a", "x", 1)], &[], &[], now());
        assert!(s.unsorted.is_empty());
        assert!(s.is_empty());
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(suggest(&[], &[], &[], now()).is_empty());
    }

    #[test]
    fn output_follows_open_order() {
        let tabs = [tab("z", "x", 20), tab("a", "x", 30), tab("m", "x", 3)];
        let s = suggest(&tabs, &[], &[], now());
        assert_eq!(ids(&s.idle, |i| &i.session_id), vec!["z", "a"]);
    }
}
