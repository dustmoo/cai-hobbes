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
pub mod quick_add;
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
        let mut state = store::load_all();
        let stale = state.sanitize_stale_focus(chrono::Utc::now());
        for id in &stale {
            if let Some(t) = state.todo(id) {
                if let Err(e) = store::save_todo(t) {
                    tracing::error!("Failed to persist stale-focus pause for {}: {}", id, e);
                }
            }
            tracing::info!("Paused stale focus session on todo {}", id);
        }
        state
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

    /// The todo currently in focus, if any. The single-focus invariant makes
    /// `find` correct: `start_focus` never leaves two in progress.
    pub fn focused(&self) -> Option<&Todo> {
        self.todos
            .iter()
            .find(|t| t.status == model::TodoStatus::InProgress)
    }

    /// Enter focus on one todo, pausing whichever held it before — at most one
    /// task is ever in progress (the Sunsama model: focus is singular).
    /// Returns the ids of every todo whose state changed, for persistence.
    pub fn start_focus(
        &mut self,
        todo_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<String> {
        if self.todo(todo_id).is_none() {
            return Vec::new();
        }
        let mut changed = Vec::new();
        for t in &mut self.todos {
            if t.status == model::TodoStatus::InProgress && t.id != todo_id {
                t.pause(now);
                changed.push(t.id.clone());
            }
        }
        if let Some(t) = self.todo_mut(todo_id) {
            if t.status != model::TodoStatus::InProgress {
                t.fold_elapsed(now); // defensive: a stray marker never double-counts
                t.status = model::TodoStatus::InProgress;
                t.started_at = Some(now);
                t.updated_at = now;
                changed.push(t.id.clone());
            }
        }
        changed
    }

    /// Leave focus mode, banking the session. Returns the paused todo's id.
    pub fn stop_focus(&mut self, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
        let id = self.focused().map(|t| t.id.clone())?;
        if let Some(t) = self.todo_mut(&id) {
            t.pause(now);
        }
        Some(id)
    }

    /// A focus session that survived an app quit is almost certainly abandoned
    /// — pause it on load, capping the banked time at two hours so a forgotten
    /// overnight session can't pollute the actuals. Returns the affected ids.
    pub fn sanitize_stale_focus(&mut self, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
        let mut changed = Vec::new();
        for t in &mut self.todos {
            if t.status == model::TodoStatus::InProgress {
                if let Some(started) = t.started_at {
                    let mins = (now - started).num_minutes().clamp(0, 120) as u32;
                    t.actual_minutes = t.actual_minutes.saturating_add(mins);
                    t.started_at = None;
                }
                t.status = model::TodoStatus::Open;
                t.updated_at = now;
                changed.push(t.id.clone());
            }
        }
        changed
    }

    /// A block's display title: the linked todo's *current* title when there
    /// is one, else the title stored on the block. The stored copy goes stale
    /// the moment the todo is renamed — every surface (timeline, tool
    /// responses, planner_today context) must resolve through this.
    pub fn block_display_title(&self, block: &TimeBlock) -> String {
        block
            .todo_id
            .as_deref()
            .and_then(|id| self.todo(id))
            .map(|t| t.title.clone())
            .unwrap_or_else(|| block.title.clone())
    }

    /// Remove a todo's time blocks that sit on any local day other than
    /// `keep_day` (`None` keeps nothing). Returns the removed blocks so the
    /// caller can delete their store rows and report the count.
    ///
    /// The timebox follows the schedule: rescheduling or unscheduling a todo
    /// without this leaves its old blocks squatting on the calendar, counting
    /// for nothing.
    pub fn prune_blocks_for_todo(
        &mut self,
        todo_id: &str,
        keep_day: Option<chrono::NaiveDate>,
    ) -> Vec<TimeBlock> {
        let mut removed = Vec::new();
        self.blocks.retain(|b| {
            let is_linked = b.todo_id.as_deref() == Some(todo_id);
            let keeps = keep_day
                .is_some_and(|d| b.start.with_timezone(&chrono::Local).date_naive() == d);
            if is_linked && !keeps {
                removed.push(b.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Placing (or moving) a linked block onto a day IS scheduling the work
    /// there — the inverse of [`prune_blocks_for_todo`]: the schedule and the
    /// timebox must never disagree. Sets `scheduled_for`, stamps `updated_at`,
    /// and prunes the todo's blocks left on other days. Returns whether the
    /// schedule actually changed and the pruned blocks (for store deletion).
    pub fn schedule_todo_on(
        &mut self,
        todo_id: &str,
        date: chrono::NaiveDate,
        now: chrono::DateTime<chrono::Utc>,
    ) -> (bool, Vec<TimeBlock>) {
        let Some(todo) = self.todo_mut(todo_id) else {
            return (false, Vec::new());
        };
        let changed = todo.scheduled_for != Some(date);
        if changed {
            todo.scheduled_for = Some(date);
            todo.updated_at = now;
        }
        let pruned = self.prune_blocks_for_todo(todo_id, Some(date));
        (changed, pruned)
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
    fn block_display_title_follows_the_linked_todo() {
        let mut state = PlannerState::default();
        let mut todo = Todo::new("Original", 0.0);
        todo.id = "td_1".into();
        state.upsert_todo(todo);

        let linked = TimeBlock {
            id: "blk_1".into(),
            todo_id: Some("td_1".into()),
            title: "Original".into(),
            start: chrono::Utc::now(),
            end: chrono::Utc::now(),
            source: BlockSource::Manual,
        };
        let bare = TimeBlock {
            id: "blk_2".into(),
            todo_id: None,
            title: "Standup".into(),
            start: chrono::Utc::now(),
            end: chrono::Utc::now(),
            source: BlockSource::Manual,
        };

        state.todo_mut("td_1").unwrap().title = "Renamed".into();
        assert_eq!(state.block_display_title(&linked), "Renamed");
        assert_eq!(state.block_display_title(&bare), "Standup");
    }

    #[test]
    fn prune_blocks_follows_the_schedule() {
        let mut state = PlannerState::default();
        let day = date("2026-08-12");
        let other = date("2026-08-13");
        let mut push = |id: &str, todo_id: Option<&str>, on: chrono::NaiveDate| {
            state.blocks.push(TimeBlock {
                id: id.into(),
                todo_id: todo_id.map(String::from),
                title: id.into(),
                start: local_instant(on, 9),
                end: local_instant(on, 10),
                source: BlockSource::Manual,
            });
        };
        push("keep-day", Some("td_1"), day);
        push("wrong-day", Some("td_1"), other);
        push("other-todo", Some("td_2"), other);
        push("bare", None, other);

        // Rescheduled onto `day`: only the block on another day goes.
        let removed = state.prune_blocks_for_todo("td_1", Some(day));
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "wrong-day");
        assert_eq!(state.blocks.len(), 3);

        // Unscheduled entirely: its remaining block goes too; unrelated and
        // bare blocks are never touched.
        let removed = state.prune_blocks_for_todo("td_1", None);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "keep-day");
        let ids: Vec<&str> = state.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["other-todo", "bare"]);
    }

    #[test]
    fn focus_is_singular() {
        let now = chrono::Utc::now();
        let mut state = PlannerState::default();
        let (a, b) = (Todo::new("a", 0.0), Todo::new("b", 0.0));
        let (id_a, id_b) = (a.id.clone(), b.id.clone());
        state.upsert_todo(a);
        state.upsert_todo(b);

        let changed = state.start_focus(&id_a, now);
        assert_eq!(changed, vec![id_a.clone()]);
        assert_eq!(state.focused().unwrap().id, id_a);

        // Focusing b pauses a — both report as changed.
        let changed = state.start_focus(&id_b, now);
        assert_eq!(changed.len(), 2);
        assert_eq!(state.focused().unwrap().id, id_b);
        assert_eq!(
            state.todo(&id_a).unwrap().status,
            model::TodoStatus::Open,
            "the previous focus must be paused, not left dangling"
        );

        // Re-focusing the focused todo is a no-op (no double-start).
        assert!(state.start_focus(&id_b, now).is_empty());
        // Unknown ids change nothing.
        assert!(state.start_focus("nope", now).is_empty());

        assert_eq!(state.stop_focus(now), Some(id_b.clone()));
        assert!(state.focused().is_none());
        assert_eq!(state.stop_focus(now), None);
    }

    #[test]
    fn stale_focus_is_paused_on_load_with_capped_accrual() {
        let now: chrono::DateTime<chrono::Utc> = "2026-08-13T09:00:00Z".parse().unwrap();
        let mut state = PlannerState::default();
        let mut t = Todo::new("forgotten overnight", 0.0);
        t.status = model::TodoStatus::InProgress;
        t.started_at = Some(now - chrono::Duration::hours(14));
        let id = t.id.clone();
        state.upsert_todo(t);

        let changed = state.sanitize_stale_focus(now);
        assert_eq!(changed, vec![id.clone()]);
        let t = state.todo(&id).unwrap();
        assert_eq!(t.status, model::TodoStatus::Open);
        assert_eq!(t.actual_minutes, 120, "overnight sessions cap at 2h");
        assert!(t.started_at.is_none());
    }

    #[test]
    fn scheduling_via_block_placement_syncs_the_todo() {
        let mut state = PlannerState::default();
        let day = date("2026-08-13");
        let yesterday = date("2026-08-12");
        let mut todo = Todo::new("Email", 0.0);
        todo.id = "td_1".into();
        todo.scheduled_for = Some(yesterday);
        state.upsert_todo(todo);
        // A stale block from yesterday, and today's fresh one.
        for (id, on) in [("old", yesterday), ("new", day)] {
            state.blocks.push(TimeBlock {
                id: id.into(),
                todo_id: Some("td_1".into()),
                title: "Email".into(),
                start: local_instant(on, 10),
                end: local_instant(on, 11),
                source: BlockSource::Manual,
            });
        }

        let (changed, pruned) = state.schedule_todo_on("td_1", day, chrono::Utc::now());
        assert!(changed, "the line item must move to the block's day");
        assert_eq!(state.todo("td_1").unwrap().scheduled_for, Some(day));
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].id, "old");
        assert_eq!(state.blocks.len(), 1, "today's block survives the prune");

        // Same day again: no-op, nothing pruned.
        let (changed, pruned) = state.schedule_todo_on("td_1", day, chrono::Utc::now());
        assert!(!changed);
        assert!(pruned.is_empty());
        // Unknown id: harmless.
        let (changed, pruned) = state.schedule_todo_on("nope", day, chrono::Utc::now());
        assert!(!changed && pruned.is_empty());
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
