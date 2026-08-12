//! The planner domain model.
//!
//! Pure data plus pure logic — no persistence, no Dioxus. Everything here is
//! unit-testable without a database or a UI.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

// ── Enums ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Open,
    Completed,
    /// Explicitly abandoned. Distinct from completed so the logbook can tell
    /// "I did this" from "I decided not to".
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Open => "open",
            TodoStatus::Completed => "completed",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    /// Whether the todo is finished, either way. Both leave the active lists.
    pub fn is_closed(self) -> bool {
        matches!(self, TodoStatus::Completed | TodoStatus::Cancelled)
    }
}

/// Which Things-style list an *unscheduled* todo belongs to.
///
/// Orthogonal to `scheduled_for`: a todo with a date shows up in Today or
/// Upcoming regardless of bucket, and falls back to its bucket if the date is
/// cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoBucket {
    /// Captured but not yet triaged.
    #[default]
    Inbox,
    /// Triaged, do it whenever.
    Anytime,
    /// Deliberately deferred out of sight.
    Someday,
}

impl TodoBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoBucket::Inbox => "inbox",
            TodoBucket::Anytime => "anytime",
            TodoBucket::Someday => "someday",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeOfDay {
    Morning,
    Afternoon,
    /// Things' "This Evening" — a separate group at the bottom of Today.
    Evening,
}

/// Who created the todo. `Ai` records the originating session so provenance
/// survives even though the list itself is global.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TodoOrigin {
    #[default]
    User,
    Ai {
        session_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub done: bool,
}

// ── Todo ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub status: TodoStatus,
    #[serde(default)]
    pub bucket: TodoBucket,

    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub area_id: Option<String>,

    /// The day you intend to work on it. Deliberately distinct from `deadline`
    /// — collapsing the two is what makes most to-do apps nag.
    #[serde(default)]
    pub scheduled_for: Option<NaiveDate>,
    #[serde(default)]
    pub time_of_day: Option<TimeOfDay>,
    /// The day it is actually due.
    #[serde(default)]
    pub deadline: Option<NaiveDate>,

    #[serde(default)]
    pub estimate_minutes: Option<u32>,
    #[serde(default)]
    pub actual_minutes: u32,

    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub checklist: Vec<ChecklistItem>,

    /// Fractional index — see [`sort_between`].
    #[serde(default)]
    pub sort_order: f64,
    #[serde(default)]
    pub origin: TodoOrigin,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl Todo {
    /// A new todo in the Inbox, sorted to the end.
    pub fn new(title: impl Into<String>, sort_order: f64) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            notes: String::new(),
            status: TodoStatus::Open,
            bucket: TodoBucket::Inbox,
            project_id: None,
            area_id: None,
            scheduled_for: None,
            time_of_day: None,
            deadline: None,
            estimate_minutes: None,
            actual_minutes: 0,
            tags: Vec::new(),
            checklist: Vec::new(),
            sort_order,
            origin: TodoOrigin::User,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    /// Past its deadline and still open. Uses the *deadline*, never the
    /// scheduled date — missing a day you planned something is not a failure.
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        !self.status.is_closed() && self.deadline.is_some_and(|d| d < today)
    }

    pub fn mark_completed(&mut self, now: DateTime<Utc>) {
        self.status = TodoStatus::Completed;
        self.completed_at = Some(now);
        self.updated_at = now;
    }

    pub fn reopen(&mut self, now: DateTime<Utc>) {
        self.status = TodoStatus::Open;
        self.completed_at = None;
        self.updated_at = now;
    }

    /// One-line summary for AI tool responses. Line-oriented on purpose: tool
    /// results are re-fed into the next prompt, so this stays cheap.
    pub fn summary(&self) -> String {
        let mark = match self.status {
            TodoStatus::Open => "○",
            TodoStatus::Completed => "✓",
            TodoStatus::Cancelled => "✗",
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = self.estimate_minutes {
            parts.push(format_minutes(m));
        }
        if let Some(d) = self.scheduled_for {
            parts.push(d.to_string());
        }
        if let Some(d) = self.deadline {
            parts.push(format!("due {}", d));
        }
        for tag in &self.tags {
            parts.push(format!("#{}", tag));
        }

        if parts.is_empty() {
            format!("[{}] {} {}", self.id, mark, self.title)
        } else {
            format!(
                "[{}] {} {} — {}",
                self.id,
                mark,
                self.title,
                parts.join(", ")
            )
        }
    }
}

