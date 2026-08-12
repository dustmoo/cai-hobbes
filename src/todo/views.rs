//! Pure view logic: which todos belong in which list, in what order.
//!
//! Kept free of SQL and Dioxus so the rules that define "Today" are testable
//! directly. The store loads everything into memory (the list is bounded by
//! human effort), so filtering here rather than in SQL keeps one definition of
//! each view for both the UI and the AI's `HOBBES_TODO_LIST`.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::model::{TimeOfDay, Todo, TodoBucket};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoView {
    /// Captured but not yet triaged.
    #[default]
    Inbox,
    /// Scheduled for today or earlier — the day's actual workload.
    Today,
    /// Scheduled for a future day.
    Upcoming,
    /// Triaged, undated, do whenever.
    Anytime,
    /// Deliberately deferred.
    Someday,
    /// Finished and abandoned work, newest first.
    Logbook,
}

impl TodoView {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoView::Inbox => "inbox",
            TodoView::Today => "today",
            TodoView::Upcoming => "upcoming",
            TodoView::Anytime => "anytime",
            TodoView::Someday => "someday",
            TodoView::Logbook => "logbook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "inbox" => TodoView::Inbox,
            "today" => TodoView::Today,
            "upcoming" => TodoView::Upcoming,
            "anytime" => TodoView::Anytime,
            "someday" => TodoView::Someday,
            "logbook" => TodoView::Logbook,
            _ => return None,
        })
    }
}

/// Whether a single todo belongs in a view on a given day.
///
/// Today deliberately includes todos scheduled *before* today: work you planned
/// and didn't finish is still work you owe, and silently hiding it is how a
/// planner loses the user's trust.
pub fn matches_view(todo: &Todo, view: TodoView, today: NaiveDate) -> bool {
    let open = !todo.status.is_closed();
    let undated = todo.scheduled_for.is_none();

    match view {
        TodoView::Inbox => open && undated && todo.bucket == TodoBucket::Inbox,
        TodoView::Today => open && todo.scheduled_for.is_some_and(|d| d <= today),
        TodoView::Upcoming => open && todo.scheduled_for.is_some_and(|d| d > today),
        TodoView::Anytime => open && undated && todo.bucket == TodoBucket::Anytime,
        TodoView::Someday => open && undated && todo.bucket == TodoBucket::Someday,
        TodoView::Logbook => todo.status.is_closed(),
    }
}

/// The todos in a view, ordered for display.
pub fn in_view(todos: &[Todo], view: TodoView, today: NaiveDate) -> Vec<&Todo> {
    let mut out: Vec<&Todo> = todos
        .iter()
        .filter(|t| matches_view(t, view, today))
        .collect();

    if view == TodoView::Logbook {
        // Newest first — the logbook is a record, not a queue.
        out.sort_by(|a, b| {
            b.completed_at
                .unwrap_or(b.updated_at)
                .cmp(&a.completed_at.unwrap_or(a.updated_at))
        });
    } else {
        out.sort_by(|a, b| {
            a.sort_order
                .partial_cmp(&b.sort_order)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
    }
    out
}

/// Today split into the main list and Things' "This Evening" group.
pub struct TodaySections<'a> {
    pub daytime: Vec<&'a Todo>,
    pub evening: Vec<&'a Todo>,
}

pub fn today_sections(todos: &[Todo], today: NaiveDate) -> TodaySections<'_> {
    let all = in_view(todos, TodoView::Today, today);
    let (evening, daytime) = all
        .into_iter()
        .partition(|t| t.time_of_day == Some(TimeOfDay::Evening));
    TodaySections { daytime, evening }
}

/// Open todos past their deadline, soonest-missed first.
pub fn overdue(todos: &[Todo], today: NaiveDate) -> Vec<&Todo> {
    let mut out: Vec<&Todo> = todos.iter().filter(|t| t.is_overdue(today)).collect();
    out.sort_by_key(|t| t.deadline);
    out
}

