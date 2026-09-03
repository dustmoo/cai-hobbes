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
pub mod calendar_sync;
pub mod composio_calendar;
pub mod dispatch;
pub mod handlers;
pub mod ics;
pub mod model;
pub mod quick_add;
pub mod store;
pub mod views;

use model::{Area, DayPlan, FocusActor, FocusEndReason, FocusSession, Project, TimeBlock, Todo};

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
    /// Focus-session history (person vs agent time). The lifecycle lives at
    /// this layer — every path that starts or banks focus opens/closes a row
    /// here — so the model's `fold_elapsed` stays pure.
    pub focus_sessions: Vec<FocusSession>,
    /// Session rows touched since the last [`Self::take_dirty_focus_sessions`]
    /// drain. Lets every caller persist exactly what changed without the
    /// lifecycle methods growing store side effects (handlers pass
    /// `persist: false` in tests and must stay off the database).
    dirty_focus_sessions: Vec<String>,
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
        for s in state.take_dirty_focus_sessions() {
            if let Err(e) = store::save_focus_session(&s) {
                tracing::error!("Failed to persist recovered focus session {}: {}", s.id, e);
            }
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
        // Deleting mid-focus is abandoning the work: the session row must not
        // stay open forever (session history itself is kept — the time was
        // still spent).
        self.close_focus_rows_for(id, chrono::Utc::now(), FocusEndReason::Cancelled);
        Some(self.todos.remove(idx))
    }

    /// The todo currently in focus, if any. The single-focus invariant makes
    /// `find` correct: `start_focus` never leaves two in progress.
    pub fn focused(&self) -> Option<&Todo> {
        self.todos
            .iter()
            .find(|t| t.status == model::TodoStatus::InProgress)
    }

    // ── Focus-session rows ──────────────────────────────────────────────────

    /// The live (unended) focus-session row, if any. The lifecycle below keeps
    /// at most one open — the row-level mirror of the single-focus invariant.
    #[allow(dead_code)] // invariant checks in tests; UI surfaces use _for(todo_id)
    pub fn open_focus_session(&self) -> Option<&FocusSession> {
        self.focus_sessions.iter().find(|s| s.is_open())
    }

    pub fn open_focus_session_for(&self, todo_id: &str) -> Option<&FocusSession> {
        self.focus_sessions
            .iter()
            .find(|s| s.is_open() && s.todo_id == todo_id)
    }

    /// Drain the ids of session rows touched since the last drain, returning
    /// the rows to persist. Every mutation path calls this after the operation
    /// (handlers gate the actual store write on their `persist` flag).
    pub fn take_dirty_focus_sessions(&mut self) -> Vec<FocusSession> {
        let mut ids = std::mem::take(&mut self.dirty_focus_sessions);
        ids.dedup();
        ids.iter()
            .filter_map(|id| self.focus_sessions.iter().find(|s| &s.id == id).cloned())
            .collect()
    }

    fn open_focus_row(&mut self, todo_id: &str, now: chrono::DateTime<chrono::Utc>, actor: FocusActor) {
        let s = FocusSession::open(todo_id, now, actor);
        self.dirty_focus_sessions.push(s.id.clone());
        self.focus_sessions.push(s);
    }

    /// Close every open session row on one todo with `reason`. Plural on
    /// purpose: a duplicate open row (crash artifact) must not survive as a
    /// forever-live session.
    fn close_focus_rows_for(
        &mut self,
        todo_id: &str,
        now: chrono::DateTime<chrono::Utc>,
        reason: FocusEndReason,
    ) {
        let mut dirty = Vec::new();
        for s in self
            .focus_sessions
            .iter_mut()
            .filter(|s| s.is_open() && s.todo_id == todo_id)
        {
            s.close(now, reason);
            dirty.push(s.id.clone());
        }
        self.dirty_focus_sessions.extend(dirty);
    }

    /// Enter focus on one todo, pausing whichever held it before — at most one
    /// task is ever in progress (the Sunsama model: focus is singular).
    ///
    /// `actor` records who is driving the sitting (UI buttons pass `Person`,
    /// the AI's `HOBBES_TODO_UPDATE` passes `Agent` with its chat session id);
    /// a `FocusSession` row opens for it, and any preempted todo's row closes
    /// as `preempted`.
    ///
    /// Focus means "working on it now", so a focused todo that isn't on
    /// today's plan contradicts itself: any todo not already scheduled today
    /// is pulled onto today, pruning its off-day blocks per the
    /// schedule↔timebox rule. Returns the ids of every todo whose state
    /// changed (for persistence) and the pruned blocks (the caller must
    /// delete their store rows). Touched session rows are persisted via
    /// [`Self::take_dirty_focus_sessions`].
    pub fn start_focus(
        &mut self,
        todo_id: &str,
        now: chrono::DateTime<chrono::Utc>,
        actor: FocusActor,
    ) -> (Vec<String>, Vec<TimeBlock>) {
        if self.todo(todo_id).is_none() {
            return (Vec::new(), Vec::new());
        }
        let mut changed = Vec::new();
        let mut preempted = Vec::new();
        for t in &mut self.todos {
            if t.status == model::TodoStatus::InProgress && t.id != todo_id {
                t.pause(now);
                changed.push(t.id.clone());
                preempted.push(t.id.clone());
            }
        }
        for id in preempted {
            self.close_focus_rows_for(&id, now, FocusEndReason::Preempted);
        }
        let mut started = false;
        if let Some(t) = self.todo_mut(todo_id) {
            if t.status != model::TodoStatus::InProgress {
                t.fold_elapsed(now); // defensive: a stray marker never double-counts
                // Focusing a closed todo revives it — full reopen semantics
                // (see Todo::reopen): without this, a later pause parks it
                // Open with a stale completed_at still set.
                t.completed_at = None;
                t.status = model::TodoStatus::InProgress;
                t.started_at = Some(now);
                t.updated_at = now;
                changed.push(t.id.clone());
                started = true;
            }
        }
        if started {
            // Defensive: a stray open row on this todo would double-open.
            self.close_focus_rows_for(todo_id, now, FocusEndReason::Recovered);
            self.open_focus_row(todo_id, now, actor);
        } else if self.open_focus_session_for(todo_id).is_none() {
            // Already in progress but row-less (pre-migration in-flight
            // session): give the live session a row so the invariant holds.
            self.open_focus_row(todo_id, now, actor);
        }
        let today = now.with_timezone(&chrono::Local).date_naive();
        let mut pruned = Vec::new();
        if self
            .todo(todo_id)
            .is_some_and(|t| t.scheduled_for != Some(today))
        {
            let (rescheduled, p) = self.schedule_todo_on(todo_id, today, now);
            pruned = p;
            if rescheduled && !changed.iter().any(|c| c == todo_id) {
                changed.push(todo_id.to_string());
            }
        }
        (changed, pruned)
    }

    /// Leave focus mode, banking the session. Returns the paused todo's id.
    /// The open session row closes as `paused`.
    pub fn stop_focus(&mut self, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
        let id = self.focused().map(|t| t.id.clone())?;
        if let Some(t) = self.todo_mut(&id) {
            t.pause(now);
        }
        self.close_focus_rows_for(&id, now, FocusEndReason::Paused);
        Some(id)
    }

    /// Complete a todo, closing its focus session (if live) as `completed`.
    /// The state-level entry for every "done" surface — going through
    /// `Todo::mark_completed` directly would leave the session row open.
    pub fn complete_todo(&mut self, id: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(t) = self.todo_mut(id) else {
            return false;
        };
        let was_in_progress = t.status == model::TodoStatus::InProgress;
        t.mark_completed(now);
        if was_in_progress {
            self.close_focus_rows_for(id, now, FocusEndReason::Completed);
        }
        true
    }

    /// Reopen a todo. Banking an in-progress one closes its session row as
    /// `stopped`; reopening closed work touches no session.
    pub fn reopen_todo(&mut self, id: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(t) = self.todo_mut(id) else {
            return false;
        };
        let was_in_progress = t.status == model::TodoStatus::InProgress;
        t.reopen(now);
        if was_in_progress {
            self.close_focus_rows_for(id, now, FocusEndReason::Stopped);
        }
        true
    }

    /// Cancel a todo. Cancelling mid-focus still banks the elapsed time (the
    /// work happened even if the goal was abandoned) and closes the session
    /// row as `cancelled`. The logbook orders by `completed_at`; a cancelled
    /// todo's "closed moment" is when it was abandoned.
    pub fn cancel_todo(&mut self, id: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(t) = self.todo_mut(id) else {
            return false;
        };
        let was_in_progress = t.status == model::TodoStatus::InProgress;
        t.fold_elapsed(now);
        t.status = model::TodoStatus::Cancelled;
        t.completed_at = Some(now);
        t.updated_at = now;
        if was_in_progress {
            self.close_focus_rows_for(id, now, FocusEndReason::Cancelled);
        }
        true
    }

    /// A focus session that survived an app quit is almost certainly abandoned
    /// — pause it on load, capping the banked time at two hours so a forgotten
    /// overnight session can't pollute the actuals. Returns the affected ids.
    ///
    /// The session row is closed honestly: `end_reason = recovered`, its real
    /// wall-clock bounds kept, `minutes` matching the clamped bank, and the
    /// real elapsed noted in `unclamped_minutes` when the clamp bit.
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
        // Close EVERY open row, not just those of in-progress todos — an
        // orphaned open row (crash between writes) must not live forever.
        let mut dirty = Vec::new();
        for s in self.focus_sessions.iter_mut().filter(|s| s.is_open()) {
            s.close(now, model::FocusEndReason::Recovered);
            let real = s.minutes;
            if real > 120 {
                s.minutes = 120;
                s.unclamped_minutes = Some(real);
            }
            dirty.push(s.id.clone());
        }
        self.dirty_focus_sessions.extend(dirty);
        changed
    }

    /// Estimate → timebox sync: after a todo's estimate changes, resize its
    /// block on the scheduled day so the start stays anchored and the end
    /// lands at start + estimate. Only applies when exactly ONE block holds
    /// the todo that day — a task deliberately split across several sittings
    /// is ambiguous, and resizing each to the full estimate would double the
    /// plan. Returns the changed blocks for persistence.
    pub fn resize_blocks_to_estimate(&mut self, todo_id: &str) -> Vec<TimeBlock> {
        let Some(todo) = self.todo(todo_id) else {
            return Vec::new();
        };
        let (Some(day), Some(estimate)) = (todo.scheduled_for, todo.estimate_minutes) else {
            return Vec::new();
        };
        let estimate = estimate.max(15) as i64;

        let mut on_day: Vec<usize> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.todo_id.as_deref() == Some(todo_id)
                    && b.start.with_timezone(&chrono::Local).date_naive() == day
            })
            .map(|(i, _)| i)
            .collect();
        if on_day.len() != 1 {
            return Vec::new();
        }
        let idx = on_day.pop().expect("len checked");

        let block = &mut self.blocks[idx];
        let new_end = block.start + chrono::Duration::minutes(estimate);
        if block.end == new_end {
            return Vec::new(); // already in sync (e.g. the resize-drag path)
        }
        block.end = new_end;
        vec![block.clone()]
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
    fn focusing_a_closed_todo_clears_completed_at() {
        // Regression: start_focus revived Completed/Cancelled todos without
        // clearing completed_at, so a later pause parked them Open with a
        // stale completion timestamp.
        let mut state = PlannerState::default();
        let mut todo = Todo::new("done once", 0.0);
        let id = todo.id.clone();
        let now = chrono::Utc::now();
        todo.status = model::TodoStatus::Completed;
        todo.completed_at = Some(now);
        state.upsert_todo(todo);

        state.start_focus(&id, now, model::FocusActor::Person);

        let t = state.todo(&id).unwrap();
        assert_eq!(t.status, model::TodoStatus::InProgress);
        assert_eq!(t.completed_at, None, "reopen semantics must clear completed_at");
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

        let today = now.with_timezone(&chrono::Local).date_naive();
        let (changed, pruned) = state.start_focus(&id_a, now, model::FocusActor::Person);
        assert_eq!(changed, vec![id_a.clone()]);
        assert!(pruned.is_empty());
        assert_eq!(state.focused().unwrap().id, id_a);
        // Focus implies today: the undated todo lands on today's plan.
        assert_eq!(state.todo(&id_a).unwrap().scheduled_for, Some(today));

        // Focusing b pauses a — both report as changed.
        let (changed, _) = state.start_focus(&id_b, now, model::FocusActor::Person);
        assert_eq!(changed.len(), 2);
        assert_eq!(state.focused().unwrap().id, id_b);
        assert_eq!(
            state.todo(&id_a).unwrap().status,
            model::TodoStatus::Open,
            "the previous focus must be paused, not left dangling"
        );

        // Re-focusing the focused todo is a no-op (no double-start).
        assert!(state.start_focus(&id_b, now, model::FocusActor::Person).0.is_empty());
        // Unknown ids change nothing.
        assert!(state.start_focus("nope", now, model::FocusActor::Person).0.is_empty());

        assert_eq!(state.stop_focus(now), Some(id_b.clone()));
        assert!(state.focused().is_none());
        assert_eq!(state.stop_focus(now), None);
    }

    #[test]
    fn focusing_a_future_todo_pulls_it_onto_today_and_prunes_its_block() {
        let now = chrono::Utc::now();
        let today = now.with_timezone(&chrono::Local).date_naive();
        let tomorrow = today.succ_opt().unwrap();

        let mut state = PlannerState::default();
        let mut todo = Todo::new("planned ahead", 0.0);
        todo.id = "td_1".into();
        todo.scheduled_for = Some(tomorrow);
        state.upsert_todo(todo);
        state.blocks.push(TimeBlock {
            id: "blk_tomorrow".into(),
            todo_id: Some("td_1".into()),
            title: "planned ahead".into(),
            start: local_instant(tomorrow, 9),
            end: local_instant(tomorrow, 10),
            source: BlockSource::Manual,
        });

        let (changed, pruned) = state.start_focus("td_1", now, model::FocusActor::Person);
        assert_eq!(changed, vec!["td_1".to_string()]);
        assert_eq!(
            state.todo("td_1").unwrap().scheduled_for,
            Some(today),
            "working on it now means it belongs on today's plan"
        );
        assert_eq!(pruned.len(), 1, "tomorrow's block follows the schedule off the calendar");
        assert_eq!(pruned[0].id, "blk_tomorrow");
        assert!(state.blocks.is_empty());

        // A todo already scheduled today keeps its date and blocks.
        state.stop_focus(now);
        let (_, pruned) = state.start_focus("td_1", now, model::FocusActor::Person);
        assert!(pruned.is_empty());
        assert_eq!(state.todo("td_1").unwrap().scheduled_for, Some(today));
    }

    // ── Focus-session rows ──────────────────────────────────────────────────

    /// The row-level invariant: exactly one open session row exists iff one
    /// todo is `InProgress`, and it points at that todo.
    fn assert_focus_invariant(state: &PlannerState) {
        let open: Vec<&model::FocusSession> =
            state.focus_sessions.iter().filter(|s| s.is_open()).collect();
        match state.focused() {
            Some(t) => {
                assert_eq!(open.len(), 1, "one InProgress todo ⇒ exactly one open row");
                assert_eq!(open[0].todo_id, t.id, "the open row tracks the focused todo");
            }
            None => assert!(open.is_empty(), "no InProgress todo ⇒ no open rows"),
        }
    }

    fn reasons_for<'a>(state: &'a PlannerState, todo_id: &str) -> Vec<&'a str> {
        state
            .focus_sessions
            .iter()
            .filter(|s| s.todo_id == todo_id)
            .filter_map(|s| s.end_reason.map(|r| r.as_str()))
            .collect()
    }

    #[test]
    fn focus_lifecycle_opens_and_closes_session_rows() {
        let start: chrono::DateTime<chrono::Utc> = "2026-08-13T10:00:00Z".parse().unwrap();
        let later: chrono::DateTime<chrono::Utc> = "2026-08-13T10:25:00Z".parse().unwrap();
        let mut state = PlannerState::default();
        let t = Todo::new("deep work", 0.0);
        let id = t.id.clone();
        state.upsert_todo(t);

        state.start_focus(&id, start, model::FocusActor::Person);
        assert_focus_invariant(&state);
        let open = state.open_focus_session_for(&id).expect("row opened");
        assert_eq!(open.actor, model::FocusActor::Person);
        assert_eq!(open.started_at, start);

        // Stop banks the sitting: closed as 'paused' with its minutes.
        state.stop_focus(later);
        assert_focus_invariant(&state);
        let row = &state.focus_sessions[0];
        assert_eq!(row.end_reason, Some(model::FocusEndReason::Paused));
        assert_eq!(row.minutes, 25);
        assert_eq!(row.ended_at, Some(later));
        assert_eq!(
            state.todo(&id).unwrap().actual_minutes,
            25,
            "the aggregate and the row must agree"
        );

        // Every touched row is queued for persistence, and draining is one-shot.
        let dirty = state.take_dirty_focus_sessions();
        assert_eq!(dirty.len(), 1, "opened and closed the same row: one persist");
        assert!(state.take_dirty_focus_sessions().is_empty());
    }

    #[test]
    fn preemption_closes_the_previous_row_as_preempted() {
        let now = chrono::Utc::now();
        let mut state = PlannerState::default();
        let (a, b) = (Todo::new("a", 0.0), Todo::new("b", 0.0));
        let (id_a, id_b) = (a.id.clone(), b.id.clone());
        state.upsert_todo(a);
        state.upsert_todo(b);

        state.start_focus(&id_a, now, model::FocusActor::Person);
        state.start_focus(&id_b, now, model::FocusActor::Person);
        assert_focus_invariant(&state);
        assert_eq!(reasons_for(&state, &id_a), vec!["preempted"]);
        assert!(state.open_focus_session_for(&id_b).is_some());

        // Re-focusing the focused todo neither closes nor duplicates the row.
        state.start_focus(&id_b, now, model::FocusActor::Person);
        assert_focus_invariant(&state);
        assert_eq!(state.focus_sessions.len(), 2);
    }

    #[test]
    fn agent_focus_records_the_driving_chat_session() {
        let now = chrono::Utc::now();
        let mut state = PlannerState::default();
        let t = Todo::new("delegated", 0.0);
        let id = t.id.clone();
        state.upsert_todo(t);

        state.start_focus(
            &id,
            now,
            model::FocusActor::Agent {
                session_id: Some("sess-42".into()),
            },
        );
        let open = state.open_focus_session_for(&id).unwrap();
        assert!(open.actor.is_agent());
        assert_eq!(open.actor.agent_session_id(), Some("sess-42"));
    }

    #[test]
    fn closing_transitions_stamp_their_end_reasons() {
        let now = chrono::Utc::now();
        let mut state = PlannerState::default();
        for title in ["complete me", "reopen me", "cancel me"] {
            state.upsert_todo(Todo::new(title, 0.0));
        }
        let ids: Vec<String> = state.todos.iter().map(|t| t.id.clone()).collect();

        // Complete an in-progress todo → 'completed'.
        state.start_focus(&ids[0], now, model::FocusActor::Person);
        assert!(state.complete_todo(&ids[0], now));
        assert_focus_invariant(&state);
        assert_eq!(reasons_for(&state, &ids[0]), vec!["completed"]);
        assert_eq!(state.todo(&ids[0]).unwrap().status, model::TodoStatus::Completed);

        // Reopen (banking an in-progress) → 'stopped'.
        state.start_focus(&ids[1], now, model::FocusActor::Person);
        assert!(state.reopen_todo(&ids[1], now));
        assert_focus_invariant(&state);
        assert_eq!(reasons_for(&state, &ids[1]), vec!["stopped"]);
        assert_eq!(state.todo(&ids[1]).unwrap().status, model::TodoStatus::Open);

        // Cancel mid-focus → 'cancelled', with the time still banked.
        state.start_focus(&ids[2], now, model::FocusActor::Person);
        assert!(state.cancel_todo(&ids[2], now));
        assert_focus_invariant(&state);
        assert_eq!(reasons_for(&state, &ids[2]), vec!["cancelled"]);
        let t = state.todo(&ids[2]).unwrap();
        assert_eq!(t.status, model::TodoStatus::Cancelled);
        assert!(t.completed_at.is_some());

        // Reopening completed (not in-progress) work touches no session.
        let rows_before = state.focus_sessions.len();
        assert!(state.reopen_todo(&ids[0], now));
        assert_eq!(state.focus_sessions.len(), rows_before);
        assert_focus_invariant(&state);

        // Unknown ids are harmless no-ops.
        assert!(!state.complete_todo("nope", now));
        assert!(!state.reopen_todo("nope", now));
        assert!(!state.cancel_todo("nope", now));
    }

    #[test]
    fn open_row_exists_iff_a_todo_is_in_progress() {
        let now = chrono::Utc::now();
        let mut state = PlannerState::default();
        let (a, b) = (Todo::new("a", 0.0), Todo::new("b", 0.0));
        let (id_a, id_b) = (a.id.clone(), b.id.clone());
        state.upsert_todo(a);
        state.upsert_todo(b);

        assert_focus_invariant(&state);
        state.start_focus(&id_a, now, model::FocusActor::Person);
        assert_focus_invariant(&state);
        state.stop_focus(now);
        assert_focus_invariant(&state);
        state.start_focus(
            &id_b,
            now,
            model::FocusActor::Agent { session_id: None },
        );
        assert_focus_invariant(&state);
        state.start_focus(&id_a, now, model::FocusActor::Person); // preempts b
        assert_focus_invariant(&state);
        state.complete_todo(&id_a, now);
        assert_focus_invariant(&state);
        state.sanitize_stale_focus(now);
        assert_focus_invariant(&state);
        // Deleting a focused todo must not strand an open row.
        state.start_focus(&id_b, now, model::FocusActor::Person);
        assert_focus_invariant(&state);
        state.remove_todo(&id_b);
        assert_focus_invariant(&state);
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
    fn sanitize_closes_the_session_row_honestly() {
        let now: chrono::DateTime<chrono::Utc> = "2026-08-13T09:00:00Z".parse().unwrap();
        let started = now - chrono::Duration::hours(14);
        let mut state = PlannerState::default();
        let mut t = Todo::new("forgotten overnight", 0.0);
        t.status = model::TodoStatus::InProgress;
        t.started_at = Some(started);
        let id = t.id.clone();
        state.upsert_todo(t);
        // The row a previous process left open (as start_focus writes it).
        state
            .focus_sessions
            .push(model::FocusSession::open(&id, started, model::FocusActor::Person));

        state.sanitize_stale_focus(now);
        assert_focus_invariant(&state);
        let row = &state.focus_sessions[0];
        assert_eq!(row.end_reason, Some(model::FocusEndReason::Recovered));
        // Honest bounds, clamped bank, real elapsed noted.
        assert_eq!(row.started_at, started);
        assert_eq!(row.ended_at, Some(now), "real wall-clock end, not the clamp");
        assert_eq!(row.minutes, 120, "the row's bank matches the clamped aggregate");
        assert_eq!(row.unclamped_minutes, Some(14 * 60), "the truth is noted in data");
        assert_eq!(state.todo(&id).unwrap().actual_minutes, 120);
        // The closed row is queued for persistence (PlannerState::load drains it).
        assert_eq!(state.take_dirty_focus_sessions().len(), 1);

        // A short stale session (under the clamp) closes without the note.
        let mut state = PlannerState::default();
        let mut t = Todo::new("brief", 0.0);
        t.status = model::TodoStatus::InProgress;
        t.started_at = Some(now - chrono::Duration::minutes(30));
        let id = t.id.clone();
        state.upsert_todo(t);
        state.focus_sessions.push(model::FocusSession::open(
            &id,
            now - chrono::Duration::minutes(30),
            model::FocusActor::Person,
        ));
        state.sanitize_stale_focus(now);
        let row = &state.focus_sessions[0];
        assert_eq!(row.minutes, 30);
        assert_eq!(row.unclamped_minutes, None);

        // Orphaned open rows (no matching InProgress todo) are closed too —
        // nothing survives sanitize as a forever-live session.
        let mut state = PlannerState::default();
        state.focus_sessions.push(model::FocusSession::open(
            "td_gone",
            now - chrono::Duration::minutes(10),
            model::FocusActor::Person,
        ));
        state.sanitize_stale_focus(now);
        assert_focus_invariant(&state);
        assert_eq!(
            state.focus_sessions[0].end_reason,
            Some(model::FocusEndReason::Recovered)
        );
    }

    #[test]
    fn estimate_change_resizes_the_single_timebox_from_its_start() {
        let mut state = PlannerState::default();
        let day = date("2026-08-13");
        let mut todo = Todo::new("Case study", 0.0);
        todo.id = "td_1".into();
        todo.scheduled_for = Some(day);
        todo.estimate_minutes = Some(120);
        state.upsert_todo(todo);
        state.blocks.push(TimeBlock {
            id: "blk_1".into(),
            todo_id: Some("td_1".into()),
            title: "Case study".into(),
            start: local_instant(day, 12) + chrono::Duration::minutes(30),
            end: local_instant(day, 14) + chrono::Duration::minutes(30),
            source: BlockSource::Manual,
        });

        // 2h -> 30m: the block keeps its 12:30 start and shrinks to 13:00.
        state.todo_mut("td_1").unwrap().estimate_minutes = Some(30);
        let changed = state.resize_blocks_to_estimate("td_1");
        assert_eq!(changed.len(), 1);
        let b = &state.blocks[0];
        assert_eq!(b.start, local_instant(day, 12) + chrono::Duration::minutes(30));
        assert_eq!(b.end, b.start + chrono::Duration::minutes(30));

        // Same estimate again: no-op, nothing to persist.
        assert!(state.resize_blocks_to_estimate("td_1").is_empty());

        // No estimate: the block is left alone rather than deleted or zeroed.
        state.todo_mut("td_1").unwrap().estimate_minutes = None;
        assert!(state.resize_blocks_to_estimate("td_1").is_empty());
        assert_eq!(state.blocks[0].end, state.blocks[0].start + chrono::Duration::minutes(30));
    }

    #[test]
    fn split_timeboxes_are_never_resized() {
        let mut state = PlannerState::default();
        let day = date("2026-08-13");
        let mut todo = Todo::new("split", 0.0);
        todo.id = "td_1".into();
        todo.scheduled_for = Some(day);
        todo.estimate_minutes = Some(60);
        state.upsert_todo(todo);
        for (id, h) in [("a", 9), ("b", 15)] {
            state.blocks.push(TimeBlock {
                id: id.into(),
                todo_id: Some("td_1".into()),
                title: "split".into(),
                start: local_instant(day, h),
                end: local_instant(day, h) + chrono::Duration::minutes(30),
                source: BlockSource::Manual,
            });
        }

        // Two sittings for one estimate is ambiguous — resizing both to the
        // full hour would double the planned time. Leave them.
        assert!(state.resize_blocks_to_estimate("td_1").is_empty());
        for b in &state.blocks {
            assert_eq!(b.end, b.start + chrono::Duration::minutes(30));
        }
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