// ── Projects & areas ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub status: TodoStatus,
    #[serde(default)]
    pub deadline: Option<NaiveDate>,
    #[serde(default)]
    pub sort_order: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub sort_order: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Time blocks & day plans ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BlockSource {
    /// The user dragged it onto the timeline.
    Manual,
    /// Placed by the planner.
    Auto,
    /// Mirrored from a real calendar; read-only in the UI.
    External { uid: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeBlock {
    pub id: String,
    /// `None` for meetings and other blocks that aren't a todo.
    #[serde(default)]
    pub todo_id: Option<String>,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub source: BlockSource,
}

impl TimeBlock {
    // Consumed by P4's inline cards (elapsed-vs-estimate display).
    #[allow(dead_code)]
    pub fn duration_minutes(&self) -> u32 {
        (self.end - self.start).num_minutes().max(0) as u32
    }

    /// Whether two blocks overlap in time. Touching edges do not overlap, so a
    /// 09:00–10:00 block and a 10:00–11:00 block sit back to back cleanly.
    pub fn overlaps(&self, other: &TimeBlock) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayPlan {
    pub date: NaiveDate,
    pub capacity_minutes: u32,
    #[serde(default)]
    pub planned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub shutdown_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reflection: String,
}

impl DayPlan {
    pub fn new(date: NaiveDate, capacity_minutes: u32) -> Self {
        Self {
            date,
            capacity_minutes,
            planned_at: None,
            shutdown_at: None,
            reflection: String::new(),
        }
    }
}

/// The result of measuring a day's planned work against its capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub planned_minutes: u32,
    pub capacity_minutes: u32,
    /// Todos scheduled for the day with no estimate — they make `planned`
    /// an undercount, so the UI and the AI should say so rather than imply
    /// the day fits.
    pub unestimated: usize,
}

impl Capacity {
    pub fn over_by(&self) -> u32 {
        self.planned_minutes.saturating_sub(self.capacity_minutes)
    }

    pub fn remaining(&self) -> u32 {
        self.capacity_minutes.saturating_sub(self.planned_minutes)
    }

    pub fn is_overcommitted(&self) -> bool {
        self.planned_minutes > self.capacity_minutes
    }

    /// Human summary for tool responses and the capacity bar's tooltip.
    pub fn summary(&self) -> String {
        let base = format!(
            "{} planned of {} capacity",
            format_minutes(self.planned_minutes),
            format_minutes(self.capacity_minutes)
        );
        let mut out = if self.is_overcommitted() {
            format!("{} — over by {}", base, format_minutes(self.over_by()))
        } else {
            format!("{} — {} free", base, format_minutes(self.remaining()))
        };
        if self.unestimated > 0 {
            out.push_str(&format!(
                " ({} unestimated, so the real total is higher)",
                self.unestimated
            ));
        }
        out
    }
}

/// Measure the open todos scheduled for a day against its capacity.
pub fn measure_capacity(todos: &[Todo], date: NaiveDate, capacity_minutes: u32) -> Capacity {
    let scheduled = todos
        .iter()
        .filter(|t| t.scheduled_for == Some(date) && !t.status.is_closed());

    let mut planned = 0u32;
    let mut unestimated = 0usize;
    for todo in scheduled {
        match todo.estimate_minutes {
            Some(m) => planned = planned.saturating_add(m),
            None => unestimated += 1,
        }
    }

    Capacity {
        planned_minutes: planned,
        capacity_minutes,
        unestimated,
    }
}

// ── Fractional ordering ─────────────────────────────────────────────────────

/// Gap between freshly appended items. Large enough that thousands of
/// in-between insertions never need a renormalise.
pub const SORT_STEP: f64 = 1024.0;

/// Below this gap, `sort_between` can no longer split cleanly and the list
/// should be renormalised.
#[allow(dead_code)] // consumer is drag-and-drop reordering, deferred from P2
pub const SORT_MIN_GAP: f64 = 1e-6;

/// A sort key placing an item between two neighbours.
///
/// `None` means "no neighbour on that side". Writing one row instead of
/// renumbering the whole list is what keeps drag-and-drop responsive.
#[allow(dead_code)] // consumer is drag-and-drop reordering, deferred from P2
pub fn sort_between(before: Option<f64>, after: Option<f64>) -> f64 {
    match (before, after) {
        (None, None) => 0.0,
        (Some(b), None) => b + SORT_STEP,
        (None, Some(a)) => a - SORT_STEP,
        (Some(b), Some(a)) => (b + a) / 2.0,
    }
}

/// Whether the neighbours have collapsed too close together to split again.
/// The caller should renormalise the list and retry.
#[allow(dead_code)] // consumer is drag-and-drop reordering, deferred from P2
pub fn needs_renormalise(before: Option<f64>, after: Option<f64>) -> bool {
    match (before, after) {
        (Some(b), Some(a)) => (a - b).abs() < SORT_MIN_GAP,
        _ => false,
    }
}

/// Rewrite an ordered slice's sort keys onto a fresh evenly-spaced ladder.
#[allow(dead_code)] // consumer is drag-and-drop reordering, deferred from P2
pub fn renormalise(todos: &mut [Todo]) {
    for (i, todo) in todos.iter_mut().enumerate() {
        todo.sort_order = i as f64 * SORT_STEP;
    }
}

// ── Formatting ──────────────────────────────────────────────────────────────

/// `95` → `"1h 35m"`, `45` → `"45m"`, `120` → `"2h"`.
pub fn format_minutes(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => format!("{}m", m),
        (h, 0) => format!("{}h", h),
        (h, m) => format!("{}h {}m", h, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn todo_with(estimate: Option<u32>, scheduled: Option<NaiveDate>) -> Todo {
        let mut t = Todo::new("t", 0.0);
        t.estimate_minutes = estimate;
        t.scheduled_for = scheduled;
        t
    }

    #[test]
    fn format_minutes_reads_naturally() {
        assert_eq!(format_minutes(0), "0m");
        assert_eq!(format_minutes(45), "45m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(95), "1h 35m");
        assert_eq!(format_minutes(120), "2h");
    }

    #[test]
    fn overdue_uses_deadline_not_scheduled_date() {
        let today = date("2026-08-12");
        let mut t = Todo::new("x", 0.0);

        // A scheduled date in the past is not overdue — plans slip, that's fine.
        t.scheduled_for = Some(date("2026-08-01"));
        assert!(!t.is_overdue(today));

        t.deadline = Some(date("2026-08-11"));
        assert!(t.is_overdue(today));

        // Due today is not yet overdue.
        t.deadline = Some(today);
        assert!(!t.is_overdue(today));

        // Closed todos are never overdue.
        t.deadline = Some(date("2026-08-01"));
        t.mark_completed(Utc::now());
        assert!(!t.is_overdue(today));
    }

    #[test]
    fn capacity_counts_only_open_todos_scheduled_that_day() {
        let today = date("2026-08-12");
        let other = date("2026-08-13");
        let mut done = todo_with(Some(60), Some(today));
        done.mark_completed(Utc::now());

        let todos = vec![
            todo_with(Some(45), Some(today)),
            todo_with(Some(30), Some(today)),
            todo_with(Some(600), Some(other)), // different day
            todo_with(None, Some(today)),      // unestimated
            done,                              // already finished
            todo_with(Some(90), None),         // unscheduled
        ];

        let cap = measure_capacity(&todos, today, 360);
        assert_eq!(cap.planned_minutes, 75);
        assert_eq!(cap.unestimated, 1);
        assert_eq!(cap.remaining(), 285);
        assert!(!cap.is_overcommitted());
    }

    #[test]
    fn capacity_reports_overcommitment() {
        let today = date("2026-08-12");
        let todos = vec![todo_with(Some(400), Some(today))];
        let cap = measure_capacity(&todos, today, 360);

        assert!(cap.is_overcommitted());
        assert_eq!(cap.over_by(), 40);
        assert_eq!(cap.remaining(), 0);
        assert!(cap.summary().contains("over by 40m"));
    }

    #[test]
    fn capacity_summary_flags_unestimated_work() {
        let today = date("2026-08-12");
        let todos = vec![todo_with(Some(60), Some(today)), todo_with(None, Some(today))];
        let cap = measure_capacity(&todos, today, 360);

        // A day that "fits" while hiding unestimated work is the exact lie the
        // planner exists to prevent.
        assert!(cap.summary().contains("1 unestimated"));
    }

    #[test]
    fn sort_between_splits_neighbours() {
        assert_eq!(sort_between(None, None), 0.0);
        assert_eq!(sort_between(Some(100.0), None), 100.0 + SORT_STEP);
        assert_eq!(sort_between(None, Some(100.0)), 100.0 - SORT_STEP);
        assert_eq!(sort_between(Some(0.0), Some(100.0)), 50.0);
    }

    #[test]
    fn renormalise_when_neighbours_collapse() {
        assert!(!needs_renormalise(Some(0.0), Some(1.0)));
        assert!(needs_renormalise(Some(1.0), Some(1.0 + 1e-9)));
        // Open-ended sides can always be extended.
        assert!(!needs_renormalise(None, Some(1.0)));

        let mut todos = vec![Todo::new("a", 5.0), Todo::new("b", 5.000001)];
        renormalise(&mut todos);
        assert_eq!(todos[0].sort_order, 0.0);
        assert_eq!(todos[1].sort_order, SORT_STEP);
    }

    #[test]
    fn blocks_touching_edges_do_not_overlap() {
        let base = "2026-08-12T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let block = |from_h: i64, to_h: i64| TimeBlock {
            id: "b".into(),
            todo_id: None,
            title: "b".into(),
            start: base + chrono::Duration::hours(from_h),
            end: base + chrono::Duration::hours(to_h),
            source: BlockSource::Manual,
        };

        assert!(!block(0, 1).overlaps(&block(1, 2)));
        assert!(block(0, 2).overlaps(&block(1, 3)));
        assert_eq!(block(0, 2).duration_minutes(), 120);
    }

    #[test]
    fn todo_roundtrips_with_missing_optional_fields() {
        // Rows written by an older build must still load.
        let json = serde_json::json!({
            "id": "td_1",
            "title": "Draft the proposal",
            "created_at": "2026-08-12T09:00:00Z",
            "updated_at": "2026-08-12T09:00:00Z"
        });
        let t: Todo = serde_json::from_value(json).unwrap();
        assert_eq!(t.status, TodoStatus::Open);
        assert_eq!(t.bucket, TodoBucket::Inbox);
        assert_eq!(t.origin, TodoOrigin::User);
        assert!(t.tags.is_empty());
        assert!(t.scheduled_for.is_none());
    }

    #[test]
    fn summary_is_line_oriented() {
        let mut t = Todo::new("Draft the proposal", 0.0);
        t.id = "td_1a2b".into();
        t.estimate_minutes = Some(45);
        t.scheduled_for = Some(date("2026-08-12"));
        t.tags = vec!["writing".into()];

        assert_eq!(
            t.summary(),
            "[td_1a2b] ○ Draft the proposal — 45m, 2026-08-12, #writing"
        );

        let bare = Todo::new("Bare", 0.0);
        assert!(bare.summary().ends_with("○ Bare"));
    }
}