/// Pull unfinished work forward from previous days onto `today`.
///
/// Returns the number of todos moved. This is the Sunsama rollover: yesterday's
/// leftovers reappear on today's plate rather than quietly rotting in the past.
/// Deadlines are untouched — only the intended workday moves.
pub fn rollover_unfinished(todos: &mut [Todo], today: NaiveDate, now: DateTime<Utc>) -> usize {
    let mut moved = 0;
    for todo in todos.iter_mut() {
        if !todo.status.is_closed() && todo.scheduled_for.is_some_and(|d| d < today) {
            todo.scheduled_for = Some(today);
            todo.updated_at = now;
            moved += 1;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{TodoBucket, TodoStatus};

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn todo(title: &str, bucket: TodoBucket, scheduled: Option<&str>) -> Todo {
        let mut t = Todo::new(title, 0.0);
        t.bucket = bucket;
        t.scheduled_for = scheduled.map(date);
        t
    }

    fn titles(todos: Vec<&Todo>) -> Vec<String> {
        todos.into_iter().map(|t| t.title.clone()).collect()
    }

    fn fixture() -> Vec<Todo> {
        let mut done = todo("finished", TodoBucket::Anytime, Some("2026-08-12"));
        done.mark_completed(Utc::now());

        vec![
            todo("untriaged", TodoBucket::Inbox, None),
            todo("today", TodoBucket::Anytime, Some("2026-08-12")),
            todo("yesterday", TodoBucket::Anytime, Some("2026-08-11")),
            todo("tomorrow", TodoBucket::Anytime, Some("2026-08-13")),
            todo("whenever", TodoBucket::Anytime, None),
            todo("maybe", TodoBucket::Someday, None),
            done,
        ]
    }

    #[test]
    fn today_includes_unfinished_work_from_earlier_days() {
        let todos = fixture();
        let t = titles(in_view(&todos, TodoView::Today, date("2026-08-12")));
        assert!(t.contains(&"today".to_string()));
        assert!(
            t.contains(&"yesterday".to_string()),
            "work scheduled before today is still owed and must not be hidden"
        );
        assert!(!t.contains(&"tomorrow".to_string()));
        assert!(!t.contains(&"finished".to_string()));
    }

    #[test]
    fn views_are_mutually_exclusive_for_open_todos() {
        let todos = fixture();
        let today = date("2026-08-12");
        let views = [
            TodoView::Inbox,
            TodoView::Today,
            TodoView::Upcoming,
            TodoView::Anytime,
            TodoView::Someday,
        ];

        for todo in todos.iter().filter(|t| !t.status.is_closed()) {
            let hits = views.iter().filter(|v| matches_view(todo, **v, today)).count();
            assert_eq!(hits, 1, "'{}' should appear in exactly one list", todo.title);
        }
    }

    #[test]
    fn scheduling_a_todo_removes_it_from_the_undated_lists() {
        let today = date("2026-08-12");
        let mut t = todo("whenever", TodoBucket::Anytime, None);
        assert!(matches_view(&t, TodoView::Anytime, today));

        t.scheduled_for = Some(today);
        assert!(!matches_view(&t, TodoView::Anytime, today));
        assert!(matches_view(&t, TodoView::Today, today));
    }

    #[test]
    fn logbook_holds_both_completed_and_cancelled_newest_first() {
        let mut older = todo("older", TodoBucket::Anytime, None);
        older.mark_completed("2026-08-10T09:00:00Z".parse().unwrap());
        let mut newer = todo("newer", TodoBucket::Anytime, None);
        newer.mark_completed("2026-08-12T09:00:00Z".parse().unwrap());
        let mut dropped = todo("dropped", TodoBucket::Anytime, None);
        dropped.status = TodoStatus::Cancelled;
        dropped.completed_at = Some("2026-08-11T09:00:00Z".parse().unwrap());

        let todos = vec![older, newer, dropped];
        assert_eq!(
            titles(in_view(&todos, TodoView::Logbook, date("2026-08-12"))),
            vec!["newer", "dropped", "older"]
        );
    }

    #[test]
    fn today_splits_out_the_evening_group() {
        let today = date("2026-08-12");
        let mut evening = todo("read the spec", TodoBucket::Anytime, Some("2026-08-12"));
        evening.time_of_day = Some(TimeOfDay::Evening);
        let todos = vec![todo("work", TodoBucket::Anytime, Some("2026-08-12")), evening];

        let sections = today_sections(&todos, today);
        assert_eq!(titles(sections.daytime), vec!["work"]);
        assert_eq!(titles(sections.evening), vec!["read the spec"]);
    }

    #[test]
    fn rollover_moves_only_unfinished_past_work() {
        let today = date("2026-08-12");
        let now = Utc::now();
        let mut todos = fixture();

        let moved = rollover_unfinished(&mut todos, today, now);
        assert_eq!(moved, 1, "only 'yesterday' should move");

        let by_title = |name: &str| {
            todos
                .iter()
                .find(|t| t.title == name)
                .unwrap()
                .scheduled_for
        };
        assert_eq!(by_title("yesterday"), Some(today));
        assert_eq!(by_title("tomorrow"), Some(date("2026-08-13")));
        assert_eq!(by_title("whenever"), None);
        // A completed todo keeps its original date — the logbook is history.
        assert_eq!(by_title("finished"), Some(date("2026-08-12")));

        // Running it twice is a no-op.
        assert_eq!(rollover_unfinished(&mut todos, today, now), 0);
    }

    #[test]
    fn rollover_leaves_deadlines_alone() {
        let today = date("2026-08-12");
        let mut t = todo("ship it", TodoBucket::Anytime, Some("2026-08-10"));
        t.deadline = Some(date("2026-08-11"));
        let mut todos = vec![t];

        rollover_unfinished(&mut todos, today, Utc::now());
        assert_eq!(todos[0].scheduled_for, Some(today));
        assert_eq!(
            todos[0].deadline,
            Some(date("2026-08-11")),
            "rolling the plan forward must not silently extend the deadline"
        );
        assert!(todos[0].is_overdue(today));
    }

    #[test]
    fn overdue_lists_worst_first() {
        let today = date("2026-08-12");
        let mut a = todo("a", TodoBucket::Anytime, None);
        a.deadline = Some(date("2026-08-09"));
        let mut b = todo("b", TodoBucket::Anytime, None);
        b.deadline = Some(date("2026-08-11"));
        let mut fine = todo("fine", TodoBucket::Anytime, None);
        fine.deadline = Some(date("2026-08-20"));

        let todos = vec![b, fine, a];
        assert_eq!(titles(overdue(&todos, today)), vec!["a", "b"]);
    }

    #[test]
    fn view_names_round_trip() {
        for view in [
            TodoView::Inbox,
            TodoView::Today,
            TodoView::Upcoming,
            TodoView::Anytime,
            TodoView::Someday,
            TodoView::Logbook,
        ] {
            assert_eq!(TodoView::parse(view.as_str()), Some(view));
        }
        assert_eq!(TodoView::parse("  TODAY "), Some(TodoView::Today));
        assert_eq!(TodoView::parse("nonsense"), None);
    }
}
