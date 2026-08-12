//! The planner: a built-in to-do list and daily planner.
//!
//! Structure follows Things (Inbox / Today / Upcoming, Areas ▸ Projects, and a
//! todo's "when" kept distinct from its "deadline"); the daily planning ritual
//! follows Sunsama (per-todo estimates measured against a day's capacity).
//!
//! Todos are **global**, not session-scoped: one list visible from every tab,
//! shared by the user and the assistant. `Todo::origin` records which session an
//! AI-created todo came from, so provenance survives even though the list does
//! not belong to any one conversation.
//!
//! See `PLANNER_DESIGN.md` for the full specification.
//!
pub mod handlers;
pub mod model;
pub mod store;
pub mod views;

use model::{Area, DayPlan, Project, TimeBlock, Todo};

/// The whole planner, hydrated in memory.
///
/// Unlike `SessionState`, this is loaded in full at startup and written through
/// per mutation. Planner rows are small and bounded by human effort, so lazy
/// hydration and dirty-diff batching would be complexity without benefit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlannerState {
    pub todos: Vec<Todo>,
    pub projects: Vec<Project>,
    pub areas: Vec<Area>,
    pub blocks: Vec<TimeBlock>,
    pub day_plans: Vec<DayPlan>,
}

impl PlannerState {
    /// Load everything from the store. Returns an empty planner if the database
    /// is unavailable, so a storage failure degrades rather than blocking launch.
    pub fn load() -> Self {
        store::load_all()
    }

    pub fn todo(&self, id: &str) -> Option<&Todo> {
        self.todos.iter().find(|t| t.id == id)
    }

    pub fn todo_mut(&mut self, id: &str) -> Option<&mut Todo> {
        self.todos.iter_mut().find(|t| t.id == id)
    }

    /// Sort key placing a new todo at the end of the list.
    pub fn next_sort_order(&self) -> f64 {
        self.todos
            .iter()
            .map(|t| t.sort_order)
            .fold(f64::NEG_INFINITY, f64::max)
            .max(0.0)
            + model::SORT_STEP
    }

    /// The capacity a day is planned against: its stored `DayPlan` if the day
    /// has been planned, otherwise the configured default.
    pub fn capacity_for(&self, date: chrono::NaiveDate, default_minutes: u32) -> u32 {
        self.day_plans
            .iter()
            .find(|p| p.date == date)
            .map(|p| p.capacity_minutes)
            .unwrap_or(default_minutes)
    }

    /// Insert or replace a todo in memory. Persisting is the caller's job — see
    /// [`store::save_todo`] — so a single UI action can batch several edits into
    /// one write.
    pub fn upsert_todo(&mut self, todo: Todo) {
        match self.todos.iter_mut().find(|t| t.id == todo.id) {
            Some(existing) => *existing = todo,
            None => self.todos.push(todo),
        }
    }

    pub fn remove_todo(&mut self, id: &str) -> Option<Todo> {
        let idx = self.todos.iter().position(|t| t.id == id)?;
        // Blocks referencing the todo would otherwise linger on the timeline
        // pointing at nothing.
        self.blocks.retain(|b| b.todo_id.as_deref() != Some(id));
        Some(self.todos.remove(idx))
    }

    /// Time blocks starting on a day, earliest first.
    ///
    /// `date` is a **local** calendar day — planner days are user-local
    /// everywhere (handlers and the timeline both pass
    /// `chrono::Local::now().date_naive()`). Blocks are stored in UTC, so the
    /// start must be converted back to local time before comparing; comparing
    /// `start.date_naive()` (the UTC date) makes any late-evening local block
    /// fall on the next UTC day and vanish from today's timeline.
    pub fn blocks_on(&self, date: chrono::NaiveDate) -> Vec<&TimeBlock> {
        let mut out: Vec<&TimeBlock> = self
            .blocks
            .iter()
            .filter(|b| b.start.with_timezone(&chrono::Local).date_naive() == date)
            .collect();
        out.sort_by_key(|b| b.start);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{BlockSource, TimeBlock, Todo};

    fn date(s: &str) -> chrono::NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn next_sort_order_appends_past_the_end() {
        let mut state = PlannerState::default();
        assert_eq!(state.next_sort_order(), model::SORT_STEP);

        state.upsert_todo(Todo::new("a", 5000.0));
        assert_eq!(state.next_sort_order(), 5000.0 + model::SORT_STEP);

        // Negative keys (from repeated prepends) must not drag the end backwards.
        state.upsert_todo(Todo::new("b", -9000.0));
        assert_eq!(state.next_sort_order(), 5000.0 + model::SORT_STEP);
    }

    #[test]
    fn upsert_replaces_rather_than_duplicates() {
        let mut state = PlannerState::default();
        let mut todo = Todo::new("original", 0.0);
        state.upsert_todo(todo.clone());

        todo.title = "edited".into();
        state.upsert_todo(todo);

        assert_eq!(state.todos.len(), 1);
        assert_eq!(state.todos[0].title, "edited");
    }

    #[test]
    fn removing_a_todo_takes_its_time_blocks_with_it() {
        let mut state = PlannerState::default();
        let todo = Todo::new("focus", 0.0);
        let id = todo.id.clone();
        state.upsert_todo(todo);

        let now = chrono::Utc::now();
        state.blocks.push(TimeBlock {
            id: "blk_1".into(),
            todo_id: Some(id.clone()),
            title: "focus".into(),
            start: now,
            end: now + chrono::Duration::hours(1),
            source: BlockSource::Manual,
        });
        state.blocks.push(TimeBlock {
            id: "blk_2".into(),
            todo_id: None,
            title: "standup".into(),
            start: now,
            end: now + chrono::Duration::minutes(15),
            source: BlockSource::Manual,
        });

        assert!(state.remove_todo(&id).is_some());
        assert!(state.todos.is_empty());
        assert_eq!(state.blocks.len(), 1, "orphaned block should be dropped");
        assert_eq!(state.blocks[0].id, "blk_2");
        assert!(state.remove_todo(&id).is_none());
    }

    #[test]
    fn capacity_falls_back_to_the_default_until_the_day_is_planned() {
        let mut state = PlannerState::default();
        let day = date("2026-08-12");
        assert_eq!(state.capacity_for(day, 360), 360);

        state.day_plans.push(DayPlan::new(day, 240));
        assert_eq!(state.capacity_for(day, 360), 240);
        assert_eq!(state.capacity_for(date("2026-08-13"), 360), 360);
    }

    /// Build a UTC instant from a *local* date and hour, the way blocks are
    /// created by the timeline and the HOBBES_TIME_BLOCK handler. Keeps these
    /// tests independent of the machine's timezone.
    fn local_instant(date: chrono::NaiveDate, hour: u32) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn blocks_on_filters_by_local_day_and_sorts_by_start() {
        let mut state = PlannerState::default();
        let day = date("2026-08-12");
        let next = date("2026-08-13");
        let mut push = |id: &str, on: chrono::NaiveDate, hour: u32| {
            state.blocks.push(TimeBlock {
                id: id.into(),
                todo_id: None,
                title: id.into(),
                start: local_instant(on, hour),
                end: local_instant(on, hour) + chrono::Duration::hours(1),
                source: BlockSource::Manual,
            });
        };
        push("later", day, 12);
        push("earlier", day, 9);
        // 22:00 local is the case the UTC-date comparison got wrong: in any
        // timezone at or west of UTC-2 it lands on the next *UTC* day.
        push("late-evening", day, 22);
        push("next-day", next, 10);

        let ids: Vec<&str> = state
            .blocks_on(day)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, vec!["earlier", "later", "late-evening"]);
    }
}
