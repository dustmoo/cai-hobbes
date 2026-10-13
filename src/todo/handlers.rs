//! AI tool handlers for the planner.
//!
//! Free functions over `&mut PlannerState` returning `(ToolCallStatus, String)`
//! — the convention of `SessionState::handle_set_timer`. Responses are compact
//! line-oriented text (`Todo::summary`, `Capacity::summary`), never JSON dumps:
//! tool results are re-fed into the next prompt, so every line costs tokens on
//! every later turn.
//!
//! `today` is always passed in (the dispatcher computes
//! `chrono::Local::now().date_naive()` — planner days are user-local) so tests
//! can pin the calendar. `persist` gates the write-through to `todo::store`:
//! the dispatcher passes `true`, tests pass `false` to stay off the database.
//! Store writes are small single-row blocking rusqlite calls behind the shared
//! session-store mutex; per P-010 they are never made while holding an MCP lock.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde_json::Value;

use crate::components::shared::ToolCallStatus;

use super::model::{
    self, BlockSource, CalendarEvent, ChecklistItem, DayPlan, HumanKind, Lane,
    Priority, Project, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoOrigin, TodoStatus, SORT_STEP,
};
use super::views::{self, TodoView};
use super::{store, PlannerState};

/// Longest response any list-shaped tool returns, in lines. Anything past the
/// cap is summarised as "…and N more" — the model can filter if it needs more.
const MAX_LIST_LINES: usize = 50;

// ── Argument parsing ────────────────────────────────────────────────────────

const DATE_FORMAT_HINT: &str = "expected YYYY-MM-DD (e.g. 2026-08-12), 'today', or 'tomorrow'";

fn parse_date(raw: &str, today: NaiveDate) -> Result<NaiveDate, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "today" => Ok(today),
        "tomorrow" => Ok(today + chrono::Duration::days(1)),
        trimmed => trimmed
            .parse()
            .map_err(|_| format!("invalid date '{}' — {}", raw, DATE_FORMAT_HINT)),
    }
}

fn parse_bucket(raw: &str) -> Result<TodoBucket, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "inbox" => Ok(TodoBucket::Inbox),
        "anytime" => Ok(TodoBucket::Anytime),
        "someday" => Ok(TodoBucket::Someday),
        _ => Err(format!(
            "invalid bucket '{}' — expected inbox, anytime, or someday",
            raw
        )),
    }
}

fn parse_time_of_day(raw: &str) -> Result<TimeOfDay, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "morning" => Ok(TimeOfDay::Morning),
        "afternoon" => Ok(TimeOfDay::Afternoon),
        "evening" => Ok(TimeOfDay::Evening),
        _ => Err(format!(
            "invalid time_of_day '{}' — expected morning, afternoon, or evening",
            raw
        )),
    }
}

fn parse_priority(raw: &str) -> Result<Priority, String> {
    Priority::parse(raw).ok_or_else(|| {
        format!(
            "invalid priority '{}' — expected urgent_important, important, urgent, or neither",
            raw
        )
    })
}

fn parse_lane(raw: &str) -> Result<Lane, String> {
    Lane::parse(raw).ok_or_else(|| format!("invalid lane '{}' — expected human or agent", raw))
}

fn parse_kind(raw: &str) -> Result<HumanKind, String> {
    HumanKind::parse(raw).ok_or_else(|| {
        format!("invalid kind '{}' — expected commitment, chore, or your_time", raw)
    })
}

fn trimmed_nonempty(v: &Value) -> Result<String, String> {
    let s = as_str_value(v)?.trim();
    if s.is_empty() {
        Err("expected a non-empty string (use null to clear)".to_string())
    } else {
        Ok(s.to_string())
    }
}

fn parse_status(raw: &str) -> Result<TodoStatus, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(TodoStatus::Open),
        "in_progress" | "in-progress" | "started" => Ok(TodoStatus::InProgress),
        "completed" => Ok(TodoStatus::Completed),
        "cancelled" | "archived" => Ok(TodoStatus::Cancelled),
        _ => Err(format!(
            "invalid status '{}' — expected open, in_progress, completed, or cancelled",
            raw
        )),
    }
}

fn parse_hhmm(raw: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(raw.trim(), "%H:%M")
        .map_err(|_| format!("invalid time '{}' — expected local 'HH:MM' (e.g. 09:30)", raw))
}

/// A patch for an optional field: absent leaves it alone, explicit JSON null
/// clears it, a value sets it. This is what lets one UPDATE schema express
/// "unschedule this todo" without a dedicated tool.
enum Patch<T> {
    Absent,
    Clear,
    Set(T),
}

impl<T> Patch<T> {
    fn apply(self, slot: &mut Option<T>) {
        match self {
            Patch::Absent => {}
            Patch::Clear => *slot = None,
            Patch::Set(v) => *slot = Some(v),
        }
    }
}

fn patch_from<T>(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    parse: impl Fn(&Value) -> Result<T, String>,
) -> Result<Patch<T>, String> {
    match obj.get(key) {
        None => Ok(Patch::Absent),
        Some(Value::Null) => Ok(Patch::Clear),
        Some(v) => parse(v).map(Patch::Set),
    }
}

fn as_str_value(v: &Value) -> Result<&str, String> {
    v.as_str().ok_or_else(|| "expected a string".to_string())
}

fn parse_estimate(v: &Value) -> Result<u32, String> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| "invalid estimate_minutes — expected a non-negative integer".to_string())
}

/// Convert a local wall-clock date + time to UTC for storage, via the
/// planner's single DST policy (`ics::local_naive_to_utc`): ambiguous times
/// take the earlier instant, spring-forward gaps slide an hour forward — the
/// same instant the timeline UI and calendar sync would produce.
fn local_to_utc(date: NaiveDate, time: NaiveTime) -> Result<DateTime<Utc>, String> {
    super::ics::local_naive_to_utc(&chrono::Local, &date.and_time(time)).ok_or_else(|| {
        format!("local time {} {} could not be resolved to an instant", date, time)
    })
}

// ── Persistence helpers ─────────────────────────────────────────────────────

// Store failures are logged rather than surfaced: the in-memory state already
// holds the mutation, so the turn keeps working and the miss only matters
// across a restart. Single-row local SQLite writes failing is exceptional.
fn persist_todo(todo: &Todo, persist: bool) {
    if persist {
        if let Err(e) = store::save_todo(todo) {
            tracing::error!("Failed to persist todo {}: {}", todo.id, e);
        }
    }
}

fn persist_block(block: &TimeBlock, persist: bool) {
    if persist {
        if let Err(e) = store::save_block(block) {
            tracing::error!("Failed to persist time block {}: {}", block.id, e);
        }
    }
}

/// Drain and (when `persist`) write through the focus-session rows the last
/// operation touched. Always drains — tests with `persist: false` must not
/// leave dirty ids accumulating across calls.
fn persist_focus_sessions(state: &mut PlannerState, persist: bool) {
    for s in state.take_dirty_focus_sessions() {
        if persist {
            if let Err(e) = store::save_focus_session(&s) {
                tracing::error!("Failed to persist focus session {}: {}", s.id, e);
            }
        }
    }
}

// ── HOBBES_TODO_CREATE ──────────────────────────────────────────────────────

pub fn handle_todo_create(
    state: &mut PlannerState,
    args: &Value,
    session_id: &str,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let Some(items) = args.get("todos").and_then(|v| v.as_array()) else {
        return (
            ToolCallStatus::Error,
            "Provide 'todos': an array of todo objects, each with at least a 'title'."
                .to_string(),
        );
    };
    if items.is_empty() {
        return (
            ToolCallStatus::Error,
            "'todos' is empty — nothing to create.".to_string(),
        );
    }

    let mut created: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        match build_todo(state, item, session_id, today) {
            Ok(todo) => {
                persist_todo(&todo, persist);
                created.push(todo.summary());
                state.upsert_todo(todo);
            }
            Err(e) => errors.push(format!("todos[{}]: {}", i, e)),
        }
    }

    compose_batch_response("Created", &created, &errors)
}

fn build_todo(
    state: &PlannerState,
    item: &Value,
    session_id: &str,
    today: NaiveDate,
) -> Result<Todo, String> {
    let obj = item.as_object().ok_or("expected an object")?;
    refuse_prep_flag(obj)?;
    let title = obj
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("missing required 'title'")?;

    let mut todo = Todo::new(title, state.next_sort_order());
    todo.origin = TodoOrigin::Ai {
        session_id: session_id.to_string(),
    };

    if let Some(notes) = obj.get("notes").and_then(|v| v.as_str()) {
        todo.notes = notes.to_string();
    }
    if let Some(bucket) = obj.get("bucket").and_then(|v| v.as_str()) {
        todo.bucket = parse_bucket(bucket)?;
    }
    if let Some(raw) = obj.get("scheduled_for").and_then(|v| v.as_str()) {
        todo.scheduled_for = Some(parse_date(raw, today)?);
    }
    if let Some(raw) = obj.get("time_of_day").and_then(|v| v.as_str()) {
        todo.time_of_day = Some(parse_time_of_day(raw)?);
    }
    if let Some(raw) = obj.get("deadline").and_then(|v| v.as_str()) {
        todo.deadline = Some(parse_date(raw, today)?);
    }
    if let Some(v) = obj.get("estimate_minutes").filter(|v| !v.is_null()) {
        todo.estimate_minutes = Some(parse_estimate(v)?);
    }
    if let Some(raw) = obj.get("priority").and_then(|v| v.as_str()) {
        todo.priority = Some(parse_priority(raw)?);
    }
    if let Some(project_id) = obj.get("project_id").and_then(|v| v.as_str()) {
        if !state.projects.iter().any(|p| p.id == project_id) {
            return Err(format!("unknown project_id '{}' — HOBBES_PROJECT_LIST shows the ids", project_id));
        }
        todo.project_id = Some(project_id.to_string());
    }
    // Pre-resolved to a session id by the dispatch layer.
    if let Some(link) = obj.get("linked_session").and_then(|v| v.as_str()) {
        todo.linked_fleet_session = Some(link.to_string());
    }
    if let Some(tags) = obj.get("tags").and_then(|v| v.as_array()) {
        todo.tags = tags
            .iter()
            .filter_map(|t| t.as_str())
            .map(str::to_string)
            .collect();
    }
    if let Some(steps) = obj.get("checklist").and_then(|v| v.as_array()) {
        todo.checklist = steps
            .iter()
            .filter_map(|s| s.as_str())
            .map(new_checklist_item)
            .collect();
    }
    // Lanes. Human is the default; the agent lane is only ever the user's
    // call (the tool description tells the model to suggest, not choose).
    let lane = match obj.get("lane").and_then(|v| v.as_str()) {
        Some(raw) => parse_lane(raw)?,
        None => Lane::Human,
    };
    let kind = match obj.get("kind").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_kind(raw)?),
        None => None,
    };
    if lane == Lane::Agent && kind.is_some() {
        return Err("'kind' applies to human to-dos only — omit it for lane 'agent'".to_string());
    }
    todo.kind = kind;
    if let Some(c) = obj.get("time_category").and_then(|v| v.as_str()).map(str::trim) {
        if !c.is_empty() {
            todo.time_category = Some(c.to_string());
        }
    }
    if let Some(o) = obj.get("outcome").and_then(|v| v.as_str()).map(str::trim) {
        if !o.is_empty() {
            todo.outcome = Some(o.to_string());
        }
    }
    todo.set_lane(lane);
    Ok(todo)
}

fn new_checklist_item(title: &str) -> ChecklistItem {
    ChecklistItem {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        done: false,
    }
}

/// "Prep this" is the person's flag: the model's to-do tools refuse it.
fn refuse_prep_flag(obj: &serde_json::Map<String, Value>) -> Result<(), String> {
    if obj.contains_key("prep_requested") || obj.contains_key("prep") {
        return Err("'prep_requested' (\"Prep this\") is the user's own flag: only they set it, \
                    in the Planner. Leave it out."
            .into());
    }
    Ok(())
}

// ── HOBBES_TODO_UPDATE ──────────────────────────────────────────────────────

pub fn handle_todo_update(
    state: &mut PlannerState,
    args: &Value,
    session_id: &str,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let Some(items) = args.get("updates").and_then(|v| v.as_array()) else {
        return (
            ToolCallStatus::Error,
            "Provide 'updates': an array of patch objects, each with at least an 'id'."
                .to_string(),
        );
    };
    if items.is_empty() {
        return (
            ToolCallStatus::Error,
            "'updates' is empty — nothing to change.".to_string(),
        );
    }

    let now = Utc::now();
    let mut updated: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let mut pruned_blocks = 0usize;
    let mut resized_blocks = 0usize;
    for (i, item) in items.iter().enumerate() {
        match apply_todo_update(state, item, today, now, session_id) {
            Ok((summary, status_intent)) => {
                let mut summary = summary;
                // The borrow of the patched todo has ended; re-fetch by id for
                // the store write.
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    match status_intent {
                        Some(TodoStatus::InProgress) => {
                            let scheduled_before = state.todo(id).and_then(|t| t.scheduled_for);
                            // Single-focus: starting this one pauses any other.
                            // Focus also implies today (see start_focus), so the
                            // pruned off-day blocks need their store rows deleted.
                            // The AI drove this, so the session is agent-actor,
                            // attributed to the chat session — the same
                            // provenance source as TodoOrigin::Ai.
                            let (changed, pruned) = state.start_focus(
                                id,
                                now,
                                model::FocusActor::Agent {
                                    session_id: Some(session_id.to_string()),
                                },
                            );
                            for changed in changed {
                                if let Some(t) = state.todo(&changed) {
                                    persist_todo(t, persist);
                                }
                            }
                            if persist {
                                for b in &pruned {
                                    if let Err(e) = store::delete_block(&b.id) {
                                        tracing::error!(
                                            "Failed to delete stale block {}: {}",
                                            b.id,
                                            e
                                        );
                                    }
                                }
                            }
                            pruned_blocks += pruned.len();
                            // Re-summarise so the response shows ▶, not the
                            // pre-focus state — and say so when focus pulled the
                            // todo onto today, so the model knows the date moved.
                            if let Some(t) = state.todo(id) {
                                summary = t.summary();
                                if t.scheduled_for != scheduled_before {
                                    if let Some(d) = t.scheduled_for {
                                        summary.push_str(&format!(
                                            "\nScheduled '{}' onto {}.",
                                            t.title, d
                                        ));
                                    }
                                }
                            }
                        }
                        // The closing transitions run at the PlannerState
                        // layer so the open focus-session row (if this todo
                        // holds one) closes with the matching end_reason.
                        Some(TodoStatus::Completed) => {
                            state.complete_todo(id, now);
                            if let Some(t) = state.todo(id) {
                                summary = t.summary();
                            }
                        }
                        Some(TodoStatus::Open) => {
                            state.reopen_todo(id, now);
                            if let Some(t) = state.todo(id) {
                                summary = t.summary();
                            }
                        }
                        Some(TodoStatus::Cancelled) => {
                            state.cancel_todo(id, now);
                            if let Some(t) = state.todo(id) {
                                summary = t.summary();
                            }
                        }
                        None => {}
                    }
                    if let Some(todo) = state.todo(id) {
                        persist_todo(todo, persist);
                    }
                    // Rescheduling moves the work's timebox with it.
                    if item.get("scheduled_for").is_some() {
                        pruned_blocks += prune_rescheduled_blocks(state, id, persist);
                    }
                    // Re-estimating resizes it from its anchored start.
                    if item.get("estimate_minutes").is_some() {
                        resized_blocks += resize_blocks_after_estimate(state, id, persist);
                    }
                }
                updated.push(summary);
            }
            Err(e) => errors.push(format!("updates[{}]: {}", i, e)),
        }
    }

    // Session rows opened/closed by the status transitions above.
    persist_focus_sessions(state, persist);

    let (status, mut out) = compose_batch_response("Updated", &updated, &errors);
    if pruned_blocks > 0 {
        out.push_str(&format!(
            "\nRemoved {} timeline block(s) that no longer matched the schedule.",
            pruned_blocks
        ));
    }
    if resized_blocks > 0 {
        out.push_str(&format!(
            "\nResized {} timeline block(s) to the new estimate.",
            resized_blocks
        ));
    }
    (status, out)
}

/// Returns the patched todo's summary plus the requested status transition,
/// if any. Status changes are whole-state operations (focus pauses the
/// previous focus; every closing transition must also close the open
/// focus-session row), so they can't run inside this single-todo borrow —
/// the caller applies them through the `PlannerState`-level methods once the
/// borrow has ended and re-summarises.
fn apply_todo_update(
    state: &mut PlannerState,
    item: &Value,
    today: NaiveDate,
    now: DateTime<Utc>,
    session_id: &str,
) -> Result<(String, Option<TodoStatus>), String> {
    let obj = item.as_object().ok_or("expected an object")?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing required 'id'")?;

    refuse_prep_flag(obj)?;
    // Parse every patch before touching the todo so a bad field can't leave a
    // half-applied update behind.
    let scheduled = patch_from(obj, "scheduled_for", |v| {
        as_str_value(v).and_then(|s| parse_date(s, today))
    })?;
    let deadline = patch_from(obj, "deadline", |v| {
        as_str_value(v).and_then(|s| parse_date(s, today))
    })?;
    let time_of_day = patch_from(obj, "time_of_day", |v| {
        as_str_value(v).and_then(parse_time_of_day)
    })?;
    let estimate = patch_from(obj, "estimate_minutes", parse_estimate)?;
    let priority = patch_from(obj, "priority", |v| as_str_value(v).and_then(parse_priority))?;
    let project_id = patch_from(obj, "project_id", |v| {
        as_str_value(v).map(str::to_string)
    })?;
    if let Patch::Set(ref pid) = project_id {
        if !state.projects.iter().any(|p| p.id == *pid) {
            return Err(format!("unknown project_id '{}' — HOBBES_PROJECT_LIST shows the ids", pid));
        }
    }
    // Pre-resolved to a session id by the dispatch layer (fleet lookup);
    // explicit null clears the link.
    let linked_session = patch_from(obj, "linked_session", |v| {
        as_str_value(v).map(str::to_string)
    })?;
    let bucket = match obj.get("bucket").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_bucket(raw)?),
        None => None,
    };
    let status = match obj.get("status").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_status(raw)?),
        None => None,
    };
    let lane = match obj.get("lane").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_lane(raw)?),
        None => None,
    };
    let kind = patch_from(obj, "kind", |v| as_str_value(v).and_then(parse_kind))?;
    let outcome = patch_from(obj, "outcome", trimmed_nonempty)?;
    let time_category = patch_from(obj, "time_category", trimmed_nonempty)?;

    let todo = state
        .todo_mut(id)
        .ok_or_else(|| format!("unknown id '{}'", id))?;
    let lane_after = lane.unwrap_or(todo.lane);
    if lane_after == Lane::Agent && matches!(kind, Patch::Set(_)) {
        return Err("'kind' applies to human to-dos only — this one is in the agent lane".to_string());
    }

    if let Some(title) = obj.get("title").and_then(|v| v.as_str()) {
        let title = title.trim();
        if title.is_empty() {
            return Err("'title' cannot be empty".to_string());
        }
        todo.title = title.to_string();
    }
    if let Some(notes) = obj.get("notes").and_then(|v| v.as_str()) {
        todo.notes = notes.to_string();
    }
    if let Some(bucket) = bucket {
        todo.bucket = bucket;
    }
    scheduled.apply(&mut todo.scheduled_for);
    deadline.apply(&mut todo.deadline);
    time_of_day.apply(&mut todo.time_of_day);
    estimate.apply(&mut todo.estimate_minutes);
    priority.apply(&mut todo.priority);
    project_id.apply(&mut todo.project_id);
    linked_session.apply(&mut todo.linked_fleet_session);
    if let Some(lane) = lane {
        todo.set_lane(lane);
    }
    kind.apply(&mut todo.kind);
    outcome.apply(&mut todo.outcome);
    time_category.apply(&mut todo.time_category);
    // Auto-link: agent focus starting on an unlinked todo binds it to the
    // driving chat session (never overwrites an existing link).
    if status == Some(TodoStatus::InProgress)
        && todo.linked_fleet_session.is_none()
        && !session_id.is_empty()
    {
        todo.linked_fleet_session = Some(session_id.to_string());
    }
    if let Some(tags) = obj.get("tags").and_then(|v| v.as_array()) {
        todo.tags = tags
            .iter()
            .filter_map(|t| t.as_str())
            .map(str::to_string)
            .collect();
    }
    if let Some(steps) = obj.get("checklist").and_then(|v| v.as_array()) {
        todo.checklist = steps
            .iter()
            .filter_map(|s| s.as_str())
            .map(new_checklist_item)
            .collect();
    }
    todo.updated_at = now;
    Ok((todo.summary(), status))
}

/// The inverse rule: placing a linked block on a day schedules the todo there
/// (see `PlannerState::schedule_todo_on`). Persists the todo and deletes any
/// pruned stale blocks; appends what happened to the response so the model
/// knows the schedule moved.
fn sync_schedule_to_block(
    state: &mut PlannerState,
    todo_id: &str,
    date: NaiveDate,
    persist: bool,
    out: &mut String,
) {
    let (changed, pruned) = state.schedule_todo_on(todo_id, date, Utc::now());
    if changed {
        if let Some(todo) = state.todo(todo_id) {
            persist_todo(todo, persist);
            out.push_str(&format!("\nScheduled '{}' onto {}.", todo.title, date));
        }
    }
    if !pruned.is_empty() {
        if persist {
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("Failed to delete stale block {}: {}", b.id, e);
                }
            }
        }
        out.push_str(&format!(
            "\nRemoved {} of its timeline block(s) left on other days.",
            pruned.len()
        ));
    }
}

/// Estimate → timebox: after a re-estimate, resize the todo's (single) block
/// on its scheduled day from the anchored start, persisting the change.
/// Returns how many blocks were resized.
fn resize_blocks_after_estimate(state: &mut PlannerState, todo_id: &str, persist: bool) -> usize {
    let changed = state.resize_blocks_to_estimate(todo_id);
    if persist {
        for b in &changed {
            persist_block(b, true);
        }
    }
    changed.len()
}

/// The timebox follows the schedule: after a todo's `scheduled_for` changes,
/// drop its blocks on days that no longer match, everywhere. Returns how many
/// were removed so responses can say so.
fn prune_rescheduled_blocks(state: &mut PlannerState, todo_id: &str, persist: bool) -> usize {
    let keep = state.todo(todo_id).and_then(|t| t.scheduled_for);
    let removed = state.prune_blocks_for_todo(todo_id, keep);
    if persist {
        for b in &removed {
            if let Err(e) = store::delete_block(&b.id) {
                tracing::error!("Failed to delete rescheduled block {}: {}", b.id, e);
            }
        }
    }
    removed.len()
}

/// Shared partial-success shape for the batch tools: valid items are applied
/// even when others fail, the response says so, and any failure makes the call
/// an Error so the model re-examines its input.
fn compose_batch_response(
    verb: &str,
    applied: &[String],
    errors: &[String],
) -> (ToolCallStatus, String) {
    if errors.is_empty() {
        return (
            ToolCallStatus::Completed,
            format!("{} {} todo(s):\n{}", verb, applied.len(), applied.join("\n")),
        );
    }
    if applied.is_empty() {
        return (ToolCallStatus::Error, errors.join("\n"));
    }
    (
        ToolCallStatus::Error,
        format!(
            "{} {} todo(s) (the valid items were applied):\n{}\nFailed:\n{}",
            verb,
            applied.len(),
            applied.join("\n"),
            errors.join("\n")
        ),
    )
}

// ── HOBBES_TODO_LIST ────────────────────────────────────────────────────────

pub fn handle_todo_list(
    state: &PlannerState,
    args: &Value,
    today: NaiveDate,
) -> (ToolCallStatus, String) {
    let project_id = args.get("project_id").and_then(|v| v.as_str());
    let tag = args.get("tag").and_then(|v| v.as_str());
    let text = args.get("text").and_then(|v| v.as_str());
    let date_from_raw = args.get("date_from").and_then(|v| v.as_str());
    let date_to_raw = args.get("date_to").and_then(|v| v.as_str());

    let has_filters = project_id.is_some()
        || tag.is_some()
        || text.is_some()
        || date_from_raw.is_some()
        || date_to_raw.is_some();

    let (header, selected): (String, Vec<&Todo>) = if has_filters {
        let date_from = match date_from_raw.map(|s| parse_date(s, today)).transpose() {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date_from: {}", e)),
        };
        let date_to = match date_to_raw.map(|s| parse_date(s, today)).transpose() {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date_to: {}", e)),
        };
        let needle = text.map(|t| t.to_lowercase());

        let mut matches: Vec<&Todo> = state
            .todos
            .iter()
            .filter(|t| project_id.is_none_or(|p| t.project_id.as_deref() == Some(p)))
            .filter(|t| tag.is_none_or(|tag| t.tags.iter().any(|x| x == tag)))
            .filter(|t| {
                needle.as_ref().is_none_or(|n| {
                    t.title.to_lowercase().contains(n) || t.notes.to_lowercase().contains(n)
                })
            })
            .filter(|t| {
                // A date-range filter only makes sense for scheduled todos.
                if date_from.is_none() && date_to.is_none() {
                    return true;
                }
                let Some(d) = t.scheduled_for else { return false };
                date_from.is_none_or(|f| d >= f) && date_to.is_none_or(|u| d <= u)
            })
            .collect();
        matches.sort_by(|a, b| {
            a.sort_order
                .partial_cmp(&b.sort_order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (format!("{} todo(s) matching filters", matches.len()), matches)
    } else {
        let view = match args.get("view").and_then(|v| v.as_str()) {
            Some(raw) => match TodoView::parse(raw) {
                Some(v) => v,
                None => {
                    return (
                        ToolCallStatus::Error,
                        format!(
                            "Unknown view '{}' — expected today, inbox, upcoming, anytime, someday, or logbook.",
                            raw
                        ),
                    )
                }
            },
            None => TodoView::Today,
        };
        // Upcoming flattens the same grouped query the UI's day headers use —
        // date ascending, then sort order — so the two surfaces can't drift.
        let matches: Vec<&Todo> = if view == TodoView::Upcoming {
            views::upcoming_grouped(&state.todos, today)
                .into_iter()
                .flat_map(|(_, ts)| ts)
                .collect()
        } else {
            views::in_view(&state.todos, view, today)
        };
        (
            format!("{} — {} todo(s)", view.as_str(), matches.len()),
            matches,
        )
    };

    if selected.is_empty() {
        return (ToolCallStatus::Completed, format!("{}.", header));
    }

    let mut lines: Vec<String> = selected
        .iter()
        .take(MAX_LIST_LINES)
        .map(|t| t.summary())
        .collect();
    if selected.len() > MAX_LIST_LINES {
        lines.push(format!("…and {} more", selected.len() - MAX_LIST_LINES));
    }
    (
        ToolCallStatus::Completed,
        format!("{}:\n{}", header, lines.join("\n")),
    )
}

// ── HOBBES_PLAN_DAY ─────────────────────────────────────────────────────────

pub fn handle_plan_day(
    state: &mut PlannerState,
    args: &Value,
    default_capacity: u32,
    meetings_count_against_capacity: bool,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let date = match args.get("date").and_then(|v| v.as_str()) {
        Some(raw) => match parse_date(raw, today) {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date: {}", e)),
        },
        None => today,
    };
    let Some(items) = args.get("items").and_then(|v| v.as_array()) else {
        return (
            ToolCallStatus::Error,
            "Provide 'items': an array of {id, estimate_minutes?, time_of_day?} in the order the day should run.".to_string(),
        );
    };

    let capacity_override = match args.get("capacity_minutes").filter(|v| !v.is_null()) {
        Some(v) => match parse_estimate(v) {
            Ok(m) => Some(m),
            Err(_) => {
                return (
                    ToolCallStatus::Error,
                    "invalid capacity_minutes — expected a non-negative integer".to_string(),
                )
            }
        },
        None => None,
    };
    let capacity = capacity_override.unwrap_or_else(|| state.capacity_for(date, default_capacity));

    let now = Utc::now();
    let mut scheduled: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut pruned_blocks = 0usize;
    let mut skipped_agent: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            errors.push(format!("items[{}]: missing required 'id'", i));
            continue;
        };
        let estimate = match item.get("estimate_minutes").filter(|v| !v.is_null()) {
            Some(v) => match parse_estimate(v) {
                Ok(m) => Some(m),
                Err(e) => {
                    errors.push(format!("items[{}]: {}", i, e));
                    continue;
                }
            },
            None => None,
        };
        let time_of_day = match item.get("time_of_day").and_then(|v| v.as_str()) {
            Some(raw) => match parse_time_of_day(raw) {
                Ok(t) => Some(t),
                Err(e) => {
                    errors.push(format!("items[{}]: {}", i, e));
                    continue;
                }
            },
            None => None,
        };

        // The agent lane is not the person's day: never put it on the
        // human timeline. It is reported in its own section below.
        if state.todo(id).is_some_and(|t| t.lane == Lane::Agent) {
            skipped_agent.push(id.to_string());
            continue;
        }
        // Re-key onto the end of the ordering so the day runs in the order the
        // items were given.
        let sort_order = state.next_sort_order();
        let Some(todo) = state.todo_mut(id) else {
            errors.push(format!("items[{}]: unknown id '{}'", i, id));
            continue;
        };
        todo.scheduled_for = Some(date);
        if let Some(m) = estimate {
            todo.estimate_minutes = Some(m);
        }
        if let Some(t) = time_of_day {
            todo.time_of_day = Some(t);
        }
        todo.sort_order = sort_order;
        todo.updated_at = now;
        scheduled.push(todo.summary());
        let snapshot = todo.clone();
        persist_todo(&snapshot, persist);
        // Planning a todo onto `date` moves its timebox with it; an estimate
        // override resizes whatever block survives.
        pruned_blocks += prune_rescheduled_blocks(state, id, persist);
        if estimate.is_some() {
            resize_blocks_after_estimate(state, id, persist);
        }
    }

    // Upsert the DayPlan: planning is an event worth recording even when the
    // capacity is unchanged — planned_at is what the morning ritual checks.
    let plan = match state.day_plans.iter_mut().find(|p| p.date == date) {
        Some(plan) => {
            plan.capacity_minutes = capacity;
            plan.planned_at = Some(now);
            plan.clone()
        }
        None => {
            let mut plan = DayPlan::new(date, capacity);
            plan.planned_at = Some(now);
            state.day_plans.push(plan.clone());
            plan
        }
    };
    if persist {
        if let Err(e) = store::save_day_plan(&plan) {
            tracing::error!("Failed to persist day plan {}: {}", plan.date, e);
        }
    }

    // Chores batch into one or two Chores blocks (when slots are given).
    let chores_note = match batch_chores(state, args, date, persist) {
        Ok(note) => note,
        Err(e) => {
            errors.push(format!("chores_slots: {}", e));
            None
        }
    };

    let cap = model::measure_capacity_full(
        &state.todos,
        &state.blocks,
        &state.focus_sessions,
        date,
        capacity,
        meetings_count_against_capacity,
    );
    let mut out = String::new();
    // Your time leads: the planner's objective is to protect and grow it
    // (SYMBIOTIC_PLANNER.md §2). Only when there is something to say, so a
    // planner without goals or protected blocks reads exactly as before.
    if let Some(line) = your_time_status_line(state, date) {
        out.push_str(&line);
        out.push('\n');
    }
    // The product opinion: overcommitment leads the work lines, never a
    // footnote.
    if cap.is_overcommitted() {
        out.push_str(&format!(
            "OVERCOMMITTED: {}. Tell the user, and suggest moving something to another day.\n",
            cap.summary()
        ));
    } else {
        out.push_str(&format!("{}.\n", cap.summary()));
    }
    out.push_str(&format!(
        "Planned {} for {}:\n{}",
        scheduled.len(),
        date,
        scheduled.join("\n")
    ));
    if pruned_blocks > 0 {
        out.push_str(&format!(
            "\nRemoved {} timeline block(s) left on other days.",
            pruned_blocks
        ));
    }
    for w in protected_overlap_warnings(state, date) {
        out.push('\n');
        out.push_str(&w);
    }
    if let Some(note) = chores_note {
        out.push('\n');
        out.push_str(&note);
    }
    if !skipped_agent.is_empty() {
        out.push_str(&format!(
            "\nNot placed on the day (agent lane — it runs async, not on your time): {}",
            skipped_agent.join(", ")
        ));
    }
    if let Some(section) = agent_lane_section(state, today) {
        out.push('\n');
        out.push_str(&section);
    }
    if errors.is_empty() {
        (ToolCallStatus::Completed, out)
    } else {
        out.push_str(&format!(
            "\nFailed (the valid items were still scheduled):\n{}",
            errors.join("\n")
        ));
        (ToolCallStatus::Error, out)
    }
}

/// Cap on each agent-lane bucket listed in tool output.
const AGENT_LANE_MAX_LINES: usize = 5;

fn hhmm(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// `"Your time: 2h protected today (Reading 19:00–20:00, …); this week
/// Reading 2h/4h · Outside 30m/4h."` — or `None` when there are no goals and
/// nothing protected today.
fn your_time_status_line(state: &PlannerState, date: NaiveDate) -> Option<String> {
    let (protected, blocks) = super::measures::protected_on(&state.blocks, date);
    let week = super::measures::your_time_week(
        &state.time_goals,
        &state.blocks,
        &state.todos,
        &state.focus_sessions,
        date,
    );
    if state.time_goals.is_empty() && protected == 0 {
        return None;
    }
    let mut line = if protected > 0 {
        let list: Vec<String> = blocks
            .iter()
            .take(4)
            .map(|b| {
                format!(
                    "{} {}–{}",
                    b.your_time_category().unwrap_or("your time"),
                    hhmm(b.start),
                    hhmm(b.end)
                )
            })
            .collect();
        format!(
            "Your time: {} protected on {} ({}) — plan around it, never over it",
            model::format_minutes(protected),
            date,
            list.join(", ")
        )
    } else {
        format!("Your time: nothing protected on {}", date)
    };
    if !week.is_empty() {
        line.push_str(&format!("; this week {}", week.compact(1)));
    }
    line.push('.');
    Some(line)
}

/// Warnings for Hobbes-owned work blocks on `date` that sit over protected
/// your-time. The protected block wins: the work moves, or the person is
/// asked.
fn protected_overlap_warnings(state: &PlannerState, date: NaiveDate) -> Vec<String> {
    let on_day = state.blocks_on(date);
    let protected: Vec<&&TimeBlock> = on_day.iter().filter(|b| b.is_protected()).collect();
    on_day
        .iter()
        .filter(|b| matches!(b.source, BlockSource::Manual | BlockSource::Auto | BlockSource::Chores { .. }))
        .filter_map(|b| {
            let p = protected.iter().find(|p| p.overlaps(b))?;
            Some(format!(
                "Warning — '{}' {}–{} is over protected your-time ({} {}–{}). Move the work,                  not the protected block, or ask the user.",
                state.block_display_title(b),
                hhmm(b.start),
                hhmm(b.end),
                p.your_time_category().unwrap_or("your time"),
                hhmm(p.start),
                hhmm(p.end)
            ))
        })
        .collect()
}

/// The agent lane as a separate section: to dispatch, working, needs you,
/// stuck. `None` when the lane is empty.
fn agent_lane_section(state: &PlannerState, today: NaiveDate) -> Option<String> {
    let lane = super::lanes::agent_lane(&state.todos, today);
    if lane.is_empty() {
        return None;
    }
    let mut out = format!("Agent lane ({}):", lane.counts_line());
    let list = |label: &str, todos: &[&Todo], out: &mut String| {
        if todos.is_empty() {
            return;
        }
        let mut lines: Vec<String> = todos
            .iter()
            .take(AGENT_LANE_MAX_LINES)
            .map(|t| {
                let mut l = format!("[{}] {}", t.id, truncate_title(&t.title));
                if let Some(p) = t.latest_progress.as_deref().filter(|p| !p.is_empty()) {
                    l.push_str(&format!(" — {}", truncate_title(p)));
                }
                l
            })
            .collect();
        if todos.len() > AGENT_LANE_MAX_LINES {
            lines.push(format!("…and {} more", todos.len() - AGENT_LANE_MAX_LINES));
        }
        out.push_str(&format!("\n{}: {}", label, lines.join("; ")));
    };
    list("To dispatch (HOBBES_DISPATCH)", &lane.to_dispatch, &mut out);
    list("Working", &lane.working, &mut out);
    if !lane.needs_you.is_empty() {
        let lines: Vec<String> = lane
            .needs_you
            .iter()
            .take(AGENT_LANE_MAX_LINES)
            .map(|t| {
                super::lanes::decision_line(t)
                    .unwrap_or_else(|| format!("[{}] {} — waiting on the user", t.id, truncate_title(&t.title)))
            })
            .collect();
        out.push_str(&format!("\nNeeds you (HOBBES_DECISIONS): {}", lines.join("; ")));
    }
    list("Stuck", &lane.stuck, &mut out);
    Some(out)
}

/// `chores_slots` on HOBBES_PLAN_DAY: up to two `{start, end}` local slots.
/// The day's open human chores are split across them in order (filling the
/// first slot by estimate, the rest into the second) as `Chores` blocks,
/// replacing any earlier Chores blocks that day. Slots may not overlap
/// protected your-time. Without slots, a note suggests batching when there
/// are 2+ chores and no batch yet.
fn batch_chores(
    state: &mut PlannerState,
    args: &Value,
    date: NaiveDate,
    persist: bool,
) -> Result<Option<String>, String> {
    let chores: Vec<(String, u32)> = state
        .todos
        .iter()
        .filter(|t| {
            t.scheduled_for == Some(date)
                && t.lane == Lane::Human
                && t.kind == Some(HumanKind::Chore)
                && !t.status.is_closed()
        })
        .map(|t| (t.id.clone(), t.estimate_minutes.unwrap_or(5)))
        .collect();
    let slots = match args.get("chores_slots").and_then(|v| v.as_array()) {
        Some(s) if !s.is_empty() => s,
        _ => {
            let batched = state.blocks_on(date).iter().any(|b| matches!(b.source, BlockSource::Chores { .. }));
            if chores.len() >= 2 && !batched {
                let total: u32 = chores.iter().map(|(_, m)| *m).sum();
                return Ok(Some(format!(
                    "Chores: {} on {} (~{}). Batch them into one or two Chores blocks —                      re-run with chores_slots [{{start, end}}].",
                    chores.len(),
                    date,
                    model::format_minutes(total)
                )));
            }
            return Ok(None);
        }
    };
    if slots.len() > 2 {
        return Err("at most two slots — batching is the point".to_string());
    }
    if chores.is_empty() {
        return Ok(Some(format!("No open chores on {} to batch.", date)));
    }
    let mut spans = Vec::new();
    for slot in slots {
        let (start, end) = parse_block_times(slot, date)?;
        let probe = TimeBlock {
            id: String::new(),
            todo_id: None,
            title: String::new(),
            start,
            end,
            source: BlockSource::Chores { todo_ids: vec![] },
        };
        if let Some(p) = state.blocks.iter().find(|b| b.is_protected() && b.overlaps(&probe)) {
            return Err(format!(
                "slot {}–{} overlaps protected your-time ({} {}–{}) — pick another slot",
                hhmm(start),
                hhmm(end),
                p.your_time_category().unwrap_or("your time"),
                hhmm(p.start),
                hhmm(p.end)
            ));
        }
        spans.push((start, end));
    }
    // Replace the day's earlier batch.
    let old: Vec<String> = state
        .blocks_on(date)
        .iter()
        .filter(|b| matches!(b.source, BlockSource::Chores { .. }))
        .map(|b| b.id.clone())
        .collect();
    state.blocks.retain(|b| !old.contains(&b.id));
    if persist {
        for id in &old {
            if let Err(e) = store::delete_block(id) {
                tracing::error!("Failed to delete old chores block {}: {}", id, e);
            }
        }
    }
    let first_cap = (spans[0].1 - spans[0].0).num_minutes().max(0) as u32;
    let mut groups: Vec<Vec<String>> = vec![Vec::new(); spans.len()];
    let mut used = 0u32;
    for (id, mins) in &chores {
        let idx = if spans.len() == 2 && used + mins > first_cap && !groups[0].is_empty() { 1 } else { 0 };
        if idx == 0 {
            used += mins;
        }
        groups[idx].push(id.clone());
    }
    let mut lines = Vec::new();
    for ((start, end), ids) in spans.into_iter().zip(groups) {
        if ids.is_empty() {
            continue;
        }
        let block = TimeBlock {
            id: format!("blk_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            todo_id: None,
            title: format!("Chores · {}", ids.len()),
            start,
            end,
            source: BlockSource::Chores { todo_ids: ids.clone() },
        };
        persist_block(&block, persist);
        lines.push(format!("{}–{} ({} chores)", hhmm(start), hhmm(end), ids.len()));
        state.blocks.push(block);
    }
    Ok(Some(format!("Batched chores: {}.", lines.join(", "))))
}

// ── HOBBES_TIME_BLOCK ───────────────────────────────────────────────────────

pub fn handle_time_block(
    state: &mut PlannerState,
    args: &Value,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    match args.get("action").and_then(|v| v.as_str()) {
        Some("create") => time_block_create(state, args, today, persist),
        Some("move") => time_block_move(state, args, today, persist),
        Some("delete") => time_block_delete(state, args, persist),
        Some("list") => time_block_list(state, args, today),
        _ => (
            ToolCallStatus::Error,
            "Provide 'action': one of list, create, move, delete.".to_string(),
        ),
    }
}

/// Longest range `action: list` covers; larger requests clamp.
const TIME_BLOCK_LIST_MAX_DAYS: i64 = 14;

/// `HOBBES_TIME_BLOCK action=list` — the timeline's blocks with their ids, so
/// the model can move or delete an existing block instead of creating a
/// duplicate. Read-only; takes `&PlannerState` so dispatch can use a read
/// guard. Mirrored meetings are listed too (marked read-only) because they
/// matter when picking a slot.
pub fn time_block_list(
    state: &PlannerState,
    args: &Value,
    today: NaiveDate,
) -> (ToolCallStatus, String) {
    let start = match args.get("date").and_then(|v| v.as_str()) {
        Some(raw) => match parse_date(raw, today) {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date: {}", e)),
        },
        None => today,
    };
    let days = match args.get("days").filter(|v| !v.is_null()) {
        Some(v) => match v.as_i64() {
            Some(n) => n.clamp(1, TIME_BLOCK_LIST_MAX_DAYS),
            None => {
                return (
                    ToolCallStatus::Error,
                    "invalid 'days' — expected an integer".to_string(),
                )
            }
        },
        None => 1,
    };
    let mut lines: Vec<String> = Vec::new();
    for offset in 0..days {
        let day = start + chrono::Duration::days(offset);
        for b in state.blocks_on(day) {
            let tag = match &b.source {
                BlockSource::External { .. } => " — calendar meeting, read-only".to_string(),
                BlockSource::YourTime { category } => {
                    format!(" — protected your-time ({}), move only with user_confirmed", category)
                }
                BlockSource::Chores { .. } => " — chores".to_string(),
                _ => match &b.todo_id {
                    Some(tid) => format!(" — todo {}", tid),
                    None => String::new(),
                },
            };
            lines.push(format!("{}{}", block_line(state, b), tag));
        }
    }
    let range = if days == 1 {
        views::friendly_date(start, today)
    } else {
        format!(
            "{} to {}",
            views::friendly_date(start, today),
            views::friendly_date(start + chrono::Duration::days(days - 1), today)
        )
    };
    if lines.is_empty() {
        return (
            ToolCallStatus::Completed,
            format!("No blocks on the timeline {}.", range),
        );
    }
    let total = lines.len();
    let mut out = format!(
        "Timeline {} — {} block(s). Move or delete by id; don't create a duplicate:",
        range, total
    );
    for line in lines.iter().take(MAX_LIST_LINES) {
        out.push_str("\n  ");
        out.push_str(line);
    }
    if total > MAX_LIST_LINES {
        out.push_str(&format!("\n…and {} more", total - MAX_LIST_LINES));
    }
    (ToolCallStatus::Completed, out)
}

fn parse_block_times(
    args: &Value,
    date: NaiveDate,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let start_raw = args
        .get("start")
        .and_then(|v| v.as_str())
        .ok_or("missing 'start' (local 'HH:MM')")?;
    let end_raw = args
        .get("end")
        .and_then(|v| v.as_str())
        .ok_or("missing 'end' (local 'HH:MM')")?;
    let start_t = parse_hhmm(start_raw)?;
    let end_t = parse_hhmm(end_raw)?;
    if end_t <= start_t {
        return Err(format!(
            "'end' ({}) must be after 'start' ({})",
            end_raw, start_raw
        ));
    }
    Ok((local_to_utc(date, start_t)?, local_to_utc(date, end_t)?))
}

fn block_line(state: &PlannerState, block: &TimeBlock) -> String {
    let local_start = block.start.with_timezone(&chrono::Local);
    let local_end = block.end.with_timezone(&chrono::Local);
    format!(
        "[{}] {}–{} {} ({})",
        block.id,
        local_start.format("%H:%M"),
        local_end.format("%H:%M"),
        state.block_display_title(block),
        local_start.format("%Y-%m-%d"),
    )
}

/// Overlap note against every *other* block, or None when the slot is clear.
/// A warning rather than an error: double-booking is sometimes deliberate, and
/// the honest move is to flag it, not forbid it. Non-busy external blocks
/// (free / focus-time mirrors) don't clash — placing work over them is the
/// point of marking them free.
fn overlap_warning(state: &PlannerState, block: &TimeBlock) -> Option<String> {
    let clashes: Vec<String> = state
        .blocks
        .iter()
        .filter(|b| {
            b.id != block.id
                && b.overlaps(block)
                && !matches!(b.source, BlockSource::External { busy: false, .. })
        })
        .map(|b| block_line(state, b))
        .collect();
    if clashes.is_empty() {
        None
    } else {
        Some(format!("Warning — overlaps: {}", clashes.join("; ")))
    }
}

/// Protected your-time never moves or disappears without the user's say so:
/// the call must carry `user_confirmed: true`, set only after the user
/// explicitly agreed in this conversation.
fn protected_guard(block: &TimeBlock, args: &Value, verb: &str) -> Option<String> {
    if !block.is_protected() || args.get("user_confirmed").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    Some(format!(
        "'{}' is protected your-time ({}). Ask the user before you {} it; retry with \
         user_confirmed: true only once they have agreed.",
        block.title,
        block.your_time_category().unwrap_or("your time"),
        verb
    ))
}

/// Tell the model when it filed time under a category that isn't one of the
/// user's goals: it still counts (tracked, no target), but the goals are
/// what the user asked for.
fn non_goal_note(state: &PlannerState, category: &str) -> Option<String> {
    if state.time_goals.iter().any(|g| g.matches(category)) {
        return None;
    }
    if state.time_goals.is_empty() {
        return Some(format!(
            "Note: the user has no your-time goals yet, so '{}' is tracked without a target.",
            category
        ));
    }
    let goals: Vec<&str> = super::categories::ordered_goals(&state.time_goals)
        .into_iter()
        .map(|g| g.category.as_str())
        .collect();
    Some(format!(
        "Note: '{}' isn't one of the user's goals \u{2014} it's tracked but has no target. Goals: {}. \
         Prefer one of these unless the user asked for '{}'.",
        category,
        goals.join(", "),
        category
    ))
}

/// The cut-off for "in use" categories, from the local day.
fn in_use_since_day(today: NaiveDate) -> DateTime<Utc> {
    super::categories::in_use_since(today.and_hms_opt(0, 0, 0).expect("midnight").and_utc())
}

fn time_block_create(
    state: &mut PlannerState,
    args: &Value,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let date = match args.get("date").and_then(|v| v.as_str()) {
        Some(raw) => match parse_date(raw, today) {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date: {}", e)),
        },
        None => {
            return (
                ToolCallStatus::Error,
                format!("missing 'date' — {}", DATE_FORMAT_HINT),
            )
        }
    };
    let (start, end) = match parse_block_times(args, date) {
        Ok(t) => t,
        Err(e) => return (ToolCallStatus::Error, e),
    };

    let todo_id = args
        .get("todo_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(ref tid) = todo_id {
        if state.todo(tid).is_none() {
            return (
                ToolCallStatus::Error,
                format!("unknown todo_id '{}'", tid),
            );
        }
    }
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            todo_id
                .as_deref()
                .and_then(|tid| state.todo(tid))
                .map(|t| t.title.clone())
        });
    // `kind`: work (default), your_time (protected, needs `category`), or
    // chores (a batch slot).
    let source = match args.get("kind").and_then(|v| v.as_str()).map(|k| k.trim().to_ascii_lowercase()) {
        None => BlockSource::Auto,
        Some(k) if k == "work" || k.is_empty() => BlockSource::Auto,
        Some(k) if k == "your_time" || k == "yourtime" => {
            let Some(category) = args
                .get("category")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|c| !c.is_empty())
            else {
                return (
                    ToolCallStatus::Error,
                    "A your_time block needs a 'category' (e.g. one of the user's time goals).".to_string(),
                );
            };
            // "outside" files under the existing "Outside".
            BlockSource::YourTime {
                category: super::categories::canonical(&state.time_goals, &state.blocks, category),
            }
        }
        Some(k) if k == "chores" => BlockSource::Chores {
            todo_ids: todo_id.iter().cloned().collect(),
        },
        Some(k) => {
            return (
                ToolCallStatus::Error,
                format!("invalid kind '{}' — expected work, your_time, or chores", k),
            )
        }
    };
    let title = match (&source, title) {
        (_, Some(t)) => t,
        (BlockSource::YourTime { category }, None) => category.clone(),
        (BlockSource::Chores { .. }, None) => "Chores".to_string(),
        (_, None) => {
            return (
                ToolCallStatus::Error,
                "Provide 'title' (or a 'todo_id' to take the title from).".to_string(),
            )
        }
    };
    let todo_id = match source {
        BlockSource::YourTime { .. } | BlockSource::Chores { .. } => {
            // Protected time and chore batches are not one to-do's timebox.
            if matches!(source, BlockSource::YourTime { .. }) && todo_id.is_some() {
                return (
                    ToolCallStatus::Error,
                    "A your_time block is protected time, not a to-do's timebox — omit todo_id.".to_string(),
                );
            }
            None
        }
        _ => todo_id,
    };

    let block = TimeBlock {
        id: format!("blk_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        todo_id,
        title,
        start,
        end,
        source,
    };
    // Protected your-time is planned around: work may not be placed over it
    // unless the user explicitly asked for that.
    if !block.is_protected() && !args.get("override_protected").and_then(|v| v.as_bool()).unwrap_or(false) {
        if let Some(p) = state.blocks.iter().find(|b| b.is_protected() && b.overlaps(&block)) {
            return (
                ToolCallStatus::Error,
                format!(
                    "That slot overlaps protected your-time: {}. Pick another slot. Only if the \
                     user explicitly asks to use that time, retry with override_protected: true.",
                    block_line(state, p)
                ),
            );
        }
    }
    let warning = overlap_warning(state, &block);
    let category_note = block.your_time_category().and_then(|c| non_goal_note(state, c));
    persist_block(&block, persist);
    let mut out = format!("Created {}", block_line(state, &block));
    if let Some(n) = category_note {
        out.push('\n');
        out.push_str(&n);
    }
    let linked = block.todo_id.clone();
    state.blocks.push(block);
    // Timeboxing work on a day IS scheduling it there.
    if let Some(tid) = linked {
        sync_schedule_to_block(state, &tid, date, persist, &mut out);
    }
    if let Some(w) = warning {
        out.push('\n');
        out.push_str(&w);
    }
    (ToolCallStatus::Completed, out)
}

fn time_block_move(
    state: &mut PlannerState,
    args: &Value,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return (
            ToolCallStatus::Error,
            "Provide 'id': the block to move.".to_string(),
        );
    };
    let Some(existing) = state.blocks.iter().find(|b| b.id == id).cloned() else {
        return (
            ToolCallStatus::Error,
            format!("unknown block id '{}'", id),
        );
    };
    // Mirrored calendar events are read-only: a "successful" move here would
    // be silently reverted by the next sync pass — refuse honestly instead.
    if matches!(existing.source, BlockSource::External { .. }) {
        return (
            ToolCallStatus::Error,
            format!(
                "'{}' is a calendar event mirrored from the user's external calendar — Hobbes cannot move it. It must be rescheduled in the source calendar (e.g. Google Calendar); sync will pick the change up within ~15 minutes.",
                existing.title
            ),
        );
    }
    if let Some(e) = protected_guard(&existing, args, "move") {
        return (ToolCallStatus::Error, e);
    }

    // Unspecified parts keep the block's current local date/times, so "move it
    // to 14:00" or "push it to tomorrow" are each a single-field call.
    let cur_start = existing.start.with_timezone(&chrono::Local);
    let cur_end = existing.end.with_timezone(&chrono::Local);
    let date = match args.get("date").and_then(|v| v.as_str()) {
        Some(raw) => match parse_date(raw, today) {
            Ok(d) => d,
            Err(e) => return (ToolCallStatus::Error, format!("date: {}", e)),
        },
        None => cur_start.date_naive(),
    };
    let start_t = match args.get("start").and_then(|v| v.as_str()) {
        Some(raw) => match parse_hhmm(raw) {
            Ok(t) => t,
            Err(e) => return (ToolCallStatus::Error, e),
        },
        None => cur_start.time(),
    };
    let end_t = match args.get("end").and_then(|v| v.as_str()) {
        Some(raw) => match parse_hhmm(raw) {
            Ok(t) => t,
            Err(e) => return (ToolCallStatus::Error, e),
        },
        None => cur_end.time(),
    };
    if end_t <= start_t {
        return (
            ToolCallStatus::Error,
            format!("'end' ({}) must be after 'start' ({})", end_t, start_t),
        );
    }
    let (start, end) = match (local_to_utc(date, start_t), local_to_utc(date, end_t)) {
        (Ok(s), Ok(e)) => (s, e),
        (Err(e), _) | (_, Err(e)) => return (ToolCallStatus::Error, e),
    };

    let old_duration = existing.duration_minutes();
    let mut updated = existing;
    updated.start = start;
    updated.end = end;
    // Re-file a your-time block (the protected guard above already asked
    // for user_confirmed). An explicit title below still wins.
    let mut category_note = None;
    if let Some(raw) = args.get("category").and_then(|v| v.as_str()).map(str::trim).filter(|c| !c.is_empty()) {
        if !updated.is_protected() {
            return (
                ToolCallStatus::Error,
                "'category' only applies to your_time blocks.".to_string(),
            );
        }
        let category = super::categories::canonical(&state.time_goals, &state.blocks, raw);
        super::categories::set_block_category(&mut updated, &category);
        category_note = non_goal_note(state, &category);
    }
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        updated.title = title.to_string();
    }

    let warning = overlap_warning(state, &updated);
    persist_block(&updated, persist);
    let mut out = format!("Moved {}", block_line(state, &updated));
    if let Some(n) = category_note {
        out.push('\n');
        out.push_str(&n);
    }
    let new_duration = updated.duration_minutes();
    let linked = updated.todo_id.clone();
    if let Some(slot) = state.blocks.iter_mut().find(|b| b.id == id) {
        *slot = updated;
    }
    // Changing a linked block's length IS re-estimating the work — the same
    // rule the UI's resize drag applies. A pure move leaves the estimate alone.
    if new_duration != old_duration {
        if let Some(tid) = linked.clone() {
            if let Some(todo) = state.todo_mut(&tid) {
                todo.estimate_minutes = Some(new_duration.max(15));
                todo.updated_at = Utc::now();
                let snapshot = todo.clone();
                persist_todo(&snapshot, persist);
                out.push_str(&format!(
                    "\nEstimate updated to {}.",
                    model::format_minutes(new_duration.max(15))
                ));
            }
        }
    }
    // Moving a linked block to another day drags the schedule along with it.
    if date != cur_start.date_naive() {
        if let Some(tid) = linked {
            sync_schedule_to_block(state, &tid, date, persist, &mut out);
        }
    }
    if let Some(w) = warning {
        out.push('\n');
        out.push_str(&w);
    }
    (ToolCallStatus::Completed, out)
}

fn time_block_delete(
    state: &mut PlannerState,
    args: &Value,
    persist: bool,
) -> (ToolCallStatus, String) {
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return (
            ToolCallStatus::Error,
            "Provide 'id': the block to delete.".to_string(),
        );
    };
    let Some(idx) = state.blocks.iter().position(|b| b.id == id) else {
        return (
            ToolCallStatus::Error,
            format!("unknown block id '{}'", id),
        );
    };
    // Same read-only rule as time_block_move: deleting a mirrored calendar
    // event would only "succeed" until the next sync pass re-inserts it.
    if matches!(state.blocks[idx].source, BlockSource::External { .. }) {
        return (
            ToolCallStatus::Error,
            format!(
                "'{}' is a calendar event mirrored from the user's external calendar — Hobbes cannot delete it. It must be removed or declined in the source calendar; sync will pick the change up within ~15 minutes.",
                state.blocks[idx].title
            ),
        );
    }
    if let Some(e) = protected_guard(&state.blocks[idx], args, "delete") {
        return (ToolCallStatus::Error, e);
    }
    let removed = state.blocks.remove(idx);
    if persist {
        if let Err(e) = store::delete_block(id) {
            tracing::error!("Failed to delete time block {}: {}", id, e);
        }
    }
    (
        ToolCallStatus::Completed,
        format!("Deleted {}", block_line(state, &removed)),
    )
}

// ── HOBBES_CALENDAR_LIST ────────────────────────────────────────────────────

/// Longest range the calendar tool will list. Matches the tool schema's
/// documented max; larger requests clamp rather than fail.
const CALENDAR_LIST_MAX_DAYS: u32 = 14;

/// Parse `start_date` (default today) and `days` (default 1, clamped to
/// 1..=[`CALENDAR_LIST_MAX_DAYS`]) from the tool args.
fn calendar_list_range(args: &Value, today: NaiveDate) -> Result<(NaiveDate, u32), String> {
    let start = match args.get("start_date").and_then(|v| v.as_str()) {
        Some(raw) => parse_date(raw, today).map_err(|e| format!("start_date: {}", e))?,
        None => today,
    };
    let days = match args.get("days").filter(|v| !v.is_null()) {
        Some(v) => v
            .as_i64()
            .ok_or_else(|| "invalid 'days' — expected an integer".to_string())?
            .clamp(1, CALENDAR_LIST_MAX_DAYS as i64) as u32,
        None => 1,
    };
    Ok((start, days))
}

/// Pure formatter for the calendar listing, testable over injected events.
///
/// Events from disabled subscriptions are excluded (with a note naming the
/// disabled calendar); `sync_errors` — `(subscription name, error)` pairs read
/// from sync state at the dispatch edge — are appended as notes so the model
/// knows when the mirror may be stale. Timed events land on their local start
/// day; all-day events appear on every local day they cover.
fn calendar_list_over(
    events: &[CalendarEvent],
    subscriptions: &[crate::settings::CalendarSubscription],
    sync_errors: &[(String, String)],
    start: NaiveDate,
    days: u32,
    today: NaiveDate,
) -> String {
    let end_exclusive = start + chrono::Duration::days(days as i64);
    let end_inclusive = end_exclusive - chrono::Duration::days(1);
    let enabled: std::collections::HashSet<String> = subscriptions
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.clone())
        .collect();

    let mut day_lines: Vec<(NaiveDate, Vec<String>)> = Vec::new();
    let mut total = 0usize;
    for offset in 0..days {
        let day = start + chrono::Duration::days(offset as i64);
        let mut lines: Vec<String> = views::all_day_events_on(events, &enabled, day)
            .iter()
            .map(|e| {
                format!(
                    "(all day) {}{}",
                    truncate_title(&e.title),
                    e.location
                        .as_deref()
                        .map(|l| format!(" @ {}", l))
                        .unwrap_or_default()
                )
            })
            .collect();
        let mut timed: Vec<&CalendarEvent> = events
            .iter()
            .filter(|e| !e.all_day && enabled.contains(&e.subscription_id))
            .filter(|e| e.start.with_timezone(&chrono::Local).date_naive() == day)
            .collect();
        timed.sort_by_key(|e| e.start);
        lines.extend(timed.iter().map(|e| {
            format!(
                "{}–{} {}{}",
                e.start.with_timezone(&chrono::Local).format("%H:%M"),
                e.end.with_timezone(&chrono::Local).format("%H:%M"),
                truncate_title(&e.title),
                e.location
                    .as_deref()
                    .map(|l| format!(" @ {}", l))
                    .unwrap_or_default()
            )
        }));
        if !lines.is_empty() {
            total += lines.len();
            day_lines.push((day, lines));
        }
    }

    let mut notes: Vec<String> = subscriptions
        .iter()
        .filter(|s| !s.enabled)
        .map(|s| {
            format!(
                "Note: calendar '{}' is disabled in Settings — its events are hidden.",
                s.name
            )
        })
        .collect();
    notes.extend(sync_errors.iter().map(|(name, err)| {
        format!(
            "Note: calendar '{}' failed its last sync ({}) — events may be stale or missing.",
            name, err
        )
    }));

    let range_label = if days == 1 {
        views::friendly_date(start, today)
    } else {
        format!(
            "{} to {}",
            views::friendly_date(start, today),
            views::friendly_date(end_inclusive, today)
        )
    };

    let mut out = if total == 0 {
        format!("No calendar events {}.", range_label)
    } else {
        let mut body = format!("Calendar — {} event(s) {}:", total, range_label);
        let mut emitted = 0usize;
        'outer: for (day, lines) in &day_lines {
            body.push_str(&format!("\n{}:", views::friendly_date(*day, today)));
            for line in lines {
                if emitted >= MAX_LIST_LINES {
                    body.push_str(&format!("\n…and {} more", total - emitted));
                    break 'outer;
                }
                body.push_str(&format!("\n  {}", line));
                emitted += 1;
            }
        }
        body
    };
    for note in notes {
        out.push('\n');
        out.push_str(&note);
    }
    out
}

/// `HOBBES_CALENDAR_LIST` — read-only listing of the mirrored calendar cache.
///
/// Deliberately takes no `&mut PlannerState` / `persist`: it reads the
/// `todo_calendar_events` cache (which holds events beyond the ±14-day block
/// materialization window), never blocks, and writes nothing. The store read
/// sits here at the edge; all formatting is in [`calendar_list_over`].
pub fn handle_calendar_list(
    args: &Value,
    subscriptions: &[crate::settings::CalendarSubscription],
    sync_errors: &[(String, String)],
    today: NaiveDate,
) -> (ToolCallStatus, String) {
    let (start, days) = match calendar_list_range(args, today) {
        Ok(r) => r,
        Err(e) => return (ToolCallStatus::Error, e),
    };
    let events = match store::load_calendar_events(None) {
        Ok(events) => events,
        Err(e) => {
            return (
                ToolCallStatus::Error,
                format!("calendar cache unavailable: {}", e),
            )
        }
    };
    (
        ToolCallStatus::Completed,
        calendar_list_over(&events, subscriptions, sync_errors, start, days, today),
    )
}

// ── HOBBES_PROJECT_UPSERT ───────────────────────────────────────────────────

pub fn handle_project_upsert(
    state: &mut PlannerState,
    args: &Value,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let projects = args.get("projects").and_then(|v| v.as_array());
    let areas = args.get("areas").and_then(|v| v.as_array());
    if projects.is_none_or(|p| p.is_empty()) && areas.is_none_or(|a| a.is_empty()) {
        return (
            ToolCallStatus::Error,
            "Provide 'projects' and/or 'areas': arrays of records to create or update."
                .to_string(),
        );
    }

    let now = Utc::now();
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Areas first so projects created in the same call can reference an
    // existing area by id.
    for (i, item) in areas.into_iter().flatten().enumerate() {
        match upsert_area(state, item, now) {
            Ok((line, id)) => {
                applied.push(line);
                if persist {
                    if let Some(area) = state.areas.iter().find(|a| a.id == id) {
                        if let Err(e) = store::save_area(area) {
                            tracing::error!("Failed to persist area {}: {}", area.id, e);
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("areas[{}]: {}", i, e)),
        }
    }
    for (i, item) in projects.into_iter().flatten().enumerate() {
        match upsert_project(state, item, today, now) {
            Ok((line, id)) => {
                applied.push(line);
                if persist {
                    if let Some(project) = state.projects.iter().find(|p| p.id == id) {
                        if let Err(e) = store::save_project(project) {
                            tracing::error!("Failed to persist project {}: {}", project.id, e);
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("projects[{}]: {}", i, e)),
        }
    }

    if errors.is_empty() {
        (
            ToolCallStatus::Completed,
            format!("Applied {} record(s):\n{}", applied.len(), applied.join("\n")),
        )
    } else if applied.is_empty() {
        (ToolCallStatus::Error, errors.join("\n"))
    } else {
        (
            ToolCallStatus::Error,
            format!(
                "Applied {} record(s) (the valid items were applied):\n{}\nFailed:\n{}",
                applied.len(),
                applied.join("\n"),
                errors.join("\n")
            ),
        )
    }
}

/// Create or update one area. `Ok((line, id))`. An item without an id that
/// names an existing area (same normalized name, as for projects) updates
/// that area instead of creating a duplicate. Areas are never deleted here.
fn upsert_area(
    state: &mut PlannerState,
    item: &Value,
    now: DateTime<Utc>,
) -> Result<(String, String), String> {
    let obj = item.as_object().ok_or("expected an object")?;
    let title = obj.get("title").and_then(|v| v.as_str()).map(str::trim);

    match obj.get("id").and_then(|v| v.as_str()) {
        Some(id) => {
            if !state.areas.iter().any(|a| a.id == id) {
                return Err(format!("unknown area id '{}'", id));
            }
            if let Some(t) = title.filter(|t| !t.is_empty()) {
                state.rename_area(id, t, now)?;
            }
            let area = state.areas.iter_mut().find(|a| a.id == id).ok_or("area vanished")?;
            area.updated_at = now;
            Ok((format!("[{}] area: {}", area.id, area.title), area.id.clone()))
        }
        None => {
            let title = title
                .filter(|t| !t.is_empty())
                .ok_or("missing required 'title' for a new area")?;
            let (id, created) = state.create_area(title, now)?;
            let area = state.areas.iter_mut().find(|a| a.id == id).ok_or("area vanished")?;
            if created {
                Ok((format!("[{}] area: {}", area.id, area.title), id))
            } else {
                area.updated_at = now;
                Ok((
                    format!(
                        "[{}] area: {} (matched existing area [{}]; no duplicate created)",
                        area.id, area.title, area.id
                    ),
                    id,
                ))
            }
        }
    }
}

/// `[id] 'Title' (status)` — how refusals name candidate projects.
fn project_ref(p: &Project) -> String {
    if p.status.is_closed() {
        format!("[{}] '{}' ({})", p.id, p.title, project_status_label(p.status))
    } else {
        format!("[{}] '{}'", p.id, p.title)
    }
}

/// Project statuses in the words Manage projects uses: a cancelled project
/// is "archived".
pub fn project_status_label(s: TodoStatus) -> &'static str {
    match s {
        TodoStatus::Open => "open",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Cancelled => "archived",
    }
}

/// `aliases` as given (trimmed, blanks and repeats of the title dropped).
fn parse_aliases(obj: &serde_json::Map<String, Value>, title: &str) -> Result<Option<Vec<String>>, String> {
    let Some(v) = obj.get("aliases") else { return Ok(None) };
    let arr = v.as_array().ok_or("'aliases' must be an array of strings")?;
    let mut out: Vec<String> = Vec::new();
    for a in arr {
        let a = a.as_str().ok_or("'aliases' must be an array of strings")?;
        super::project_identity::add_alias(title, &mut out, a);
    }
    Ok(Some(out))
}

/// The optional fields every project upsert may carry.
struct ProjectPatch {
    deadline: Option<NaiveDate>,
    status: Option<TodoStatus>,
    area_id: Option<String>,
}

fn apply_project_patch(
    project: &mut Project,
    obj: &serde_json::Map<String, Value>,
    patch: ProjectPatch,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if let Some(notes) = obj.get("notes").and_then(|v| v.as_str()) {
        project.notes = notes.to_string();
    }
    if let Some(aid) = patch.area_id {
        project.area_id = Some(aid);
    }
    if let Some(d) = patch.deadline {
        project.deadline = Some(d);
    }
    if let Some(s) = patch.status {
        project.status = s;
    }
    if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
        project.path = Some(p.trim().to_string()).filter(|p| !p.is_empty());
    }
    if let Some(aliases) = parse_aliases(obj, &project.title)? {
        project.aliases = aliases;
    }
    project.updated_at = now;
    Ok(())
}

/// Create or update one project. Returns the response line and the id of
/// the project it touched (for the store write).
///
/// Without an `id`, the title is first looked up by name
/// (`project_identity`): an exact match (title or alias, normalized)
/// updates that project instead of creating a twin; only-similar matches
/// are refused with the candidates unless `create_new: true`.
fn upsert_project(
    state: &mut PlannerState,
    item: &Value,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<(String, String), String> {
    use super::project_identity::{self, ExactLookup};

    let obj = item.as_object().ok_or("expected an object")?;
    let title = obj.get("title").and_then(|v| v.as_str()).map(str::trim);
    let deadline = match obj.get("deadline").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_date(raw, today)?),
        None => None,
    };
    let status = match obj.get("status").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_status(raw)?),
        None => None,
    };
    let area_id = obj.get("area_id").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(ref aid) = area_id {
        if !state.areas.iter().any(|a| a.id == *aid) {
            return Err(format!("unknown area_id '{}'", aid));
        }
    }
    let patch = ProjectPatch { deadline, status, area_id };

    if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
        let project = state
            .projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| {
                format!("unknown project id '{}' — HOBBES_PROJECT_LIST shows the ids", id)
            })?;
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            project.title = t.to_string();
        }
        apply_project_patch(project, obj, patch, now)?;
        return Ok((format!("[{}] project: {}", project.id, project.title), project.id.clone()));
    }

    let title = title
        .filter(|t| !t.is_empty())
        .ok_or("missing required 'title' for a new project")?;
    let create_new = obj.get("create_new").and_then(|v| v.as_bool()).unwrap_or(false);

    match project_identity::lookup_exact(&state.projects, title) {
        ExactLookup::One(id) => {
            let project = state
                .projects
                .iter_mut()
                .find(|p| p.id == id)
                .expect("lookup returned a live id");
            // The name it was asked for becomes an alias when it isn't
            // already one of the project's names.
            let mut aliases = project.aliases.clone();
            project_identity::add_alias(&project.title, &mut aliases, title);
            project.aliases = aliases;
            apply_project_patch(project, obj, patch, now)?;
            let note = if project.status.is_closed() {
                format!(
                    " It is {} — pass status 'open' with this id to reopen it.",
                    project_status_label(project.status)
                )
            } else {
                String::new()
            };
            return Ok((
                format!(
                    "matched existing project [{}] '{}' — updated it instead of creating a duplicate. Use this id from now on.{}",
                    project.id, project.title, note
                ),
                project.id.clone(),
            ));
        }
        ExactLookup::Many(ids) => {
            let refs: Vec<String> = ids
                .iter()
                .filter_map(|id| state.projects.iter().find(|p| &p.id == id))
                .map(project_ref)
                .collect();
            return Err(format!(
                "not created: several existing projects are named '{}': {}. Pass the 'id' of the one to update (they look like duplicates — the user can merge them in Planner \u{25b8} Manage projects).",
                title,
                refs.join(", ")
            ));
        }
        ExactLookup::None => {}
    }

    if !create_new {
        let similar = project_identity::similar_projects(&state.projects, title);
        if !similar.is_empty() {
            let refs: Vec<String> = similar.iter().take(8).map(|p| project_ref(p)).collect();
            return Err(format!(
                "not created: '{}' looks like existing project(s) {}. Pass the 'id' to update one of them, or set create_new: true only if this really is a different project.",
                title,
                refs.join(", ")
            ));
        }
    }

    let sort_order = state
        .projects
        .iter()
        .map(|p| p.sort_order)
        .fold(0.0f64, f64::max)
        + SORT_STEP;
    let mut project = Project {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        notes: String::new(),
        area_id: None,
        status: TodoStatus::default(),
        deadline: None,
        sort_order,
        path: None,
        aliases: Vec::new(),
        distinct_from: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    apply_project_patch(&mut project, obj, patch, now)?;
    let line = format!("[{}] project: {} (created)", project.id, project.title);
    let id = project.id.clone();
    state.projects.push(project);
    Ok((line, id))
}

// ── HOBBES_PROJECT_LIST ─────────────────────────────────────────────────────

/// List projects with their ids, names, and activity — what the model looks
/// at before creating or filing into a project. `query` filters by name
/// (title or alias: exact, similar, or a normalized substring);
/// `include_closed` adds completed/archived projects.
pub fn handle_project_list(state: &PlannerState, args: &Value) -> (ToolCallStatus, String) {
    use super::project_identity::{self, NameMatch};
    use super::project_merge::project_stats;

    let include_closed = args.get("include_closed").and_then(|v| v.as_bool()).unwrap_or(false);
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|q| !q.is_empty());

    let mut rows: Vec<(&Project, NameMatch)> = state
        .projects
        .iter()
        .filter(|p| include_closed || !p.status.is_closed())
        .filter_map(|p| match query {
            None => Some((p, NameMatch::Similar)),
            Some(q) => project_identity::match_name(p, q).or_else(|| {
                let nq = project_identity::normalize(q);
                (!nq.is_empty()
                    && project_identity::project_keys(p).iter().any(|k| k.contains(&nq)))
                .then_some(NameMatch::Similar)
            }).map(|m| (p, m)),
        })
        .collect();
    rows.sort_by(|(a, ma), (b, mb)| {
        mb.cmp(ma)
            .then(a.status.is_closed().cmp(&b.status.is_closed()))
            .then(a.sort_order.total_cmp(&b.sort_order))
    });

    if rows.is_empty() {
        let msg = match query {
            Some(q) => format!(
                "No {}project matches '{}'. Create one with HOBBES_PROJECT_UPSERT{}.",
                if include_closed { "" } else { "open " },
                q,
                if include_closed { "" } else { " (or retry with include_closed: true)" }
            ),
            None => "No projects yet. Create one with HOBBES_PROJECT_UPSERT.".to_string(),
        };
        return (ToolCallStatus::Completed, msg);
    }

    let mut lines: Vec<String> = Vec::new();
    for (p, _) in rows.iter().take(MAX_LIST_LINES) {
        let stats = project_stats(state, p);
        let mut line = format!("[{}] {}", p.id, p.title);
        if !p.aliases.is_empty() {
            line.push_str(&format!(" (aka {})", p.aliases.join(", ")));
        }
        if let Some(area) = p
            .area_id
            .as_ref()
            .and_then(|aid| state.areas.iter().find(|a| &a.id == aid))
        {
            line.push_str(&format!(" · area: {}", area.title));
        }
        if let Some(path) = &p.path {
            line.push_str(&format!(" · path: {}", path));
        }
        line.push_str(&format!(
            " · {} · {} open to-do{} · last activity {}",
            project_status_label(p.status),
            stats.open_todos,
            if stats.open_todos == 1 { "" } else { "s" },
            stats.last_activity.with_timezone(&chrono::Local).date_naive()
        ));
        lines.push(line);
    }
    if rows.len() > MAX_LIST_LINES {
        lines.push(format!("…and {} more (narrow with 'query')", rows.len() - MAX_LIST_LINES));
    }

    // Areas first (with ids), so the model can file a project under one.
    let areas = state.areas_sorted();
    let mut out = if areas.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = areas.iter().map(|a| format!("[{}] {}", a.id, a.title)).collect();
        format!("Areas (set a project's area_id): {}\n", names.join(", "))
    };
    out.push_str(&format!("{} project(s):\n{}", rows.len(), lines.join("\n")));
    if query.is_none() {
        let groups = project_identity::duplicate_groups(&state.projects);
        if !groups.is_empty() {
            out.push_str(
                "\nPossible duplicates (don't create another; the user decides — suggest merging with HOBBES_PROJECT_MERGE, which asks them to approve, or they can merge in Planner \u{25b8} Manage projects):",
            );
            for g in groups {
                let names: Vec<String> = g
                    .ids
                    .iter()
                    .filter_map(|id| state.projects.iter().find(|p| &p.id == id))
                    .map(project_ref)
                    .collect();
                out.push_str(&format!("\n- {}", names.join(" ~ ")));
            }
        }
    }
    (ToolCallStatus::Completed, out)
}

// ── HOBBES_PROJECT_MERGE ────────────────────────────────────────────────────

/// Validate `HOBBES_PROJECT_MERGE` arguments against the current projects:
/// `keep_id` must exist; `merge_ids` must be a non-empty array of other
/// existing project ids (duplicates collapse). `Ok((keep_id, merge_ids))`.
pub fn validate_project_merge(state: &PlannerState, args: &Value) -> Result<(String, Vec<String>), String> {
    let known = |id: &str| state.projects.iter().any(|p| p.id == id);
    let keep_id = args
        .get("keep_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("missing 'keep_id': the id of the project to keep (HOBBES_PROJECT_LIST shows ids)")?;
    if !known(keep_id) {
        return Err(format!("unknown project id '{}' — HOBBES_PROJECT_LIST shows the ids", keep_id));
    }
    let raw = args
        .get("merge_ids")
        .and_then(|v| v.as_array())
        .ok_or("missing 'merge_ids': an array of the project ids to fold into keep_id")?;
    let mut merge_ids: Vec<String> = Vec::new();
    for v in raw {
        let id = v.as_str().map(str::trim).ok_or("merge_ids must be strings (project ids)")?;
        if id == keep_id {
            return Err("keep_id can't also be in merge_ids".to_string());
        }
        if !known(id) {
            return Err(format!("unknown project id '{}' — HOBBES_PROJECT_LIST shows the ids", id));
        }
        if !merge_ids.iter().any(|m| m == id) {
            merge_ids.push(id.to_string());
        }
    }
    if merge_ids.is_empty() {
        return Err("merge_ids is empty: name at least one project to fold into keep_id".to_string());
    }
    Ok((keep_id.to_string(), merge_ids))
}

/// What a merge will move, for the approval prompt: the titles, how many
/// to-dos, and how many chats (`None` while still counting).
pub fn project_merge_preview(
    state: &PlannerState,
    keep_id: &str,
    merge_ids: &[String],
    chats: Option<usize>,
) -> String {
    let title = |id: &str| {
        state
            .projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.title.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let merged: Vec<String> = merge_ids.iter().map(|id| title(id)).collect();
    let todos = state
        .todos
        .iter()
        .filter(|t| t.project_id.as_ref().is_some_and(|p| merge_ids.contains(p)))
        .count();
    crate::components::planner_view::manage_projects::merge_message(&title(keep_id), &merged, todos, chats)
}

/// (session_id, todo title) pairs for every open todo linked to a fleet
/// session — feeds brief framing and the fleet status report.
pub fn linked_session_pairs(state: &PlannerState) -> Vec<(String, String)> {
    state
        .todos
        .iter()
        .filter(|t| !matches!(t.status, TodoStatus::Completed | TodoStatus::Cancelled))
        .filter_map(|t| {
            t.linked_fleet_session
                .clone()
                .map(|sid| (sid, t.title.clone()))
        })
        .collect()
}

/// Write a linked session's fresh brief line onto its open todos — display
/// only, never touches status (progress is reported, not judged). Returns
/// changed todo ids; callers persist each via `store::save_todo`.
pub fn apply_brief_progress(
    state: &mut PlannerState,
    session_id: &str,
    line: &str,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for todo in state.todos.iter_mut() {
        if todo.linked_fleet_session.as_deref() == Some(session_id)
            && !matches!(todo.status, TodoStatus::Completed | TodoStatus::Cancelled)
            && todo.latest_progress.as_deref() != Some(line)
        {
            todo.latest_progress = Some(line.to_string());
            todo.updated_at = now;
            changed.push(todo.id.clone());
        }
    }
    // The agent-state hook: a fresh brief is the moment the worker's state is
    // worth re-reading. The fleet row (live, else stored) supplies the
    // explicit fields; the derivation never reads the line's text.
    let live = crate::fleet::shared().snapshot();
    let row = live
        .sessions
        .get(session_id)
        .cloned()
        .or_else(|| crate::fleet::store::load_session(session_id));
    if let Some(row) = row {
        let signal = worker_signal_from_fleet(&row);
        for id in apply_signal_to_linked(state, session_id, &signal, now) {
            if !changed.contains(&id) {
                changed.push(id);
            }
        }
    }
    changed
}

/// Reduce a fleet row to the explicit fields `lanes::derive_agent_state`
/// may use: status, a held gate, the attention message, the brief's
/// headline/`blocked_on`, and whether the session ended.
pub fn worker_signal_from_fleet(row: &crate::fleet::FleetSession) -> super::lanes::WorkerSignal {
    use super::lanes::{GateSignal, WorkerSignal, WorkerStatus};
    use crate::fleet::{AttentionKind, FleetStatus};
    let (status, attention_message) = match &row.status {
        FleetStatus::Working | FleetStatus::WorkingBackground => (WorkerStatus::Working, None),
        FleetStatus::Idle => (WorkerStatus::Idle, None),
        FleetStatus::NeedsAttention(AttentionKind::Notification { message, .. }) => {
            (WorkerStatus::WaitingOnUser, Some(message.clone()).filter(|m| !m.trim().is_empty()))
        }
        FleetStatus::NeedsAttention(AttentionKind::Gate) => (WorkerStatus::WaitingOnUser, None),
    };
    WorkerSignal {
        status,
        ended: row.ended_at.is_some(),
        gate: row.pending_gate.as_ref().map(|g| GateSignal {
            request_id: g.request_id.clone(),
            tool_name: g.tool_name.clone(),
            input_summary: g.input_summary.clone(),
        }),
        attention_message,
        attention_at: row.attention_at,
        brief: row.brief.as_ref().map(|b| model::Brief {
            did: b.headline.clone(),
            blocked: b.blocked_on.clone().filter(|x| !x.trim().is_empty()),
            next: None,
        }),
        worker_name: row.display_name().to_string(),
    }
}

/// Apply one worker's signal to every open agent to-do linked to it.
/// Returns the changed ids (persist each via `store::save_todo`).
pub fn apply_signal_to_linked(
    state: &mut PlannerState,
    session_id: &str,
    signal: &super::lanes::WorkerSignal,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for todo in state.todos.iter_mut() {
        if todo.linked_fleet_session.as_deref() == Some(session_id)
            && todo.status != TodoStatus::Cancelled
            && super::lanes::apply_worker_signal(todo, signal, now)
        {
            changed.push(todo.id.clone());
        }
    }
    changed
}

/// **For the fleet-change listener** (wave 2: call on every `FleetShared`
/// poke): re-derive `agent_state` for every linked agent to-do from a fleet
/// snapshot plus ended rows (`fleet::store` rows leave the live map on
/// SessionEnd). Pure over its inputs. Returns changed ids to persist.
#[allow(dead_code)] // consumer: the fleet-change listener (wave 2)
pub fn sync_agent_states(
    state: &mut PlannerState,
    live: &crate::fleet::FleetState,
    ended_rows: &[crate::fleet::FleetSession],
    now: DateTime<Utc>,
) -> Vec<String> {
    let linked: Vec<String> = state
        .todos
        .iter()
        .filter(|t| t.lane == Lane::Agent && !t.status.is_closed())
        .filter_map(|t| t.linked_fleet_session.clone())
        .collect();
    let mut changed = Vec::new();
    for sid in linked {
        let row = live
            .sessions
            .get(&sid)
            .or_else(|| ended_rows.iter().find(|r| r.id == sid));
        if let Some(row) = row {
            let signal = worker_signal_from_fleet(row);
            for id in apply_signal_to_linked(state, &sid, &signal, now) {
                if !changed.contains(&id) {
                    changed.push(id);
                }
            }
        }
    }
    changed
}

// ── HOBBES_DECISIONS ────────────────────────────────────────────────────────

/// What `HOBBES_DECISIONS` produced. `deliver` is set after a `resolve`: the
/// dispatch edge hands the answer back to the worker (it owns the chat queue
/// and the session/fleet lookups) and records the delivery
/// (`lanes::record_delivery`).
pub struct DecisionsOutcome {
    pub status: ToolCallStatus,
    pub response: String,
    pub deliver: Option<model::ResolvedDecision>,
}

impl DecisionsOutcome {
    fn done(status: ToolCallStatus, response: String) -> Self {
        Self { status, response, deliver: None }
    }
}

/// `HOBBES_DECISIONS`: `list` (default), `ask`, or `resolve`.
///
/// `rule_hits_today` is the count of routing calls a trust rule resolved
/// without asking today (from `context::trust_store`), when derivable.
/// `caller_name` names the calling chat session (the worker, for `ask`).
pub fn handle_decisions(
    state: &mut PlannerState,
    args: &Value,
    caller_name: Option<&str>,
    rule_hits_today: Option<u32>,
    persist: bool,
) -> DecisionsOutcome {
    let now = Utc::now();
    match args.get("action").and_then(|v| v.as_str()).unwrap_or("list") {
        "list" => DecisionsOutcome::done(ToolCallStatus::Completed, decisions_list(state, rule_hits_today)),
        "ask" => {
            let Some(todo_id) = args.get("todo_id").and_then(|v| v.as_str()) else {
                return DecisionsOutcome::done(ToolCallStatus::Error, "Provide 'todo_id'.".into());
            };
            let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
            let options: Vec<String> = args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_str()).map(str::to_string).collect())
                .unwrap_or_default();
            let est = args.get("est_minutes").and_then(|v| v.as_u64()).map(|n| n.min(240) as u32);
            let text = |k: &str| {
                args.get(k)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_string)
            };
            let brief = match (text("did"), text("blocked"), text("next")) {
                (None, None, None) => None,
                (did, blocked, next) => Some(model::Brief { did: did.unwrap_or_default(), blocked, next }),
            };
            let Some(todo) = state.todo_mut(todo_id) else {
                return DecisionsOutcome::done(ToolCallStatus::Error, format!("unknown todo id '{}'", todo_id));
            };
            let worker = caller_name.map(str::to_string).filter(|w| !w.is_empty());
            match super::lanes::ask_decision(todo, question, options, est, brief, worker, now) {
                Ok(()) => {
                    let line = super::lanes::decision_line(todo).unwrap_or_default();
                    persist_todo(todo, persist);
                    DecisionsOutcome::done(
                        ToolCallStatus::Completed,
                        format!(
                            "Queued for the user: {}\nStop here and wait — the answer comes back to this \
                             conversation as a message.",
                            line
                        ),
                    )
                }
                Err(e) => DecisionsOutcome::done(ToolCallStatus::Error, e),
            }
        }
        "resolve" => {
            let Some(todo_id) = args.get("todo_id").and_then(|v| v.as_str()) else {
                return DecisionsOutcome::done(ToolCallStatus::Error, "Provide 'todo_id'.".into());
            };
            let choice = args.get("choice").and_then(|v| v.as_str()).unwrap_or("");
            let note = args.get("note").and_then(|v| v.as_str());
            match super::lanes::resolve_decision(state, todo_id, choice, note, now) {
                Ok(entry) => {
                    if let Some(t) = state.todo(todo_id) {
                        persist_todo(t, persist);
                    }
                    if persist {
                        if let Err(e) = store::save_resolved_decision(&entry) {
                            tracing::error!("Failed to persist decision {}: {}", entry.id, e);
                        }
                    }
                    DecisionsOutcome {
                        status: ToolCallStatus::Completed,
                        response: format!("Recorded: \"{}\" → {}.", entry.question, entry.choice),
                        deliver: Some(entry),
                    }
                }
                Err(e) => DecisionsOutcome::done(ToolCallStatus::Error, e),
            }
        }
        other => DecisionsOutcome::done(
            ToolCallStatus::Error,
            format!("Unknown action '{}' — expected list, ask, or resolve.", other),
        ),
    }
}

fn decisions_list(state: &PlannerState, rule_hits_today: Option<u32>) -> String {
    let queue = super::lanes::needs_you(&state.todos);
    let total: u32 = queue
        .iter()
        .map(|t| t.decision.as_ref().and_then(|d| d.est_minutes).unwrap_or(1).max(1))
        .sum();
    let mut out = if queue.is_empty() {
        "No decisions waiting.".to_string()
    } else {
        format!("{} decision(s) waiting, about {} min in all:", queue.len(), total)
    };
    for t in queue.iter().take(MAX_LIST_LINES) {
        match super::lanes::decision_line(t) {
            Some(line) => {
                out.push_str(&format!("\n{}", line));
                let d = t.decision.as_ref().expect("decision_line implies a decision");
                if let Some(b) = &d.brief {
                    let mut parts = Vec::new();
                    if !b.did.trim().is_empty() {
                        parts.push(format!("did: {}", truncate_title(&b.did)));
                    }
                    if let Some(x) = &b.blocked {
                        parts.push(format!("blocked: {}", truncate_title(x)));
                    }
                    if let Some(x) = &b.next {
                        parts.push(format!("next: {}", truncate_title(x)));
                    }
                    if !parts.is_empty() {
                        out.push_str(&format!("\n  Brief — {}", parts.join("; ")));
                    }
                }
                out.push_str(&format!("\n  For: {}", truncate_title(&t.title)));
                if let Some(o) = t.outcome.as_deref() {
                    out.push_str(&format!(" → {}", truncate_title(o)));
                }
            }
            None => out.push_str(&format!(
                "\n[{}] {} — waiting on the user (no question recorded; check its worker)",
                t.id,
                truncate_title(&t.title)
            )),
        }
    }
    if let Some(n) = rule_hits_today.filter(|n| *n > 0) {
        out.push_str(&format!(
            "\n{} routing call(s) handled by the user's trust rules today (not asked).",
            n
        ));
    }
    out
}

// ── HOBBES_TIME_GOALS ───────────────────────────────────────────────────────

/// `HOBBES_TIME_GOALS`: `get` (default) or `set`. `set` upserts goals by
/// category (case-insensitive) and removes the categories in `remove`.
/// Both return the goals with this week's progress.
pub fn handle_time_goals(
    state: &mut PlannerState,
    args: &Value,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
    let mut notes: Vec<String> = Vec::new();
    match action {
        "get" => {}
        "set" => {
            let mut errors = Vec::new();
            if let Some(goals) = args.get("goals").and_then(|v| v.as_array()) {
                for (i, g) in goals.iter().enumerate() {
                    let Some(category) = g
                        .get("category")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|c| !c.is_empty())
                    else {
                        errors.push(format!("goals[{}]: missing 'category'", i));
                        continue;
                    };
                    let minutes = match g.get("weekly_target_minutes").map(parse_estimate) {
                        Some(Ok(m)) => Some(m),
                        Some(Err(e)) => {
                            errors.push(format!("goals[{}]: {}", i, e.replace("estimate_minutes", "weekly_target_minutes")));
                            continue;
                        }
                        None => None,
                    };
                    let colour = match g.get("colour").or_else(|| g.get("color")) {
                        Some(Value::String(c)) if is_hex_colour(c) => Some(Some(c.trim().to_string())),
                        Some(Value::Null) => Some(None),
                        Some(other) => {
                            errors.push(format!("goals[{}]: invalid colour {} — expected #RRGGBB", i, other));
                            continue;
                        }
                        None => None,
                    };
                    let next_order = state.time_goals.iter().map(|g| g.sort_order).fold(0.0, f64::max) + SORT_STEP;
                    let goal = match state.time_goals.iter_mut().find(|x| x.matches(category)) {
                        Some(existing) => {
                            if let Some(m) = minutes {
                                existing.weekly_target_minutes = m;
                            }
                            if let Some(c) = colour {
                                existing.colour = c;
                            }
                            existing.clone()
                        }
                        None => {
                            let Some(m) = minutes else {
                                errors.push(format!("goals[{}]: new goal '{}' needs weekly_target_minutes", i, category));
                                continue;
                            };
                            let mut goal = model::TimeGoal::new(category, m);
                            goal.colour = colour.flatten();
                            goal.sort_order = next_order;
                            state.time_goals.push(goal.clone());
                            goal
                        }
                    };
                    if persist {
                        if let Err(e) = store::save_time_goal(&goal) {
                            tracing::error!("Failed to persist time goal {}: {}", goal.id, e);
                        }
                    }
                }
            }
            // Rename a category: its goal (if any), every your-time block
            // and your-time to-do move to the new name.
            if let Some(renames) = args.get("rename").and_then(|v| v.as_array()) {
                for (i, r) in renames.iter().enumerate() {
                    let field = |k: &str| r.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|c| !c.is_empty());
                    let (Some(from), Some(to)) = (field("from"), field("to")) else {
                        errors.push(format!("rename[{}]: needs 'from' and 'to'", i));
                        continue;
                    };
                    let to = match state.time_goals.iter().find(|g| g.matches(to)) {
                        Some(g) if !g.matches(from) => g.category.clone(),
                        _ => to.to_string(),
                    };
                    let goal = match super::categories::rename_goal(&mut state.time_goals, from, &to) {
                        Ok(g) => g,
                        Err(e) => {
                            errors.push(format!(
                                "rename[{}]: {} \u{2014} to merge two goals, remove '{}' and rename its category into '{}'",
                                i, e, from, to
                            ));
                            continue;
                        }
                    };
                    let st = &mut *state;
                    let moved = super::categories::recategorise(&mut st.blocks, &mut st.todos, from, &to, Utc::now());
                    if goal.is_none() && moved.is_empty() {
                        errors.push(format!("rename[{}]: no goal, block or to-do in '{}'", i, from));
                        continue;
                    }
                    if persist {
                        if let Some(g) = &goal {
                            if let Err(e) = store::save_time_goal(g) {
                                tracing::error!("Failed to persist time goal {}: {}", g.id, e);
                            }
                        }
                        for b in st.blocks.iter().filter(|b| moved.blocks.contains(&b.id)) {
                            if let Err(e) = store::save_block(b) {
                                tracing::error!("Failed to persist time block {}: {}", b.id, e);
                            }
                        }
                        for t in st.todos.iter().filter(|t| moved.todos.contains(&t.id)) {
                            if let Err(e) = store::save_todo(t) {
                                tracing::error!("Failed to persist todo {}: {}", t.id, e);
                            }
                        }
                    }
                    notes.push(format!(
                        "Renamed '{}' to '{}'{}: {} block(s), {} to-do(s) moved.",
                        from,
                        to,
                        if goal.is_some() { " (goal kept)" } else { "" },
                        moved.blocks.len(),
                        moved.todos.len()
                    ));
                }
            }
            if let Some(remove) = args.get("remove").and_then(|v| v.as_array()) {
                for c in remove.iter().filter_map(|v| v.as_str()) {
                    match state.time_goals.iter().position(|g| g.matches(c)) {
                        Some(idx) => {
                            let g = state.time_goals.remove(idx);
                            notes.push(format!("Removed goal '{}'.", g.category));
                            if persist {
                                if let Err(e) = store::delete_time_goal(&g.id) {
                                    tracing::error!("Failed to delete time goal {}: {}", g.id, e);
                                }
                            }
                        }
                        None => errors.push(format!("remove: no goal named '{}'", c)),
                    }
                }
            }
            if !errors.is_empty() {
                let (_, summary) = handle_time_goals(state, &serde_json::json!({}), today, false);
                return (
                    ToolCallStatus::Error,
                    format!("{}\nFailed (valid changes were applied):\n{}", summary, errors.join("\n")),
                );
            }
        }
        other => {
            return (
                ToolCallStatus::Error,
                format!("Unknown action '{}' — expected get or set.", other),
            )
        }
    }

    let week = super::measures::your_time_week(
        &state.time_goals,
        &state.blocks,
        &state.todos,
        &state.focus_sessions,
        today,
    );
    let mut out = if state.time_goals.is_empty() {
        "No your-time goals yet. Ask the user what their time is for (reading, gaming, getting \
         outside…) and how many hours a week they want for each."
            .to_string()
    } else {
        format!(
            "Your time, week of {}: {} of {} goal.",
            week.week_start,
            model::format_minutes(week.total_minutes()),
            model::format_minutes(week.goal_minutes())
        )
    };
    for c in &week.categories {
        let goal = match c.goal_minutes {
            Some(g) if c.goal_met() => format!(" / {} goal — met", model::format_minutes(g)),
            Some(g) => format!(" / {} goal — {} to go", model::format_minutes(g), model::format_minutes(c.remaining())),
            None => " (no goal)".to_string(),
        };
        out.push_str(&format!(
            "\n- {}: {}{} ({} protected, {} logged)",
            c.category,
            model::format_minutes(c.minutes()),
            goal,
            model::format_minutes(c.protected_minutes),
            model::format_minutes(c.focus_minutes)
        ));
    }
    let (protected_today, _) = super::measures::protected_on(&state.blocks, today);
    if protected_today > 0 {
        out.push_str(&format!("\nProtected today: {}.", model::format_minutes(protected_today)));
    }
    let untracked =
        super::categories::in_use_without_goal(&state.time_goals, &state.blocks, in_use_since_day(today));
    if !untracked.is_empty() {
        out.push_str(&format!(
            "\nIn use without a goal (last {} weeks): {}. Tracked, no target; the user can make one a goal.",
            super::categories::IN_USE_LOOKBACK_DAYS / 7,
            untracked.join(", ")
        ));
    }
    for n in notes {
        out.push('\n');
        out.push_str(&n);
    }
    (ToolCallStatus::Completed, out)
}

fn is_hex_colour(c: &str) -> bool {
    let c = c.trim();
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit())
}

// ── HOBBES_AGENT_TIME ───────────────────────────────────────────────────────

/// `HOBBES_AGENT_TIME`: `start(intent)`, `log(summary)` (optional note),
/// `propose(kind, title, body)`, `end(summary)`. Refused unless the call
/// comes from the leased agent-time session and the grant allows it today
/// (see `agent_time`). Writes through the session, proposal and ledger rows.
/// The app calls [`handle_agent_time_for`] with the person's name.
#[cfg(test)]
pub fn handle_agent_time(
    state: &mut PlannerState,
    args: &Value,
    session_id: &str,
    today: NaiveDate,
    persist: bool,
) -> (ToolCallStatus, String) {
    handle_agent_time_for(state, args, session_id, today, persist, "the user")
}

/// Whether a fleet session is running now (a live worker on a to-do).
pub fn fleet_worker_live(snapshot: &crate::fleet::FleetState, session_id: &str) -> bool {
    snapshot.sessions.get(session_id).is_some_and(|r| {
        r.ended_at.is_none()
            && matches!(
                r.status,
                crate::fleet::FleetStatus::Working
                    | crate::fleet::FleetStatus::WorkingBackground
                    | crate::fleet::FleetStatus::NeedsAttention(_)
            )
    })
}

/// [`handle_agent_time`] with the person's name (`owner`) for the
/// agent-time context lists ("Dustin's tasks: NOT yours").
pub fn handle_agent_time_for(
    state: &mut PlannerState,
    args: &Value,
    session_id: &str,
    today: NaiveDate,
    persist: bool,
    owner: &str,
) -> (ToolCallStatus, String) {
    use super::agent_time::AgentTimeRefusal;
    let fleet = crate::fleet::shared().snapshot();
    let worker_live = |sid: &str| fleet_worker_live(&fleet, sid);
    let now = Utc::now();
    let text = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let persist_session = |s: &model::AgentTimeSession| {
        if persist {
            if let Err(e) = store::save_agent_time_session(s) {
                tracing::error!("Failed to persist agent-time session {}: {}", s.id, e);
            }
        }
    };
    // A session found over budget is closed on the record before refusing.
    if state.is_agent_time_session(session_id) {
        if let Some(ended) = state.enforce_agent_time_budget(today, now) {
            persist_session(&ended);
            if persist {
                if let Err(e) = store::save_trust_ledger(&state.trust_ledger) {
                    tracing::error!("Failed to persist trust ledger: {}", e);
                }
            }
        }
    }
    let result: Result<String, AgentTimeRefusal> = match text("action") {
        "start" => {
            let todo_id = Some(text("todo_id")).filter(|t| !t.trim().is_empty());
            state
                .agent_time_start_on(session_id, text("intent"), todo_id, &worker_live, today, now)
                .map(|s| {
                    persist_session(&s);
                    let b = state.agent_time_budget(today, now);
                    let on = match s.todo_id.as_deref().and_then(|id| state.todo(id)) {
                        Some(t) => format!("Working on: {} [{}].", t.title, t.id),
                        None => "Working in Hobbes-owned space (no to-do).".to_string(),
                    };
                    let lists = super::agent_time::agent_time_task_lists(state, today, &worker_live);
                    format!(
                        "Agent time started (session {}). Intent logged. {on} Budget left today: {} and \
                         ${:.2}. Deliver finished work with deliver (job: prepare, finish or tend); file \
                         changes to the user's stuff with propose; save questions with ask instead of \
                         stopping. Anything with an outside effect goes through the normal approval \
                         gate. Every tool call is recorded for you. Call end(summary) when done.\n\n{}",
                        s.id,
                        model::format_minutes(b.minutes_left()),
                        b.spend_left_usd(),
                        super::agent_time::agent_time_context(&lists, owner)
                    )
                })
        }
        "log" => state
            .agent_time_log(session_id, text("tool"), text("summary"), today, now)
            .map(|s| {
                persist_session(&s);
                "Noted. (Optional: every tool call is already recorded automatically.)".to_string()
            }),
        "propose" => match super::model::ProposalKind::parse(text("kind")) {
            None => Err(AgentTimeRefusal::Invalid(
                "Provide 'kind': note, todo, skill, research, other, or offer (with todo_id).".into(),
            )),
            Some(kind) => {
                let filed = if kind == super::model::ProposalKind::Offer {
                    let pitch = if text("body").trim().is_empty() { text("title") } else { text("body") };
                    state.agent_time_offer(session_id, text("todo_id"), pitch, today, now)
                } else {
                    let lane = match text("lane").trim().to_ascii_lowercase().as_str() {
                        "agent" | "hobbes" | "mine" => Some(model::Lane::Agent),
                        _ => None,
                    };
                    state.agent_time_propose_in(session_id, kind, text("title"), text("body"), lane, today, now)
                };
                filed.map(|(p, s)| {
                    persist_session(&s);
                    if persist {
                        if let Err(e) = store::save_proposal(&p) {
                            tracing::error!("Failed to persist proposal {}: {}", p.id, e);
                        }
                    }
                    format!(
                        "Proposal {} filed ({}: {}). The user will accept, edit, or dismiss it.",
                        p.id,
                        p.kind.as_str(),
                        p.title
                    )
                })
            }
        },
        "deliver" => match super::model::Job::parse(text("job")) {
            None => Err(AgentTimeRefusal::Invalid(
                "Provide 'job': prepare, finish, or tend. (Changes to the user's stuff are \
                 proposals: use propose.)"
                    .into(),
            )),
            Some(job) => {
                let kind = match text("kind").trim() {
                    "" => Ok(None),
                    k => super::model::ProposalKind::parse(k).map(Some).ok_or_else(|| {
                        AgentTimeRefusal::Invalid(
                            "Unknown 'kind': use note, todo, skill, research, or other.".into(),
                        )
                    }),
                };
                kind.and_then(|kind| {
                    let todo_id = Some(text("todo_id")).filter(|t| !t.trim().is_empty());
                    let correction = args.get("correction").and_then(|v| v.as_bool()).unwrap_or(false);
                    state.agent_time_deliver(
                        session_id,
                        super::agent_time::Delivery {
                            job,
                            kind,
                            title: text("title"),
                            body: text("body"),
                            todo_id,
                            correction,
                        },
                        today,
                        now,
                    )
                })
                .map(|d| {
                    persist_session(&d.session);
                    if persist {
                        if let Err(e) = store::save_proposal(&d.result) {
                            tracing::error!("Failed to persist result {}: {}", d.result.id, e);
                        }
                        if let Some(t) = &d.todo {
                            persist_todo(t, true);
                        }
                        if d.result.correction {
                            if let Err(e) = store::save_trust_ledger(&state.trust_ledger) {
                                tracing::error!("Failed to persist trust ledger: {}", e);
                            }
                        }
                    }
                    let flag = if d.result.correction { " (correction)" } else { "" };
                    format!(
                        "Delivered {} ({}{flag}: {}). It waits in the user's review; they will keep \
                         it, send it back as needs work, or discard it.",
                        d.result.id,
                        d.result.job.as_str(),
                        d.result.title
                    )
                })
            }
        },
        "ask" => {
            let options: Vec<String> = args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            state
                .agent_time_ask(session_id, text("question"), &options, today, now)
                .map(|(q, s)| {
                    persist_session(&s);
                    if persist {
                        if let Err(e) = store::save_agent_question(&q) {
                            tracing::error!("Failed to persist question {}: {}", q.id, e);
                        }
                    }
                    format!(
                        "Question {} saved for the user's review. Don't wait for the answer: carry \
                         on with what you can do without it. The answer reaches your next window.",
                        q.id
                    )
                })
        }
        "end" => state.agent_time_end(session_id, text("summary"), now).map(|s| {
            persist_session(&s);
            if persist {
                if let Err(e) = store::save_trust_ledger(&state.trust_ledger) {
                    tracing::error!("Failed to persist trust ledger: {}", e);
                }
            }
            format!(
                "Agent time ended after {} with {} result(s) and proposal(s), {} tool call(s).",
                model::format_minutes(s.minutes_used(now)),
                s.outputs.len(),
                s.call_count()
            )
        }),
        other => Err(AgentTimeRefusal::Invalid(format!(
            "Unknown action '{}' — expected start, log, deliver, propose, ask, or end.",
            other
        ))),
    };
    match result {
        Ok(msg) => (ToolCallStatus::Completed, msg),
        Err(r) => (ToolCallStatus::Error, r.message()),
    }
}

// ── planner_today context injection ─────────────────────────────────────────

/// Hard caps for the `planner_today` system-context block. The block rides on
/// *every* prompt, so it is budgeted like a header, not a report — the model
/// can always call HOBBES_TODO_LIST for the rest.
const CTX_MAX_TODOS: usize = 20;
/// Hobbes-owned timeboxes (`Manual` / `Auto`) keep the full historical block
/// budget to themselves; external meetings get their own smaller lane. The
/// split (rather than one shared cap) is the point: a meeting-heavy day must
/// never crowd Hobbes' own timeboxes out of the context.
const CTX_MAX_OWN_BLOCKS: usize = 10;
const CTX_MAX_MEETINGS: usize = 8;
/// All-day events are banner facts ("company offsite"), not schedule rows —
/// a couple of lines is plenty.
const CTX_MAX_ALL_DAY: usize = 3;
const CTX_MAX_OVERDUE: usize = 5;
const CTX_MAX_TITLE_CHARS: usize = 80;

fn truncate_title(title: &str) -> String {
    if title.chars().count() <= CTX_MAX_TITLE_CHARS {
        title.to_string()
    } else {
        let cut: String = title.chars().take(CTX_MAX_TITLE_CHARS - 1).collect();
        format!("{}…", cut)
    }
}

/// The `planner_today` block injected into system context, or `None` when the
/// planner (or the injection) is disabled in Settings.
///
/// The store-reading edge: loads today's all-day calendar events from the
/// cache (all-day events never materialize as blocks, so `PlannerState` cannot
/// supply them) and delegates to the pure [`planner_today_context_with_events`].
/// A missing/unavailable store degrades to "no all-day events", never an error.
pub fn planner_today_context(
    planner: &PlannerState,
    settings: &crate::settings::Settings,
    today: NaiveDate,
) -> Option<serde_json::Value> {
    if !settings.planner_enabled || !settings.planner_inject_today_context {
        return None;
    }
    let calendar_events = store::load_calendar_events(None).unwrap_or_default();
    let mut ctx = planner_today_context_with_events(planner, settings, today, &calendar_events)?;

    // Fleet briefs ride along when observation + briefs are on (Pro): one
    // capped line per external coding-agent session active today, so any chat
    // can answer "what did I work on today?". Store-reading, so it lives on
    // this edge — the pure core stays fleet-free.
    if settings.fleet_enabled
        && settings.fleet_briefs_enabled
        && crate::entitlement::pro_active()
    {
        let now = chrono::Utc::now();
        let rows = crate::fleet::store::sessions_active_on(today);
        let lines = crate::fleet::briefs::fleet_context_lines(
            &rows,
            today,
            now,
            crate::fleet::briefs::CTX_MAX_FLEET,
        );
        if !lines.is_empty() {
            ctx["fleet"] = serde_json::json!(lines);
        }
    }
    Some(ctx)
}

/// Pure core of [`planner_today_context`]: everything but the store read, so
/// tests can inject `CalendarEvent`s directly.
pub fn planner_today_context_with_events(
    planner: &PlannerState,
    settings: &crate::settings::Settings,
    today: NaiveDate,
    calendar_events: &[CalendarEvent],
) -> Option<serde_json::Value> {
    if !settings.planner_enabled || !settings.planner_inject_today_context {
        return None;
    }

    let capacity = planner.capacity_for(today, settings.planner_daily_capacity_minutes);
    let mut cap = model::measure_capacity_full(
        &planner.todos,
        &planner.blocks,
        &planner.focus_sessions,
        today,
        capacity,
        settings.planner_calendar_counts_against_capacity,
    );
    // Per-actor minutes ride as their own compact keys below; keep the
    // capacity line itself in its familiar shape.
    cap.agent_minutes = 0;
    // Live minutes are rounded down to the fleet's 15-minute step: this block
    // reaches the model through the turn context ledger, which records it
    // again whenever its rendering changes, and an open focus session would
    // otherwise change it every minute.
    let step = crate::fleet::briefs::CTX_MINUTES_STEP;
    let person_done_minutes = model::person_minutes_on(&planner.focus_sessions, today) / step * step;
    let agent_done_minutes = model::agent_minutes_on(&planner.focus_sessions, today) / step * step;

    let todos: Vec<String> = views::in_view(&planner.todos, TodoView::Today, today)
        .into_iter()
        .take(CTX_MAX_TODOS)
        .map(|t| {
            let mut clipped = t.clone();
            clipped.title = truncate_title(&t.title);
            clipped.summary()
        })
        .collect();

    // Interleave with priority: Hobbes-owned timeboxes first, then external
    // meetings, each group time-sorted (blocks_on sorts; partition preserves
    // order) and capped separately so meetings can't evict timeboxes.
    let (own, meetings): (Vec<&TimeBlock>, Vec<&TimeBlock>) = planner
        .blocks_on(today)
        .into_iter()
        .partition(|b| !matches!(b.source, BlockSource::External { .. }));
    let block_line = |b: &TimeBlock, suffix: &str| {
        format!(
            "{}–{} {}{}",
            b.start.with_timezone(&chrono::Local).format("%H:%M"),
            b.end.with_timezone(&chrono::Local).format("%H:%M"),
            truncate_title(&planner.block_display_title(b)),
            suffix
        )
    };
    // Own blocks carry their id so "move my 2pm" is a HOBBES_TIME_BLOCK move,
    // not a duplicate create.
    let mut blocks: Vec<String> = own
        .into_iter()
        .take(CTX_MAX_OWN_BLOCKS)
        .map(|b| block_line(b, &format!(" [{}]", b.id)))
        .collect();
    blocks.extend(meetings.into_iter().take(CTX_MAX_MEETINGS).map(|b| {
        // Free/focus-time mirrors are labeled so the model knows it may plan
        // work over them; tentative invitations are labeled so the model
        // knows they don't count as planned time; busy firm meetings stay
        // fixed commitments.
        let suffix = match b.source {
            BlockSource::External { busy: false, .. } => " (meeting, free)",
            BlockSource::External {
                tentative: true, ..
            } => " (meeting, tentative)",
            _ => " (meeting)",
        };
        block_line(b, suffix)
    }));

    // Today's all-day events (enabled subscriptions only) from the calendar
    // cache — they never exist as blocks, so they'd otherwise be invisible.
    let enabled_subs: std::collections::HashSet<String> = settings
        .planner_calendar_subscriptions
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.clone())
        .collect();
    let all_day: Vec<String> = views::all_day_events_on(calendar_events, &enabled_subs, today)
        .into_iter()
        .take(CTX_MAX_ALL_DAY)
        .map(|e| format!("{} (all day)", truncate_title(&e.title)))
        .collect();

    let overdue: Vec<String> = views::overdue(&planner.todos, today)
        .into_iter()
        .take(CTX_MAX_OVERDUE)
        .map(|t| {
            format!(
                "[{}] {} — was due {}",
                t.id,
                truncate_title(&t.title),
                t.deadline.map(|d| d.to_string()).unwrap_or_default()
            )
        })
        .collect();

    let now = Utc::now();
    let focus = planner.focused().map(|t| {
        format!(
            "[{}] {} — {} elapsed{}",
            t.id,
            truncate_title(&t.title),
            model::format_minutes(t.elapsed_minutes(now) / step * step),
            t.estimate_minutes
                .map(|m| format!(" of {} estimated", model::format_minutes(m)))
                .unwrap_or_default()
        )
    });

    let mut ctx = serde_json::json!({
        "date": today.to_string(),
        "capacity": cap.summary(),
        "in_focus": focus,
        "todos": todos,
        "blocks": blocks,
        "all_day": all_day,
        "overdue": overdue,
        "instruction": "The user's shared to-do list for today. Manage it with the HOBBES_TODO_* / HOBBES_PLAN_DAY / HOBBES_TIME_BLOCK tools. Projects: look them up with HOBBES_PROJECT_LIST (ids, titles, aliases) and file to-dos into an existing one by id; create a project only when none fits. Setting a todo's status to in_progress starts focus mode on it (only one at a time); completing or reopening it ends the session. Blocks marked (meeting) and the all_day list mirror the user's external calendar — fixed commitments that cannot be moved or deleted; plan around them (HOBBES_CALENDAR_LIST shows more days). When present, `fleet` lists today's external Claude Code coding-agent sessions (name — active time — latest summary); read-only observations, not managed by any tool.",
    });
    // Per-actor focus minutes for today, so the model can reason about person
    // vs agent load. Only when nonzero — an untouched day stays compact.
    if person_done_minutes > 0 {
        ctx["person_done_minutes"] = serde_json::json!(person_done_minutes);
    }
    if agent_done_minutes > 0 {
        ctx["agent_done_minutes"] = serde_json::json!(agent_done_minutes);
    }
    // Symbiotic additions — each only when it has something to say, and
    // every minute figure rounded down to the step, so the block (a cached
    // ledger entry) doesn't re-render every request.
    if let Some(v) = your_time_context(planner, today, step) {
        ctx["your_time"] = v;
    }
    if let Some(v) = agent_lane_context(planner, today) {
        ctx["agents"] = v;
    }
    if let Some(v) = agent_time_context(planner, today, now, step) {
        ctx["agent_time"] = v;
    }
    Some(ctx)
}

/// Needs-you lines carried in `planner_today.agents`.
const CTX_MAX_NEEDS_YOU: usize = 3;

/// `{"today": "1h 30m protected (Reading 19:00–20:00)", "week": "Reading
/// 2h/4h · …"}` — `None` without goals or protected time today.
fn your_time_context(planner: &PlannerState, today: NaiveDate, step: u32) -> Option<Value> {
    let (protected, blocks) = super::measures::protected_on(&planner.blocks, today);
    if planner.time_goals.is_empty() && protected == 0 {
        return None;
    }
    let mut v = serde_json::json!({});
    if protected > 0 {
        let list: Vec<String> = blocks
            .iter()
            .take(4)
            .map(|b| {
                format!(
                    "{} {}–{}",
                    b.your_time_category().unwrap_or("your time"),
                    hhmm(b.start),
                    hhmm(b.end)
                )
            })
            .collect();
        v["today"] = serde_json::json!(format!(
            "{} protected ({}) — plan around it, never over it",
            model::format_minutes(protected),
            list.join(", ")
        ));
    }
    let week = super::measures::your_time_week(
        &planner.time_goals,
        &planner.blocks,
        &planner.todos,
        &planner.focus_sessions,
        today,
    );
    if !week.is_empty() {
        v["week"] = serde_json::json!(week.compact(step));
    }
    Some(v)
}

/// `{"summary": "2 working · 1 needs you", "needs_you": [...]}` — `None`
/// when the agent lane is empty.
fn agent_lane_context(planner: &PlannerState, today: NaiveDate) -> Option<Value> {
    let lane = super::lanes::agent_lane(&planner.todos, today);
    if lane.is_empty() {
        return None;
    }
    let mut v = serde_json::json!({ "summary": lane.counts_line() });
    let needs: Vec<String> = lane
        .needs_you
        .iter()
        .take(CTX_MAX_NEEDS_YOU)
        .filter_map(|t| super::lanes::decision_line(t))
        .map(|l| truncate_title(&l))
        .collect();
    if !needs.is_empty() {
        v["needs_you"] = serde_json::json!(needs);
        v["instruction"] = serde_json::json!(
            "Agent work waiting on the user's judgment — HOBBES_DECISIONS lists and resolves it."
        );
    }
    Some(v)
}

/// `"30m of 45m left today"` (+ the running intent) when a grant is on for
/// today; `None` otherwise.
fn agent_time_context(
    planner: &PlannerState,
    today: NaiveDate,
    now: DateTime<Utc>,
    step: u32,
) -> Option<Value> {
    let b = planner.agent_time_budget(today, now);
    if !b.enabled || !b.granted_today {
        return None;
    }
    let left = b.minutes_left() / step * step;
    let mut line = format!(
        "{} of {} left today",
        model::format_minutes(left),
        model::format_minutes(b.minutes_granted)
    );
    if let Some(s) = planner.open_agent_time_session() {
        line.push_str(&format!(" — running: {}", truncate_title(&s.intent)));
    }
    Some(serde_json::json!(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TODAY: &str = "2026-08-12";

    fn today() -> NaiveDate {
        TODAY.parse().unwrap()
    }

    fn create(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_todo_create(state, &args, "sess-1", today(), false)
    }

    fn update(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_todo_update(state, &args, "sess-1", today(), false)
    }

    fn list(state: &PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_todo_list(state, &args, today())
    }

    fn plan(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_plan_day(state, &args, 360, false, today(), false)
    }

    fn block(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_time_block(state, &args, today(), false)
    }

    fn upsert(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_project_upsert(state, &args, today(), false)
    }

    /// Create one todo and return its id.
    fn seed_todo(state: &mut PlannerState, title: &str, scheduled: &str) -> String {
        let (status, _) = create(
            state,
            json!({"todos": [{"title": title, "scheduled_for": scheduled}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        state.todos.last().unwrap().id.clone()
    }

    #[test]
    fn linked_session_sets_clears_and_auto_links() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Ship briefs", "today");

        // Explicit set (pre-resolved id arrives from the dispatch layer).
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "linked_session": "term-1"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todo(&id).unwrap().linked_fleet_session.as_deref(),
            Some("term-1")
        );

        // in_progress does NOT overwrite an existing link…
        let (status, _) =
            update(&mut state, json!({"updates": [{"id": id, "status": "in_progress"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todo(&id).unwrap().linked_fleet_session.as_deref(),
            Some("term-1")
        );

        // Explicit null clears.
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "linked_session": null}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.todo(&id).unwrap().linked_fleet_session.is_none());

        // …and starting focus on an UNLINKED todo auto-links the caller.
        let id2 = seed_todo(&mut state, "Second", "today");
        let (status, _) =
            update(&mut state, json!({"updates": [{"id": id2, "status": "in_progress"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todo(&id2).unwrap().linked_fleet_session.as_deref(),
            Some("sess-1")
        );
    }

    #[test]
    fn brief_progress_lands_on_linked_open_todos_only() {
        let mut state = PlannerState::default();
        let a = seed_todo(&mut state, "Linked open", "today");
        let b = seed_todo(&mut state, "Linked done", "today");
        let c = seed_todo(&mut state, "Unlinked", "today");
        state.todo_mut(&a).unwrap().linked_fleet_session = Some("s1".into());
        state.todo_mut(&b).unwrap().linked_fleet_session = Some("s1".into());
        state.complete_todo(&b, Utc::now());

        let changed = apply_brief_progress(&mut state, "s1", "Did the thing", Utc::now());
        assert_eq!(changed, vec![a.clone()]);
        assert_eq!(
            state.todo(&a).unwrap().latest_progress.as_deref(),
            Some("Did the thing")
        );
        assert!(state.todo(&b).unwrap().latest_progress.is_none());
        assert!(state.todo(&c).unwrap().latest_progress.is_none());
        assert_eq!(
            state.todo(&a).unwrap().status,
            TodoStatus::Open,
            "progress never drives status"
        );
        // Same line again → no-op (no churn, no updated_at bump).
        assert!(apply_brief_progress(&mut state, "s1", "Did the thing", Utc::now()).is_empty());

        // Pairs feed framing/status: only open linked todos.
        let pairs = linked_session_pairs(&state);
        assert_eq!(pairs, vec![("s1".to_string(), "Linked open".to_string())]);
    }

    /// Create a block linked to `todo_id` on `TODAY` and return its id.
    fn seed_linked_block(state: &mut PlannerState, todo_id: &str) -> String {
        let (status, _) = block(
            state,
            json!({"action": "create", "todo_id": todo_id, "date": "today",
                   "start": "09:00", "end": "10:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        state.blocks.last().unwrap().id.clone()
    }

    // ── schedule / timebox consistency ──────────────────────────────────────

    #[test]
    fn rescheduling_a_todo_prunes_its_stale_blocks() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        seed_linked_block(&mut state, &id);
        assert_eq!(state.blocks.len(), 1);

        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": id, "scheduled_for": "tomorrow"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(
            state.blocks.is_empty(),
            "a block on the old day must move off the calendar with the work"
        );
        assert!(out.contains("Removed 1 timeline block"), "response says so: {}", out);
    }

    #[test]
    fn unscheduling_clears_the_timebox_too() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        seed_linked_block(&mut state, &id);

        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "scheduled_for": null}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn update_without_touching_schedule_keeps_blocks() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        seed_linked_block(&mut state, &id);

        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": id, "title": "Renamed", "status": "completed"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.blocks.len(), 1, "completing is history, not rescheduling");
        assert!(!out.contains("Removed"), "no prune note: {}", out);
    }

    #[test]
    fn plan_day_moves_timeboxes_with_the_work() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        seed_linked_block(&mut state, &id);

        let (status, out) = plan(
            &mut state,
            json!({"date": "tomorrow", "items": [{"id": id, "estimate_minutes": 60}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.blocks.is_empty(), "yesterday's block must not squat on the calendar");
        assert!(out.contains("Removed 1 timeline block"), "{}", out);
    }

    #[test]
    fn resizing_a_linked_block_reestimates_the_todo() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        let block_id = seed_linked_block(&mut state, &id); // 09:00–10:00

        let (status, out) = block(
            &mut state,
            json!({"action": "move", "id": block_id, "end": "10:30"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todos[0].estimate_minutes,
            Some(90),
            "the capacity math must follow the calendar"
        );
        assert!(out.contains("Estimate updated to 1h 30m"), "{}", out);

        // A pure move (same duration) leaves the estimate alone.
        let (status, out) = block(
            &mut state,
            json!({"action": "move", "id": block_id, "start": "13:00", "end": "14:30"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.todos[0].estimate_minutes, Some(90));
        assert!(!out.contains("Estimate updated"), "{}", out);
    }

    #[test]
    fn tool_responses_and_context_use_the_live_todo_title() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Draft", "today");
        let block_id = seed_linked_block(&mut state, &id);

        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "title": "Renamed"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);

        // Tool response path (block_line).
        let (_, out) = block(
            &mut state,
            json!({"action": "move", "id": block_id, "start": "11:00", "end": "12:00"}),
        );
        assert!(out.contains("Renamed"), "block_line must resolve live: {}", out);
        assert!(!out.contains("Draft"), "{}", out);

        // Context path (planner_today_context).
        let settings = crate::settings::Settings::default();
        // The fixture pins TODAY; using the real clock here would make this
        // test expire at midnight.
        let ctx = planner_today_context(&state, &settings, today()).expect("planner context on");
        let blocks_json = ctx.get("blocks").unwrap().to_string();
        assert!(blocks_json.contains("Renamed"), "{}", blocks_json);
    }

    #[test]
    fn creating_a_block_schedules_its_todo_onto_that_day() {
        let mut state = PlannerState::default();
        // Scheduled yesterday — the exact "dropped it into today but the line
        // item kept the old date" report.
        let id = seed_todo(&mut state, "Email", "2026-08-11");

        let (status, out) = block(
            &mut state,
            json!({"action": "create", "todo_id": id, "date": "today",
                   "start": "10:45", "end": "11:45"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todos[0].scheduled_for,
            Some(today()),
            "the timebox is the schedule — the line item must follow the block"
        );
        assert!(out.contains("Scheduled 'Email' onto"), "{}", out);
    }

    #[test]
    fn moving_a_block_across_days_drags_the_schedule_and_prunes() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Email", "today");
        let block_id = seed_linked_block(&mut state, &id); // today 09:00–10:00

        let (status, out) = block(
            &mut state,
            json!({"action": "move", "id": block_id, "date": "tomorrow"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let tomorrow = today().succ_opt().unwrap();
        assert_eq!(state.todos[0].scheduled_for, Some(tomorrow));
        assert_eq!(state.blocks.len(), 1, "the moved block itself must survive the prune");
        assert!(out.contains("Scheduled 'Email' onto"), "{}", out);

        // Same-day move: schedule untouched, no note.
        let (status, out) = block(
            &mut state,
            json!({"action": "move", "id": block_id, "start": "13:00", "end": "14:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.todos[0].scheduled_for, Some(tomorrow));
        assert!(!out.contains("Scheduled"), "{}", out);
    }

    // ── focus mode ──────────────────────────────────────────────────────────

    #[test]
    fn setting_in_progress_starts_singular_focus() {
        let mut state = PlannerState::default();
        let a = seed_todo(&mut state, "First", "today");
        let b = seed_todo(&mut state, "Second", "today");

        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": a, "status": "in_progress"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(out.contains('▶'), "response shows the focus mark: {}", out);
        assert_eq!(state.focused().unwrap().id, a);

        // Focusing the second pauses the first.
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": b, "status": "in_progress"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.focused().unwrap().id, b);
        assert_eq!(
            state.todo(&a).unwrap().status,
            TodoStatus::Open,
            "single focus: the previous task is paused"
        );

        // Completing ends the session.
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": b, "status": "completed"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.focused().is_none());
        assert!(state.todo(&b).unwrap().started_at.is_none());
    }

    #[test]
    fn ai_focus_opens_an_agent_session_and_status_changes_close_it() {
        use crate::todo::model::FocusEndReason;
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Delegated", "today");

        // status: in_progress → an agent-actor row attributed to the chat
        // session the dispatcher passed (the update() helper's "sess-1").
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "status": "in_progress"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let open = state.open_focus_session_for(&id).expect("agent row opened");
        assert!(open.actor.is_agent());
        assert_eq!(open.actor.agent_session_id(), Some("sess-1"));

        // status: open is the agent's stop path — the row closes as 'stopped'
        // (banking an in-progress todo) and nothing stays live.
        let (status, _) = update(&mut state, json!({"updates": [{"id": id, "status": "open"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.open_focus_session_for(&id).is_none());
        assert_eq!(
            state.focus_sessions.last().unwrap().end_reason,
            Some(FocusEndReason::Stopped)
        );

        // Completing mid-focus closes as 'completed'.
        update(&mut state, json!({"updates": [{"id": id, "status": "in_progress"}]}));
        update(&mut state, json!({"updates": [{"id": id, "status": "completed"}]}));
        assert!(state.focus_sessions.iter().all(|s| !s.is_open()));
        assert_eq!(
            state.focus_sessions.last().unwrap().end_reason,
            Some(FocusEndReason::Completed)
        );

        // Cancelling mid-focus closes as 'cancelled'.
        let other = seed_todo(&mut state, "Doomed", "today");
        update(&mut state, json!({"updates": [{"id": other, "status": "in_progress"}]}));
        update(&mut state, json!({"updates": [{"id": other, "status": "cancelled"}]}));
        assert!(state.focus_sessions.iter().all(|s| !s.is_open()));
        assert_eq!(
            state.focus_sessions.last().unwrap().end_reason,
            Some(FocusEndReason::Cancelled)
        );
        // persist=false throughout: rows lived purely in memory, and the
        // dirty queue was drained by the handler itself.
        assert!(state.take_dirty_focus_sessions().is_empty());
    }

    #[test]
    fn ai_refocus_preempts_the_previous_agent_session() {
        use crate::todo::model::FocusEndReason;
        let mut state = PlannerState::default();
        let a = seed_todo(&mut state, "First", "today");
        let b = seed_todo(&mut state, "Second", "today");

        update(&mut state, json!({"updates": [{"id": a, "status": "in_progress"}]}));
        update(&mut state, json!({"updates": [{"id": b, "status": "in_progress"}]}));

        assert!(state.open_focus_session_for(&b).is_some());
        let a_row = state
            .focus_sessions
            .iter()
            .find(|s| s.todo_id == a)
            .unwrap();
        assert_eq!(a_row.end_reason, Some(FocusEndReason::Preempted));
    }

    #[test]
    fn context_reports_per_actor_done_minutes() {
        use crate::todo::model::{FocusActor, FocusEndReason, FocusSession};
        let mut state = PlannerState::default();
        seed_todo(&mut state, "Tracked", "today");
        let settings = crate::settings::Settings::default();

        // Untouched day: the keys stay absent — context is token-budgeted.
        let ctx = planner_today_context(&state, &settings, today()).expect("context on");
        assert!(ctx.get("person_done_minutes").is_none());
        assert!(ctx.get("agent_done_minutes").is_none());

        // Sessions land on the *real* local day — the context aggregates over
        // the `today` it is handed, so hand it that day. Local noon keeps the
        // sitting inside that day in any timezone at any test hour.
        use chrono::TimeZone;
        let real_today = chrono::Local::now().date_naive();
        let start = chrono::Local
            .from_local_datetime(&real_today.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        let todo_id = state.todos[0].id.clone();
        let mut person = FocusSession::open(&todo_id, start, FocusActor::Person);
        person.close(start + chrono::Duration::minutes(30), FocusEndReason::Paused);
        let mut agent = FocusSession::open(
            &todo_id,
            start,
            FocusActor::Agent {
                session_id: Some("sess-1".into()),
            },
        );
        agent.close(start + chrono::Duration::minutes(45), FocusEndReason::Completed);
        state.focus_sessions.push(person);
        state.focus_sessions.push(agent);

        let ctx = planner_today_context(&state, &settings, real_today).expect("context on");
        assert_eq!(ctx.get("person_done_minutes").unwrap().as_u64(), Some(30));
        assert_eq!(ctx.get("agent_done_minutes").unwrap().as_u64(), Some(45));
        // The capacity line keeps its familiar wording; the split rides in
        // the dedicated keys.
        assert!(!ctx.get("capacity").unwrap().as_str().unwrap().contains("agent"));
    }

    #[test]
    fn setting_in_progress_pulls_unscheduled_work_onto_today() {
        let mut state = PlannerState::default();
        let (_, _) = create(&mut state, json!({"todos": [{"title": "Surprise task"}]}));
        let id = state.todos[0].id.clone();
        assert_eq!(state.todos[0].scheduled_for, None);

        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": id, "status": "in_progress"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        // start_focus derives "today" from the real clock (focus is a live
        // act), so assert against the actual local day, not the fixture date.
        let real_today = chrono::Local::now().date_naive();
        assert_eq!(
            state.todo(&id).unwrap().scheduled_for,
            Some(real_today),
            "focus means working on it now, so it belongs on today's plan"
        );
        assert!(
            out.contains("Scheduled 'Surprise task' onto"),
            "the response must say the date moved: {}",
            out
        );
    }

    #[test]
    fn context_reports_the_focused_task() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Deep work", "today");
        let (_, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "estimate_minutes": 60, "status": "in_progress"}]}),
        );

        let settings = crate::settings::Settings::default();
        let ctx = planner_today_context(&state, &settings, today()).expect("context on");
        let focus = ctx.get("in_focus").unwrap().as_str().unwrap();
        assert!(focus.contains("Deep work"), "{}", focus);
        assert!(focus.contains("of 1h estimated"), "{}", focus);

        // No focus → explicit null, not a stale string.
        let (_, _) = update(&mut state, json!({"updates": [{"id": id, "status": "open"}]}));
        let ctx = planner_today_context(&state, &settings, today()).expect("context on");
        assert!(ctx.get("in_focus").unwrap().is_null());
    }

    #[test]
    fn reestimating_resizes_the_timebox_from_its_start() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Case study", "today");
        let block_id = seed_linked_block(&mut state, &id); // 09:00–10:00

        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": id, "estimate_minutes": 30}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let b = state.blocks.iter().find(|b| b.id == block_id).unwrap();
        let start_local = b.start.with_timezone(&chrono::Local);
        let end_local = b.end.with_timezone(&chrono::Local);
        assert_eq!(
            (end_local - start_local).num_minutes(),
            30,
            "the block must shrink from its anchored start"
        );
        assert_eq!(start_local.format("%H:%M").to_string(), "09:00");
        assert!(out.contains("Resized 1 timeline block"), "{}", out);

        // Clearing the estimate leaves the block untouched.
        let (status, out) = update(
            &mut state,
            json!({"updates": [{"id": id, "estimate_minutes": null}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let b = state.blocks.iter().find(|b| b.id == block_id).unwrap();
        assert_eq!((b.end - b.start).num_minutes(), 30);
        assert!(!out.contains("Resized"), "{}", out);
    }

    // ── create ──────────────────────────────────────────────────────────────

    #[test]
    fn create_a_single_todo_with_every_field() {
        let mut state = PlannerState::default();
        let (status, resp) = create(
            &mut state,
            json!({"todos": [{
                "title": "Draft the proposal",
                "notes": "Outline first.",
                "bucket": "anytime",
                "scheduled_for": "today",
                "time_of_day": "morning",
                "deadline": "2026-08-14",
                "estimate_minutes": 45,
                "tags": ["writing"],
                "checklist": ["outline", "first pass"]
            }]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains("Created 1 todo(s)"));
        assert!(resp.contains("Draft the proposal"));

        let t = &state.todos[0];
        assert_eq!(t.bucket, TodoBucket::Anytime);
        assert_eq!(t.scheduled_for, Some(today()));
        assert_eq!(t.time_of_day, Some(TimeOfDay::Morning));
        assert_eq!(t.deadline, Some("2026-08-14".parse().unwrap()));
        assert_eq!(t.estimate_minutes, Some(45));
        assert_eq!(t.tags, vec!["writing"]);
        assert_eq!(t.checklist.len(), 2);
        assert_eq!(
            t.origin,
            TodoOrigin::Ai { session_id: "sess-1".into() },
            "AI-created todos must record the originating session"
        );
    }

    #[test]
    fn create_a_batch_in_one_call() {
        let mut state = PlannerState::default();
        let (status, resp) = create(
            &mut state,
            json!({"todos": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("Created 3 todo(s)"));
        assert_eq!(state.todos.len(), 3);
        // Batch order is preserved in the sort keys.
        assert!(state.todos[0].sort_order < state.todos[1].sort_order);
        assert!(state.todos[1].sort_order < state.todos[2].sort_order);
    }

    #[test]
    fn create_applies_valid_items_and_reports_the_bad_ones() {
        let mut state = PlannerState::default();
        let (status, resp) = create(
            &mut state,
            json!({"todos": [
                {"title": "good"},
                {"notes": "no title"},
                {"title": "bad date", "scheduled_for": "next tuesday"}
            ]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert_eq!(state.todos.len(), 1, "the valid item must still land");
        assert!(resp.contains("valid items were applied"));
        assert!(resp.contains("todos[1]: missing required 'title'"));
        assert!(resp.contains("todos[2]"));
        assert!(resp.contains("YYYY-MM-DD"), "the error must teach the format");
    }

    #[test]
    fn create_accepts_tomorrow_and_rejects_garbage_dates() {
        let mut state = PlannerState::default();
        let (status, _) = create(
            &mut state,
            json!({"todos": [{"title": "t", "scheduled_for": "tomorrow"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(
            state.todos[0].scheduled_for,
            Some("2026-08-13".parse().unwrap())
        );

        let (status, resp) = create(
            &mut state,
            json!({"todos": [{"title": "t2", "deadline": "12/08/2026"}]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("invalid date"));
    }

    #[test]
    fn create_rejects_unknown_project() {
        let mut state = PlannerState::default();
        let (status, resp) = create(
            &mut state,
            json!({"todos": [{"title": "t", "project_id": "nope"}]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("unknown project_id 'nope'"));
        assert!(state.todos.is_empty());
    }

    #[test]
    fn create_without_todos_array_is_an_error() {
        let mut state = PlannerState::default();
        let (status, resp) = create(&mut state, json!({"title": "not an array"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("'todos'"));
    }

    // ── update ──────────────────────────────────────────────────────────────

    fn seeded_state() -> (PlannerState, String) {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [{"title": "Ship it", "scheduled_for": "today", "estimate_minutes": 30}]}),
        );
        let id = state.todos[0].id.clone();
        (state, id)
    }

    #[test]
    fn update_completes_and_reopens() {
        let (mut state, id) = seeded_state();

        let (status, resp) =
            update(&mut state, json!({"updates": [{"id": id, "status": "completed"}]}));
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert_eq!(state.todos[0].status, TodoStatus::Completed);
        assert!(state.todos[0].completed_at.is_some());
        assert!(resp.contains('✓'));

        let (status, _) = update(&mut state, json!({"updates": [{"id": id, "status": "open"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.todos[0].status, TodoStatus::Open);
        assert!(state.todos[0].completed_at.is_none());
    }

    #[test]
    fn update_cancel_records_the_closed_moment() {
        let (mut state, id) = seeded_state();
        let (status, _) =
            update(&mut state, json!({"updates": [{"id": id, "status": "cancelled"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.todos[0].status, TodoStatus::Cancelled);
        assert!(
            state.todos[0].completed_at.is_some(),
            "logbook ordering relies on completed_at for cancelled todos too"
        );
    }

    #[test]
    fn update_explicit_null_clears_optional_fields() {
        let (mut state, id) = seeded_state();
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "scheduled_for": null, "estimate_minutes": null}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.todos[0].scheduled_for.is_none(), "null must clear");
        assert!(state.todos[0].estimate_minutes.is_none(), "null must clear");
        // An omitted field is untouched.
        assert_eq!(state.todos[0].title, "Ship it");
    }

    #[test]
    fn update_patches_fields_without_disturbing_the_rest() {
        let (mut state, id) = seeded_state();
        let (status, _) = update(
            &mut state,
            json!({"updates": [{"id": id, "title": "Ship it properly", "tags": ["release"], "deadline": "tomorrow"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        let t = &state.todos[0];
        assert_eq!(t.title, "Ship it properly");
        assert_eq!(t.tags, vec!["release"]);
        assert_eq!(t.deadline, Some("2026-08-13".parse().unwrap()));
        assert_eq!(t.scheduled_for, Some(today()), "untouched field survives");
        assert_eq!(t.estimate_minutes, Some(30), "untouched field survives");
    }

    #[test]
    fn update_applies_valid_patches_and_lists_unknown_ids() {
        let (mut state, id) = seeded_state();
        let (status, resp) = update(
            &mut state,
            json!({"updates": [
                {"id": id, "title": "renamed"},
                {"id": "td_missing", "title": "ghost"}
            ]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert_eq!(state.todos[0].title, "renamed", "valid patch still applied");
        assert!(resp.contains("unknown id 'td_missing'"));
        assert!(resp.contains("valid items were applied"));
    }

    #[test]
    fn update_bad_field_leaves_the_todo_untouched() {
        let (mut state, id) = seeded_state();
        let (status, resp) = update(
            &mut state,
            json!({"updates": [{"id": id, "title": "half", "deadline": "not-a-date"}]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("invalid date"));
        assert_eq!(
            state.todos[0].title, "Ship it",
            "a patch with a bad field must not half-apply"
        );
    }

    // ── list ────────────────────────────────────────────────────────────────

    fn listing_fixture() -> PlannerState {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [
                {"title": "inbox item"},
                {"title": "today item", "bucket": "anytime", "scheduled_for": "today"},
                {"title": "future item", "bucket": "anytime", "scheduled_for": "2026-09-01"},
                {"title": "whenever item", "bucket": "anytime"},
                {"title": "someday item", "bucket": "someday"},
                {"title": "tagged item", "bucket": "anytime", "tags": ["deep-work"], "notes": "quarterly planning"}
            ]}),
        );
        let done_id = state.todos[1].id.clone();
        // Complete one so the logbook has an entry — but re-create a fresh
        // "today" item first so Today stays populated.
        create(
            &mut state,
            json!({"todos": [{"title": "second today item", "bucket": "anytime", "scheduled_for": "today"}]}),
        );
        update(
            &mut state,
            json!({"updates": [{"id": done_id, "status": "completed"}]}),
        );
        state
    }

    #[test]
    fn list_defaults_to_today() {
        let state = listing_fixture();
        let (status, resp) = list(&state, json!({}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.starts_with("today"));
        assert!(resp.contains("second today item"));
        assert!(!resp.contains("future item"));
    }

    #[test]
    fn list_each_named_view() {
        let state = listing_fixture();
        for (view, expect) in [
            ("inbox", "inbox item"),
            ("upcoming", "future item"),
            ("anytime", "whenever item"),
            ("someday", "someday item"),
            ("logbook", "today item"),
        ] {
            let (status, resp) = list(&state, json!({"view": view}));
            assert_eq!(status, ToolCallStatus::Completed, "view {view}");
            assert!(resp.contains(expect), "view {view} should list '{expect}': {resp}");
        }
    }

    #[test]
    fn list_upcoming_orders_by_date_then_sort() {
        // today() is 2026-08-12; creation order deliberately scrambles dates.
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [
                {"title": "late month", "scheduled_for": "2026-08-25"},
                {"title": "tomorrow item", "scheduled_for": "2026-08-13"},
                {"title": "late month too", "scheduled_for": "2026-08-25"}
            ]}),
        );
        let (status, resp) = list(&state, json!({"view": "upcoming"}));
        assert_eq!(status, ToolCallStatus::Completed);
        let pos = |needle: &str| resp.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(pos("tomorrow item") < pos("late month"), "dates ascend: {resp}");
        assert!(
            pos("late month") < pos("late month too"),
            "within a date, creation/sort order holds: {resp}"
        );
    }

    #[test]
    fn list_rejects_unknown_views() {
        let state = listing_fixture();
        let (status, resp) = list(&state, json!({"view": "everything"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("logbook"), "error should list the valid views");
    }

    #[test]
    fn list_filters_by_tag_text_and_dates() {
        let state = listing_fixture();

        let (_, resp) = list(&state, json!({"tag": "deep-work"}));
        assert!(resp.contains("tagged item"));
        assert!(!resp.contains("inbox item"));

        // Text matches notes as well as titles, case-insensitively.
        let (_, resp) = list(&state, json!({"text": "QUARTERLY"}));
        assert!(resp.contains("tagged item"));

        let (_, resp) = list(&state, json!({"date_from": "2026-08-20", "date_to": "2026-09-30"}));
        assert!(resp.contains("future item"));
        assert!(!resp.contains("today item"));
    }

    #[test]
    fn list_filters_by_project() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"projects": [{"title": "Hobbes"}]}));
        let project_id = state.projects[0].id.clone();
        create(
            &mut state,
            json!({"todos": [
                {"title": "in project", "project_id": project_id},
                {"title": "outside"}
            ]}),
        );

        let (_, resp) = list(&state, json!({"project_id": project_id}));
        assert!(resp.contains("in project"));
        assert!(!resp.contains("outside"));
    }

    #[test]
    fn list_rejects_bad_filter_dates() {
        let state = listing_fixture();
        let (status, resp) = list(&state, json!({"date_from": "soonish"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("date_from"));
    }

    #[test]
    fn list_truncates_at_fifty_lines() {
        let mut state = PlannerState::default();
        let todos: Vec<Value> = (0..60).map(|i| json!({"title": format!("item {}", i)})).collect();
        create(&mut state, json!({"todos": todos}));

        let (status, resp) = list(&state, json!({"view": "inbox"}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("…and 10 more"));
        // Header + 50 items + trailer.
        assert_eq!(resp.lines().count(), 52);
    }

    #[test]
    fn list_reports_an_empty_view_gracefully() {
        let state = PlannerState::default();
        let (status, resp) = list(&state, json!({"view": "someday"}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("0 todo(s)"));
    }

    // ── plan day ────────────────────────────────────────────────────────────

    #[test]
    fn plan_day_schedules_in_order_and_returns_capacity_math() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [
                {"title": "write", "estimate_minutes": 120},
                {"title": "review", "estimate_minutes": 60},
                {"title": "email"}
            ]}),
        );
        // Give ids in reverse creation order to prove the plan's order wins.
        let ids: Vec<String> = state.todos.iter().map(|t| t.id.clone()).collect();
        let (status, resp) = plan(
            &mut state,
            json!({"items": [
                {"id": ids[2], "estimate_minutes": 15},
                {"id": ids[0]},
                {"id": ids[1], "time_of_day": "afternoon"}
            ]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains("3h 15m planned"), "{resp}");
        assert!(resp.contains("2h 45m free"), "{resp}");
        assert!(!resp.contains("done"), "a freshly planned day has no done segment: {resp}");

        for t in &state.todos {
            assert_eq!(t.scheduled_for, Some(today()));
        }
        // The estimate override landed.
        assert_eq!(
            state.todos.iter().find(|t| t.title == "email").unwrap().estimate_minutes,
            Some(15)
        );
        assert_eq!(
            state.todos.iter().find(|t| t.title == "review").unwrap().time_of_day,
            Some(TimeOfDay::Afternoon)
        );
        // Ordering follows the items array: email < write < review.
        let key = |title: &str| {
            state.todos.iter().find(|t| t.title == title).unwrap().sort_order
        };
        assert!(key("email") < key("write"));
        assert!(key("write") < key("review"));

        // The day plan was recorded as planned.
        let plan_row = state.day_plans.iter().find(|p| p.date == today()).unwrap();
        assert!(plan_row.planned_at.is_some());
        assert_eq!(plan_row.capacity_minutes, 360);
    }

    #[test]
    fn plan_day_leads_with_the_overcommit_warning() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [{"title": "monster", "estimate_minutes": 660}]}),
        );
        let id = state.todos[0].id.clone();
        let (status, resp) = plan(&mut state, json!({"items": [{"id": id}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(
            resp.starts_with("OVERCOMMITTED"),
            "overcommitment must lead the response, got: {resp}"
        );
        assert!(resp.contains("over by 5h"), "{resp}");
    }

    #[test]
    fn plan_day_honours_capacity_override_and_persists_it() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [{"title": "deep work", "estimate_minutes": 200}]}),
        );
        let id = state.todos[0].id.clone();
        let (_, resp) = plan(
            &mut state,
            json!({"items": [{"id": id}], "capacity_minutes": 180, "date": "tomorrow"}),
        );
        assert!(resp.starts_with("OVERCOMMITTED"), "{resp}");
        assert!(resp.contains("over by 20m"), "{resp}");
        let tomorrow: NaiveDate = "2026-08-13".parse().unwrap();
        assert_eq!(state.capacity_for(tomorrow, 360), 180);
    }

    #[test]
    fn plan_day_flags_unestimated_work() {
        let mut state = PlannerState::default();
        create(&mut state, json!({"todos": [{"title": "fuzzy"}]}));
        let id = state.todos[0].id.clone();
        let (_, resp) = plan(&mut state, json!({"items": [{"id": id}]}));
        assert!(resp.contains("1 unestimated"), "{resp}");
    }

    #[test]
    fn plan_day_applies_known_ids_and_reports_unknown_ones() {
        let mut state = PlannerState::default();
        create(&mut state, json!({"todos": [{"title": "real", "estimate_minutes": 30}]}));
        let id = state.todos[0].id.clone();
        let (status, resp) = plan(
            &mut state,
            json!({"items": [{"id": id}, {"id": "td_ghost"}]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("unknown id 'td_ghost'"));
        assert!(resp.contains("valid items were still scheduled"));
        assert_eq!(state.todos[0].scheduled_for, Some(today()));
    }

    #[test]
    fn plan_day_rejects_bad_dates() {
        let mut state = PlannerState::default();
        let (status, resp) = plan(&mut state, json!({"items": [], "date": "someday"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("YYYY-MM-DD"));
    }

    // ── time blocks ─────────────────────────────────────────────────────────

    #[test]
    fn time_block_create_move_delete_lifecycle() {
        let mut state = PlannerState::default();
        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Focus", "date": TODAY, "start": "09:30", "end": "10:15"}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains("09:30–10:15 Focus"), "{resp}");
        assert_eq!(state.blocks.len(), 1);
        assert_eq!(state.blocks[0].duration_minutes(), 45);
        let id = state.blocks[0].id.clone();
        assert!(id.starts_with("blk_"));

        // Move only the times; the date carries over.
        let (status, resp) = block(
            &mut state,
            json!({"action": "move", "id": id, "start": "14:00", "end": "15:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains("14:00–15:00"), "{resp}");
        assert_eq!(state.blocks[0].duration_minutes(), 60);

        let (status, resp) = block(&mut state, json!({"action": "delete", "id": id}));
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn time_block_list_gives_ids_to_move_by() {
        let mut state = PlannerState::default();
        block(
            &mut state,
            json!({"action": "create", "title": "Focus", "date": TODAY, "start": "09:30", "end": "10:15"}),
        );
        let id = state.blocks[0].id.clone();
        let (status, resp) = block(&mut state, json!({"action": "list", "date": TODAY}));
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains(&format!("[{id}] 09:30–10:15 Focus")), "{resp}");
        assert_eq!(state.blocks.len(), 1, "list must not change the timeline");

        let (_, resp) = block(&mut state, json!({"action": "list", "date": "tomorrow"}));
        assert!(resp.starts_with("No blocks"), "{resp}");
    }

    #[test]
    fn time_block_takes_the_title_from_a_linked_todo() {
        let mut state = PlannerState::default();
        create(&mut state, json!({"todos": [{"title": "Draft the proposal"}]}));
        let todo_id = state.todos[0].id.clone();
        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "todo_id": todo_id, "date": "today", "start": "09:00", "end": "09:45"}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert_eq!(state.blocks[0].title, "Draft the proposal");
        assert_eq!(state.blocks[0].todo_id, Some(todo_id));
    }

    #[test]
    fn time_block_warns_on_overlap_without_failing() {
        let mut state = PlannerState::default();
        block(
            &mut state,
            json!({"action": "create", "title": "Standup", "date": TODAY, "start": "10:00", "end": "10:30"}),
        );
        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Focus", "date": TODAY, "start": "10:15", "end": "11:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "overlap is a warning, not an error");
        assert!(resp.contains("Warning — overlaps"), "{resp}");
        assert!(resp.contains("Standup"), "{resp}");
        assert_eq!(state.blocks.len(), 2, "the overlapping block is still created");

        // Back-to-back blocks are clean.
        let (_, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Next", "date": TODAY, "start": "11:00", "end": "11:30"}),
        );
        assert!(!resp.contains("Warning"), "{resp}");
    }

    #[test]
    fn time_block_move_warns_when_landing_on_another_block() {
        let mut state = PlannerState::default();
        block(
            &mut state,
            json!({"action": "create", "title": "A", "date": TODAY, "start": "09:00", "end": "10:00"}),
        );
        block(
            &mut state,
            json!({"action": "create", "title": "B", "date": TODAY, "start": "11:00", "end": "12:00"}),
        );
        let b_id = state.blocks[1].id.clone();
        let (status, resp) = block(
            &mut state,
            json!({"action": "move", "id": b_id, "start": "09:30", "end": "10:30"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("Warning — overlaps"), "{resp}");
    }

    #[test]
    fn non_busy_external_blocks_do_not_trigger_overlap_warnings() {
        let mut state = PlannerState::default();
        // A free/focus-time mirror 9–11 and a busy meeting 14–15, both today.
        seed_external_block(&mut state, "focus-hold", 9, 11);
        mark_block_free(&mut state, "ext_focus-hold");
        seed_external_block(&mut state, "standup", 14, 15);

        // Create over the free block: clean, no warning.
        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Deep work", "date": TODAY, "start": "09:00", "end": "10:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(!resp.contains("Warning"), "free time doesn't clash: {resp}");

        // Move onto the busy meeting: still warns.
        let own_id = state
            .blocks
            .iter()
            .find(|b| b.title == "Deep work")
            .unwrap()
            .id
            .clone();
        let (_, resp) = block(
            &mut state,
            json!({"action": "move", "id": own_id, "start": "14:15", "end": "15:15"}),
        );
        assert!(resp.contains("Warning — overlaps"), "{resp}");

        // Move back over the free block: clean again on the move path too.
        let (_, resp) = block(
            &mut state,
            json!({"action": "move", "id": own_id, "start": "10:00", "end": "11:00"}),
        );
        assert!(!resp.contains("Warning"), "{resp}");
    }

    #[test]
    fn today_context_labels_free_and_tentative_meetings() {
        let mut state = PlannerState::default();
        seed_external_block(&mut state, "standup", 9, 10);
        seed_external_block(&mut state, "focus", 10, 12);
        mark_block_free(&mut state, "ext_focus");
        seed_external_block(&mut state, "maybe", 13, 14);
        mark_block_tentative(&mut state, "ext_maybe");

        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        let blocks: Vec<&str> = ctx["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(blocks[0].ends_with("(meeting)"), "{:?}", blocks);
        assert!(blocks[1].ends_with("(meeting, free)"), "{:?}", blocks);
        assert!(blocks[2].ends_with("(meeting, tentative)"), "{:?}", blocks);
    }

    #[test]
    fn time_block_validates_input() {
        let mut state = PlannerState::default();

        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "x", "date": TODAY, "start": "9am", "end": "10:00"}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("HH:MM"), "{resp}");

        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "x", "date": TODAY, "start": "11:00", "end": "10:00"}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("after"), "{resp}");

        let (status, resp) = block(&mut state, json!({"action": "move", "id": "blk_nope", "start": "09:00"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("unknown block id"), "{resp}");

        let (status, _) = block(&mut state, json!({"action": "explode"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(state.blocks.is_empty());
    }

    // ── projects & areas ────────────────────────────────────────────────────

    #[test]
    fn project_upsert_creates_and_updates() {
        let mut state = PlannerState::default();
        let (status, resp) = upsert(
            &mut state,
            json!({"areas": [{"title": "Work"}], "projects": []}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        let area_id = state.areas[0].id.clone();

        // A project can reference the area created in the previous call.
        let (status, resp) = upsert(
            &mut state,
            json!({"projects": [{"title": "Hobbes", "area_id": area_id, "deadline": "2026-09-01"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        let project = &state.projects[0];
        assert_eq!(project.title, "Hobbes");
        assert_eq!(project.area_id, Some(area_id.clone()));
        assert_eq!(project.deadline, Some("2026-09-01".parse().unwrap()));
        let project_id = project.id.clone();

        // Update by id patches without recreating.
        let (status, _) = upsert(
            &mut state,
            json!({"projects": [{"id": project_id, "status": "completed", "notes": "shipped"}],
                   "areas": [{"id": area_id, "title": "Deep Work"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects[0].status, TodoStatus::Completed);
        assert_eq!(state.projects[0].notes, "shipped");
        assert_eq!(state.areas[0].title, "Deep Work");
    }

    #[test]
    fn project_upsert_reports_bad_items_but_applies_the_rest() {
        let mut state = PlannerState::default();
        let (status, resp) = upsert(
            &mut state,
            json!({"projects": [
                {"title": "good"},
                {"id": "pr_ghost", "title": "ghost"},
                {"title": "bad area", "area_id": "ar_ghost"}
            ]}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert_eq!(state.projects.len(), 1);
        assert!(resp.contains("unknown project id 'pr_ghost'"));
        assert!(resp.contains("unknown area_id 'ar_ghost'"));
        assert!(resp.contains("valid items were applied"));
    }

    #[test]
    fn project_upsert_requires_something_to_do() {
        let mut state = PlannerState::default();
        let (status, _) = upsert(&mut state, json!({}));
        assert_eq!(status, ToolCallStatus::Error);
        let (status, _) = upsert(&mut state, json!({"projects": [], "areas": []}));
        assert_eq!(status, ToolCallStatus::Error);
        let (status, resp) = upsert(&mut state, json!({"areas": [{}]}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("missing required 'title'"));
    }

    #[test]
    fn project_upsert_without_id_matches_an_existing_name() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"projects": [{"title": "Hobbes (cai-hobbes)"}]}));
        let id = state.projects[0].id.clone();

        // Same name, different spelling → updated, not duplicated.
        let (status, resp) = upsert(
            &mut state,
            json!({"projects": [{"title": "cai_hobbes", "path": "~/Sites/cai-hobbes"}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains(&format!("matched existing project [{id}] 'Hobbes (cai-hobbes)'")), "{resp}");
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects[0].path.as_deref(), Some("~/Sites/cai-hobbes"));
        // The requested spelling normalizes to an existing name: no alias.
        assert!(state.projects[0].aliases.is_empty());

        // A parenthetical's base already names it: matched, no alias.
        upsert(&mut state, json!({"projects": [{"title": "Hobbes"}]}));
        assert_eq!(state.projects.len(), 1);
        assert!(state.projects[0].aliases.is_empty());
        // A new spelling that only exactly matches compactly becomes an alias.
        let (_, resp) = upsert(&mut state, json!({"projects": [{"title": "caihobbes"}]}));
        assert!(resp.contains("matched existing project"), "{resp}");
        assert_eq!(state.projects[0].aliases, vec!["caihobbes"]);

        // create_new never overrides an exact match.
        let (_, resp) = upsert(&mut state, json!({"projects": [{"title": "CAI-Hobbes", "create_new": true}]}));
        assert!(resp.contains("matched existing project"), "{resp}");
        assert_eq!(state.projects.len(), 1);
    }

    #[test]
    fn project_upsert_refuses_similar_names_unless_create_new() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"projects": [{"title": "Exceller8 AC-3 Chart Review"}]}));
        let id = state.projects[0].id.clone();

        let (status, resp) = upsert(&mut state, json!({"projects": [{"title": "cai-exceller8"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("not created"), "{resp}");
        assert!(resp.contains(&format!("[{id}] 'Exceller8 AC-3 Chart Review'")), "{resp}");
        assert!(resp.contains("create_new: true"), "{resp}");
        assert_eq!(state.projects.len(), 1);

        let (status, resp) = upsert(
            &mut state,
            json!({"projects": [{"title": "cai-exceller8", "create_new": true}]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert!(resp.contains("(created)"), "{resp}");
        assert_eq!(state.projects.len(), 2);

        // Unrelated names create without ceremony.
        let (status, _) = upsert(&mut state, json!({"projects": [{"title": "Garbage Vibe Kids"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.projects.len(), 3);
    }

    #[test]
    fn project_upsert_names_existing_twins_instead_of_adding_a_third() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"projects": [{"title": "Garbage Vibe Kids"}]}));
        // An old twin from before dedupe existed.
        let mut twin = state.projects[0].clone();
        twin.id = "pr_twin".into();
        state.projects.push(twin);

        let (status, resp) = upsert(&mut state, json!({"projects": [{"title": "garbage vibe kids"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("several existing projects"), "{resp}");
        assert!(resp.contains("[pr_twin]"), "{resp}");
        assert!(resp.contains("Manage projects"), "{resp}");
        assert_eq!(state.projects.len(), 2);
    }

    #[test]
    fn project_upsert_sets_aliases_and_archives() {
        let mut state = PlannerState::default();
        upsert(
            &mut state,
            json!({"projects": [{"title": "Hobbes", "aliases": ["cai-hobbes", "hobbes", " "]}]}),
        );
        assert_eq!(state.projects[0].aliases, vec!["cai-hobbes"]);
        let id = state.projects[0].id.clone();
        let (status, _) = upsert(&mut state, json!({"projects": [{"id": id, "status": "archived"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(state.projects[0].status, TodoStatus::Cancelled);
        // Matched by alias even when archived — and told how to reopen.
        let (_, resp) = upsert(&mut state, json!({"projects": [{"title": "CAI hobbes"}]}));
        assert!(resp.contains("archived"), "{resp}");
        assert!(resp.contains("status 'open'"), "{resp}");
        assert_eq!(state.projects.len(), 1);
    }

    #[test]
    fn area_upsert_without_id_matches_an_existing_area() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"areas": [{"title": "Deep Work"}]}));
        let id = state.areas[0].id.clone();
        let (status, resp) = upsert(&mut state, json!({"areas": [{"title": "deep-work"}]}));
        assert_eq!(status, ToolCallStatus::Completed, "{resp}");
        assert_eq!(state.areas.len(), 1, "no duplicate area: {resp}");
        assert!(resp.contains(&format!("matched existing area [{id}]")), "{resp}");
        // Renaming by id onto another area's name is refused.
        upsert(&mut state, json!({"areas": [{"title": "Home"}]}));
        let (status, resp) = upsert(&mut state, json!({"areas": [{"id": id, "title": "home"}]}));
        assert_eq!(status, ToolCallStatus::Error, "{resp}");
        assert_eq!(state.areas.len(), 2);
    }

    #[test]
    fn project_list_lists_areas_with_ids_first() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"areas": [{"title": "Work"}], "projects": [{"title": "Hobbes"}]}));
        let area = state.areas[0].id.clone();
        let (_, out) = handle_project_list(&state, &json!({}));
        assert!(out.starts_with(&format!("Areas (set a project's area_id): [{area}] Work\n1 project(s):")), "{out}");
    }

    #[test]
    fn project_merge_validation_names_the_fix() {
        let mut state = PlannerState::default();
        upsert(&mut state, json!({"projects": [{"title": "Hobbes"}, {"title": "Taxes"}, {"title": "Exceller"}]}));
        let ids: Vec<String> = state.projects.iter().map(|p| p.id.clone()).collect();
        let ok = validate_project_merge(&state, &json!({"keep_id": ids[0], "merge_ids": [ids[1], ids[2], ids[1]]}));
        assert_eq!(ok, Ok((ids[0].clone(), vec![ids[1].clone(), ids[2].clone()])), "duplicates collapse");
        let err = |args: Value| validate_project_merge(&state, &args).unwrap_err();
        assert!(err(json!({"merge_ids": [ids[1]]})).contains("keep_id"));
        assert!(err(json!({"keep_id": "nope", "merge_ids": [ids[1]]})).contains("unknown project id 'nope'"));
        assert!(err(json!({"keep_id": ids[0]})).contains("merge_ids"));
        assert!(err(json!({"keep_id": ids[0], "merge_ids": []})).contains("empty"));
        assert!(err(json!({"keep_id": ids[0], "merge_ids": [ids[0]]})).contains("can't also be"));
        assert!(err(json!({"keep_id": ids[0], "merge_ids": ["ghost"]})).contains("unknown project id 'ghost'"));
        assert!(err(json!({"keep_id": ids[0], "merge_ids": [3]})).contains("strings"));

        // The approval prompt's statement.
        let mut t = Todo::new("x", 0.0);
        t.project_id = Some(ids[1].clone());
        state.todos.push(t);
        let preview = project_merge_preview(&state, &ids[0], &[ids[1].clone()], Some(2));
        assert!(preview.contains("Merge \u{201c}Taxes\u{201d} into \u{201c}Hobbes\u{201d}"), "{preview}");
        assert!(preview.contains("1 to-do and 2 chats"), "{preview}");
        assert!(preview.contains("can't be undone"), "{preview}");
    }

    #[test]
    fn project_list_shows_ids_filters_and_flags_duplicates() {
        let mut state = PlannerState::default();
        upsert(
            &mut state,
            json!({"projects": [
                {"title": "Hobbes", "aliases": ["cai-hobbes"], "path": "~/Sites/cai-hobbes"},
                {"title": "Garbage Vibe Kids"},
                {"title": "Taxes 2026", "status": "completed"}
            ]}),
        );
        let hobbes = state.projects[0].id.clone();
        create(&mut state, json!({"todos": [{"title": "Ship RC", "project_id": hobbes}]}));
        // A pre-existing twin.
        let mut twin = state.projects[1].clone();
        twin.id = "pr_twin".into();
        state.projects.push(twin);

        let (status, out) = handle_project_list(&state, &json!({}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(out.starts_with("3 project(s):"), "{out}");
        assert!(out.contains(&format!("[{hobbes}] Hobbes (aka cai-hobbes) · path: ~/Sites/cai-hobbes · open · 1 open to-do · last activity")), "{out}");
        assert!(!out.contains("Taxes 2026"), "closed hidden by default: {out}");
        assert!(out.contains("Possible duplicates"), "{out}");
        assert!(out.contains("[pr_twin] 'Garbage Vibe Kids'"), "{out}");

        let (_, out) = handle_project_list(&state, &json!({"include_closed": true}));
        assert!(out.contains("Taxes 2026") && out.contains("· completed ·"), "{out}");

        // Query: by alias, by part of a name, and a miss.
        let (_, out) = handle_project_list(&state, &json!({"query": "cai_hobbes"}));
        assert!(out.starts_with("1 project(s):") && out.contains("Hobbes"), "{out}");
        let (_, out) = handle_project_list(&state, &json!({"query": "vibe"}));
        assert!(out.starts_with("2 project(s):"), "{out}");
        let (_, out) = handle_project_list(&state, &json!({"query": "taxes"}));
        assert!(out.contains("No open project matches 'taxes'"), "{out}");
        assert!(out.contains("include_closed"), "{out}");

        let (_, out) = handle_project_list(&PlannerState::default(), &json!({}));
        assert!(out.starts_with("No projects yet"), "{out}");
    }

    // ── external meetings vs capacity ───────────────────────────────────────

    /// An external (calendar-mirrored) block on TODAY, `from_h`:00–`to_h`:00
    /// local time.
    fn seed_external_block(state: &mut PlannerState, uid: &str, from_h: u32, to_h: u32) {
        use chrono::TimeZone;
        let at = |h: u32| {
            chrono::Local
                .from_local_datetime(&today().and_hms_opt(h, 0, 0).unwrap())
                .earliest()
                .unwrap()
                .with_timezone(&Utc)
        };
        state.blocks.push(crate::todo::model::TimeBlock {
            id: format!("ext_{}", uid),
            todo_id: None,
            title: uid.into(),
            start: at(from_h),
            end: at(to_h),
            source: crate::todo::model::BlockSource::External {
                uid: uid.into(),
                subscription_id: Some("sub1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        });
    }

    #[test]
    fn external_blocks_refuse_move_and_delete() {
        // Regression: the AI could "move"/"delete" a mirrored calendar block,
        // report success, and have the next sync pass silently revert it.
        let mut state = PlannerState::default();
        seed_external_block(&mut state, "standup", 9, 10);

        let (status, resp) = block(
            &mut state,
            json!({"action": "move", "id": "ext_standup", "start": "14:00", "end": "15:00"}),
        );
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("cannot move it"), "got: {resp}");

        let (status, resp) = block(&mut state, json!({"action": "delete", "id": "ext_standup"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("cannot delete it"), "got: {resp}");
        assert_eq!(state.blocks.len(), 1, "the mirrored block must survive");
    }

    /// Flip an already-seeded external block to non-busy (free/focus time).
    fn mark_block_free(state: &mut PlannerState, block_id: &str) {
        let block = state
            .blocks
            .iter_mut()
            .find(|b| b.id == block_id)
            .expect("seeded block exists");
        if let crate::todo::model::BlockSource::External { ref mut busy, .. } = block.source {
            *busy = false;
        }
    }

    /// Flip an already-seeded external block to tentative.
    fn mark_block_tentative(state: &mut PlannerState, block_id: &str) {
        let block = state
            .blocks
            .iter_mut()
            .find(|b| b.id == block_id)
            .expect("seeded block exists");
        if let crate::todo::model::BlockSource::External {
            ref mut tentative, ..
        } = block.source
        {
            *tentative = true;
        }
    }

    #[test]
    fn plan_day_counts_meetings_as_planned_when_enabled() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Deep work", "today");
        // 9–12 local: 3h of meetings against a 6h day.
        seed_external_block(&mut state, "offsite", 9, 12);

        let args = json!({"items": [{"id": id, "estimate_minutes": 240}]});
        let (status, out) =
            handle_plan_day(&mut state, &args, 360, true, today(), false);
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(
            out.starts_with("OVERCOMMITTED"),
            "4h of tasks + 3h of meetings into a 6h day must lead the response: {}",
            out
        );
        assert!(out.contains("7h planned"), "meetings join planned: {}", out);

        // Toggle off: the old todo-only math, and the day fits.
        let (_, out) = handle_plan_day(&mut state, &args, 360, false, today(), false);
        assert!(!out.starts_with("OVERCOMMITTED"), "{}", out);
    }

    #[test]
    fn today_context_capacity_reflects_meetings() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [{"title": "t", "scheduled_for": "today", "estimate_minutes": 60}]}),
        );
        seed_external_block(&mut state, "standup", 12, 14);

        // On: 1h of tasks + 2h of meetings = 3h planned against a full 6h day.
        let on = ctx_settings(true, true); // counts_against_capacity defaults true
        let ctx = planner_today_context(&state, &on, today()).unwrap();
        assert_eq!(ctx["capacity"], "3h planned · 3h free");

        let mut off = ctx_settings(true, true);
        off.planner_calendar_counts_against_capacity = false;
        let ctx = planner_today_context(&state, &off, today()).unwrap();
        assert_eq!(ctx["capacity"], "1h planned · 5h free");
    }

    #[test]
    fn today_context_capacity_ignores_tentative_and_free_meetings() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [{"title": "t", "scheduled_for": "today", "estimate_minutes": 60}]}),
        );
        seed_external_block(&mut state, "maybe", 9, 11);
        mark_block_tentative(&mut state, "ext_maybe");
        seed_external_block(&mut state, "hold", 12, 14);
        mark_block_free(&mut state, "ext_hold");

        // Neither the tentative nor the free mirror adds planned time.
        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        assert_eq!(ctx["capacity"], "1h planned · 5h free");
    }

    // ── planner_today context ───────────────────────────────────────────────

    fn ctx_settings(enabled: bool, inject: bool) -> crate::settings::Settings {
        crate::settings::Settings {
            planner_enabled: enabled,
            planner_inject_today_context: inject,
            ..Default::default()
        }
    }

    #[test]
    fn today_context_is_none_when_disabled() {
        let state = PlannerState::default();
        assert!(planner_today_context(&state, &ctx_settings(false, true), today()).is_none());
        assert!(planner_today_context(&state, &ctx_settings(true, false), today()).is_none());
        assert!(planner_today_context(&state, &ctx_settings(true, true), today()).is_some());
    }

    #[test]
    fn today_context_carries_capacity_todos_and_overdue() {
        let mut state = PlannerState::default();
        create(
            &mut state,
            json!({"todos": [
                {"title": "planned", "scheduled_for": "today", "estimate_minutes": 205},
                {"title": "late", "deadline": "2026-08-09"}
            ]}),
        );
        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        assert_eq!(ctx["date"], TODAY);
        assert_eq!(ctx["capacity"], "3h 25m planned · 2h 35m free");
        let todos = ctx["todos"].as_array().unwrap();
        assert_eq!(todos.len(), 1);
        assert!(todos[0].as_str().unwrap().contains("planned"));
        let overdue = ctx["overdue"].as_array().unwrap();
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].as_str().unwrap().contains("was due 2026-08-09"));
        assert!(ctx["instruction"].as_str().unwrap().contains("HOBBES_PLAN_DAY"));
    }

    #[test]
    fn today_context_enforces_the_hard_caps() {
        let mut state = PlannerState::default();
        let todos: Vec<Value> = (0..25)
            .map(|i| json!({"title": format!("t{}", i), "scheduled_for": "today"}))
            .collect();
        create(&mut state, json!({"todos": todos}));
        let overdue: Vec<Value> = (0..8)
            .map(|i| json!({"title": format!("od{}", i), "deadline": "2026-08-01"}))
            .collect();
        create(&mut state, json!({"todos": overdue}));
        // Cluster the blocks around local noon so the local→UTC conversion
        // cannot push any of them across a UTC date boundary in any timezone.
        for i in 0..12 {
            block(
                &mut state,
                json!({"action": "create", "title": format!("b{}", i), "date": TODAY,
                       "start": format!("12:{:02}", i * 4), "end": format!("12:{:02}", i * 4 + 3)}),
            );
        }

        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        assert_eq!(ctx["todos"].as_array().unwrap().len(), 20);
        assert_eq!(ctx["blocks"].as_array().unwrap().len(), 10);
        assert_eq!(ctx["overdue"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn today_context_truncates_long_titles() {
        let mut state = PlannerState::default();
        let long_title = "x".repeat(200);
        create(
            &mut state,
            json!({"todos": [{"title": long_title, "scheduled_for": "today"}]}),
        );
        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        let line = ctx["todos"][0].as_str().unwrap();
        assert!(line.contains('…'));
        assert!(!line.contains(&"x".repeat(100)));
    }

    // ── context interleaving & calendar (Phase 5) ───────────────────────────

    /// A UTC instant at `h`:`m` **local** on the test day, so assertions are
    /// timezone-independent (the same trick as `seed_external_block`).
    fn local_utc(day: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        chrono::Local
            .from_local_datetime(&day.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cal_sub(id: &str, name: &str, enabled: bool) -> crate::settings::CalendarSubscription {
        crate::settings::CalendarSubscription {
            id: id.into(),
            name: name.into(),
            color: "#4f9cf9".into(),
            enabled,
            source: crate::settings::CalendarSource::Ics {},
        }
    }

    fn cal_ev(
        sub: &str,
        uid: &str,
        title: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        all_day: bool,
    ) -> CalendarEvent {
        CalendarEvent {
            uid: uid.into(),
            subscription_id: sub.into(),
            title: title.into(),
            start,
            end,
            all_day,
            url: None,
            location: None,
            busy: true,
            tentative: false,
        }
    }

    /// An all-day event covering exactly the local day `d`.
    fn all_day_ev(sub: &str, uid: &str, title: &str, d: NaiveDate) -> CalendarEvent {
        let midnight = local_utc(d, 0, 0);
        cal_ev(sub, uid, title, midnight, midnight + chrono::Duration::days(1), true)
    }

    #[test]
    fn today_context_keeps_own_timeboxes_on_meeting_heavy_days() {
        let mut state = PlannerState::default();
        // 12 meetings (over the 8-meeting lane) crowd the day...
        for i in 0..12 {
            seed_external_block(&mut state, &format!("mtg{:02}", i), 6 + i, 7 + i);
        }
        // ...and 3 own timeboxes land late, after every meeting has started.
        for i in 0..3 {
            block(
                &mut state,
                json!({"action": "create", "title": format!("focus{}", i), "date": TODAY,
                       "start": format!("19:{:02}", i * 10), "end": format!("19:{:02}", i * 10 + 5)}),
            );
        }

        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        let blocks: Vec<&str> = ctx["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Own timeboxes survive AND lead the list, despite starting last.
        assert_eq!(blocks.len(), 3 + 8, "3 own + capped meetings: {:?}", blocks);
        for line in &blocks[..3] {
            assert!(line.contains("focus"), "own blocks first: {:?}", blocks);
            assert!(!line.contains("(meeting)"), "{:?}", blocks);
        }
        for line in &blocks[3..] {
            assert!(line.contains("(meeting)"), "meetings marked: {:?}", blocks);
        }
        // Meetings are time-sorted within their lane and cap at the earliest 8.
        assert!(blocks[3].contains("mtg00") && blocks[10].contains("mtg07"), "{:?}", blocks);
    }

    #[test]
    fn today_context_caps_own_blocks_and_meetings_separately() {
        let mut state = PlannerState::default();
        for i in 0..12 {
            block(
                &mut state,
                json!({"action": "create", "title": format!("b{}", i), "date": TODAY,
                       "start": format!("12:{:02}", i * 4), "end": format!("12:{:02}", i * 4 + 3)}),
            );
        }
        for i in 0..10 {
            seed_external_block(&mut state, &format!("m{}", i), 6 + i, 7 + i);
        }
        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        let blocks = ctx["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), CTX_MAX_OWN_BLOCKS + CTX_MAX_MEETINGS);
    }

    #[test]
    fn today_context_lists_capped_all_day_events_from_enabled_subs() {
        let state = PlannerState::default();
        let mut settings = ctx_settings(true, true);
        settings.planner_calendar_subscriptions =
            vec![cal_sub("s1", "Work", true), cal_sub("s2", "Personal", false)];

        let mut events: Vec<CalendarEvent> = (0..5)
            .map(|i| all_day_ev("s1", &format!("ad{}", i), &format!("Offsite {}", i), today()))
            .collect();
        // Disabled subscription and a different day: both invisible.
        events.push(all_day_ev("s2", "hidden", "Hidden holiday", today()));
        events.push(all_day_ev(
            "s1",
            "tomorrow",
            "Tomorrow only",
            today() + chrono::Duration::days(1),
        ));

        let ctx =
            planner_today_context_with_events(&state, &settings, today(), &events).unwrap();
        let all_day: Vec<&str> = ctx["all_day"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(all_day.len(), CTX_MAX_ALL_DAY, "{:?}", all_day);
        for line in &all_day {
            assert!(line.contains("(all day)"), "{:?}", all_day);
            assert!(!line.contains("Hidden") && !line.contains("Tomorrow"), "{:?}", all_day);
        }
    }

    #[test]
    fn today_context_all_day_is_empty_without_events() {
        let state = PlannerState::default();
        let ctx = planner_today_context_with_events(
            &state,
            &ctx_settings(true, true),
            today(),
            &[],
        )
        .unwrap();
        assert_eq!(ctx["all_day"].as_array().unwrap().len(), 0);
    }

    // ── HOBBES_CALENDAR_LIST ────────────────────────────────────────────────

    #[test]
    fn calendar_list_range_defaults_parses_and_clamps() {
        assert_eq!(calendar_list_range(&json!({}), today()).unwrap(), (today(), 1));
        assert_eq!(
            calendar_list_range(&json!({"start_date": "tomorrow", "days": 3}), today()).unwrap(),
            (today() + chrono::Duration::days(1), 3)
        );
        assert_eq!(
            calendar_list_range(&json!({"start_date": "2026-09-01"}), today()).unwrap(),
            ("2026-09-01".parse().unwrap(), 1)
        );
        // Clamped, not rejected.
        assert_eq!(calendar_list_range(&json!({"days": 0}), today()).unwrap().1, 1);
        assert_eq!(calendar_list_range(&json!({"days": -5}), today()).unwrap().1, 1);
        assert_eq!(
            calendar_list_range(&json!({"days": 99}), today()).unwrap().1,
            CALENDAR_LIST_MAX_DAYS
        );
        assert!(calendar_list_range(&json!({"start_date": "junk"}), today()).is_err());
        assert!(calendar_list_range(&json!({"days": "many"}), today()).is_err());
    }

    #[test]
    fn calendar_list_handler_rejects_bad_dates_before_touching_the_store() {
        let (status, resp) =
            handle_calendar_list(&json!({"start_date": "someday"}), &[], &[], today());
        assert_eq!(status, ToolCallStatus::Error);
        assert!(resp.contains("start_date"), "{resp}");
    }

    #[test]
    fn calendar_list_groups_by_day_and_marks_all_day() {
        let subs = vec![cal_sub("s1", "Work", true)];
        let tomorrow = today() + chrono::Duration::days(1);
        let events = vec![
            cal_ev("s1", "b", "Standup", local_utc(today(), 9, 0), local_utc(today(), 9, 15), false),
            cal_ev("s1", "a", "Review", local_utc(today(), 14, 0), local_utc(today(), 15, 0), false),
            all_day_ev("s1", "c", "Conference", today()),
            cal_ev("s1", "d", "1:1", local_utc(tomorrow, 10, 30), local_utc(tomorrow, 11, 0), false),
        ];
        let out = calendar_list_over(&events, &subs, &[], today(), 2, today());
        assert!(out.starts_with("Calendar — 4 event(s) Today to Tomorrow:"), "{out}");
        let today_pos = out.find("\nToday:").unwrap();
        let tomorrow_pos = out.find("\nTomorrow:").unwrap();
        assert!(today_pos < tomorrow_pos, "{out}");
        // All-day leads its day; timed events follow sorted by start.
        let allday_pos = out.find("(all day) Conference").unwrap();
        let standup_pos = out.find("09:00–09:15 Standup").unwrap();
        let review_pos = out.find("14:00–15:00 Review").unwrap();
        assert!(allday_pos < standup_pos && standup_pos < review_pos, "{out}");
        assert!(tomorrow_pos < out.find("10:30–11:00 1:1").unwrap(), "{out}");
    }

    #[test]
    fn calendar_list_empty_range_is_friendly() {
        let subs = vec![cal_sub("s1", "Work", true)];
        let out = calendar_list_over(&[], &subs, &[], today(), 1, today());
        assert_eq!(out, "No calendar events Today.");
        // A multi-day empty range names the span.
        let out = calendar_list_over(&[], &subs, &[], today(), 3, today());
        assert!(out.starts_with("No calendar events Today to"), "{out}");
    }

    #[test]
    fn calendar_list_notes_disabled_subscriptions_and_sync_errors() {
        let subs = vec![cal_sub("s1", "Work", true), cal_sub("s2", "Personal", false)];
        let events = vec![
            cal_ev("s1", "ok", "Kept", local_utc(today(), 9, 0), local_utc(today(), 10, 0), false),
            cal_ev("s2", "no", "Dropped", local_utc(today(), 11, 0), local_utc(today(), 12, 0), false),
        ];
        let errors = vec![("Work".to_string(), "HTTP 404".to_string())];
        let out = calendar_list_over(&events, &subs, &errors, today(), 1, today());
        assert!(out.contains("Kept"), "{out}");
        assert!(!out.contains("Dropped"), "disabled sub's events must be hidden: {out}");
        assert!(out.contains("1 event(s)"), "{out}");
        assert!(out.contains("'Personal' is disabled"), "{out}");
        assert!(out.contains("'Work' failed its last sync (HTTP 404)"), "{out}");
    }

    #[test]
    fn calendar_list_includes_location_when_present() {
        let subs = vec![cal_sub("s1", "Work", true)];
        let mut ev = cal_ev("s1", "x", "Lunch", local_utc(today(), 12, 0), local_utc(today(), 13, 0), false);
        ev.location = Some("Cafe Rio".into());
        let out = calendar_list_over(&[ev], &subs, &[], today(), 1, today());
        assert!(out.contains("12:00–13:00 Lunch @ Cafe Rio"), "{out}");
    }

    // ── busy-time verification (Phase 5) ────────────────────────────────────

    #[test]
    fn time_block_overlap_warning_includes_external_meetings() {
        let mut state = PlannerState::default();
        seed_external_block(&mut state, "boardmtg", 9, 12);
        let (status, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Focus", "date": TODAY,
                   "start": "10:00", "end": "11:00"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("Warning — overlaps"), "{resp}");
        assert!(resp.contains("boardmtg"), "the meeting must be named in the warning: {resp}");
    }

    #[test]
    fn time_block_move_overlap_warning_includes_external_meetings() {
        let mut state = PlannerState::default();
        seed_external_block(&mut state, "standup", 9, 10);
        let (_, resp) = block(
            &mut state,
            json!({"action": "create", "title": "Focus", "date": TODAY,
                   "start": "14:00", "end": "15:00"}),
        );
        assert!(!resp.contains("Warning"), "{resp}");
        let id = state.blocks.iter().find(|b| b.title == "Focus").unwrap().id.clone();
        let (status, resp) = block(
            &mut state,
            json!({"action": "move", "id": id, "start": "09:30", "end": "10:30"}),
        );
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(resp.contains("Warning — overlaps"), "{resp}");
        assert!(resp.contains("standup"), "{resp}");
    }

    // ── Symbiotic planner: lanes, protected time, decisions, goals ──────────

    #[test]
    fn create_takes_lane_kind_and_outcome() {
        let mut state = PlannerState::default();
        let (status, out) = create(
            &mut state,
            json!({"todos": [
                {"title": "Pay Comcast", "kind": "chore"},
                {"title": "Ship app-packs", "lane": "agent", "outcome": "Release tagged, tests passing"},
                {"title": "Read Dune", "kind": "your_time", "time_category": "Reading"},
                {"title": "Plain"}
            ]}),
        );
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        let t = |i: usize| &state.todos[i];
        assert_eq!((t(0).lane, t(0).kind), (Lane::Human, Some(HumanKind::Chore)));
        assert_eq!(t(1).lane, Lane::Agent);
        assert_eq!(t(1).agent_state, Some(model::AgentState::Queued));
        assert_eq!(t(1).outcome.as_deref(), Some("Release tagged, tests passing"));
        assert_eq!(t(2).time_category.as_deref(), Some("Reading"));
        assert_eq!((t(3).lane, t(3).kind), (Lane::Human, None), "human by default");
        assert!(out.contains("chore") && out.contains("agent: queued") && out.contains("your time: Reading"));
        assert!(out.contains("→ outcome: Release tagged"));

        let (status, out) = create(&mut state, json!({"todos": [{"title": "x", "lane": "agent", "kind": "chore"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("human to-dos only"), "{out}");
        let (status, _) = create(&mut state, json!({"todos": [{"title": "x", "lane": "robot"}]}));
        assert_eq!(status, ToolCallStatus::Error);
    }

    #[test]
    fn update_patches_lane_fields() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Renew passport", "today");
        update(&mut state, json!({"updates": [{"id": id, "kind": "chore"}]}));
        assert_eq!(state.todo(&id).unwrap().kind, Some(HumanKind::Chore));
        update(&mut state, json!({"updates": [{"id": id, "kind": null}]}));
        assert_eq!(state.todo(&id).unwrap().kind, None);

        update(&mut state, json!({"updates": [{"id": id, "kind": "chore"}]}));
        let (status, _) = update(&mut state, json!({"updates": [{"id": id, "lane": "agent", "outcome": "Form submitted"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        let t = state.todo(&id).unwrap();
        assert_eq!((t.lane, t.kind), (Lane::Agent, None), "entering the agent lane drops the kind");
        assert_eq!(t.outcome.as_deref(), Some("Form submitted"));
        let (status, _) = update(&mut state, json!({"updates": [{"id": id, "kind": "chore"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        update(&mut state, json!({"updates": [{"id": id, "outcome": null, "lane": "human"}]}));
        let t = state.todo(&id).unwrap();
        assert_eq!((t.lane, t.agent_state, t.outcome.clone()), (Lane::Human, None, None));

        // Completing an agent to-do marks it Done.
        update(&mut state, json!({"updates": [{"id": id, "lane": "agent"}]}));
        update(&mut state, json!({"updates": [{"id": id, "status": "completed"}]}));
        assert_eq!(state.todo(&id).unwrap().agent_state, Some(model::AgentState::Done));
    }

    fn goals(state: &mut PlannerState) {
        let (status, out) = handle_time_goals(
            state,
            &json!({"action": "set", "goals": [
                {"category": "Outside", "weekly_target_minutes": 240, "colour": "#8DBB7A"},
                {"category": "Reading", "weekly_target_minutes": 180}
            ]}),
            today(),
            false,
        );
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
    }

    #[test]
    fn time_goals_set_get_and_remove() {
        let mut state = PlannerState::default();
        let (_, out) = handle_time_goals(&mut state, &json!({}), today(), false);
        assert!(out.contains("No your-time goals yet"));
        goals(&mut state);
        assert_eq!(state.time_goals.len(), 2);
        // Upsert by category, case-insensitively.
        handle_time_goals(&mut state, &json!({"action": "set", "goals": [{"category": "outside", "weekly_target_minutes": 300}]}), today(), false);
        assert_eq!(state.time_goals.len(), 2);
        assert_eq!(state.time_goals[0].weekly_target_minutes, 300);
        assert_eq!(state.time_goals[0].colour.as_deref(), Some("#8DBB7A"), "untouched fields keep");

        block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Outside", "date": "today", "start": "12:30", "end": "13:30"}));
        let (status, out) = handle_time_goals(&mut state, &json!({"action": "get"}), today(), false);
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(out.contains("Outside: 1h / 5h goal — 4h to go (1h protected, 0m logged)"), "{out}");
        assert!(out.contains("Protected today: 1h."), "{out}");

        let (status, out) = handle_time_goals(&mut state, &json!({"action": "set", "remove": ["Reading", "Nope"], "goals": [{"category": "Gaming"}]}), today(), false);
        assert_eq!(status, ToolCallStatus::Error, "{out}");
        assert!(out.contains("needs weekly_target_minutes") && out.contains("no goal named 'Nope'"), "{out}");
        assert_eq!(state.time_goals.len(), 1, "valid removal still applied");
        let (status, _) = handle_time_goals(&mut state, &json!({"action": "set", "goals": [{"category": "X", "weekly_target_minutes": 60, "colour": "green"}]}), today(), false);
        assert_eq!(status, ToolCallStatus::Error);
    }

    #[test]
    fn your_time_blocks_snap_to_goal_spelling_and_flag_non_goals() {
        let mut state = PlannerState::default();
        let (_, out) = block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Break", "date": "today", "start": "15:00", "end": "15:15"}));
        assert!(out.contains("no your-time goals yet"), "{out}");
        goals(&mut state);
        let (_, out) = block(&mut state, json!({"action": "create", "kind": "your_time", "category": "outside", "date": "today", "start": "12:00", "end": "13:00"}));
        assert!(!out.contains("Note:"), "a goal needs no note: {out}");
        assert_eq!(state.blocks.last().unwrap().your_time_category(), Some("Outside"));
        let (_, out) = block(&mut state, json!({"action": "create", "kind": "your_time", "category": "break", "date": "today", "start": "16:00", "end": "16:15"}));
        assert!(out.contains("'Break' isn't one of the user's goals") && out.contains("Goals: Outside, Reading"), "{out}");
        assert_eq!(state.blocks.last().unwrap().your_time_category(), Some("Break"), "snaps to the in-use spelling");

        let (_, out) = handle_time_goals(&mut state, &json!({}), today(), false);
        assert!(out.contains("In use without a goal (last 8 weeks): Break."), "{out}");

        // Re-file one block on move (protected: needs the user's say-so).
        let id = state.blocks.last().unwrap().id.clone();
        let (status, _) = block(&mut state, json!({"action": "move", "id": id, "category": "Reading"}));
        assert_eq!(status, ToolCallStatus::Error, "protected guard");
        let (status, out) = block(&mut state, json!({"action": "move", "id": id, "category": "reading", "user_confirmed": true}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        let b = state.blocks.iter().find(|b| b.id == id).unwrap();
        assert_eq!((b.your_time_category(), b.title.as_str()), (Some("Reading"), "Reading"));
    }

    #[test]
    fn time_goals_rename_carries_blocks_and_merges_into_goals() {
        let mut state = PlannerState::default();
        goals(&mut state);
        block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Break", "date": "today", "start": "15:00", "end": "15:15"}));
        block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Outside", "date": "today", "start": "12:00", "end": "13:00"}));

        // A goal rename keeps the goal and moves its blocks.
        let (status, out) = handle_time_goals(&mut state, &json!({"action": "set", "rename": [{"from": "outside", "to": "Getting outside"}]}), today(), false);
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("Renamed 'outside' to 'Getting outside' (goal kept): 1 block(s)"), "{out}");
        assert!(state.time_goals.iter().any(|g| g.category == "Getting outside"));
        assert!(state.blocks.iter().any(|b| b.your_time_category() == Some("Getting outside")));

        // A non-goal category merges into a goal, in the goal's spelling.
        let (_, out) = handle_time_goals(&mut state, &json!({"action": "set", "rename": [{"from": "Break", "to": "getting OUTSIDE"}]}), today(), false);
        assert!(!out.contains("In use without a goal"), "{out}");
        assert!(state.blocks.iter().all(|b| b.your_time_category() == Some("Getting outside")));

        // Two goals don't silently merge; unknown categories are refused.
        let (status, out) = handle_time_goals(&mut state, &json!({"action": "set", "rename": [{"from": "Reading", "to": "Getting outside"}, {"from": "Nope", "to": "X"}]}), today(), false);
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("already a goal") && out.contains("no goal, block or to-do in 'Nope'"), "{out}");
        assert_eq!(state.time_goals.len(), 2);
    }

    #[test]
    fn protected_your_time_blocks_are_never_moved_without_the_user() {
        let mut state = PlannerState::default();
        let (status, out) = block(&mut state, json!({"action": "create", "kind": "your_time", "date": "today", "start": "19:00", "end": "20:00"}));
        assert_eq!(status, ToolCallStatus::Error, "needs a category: {out}");
        let (status, out) = block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Reading", "date": "today", "start": "19:00", "end": "20:00"}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        let yt = state.blocks[0].id.clone();
        assert!(state.blocks[0].is_protected());
        assert_eq!(state.blocks[0].title, "Reading");

        // Work can't be placed over it…
        let (status, out) = block(&mut state, json!({"action": "create", "title": "Deep work", "date": "today", "start": "19:30", "end": "20:30"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("protected your-time"), "{out}");
        // …unless the user explicitly asked.
        let (status, _) = block(&mut state, json!({"action": "create", "title": "Deep work", "date": "today", "start": "19:30", "end": "20:30", "override_protected": true}));
        assert_eq!(status, ToolCallStatus::Completed);

        // Moving or deleting it needs the user's say so.
        let (status, out) = block(&mut state, json!({"action": "move", "id": yt, "start": "21:00", "end": "22:00"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("user_confirmed"), "{out}");
        let (status, _) = block(&mut state, json!({"action": "delete", "id": yt}));
        assert_eq!(status, ToolCallStatus::Error);
        let (status, _) = block(&mut state, json!({"action": "move", "id": yt, "start": "21:00", "end": "22:00", "user_confirmed": true}));
        assert_eq!(status, ToolCallStatus::Completed);
        let (status, _) = block(&mut state, json!({"action": "delete", "id": yt, "user_confirmed": true}));
        assert_eq!(status, ToolCallStatus::Completed);
        assert!(state.blocks.iter().all(|b| !b.is_protected()));
    }

    #[test]
    fn plan_day_leads_with_your_time_skips_the_agent_lane_and_batches_chores() {
        let mut state = PlannerState::default();
        goals(&mut state);
        block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Outside", "date": "today", "start": "12:30", "end": "13:30"}));
        create(&mut state, json!({"todos": [
            {"title": "Draft SOW", "estimate_minutes": 60},
            {"title": "Pay bill", "kind": "chore", "estimate_minutes": 5},
            {"title": "Renew plates", "kind": "chore", "estimate_minutes": 10},
            {"title": "Ship release", "lane": "agent", "estimate_minutes": 600}
        ]}));
        let ids: Vec<String> = state.todos.iter().map(|t| t.id.clone()).collect();
        let items: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();

        let (status, out) = plan(&mut state, json!({"items": items}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.starts_with("Your time: 1h protected on 2026-08-12 (Outside 12:30–13:30)"), "{out}");
        assert!(out.contains("this week Outside 1h/4h · Reading 0m/3h"), "{out}");
        assert!(out.contains("1h 15m planned"), "the agent to-do never costs the person's day: {out}");
        assert!(out.contains("Not placed on the day (agent lane"), "{out}");
        assert!(state.todo(&ids[3]).unwrap().scheduled_for.is_none());
        assert!(out.contains("Agent lane (1 to dispatch)"), "{out}");
        assert!(out.contains("Chores: 2 on 2026-08-12 (~15m)"), "{out}");

        // A chores slot over protected time is refused…
        let (status, out) = plan(&mut state, json!({"items": [], "chores_slots": [{"start": "12:00", "end": "12:45"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("overlaps protected your-time"), "{out}");
        // …a clear one batches them into one Chores block.
        let (status, out) = plan(&mut state, json!({"items": [], "chores_slots": [{"start": "15:00", "end": "15:30"}]}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("Batched chores: 15:00–15:30 (2 chores)"), "{out}");
        let chores: Vec<&TimeBlock> = state.blocks.iter().filter(|b| matches!(b.source, BlockSource::Chores { .. })).collect();
        assert_eq!(chores.len(), 1);
        // Re-batching replaces, never stacks.
        plan(&mut state, json!({"items": [], "chores_slots": [{"start": "09:00", "end": "09:05"}, {"start": "16:00", "end": "16:30"}]}));
        let chores: Vec<&TimeBlock> = state.blocks.iter().filter(|b| matches!(b.source, BlockSource::Chores { .. })).collect();
        assert_eq!(chores.len(), 2, "5m fits the first slot, the 10m chore spills into the second");

        // Work timeboxed over protected time is flagged.
        state.blocks.push(TimeBlock {
            id: "w".into(),
            todo_id: Some(ids[0].clone()),
            title: "Draft SOW".into(),
            start: state.blocks[0].start,
            end: state.blocks[0].end,
            source: BlockSource::Manual,
        });
        let (_, out) = plan(&mut state, json!({"items": []}));
        assert!(out.contains("is over protected your-time"), "{out}");
    }

    #[test]
    fn plan_day_without_lanes_reads_as_before() {
        let mut state = PlannerState::default();
        let id = seed_todo(&mut state, "Write", "today");
        let (_, out) = plan(&mut state, json!({"items": [{"id": id, "estimate_minutes": 30}]}));
        assert!(out.starts_with("30m planned"), "{out}");
        assert!(!out.contains("Your time") && !out.contains("Agent lane") && !out.contains("Chores"), "{out}");
    }

    fn agent_todo(state: &mut PlannerState, title: &str) -> String {
        create(state, json!({"todos": [{"title": title, "lane": "agent", "outcome": "done well"}]}));
        state.todos.last().unwrap().id.clone()
    }

    #[test]
    fn decisions_ask_list_and_resolve() {
        let mut state = PlannerState::default();
        let empty = handle_decisions(&mut state, &json!({"action": "list"}), None, Some(3), false);
        assert!(empty.response.starts_with("No decisions waiting."));
        assert!(empty.response.contains("3 routing call(s) handled by the user's trust rules today"));

        let id = agent_todo(&mut state, "Plan Phase 2 scope");
        let human = seed_todo(&mut state, "mine", "today");
        let bad = handle_decisions(&mut state, &json!({"action": "ask", "todo_id": human, "question": "q?"}), None, None, false);
        assert_eq!(bad.status, ToolCallStatus::Error);

        let asked = handle_decisions(
            &mut state,
            &json!({"action": "ask", "todo_id": id, "question": "Keep the PII scrubber in Phase 2?",
                    "options": ["Keep in Phase 2", "Move to Phase 3"], "est_minutes": 2,
                    "did": "Drafted the scope", "blocked": "Scrubber pushes past the date", "next": "Finalise the doc"}),
            Some("Scott Marsh Phase 2"),
            None,
            false,
        );
        assert_eq!(asked.status, ToolCallStatus::Completed, "{}", asked.response);
        assert_eq!(state.todo(&id).unwrap().agent_state, Some(model::AgentState::NeedsYou));

        let listed = handle_decisions(&mut state, &json!({"action": "list"}), None, None, false).response;
        assert!(listed.starts_with("1 decision(s) waiting, about 2 min in all:"), "{listed}");
        assert!(listed.contains("1) Keep in Phase 2 / 2) Move to Phase 3; about 2 min; Scott Marsh Phase 2"), "{listed}");
        assert!(listed.contains("Brief — did: Drafted the scope; blocked: Scrubber pushes past the date; next: Finalise the doc"), "{listed}");
        assert!(listed.contains("For: Plan Phase 2 scope → done well"), "{listed}");

        let resolved = handle_decisions(&mut state, &json!({"action": "resolve", "todo_id": id, "choice": "2"}), None, None, false);
        assert_eq!(resolved.status, ToolCallStatus::Completed);
        let entry = resolved.deliver.expect("the edge delivers it");
        assert_eq!(entry.choice, "Move to Phase 3");
        assert_eq!(state.decision_log.len(), 1);
        let again = handle_decisions(&mut state, &json!({"action": "resolve", "todo_id": id, "choice": "1"}), None, None, false);
        assert_eq!(again.status, ToolCallStatus::Error);
        assert!(again.deliver.is_none());
        let bad = handle_decisions(&mut state, &json!({"action": "explode"}), None, None, false);
        assert_eq!(bad.status, ToolCallStatus::Error);
    }

    #[test]
    fn agent_time_tool_refuses_outside_a_granted_session() {
        let mut state = PlannerState::default();
        let (status, out) = handle_agent_time(&mut state, &json!({"action": "start", "intent": "x"}), "chat", today(), false);
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("never in normal chat"), "{out}");

        state.agent_time_grant = model::AgentTimeGrant { enabled: true, daily_minutes: 45, spend_cap_usd: 2.0, ..Default::default() };
        state.open_agent_time_lease("agent-chat", today(), Utc::now()).unwrap();
        let (status, _) = handle_agent_time(&mut state, &json!({"action": "start", "intent": "x"}), "normal-chat", today(), false);
        assert_eq!(status, ToolCallStatus::Error);

        let (status, out) = handle_agent_time(&mut state, &json!({"action": "start", "intent": "Digest KV-cache notes"}), "agent-chat", today(), false);
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("45m and $2.00"), "{out}");
        let (status, out) = handle_agent_time(&mut state, &json!({"action": "propose", "kind": "digest", "title": "KV-cache: 3 approaches", "body": "…"}), "agent-chat", today(), false);
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        let (status, _) = handle_agent_time(&mut state, &json!({"action": "propose", "kind": "poem", "title": "t"}), "agent-chat", today(), false);
        assert_eq!(status, ToolCallStatus::Error);
        let (status, out) = handle_agent_time(&mut state, &json!({"action": "end", "summary": "one digest"}), "agent-chat", today(), false);
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("1 result(s) and proposal(s)"), "{out}");
        assert_eq!(state.open_proposals().len(), 1);
    }

    #[test]
    fn agent_time_start_lists_both_lanes_and_offers_go_through_propose() {
        let mut state = PlannerState {
            agent_time_grant: model::AgentTimeGrant { enabled: true, daily_minutes: 45, spend_cap_usd: 2.0, ..Default::default() },
            ..Default::default()
        };
        state.open_agent_time_lease("agent-chat", today(), Utc::now()).unwrap();
        let agent_id = agent_todo(&mut state, "Digest the KV-cache notes");
        state.todo_mut(&agent_id).unwrap().agent_state = Some(model::AgentState::Queued);
        let mut mine = model::Todo::new("Sophie Asher Probate Consult", 99.0);
        mine.scheduled_for = Some(today());
        let mine_id = mine.id.clone();
        state.todos.push(mine);

        // The person's to-do can't be started on.
        let (status, out) = handle_agent_time_for(&mut state, &json!({"action": "start", "intent": "help", "todo_id": mine_id}), "agent-chat", today(), false, "Dustin");
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("not yours"), "{out}");

        let (status, out) = handle_agent_time_for(&mut state, &json!({"action": "start", "intent": "Digest", "todo_id": agent_id}), "agent-chat", today(), false, "Dustin");
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("Working on: Digest the KV-cache notes"), "{out}");
        let (yours, not_yours) = out.split_once("Dustin's tasks: NOT yours").expect("both lists: {out}");
        assert!(yours.contains("Agent lane: yours to work on") && yours.contains("Digest the KV-cache notes"));
        assert!(not_yours.contains("Sophie Asher Probate Consult"));

        let (status, out) = handle_agent_time_for(&mut state, &json!({"action": "propose", "kind": "offer", "todo_id": mine_id, "body": "I can prepare a question list."}), "agent-chat", today(), false, "Dustin");
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        let offer = state.open_proposals()[0].clone();
        assert_eq!(offer.kind, model::ProposalKind::Offer);
        assert_eq!(offer.todo_id.as_deref(), Some(mine_id.as_str()));
        let (status, _) = handle_agent_time_for(&mut state, &json!({"action": "propose", "kind": "offer", "todo_id": agent_id, "body": "x"}), "agent-chat", today(), false, "Dustin");
        assert_eq!(status, ToolCallStatus::Error, "already agent-lane");
    }

    #[test]
    fn today_context_gains_your_time_agents_and_agent_time() {
        let mut state = PlannerState::default();
        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        assert!(ctx.get("your_time").is_none() && ctx.get("agents").is_none() && ctx.get("agent_time").is_none(),
            "nothing to say, nothing added");

        goals(&mut state);
        block(&mut state, json!({"action": "create", "kind": "your_time", "category": "Reading", "date": "today", "start": "19:00", "end": "20:00"}));
        let id = agent_todo(&mut state, "Ship release");
        handle_decisions(&mut state, &json!({"action": "ask", "todo_id": id, "question": "Ship four now?"}), None, None, false);
        agent_todo(&mut state, "Queued one");
        state.agent_time_grant = model::AgentTimeGrant { enabled: true, daily_minutes: 45, ..Default::default() };

        let ctx = planner_today_context(&state, &ctx_settings(true, true), today()).unwrap();
        assert_eq!(ctx["your_time"]["today"], "1h protected (Reading 19:00–20:00) — plan around it, never over it");
        assert_eq!(ctx["your_time"]["week"], "Outside 0m/4h · Reading 1h/3h");
        assert_eq!(ctx["agents"]["summary"], "1 needs you · 1 to dispatch");
        assert!(ctx["agents"]["needs_you"][0].as_str().unwrap().contains("Ship four now?"));
        assert_eq!(ctx["agent_time"], "45m of 45m left today");
    }

    fn fleet_row(id: &str, status: Value, extra: Value) -> crate::fleet::FleetSession {
        let mut row = json!({
            "id": id, "cwd": "/x/puget-app-packs", "name": "puget-app-packs",
            "status": status, "last_event_at": "2026-08-12T16:00:00Z"
        });
        for (k, v) in extra.as_object().unwrap() {
            row[k] = v.clone();
        }
        serde_json::from_value(row).unwrap()
    }

    #[test]
    fn fleet_state_drives_agent_state() {
        let mut state = PlannerState::default();
        let id = agent_todo(&mut state, "Ship release");
        state.todo_mut(&id).unwrap().linked_fleet_session = Some("run-1".into());
        let human = seed_todo(&mut state, "mine", "today");
        state.todo_mut(&human).unwrap().linked_fleet_session = Some("run-1".into());
        let now = Utc::now();

        let mut live = crate::fleet::FleetState::default();
        live.sessions.insert("run-1".into(), fleet_row("run-1", json!("Working"), json!({})));
        assert_eq!(sync_agent_states(&mut state, &live, &[], now), vec![id.clone()]);
        assert_eq!(state.todo(&id).unwrap().agent_state, Some(model::AgentState::Working));
        assert!(state.todo(&human).unwrap().agent_state.is_none(), "human lane untouched");

        // A held gate → needs you, with a synthesized decision.
        live.sessions.insert(
            "run-1".into(),
            fleet_row("run-1", json!({"NeedsAttention": "Gate"}), json!({
                "pending_gate": {"request_id": "g1", "tool_name": "Bash", "input_summary": "npm publish", "requested_at": "2026-08-12T16:01:00Z"}
            })),
        );
        sync_agent_states(&mut state, &live, &[], now);
        let t = state.todo(&id).unwrap();
        assert_eq!(t.agent_state, Some(model::AgentState::NeedsYou));
        assert!(t.decision.as_ref().unwrap().question.contains("npm publish"));

        // Ended with a blocked brief → stuck (nothing pending).
        live.sessions.clear();
        let ended = fleet_row("run-1", json!("Idle"), json!({
            "ended_at": "2026-08-12T17:00:00Z",
            "brief": {"headline": "Built 4 of 5 images", "blocked_on": "Llama 4 driver check fails", "generated_at": "2026-08-12T17:00:00Z"}
        }));
        let signal = worker_signal_from_fleet(&ended);
        assert_eq!(signal.brief.as_ref().unwrap().did, "Built 4 of 5 images");
        sync_agent_states(&mut state, &live, &[ended], now);
        let t = state.todo(&id).unwrap();
        assert_eq!(t.agent_state, Some(model::AgentState::Stuck));
        assert!(t.decision.is_none(), "the gate's synthesized decision cleared");
    }

    #[test]
    fn the_model_cannot_set_prep_this() {
        let mut state = PlannerState::default();
        let (status, out) = create(&mut state, json!({"todos": [{"title": "Investor call", "prep_requested": true}]}));
        assert_eq!(status, ToolCallStatus::Error, "{out}");
        assert!(out.contains("user's own flag"), "{out}");
        assert!(state.todos.is_empty());
        let (status, _) = create(&mut state, json!({"todos": [{"title": "Investor call"}]}));
        assert_eq!(status, ToolCallStatus::Completed);
        let id = state.todos[0].id.clone();
        let (status, out) = update(&mut state, json!({"updates": [{"id": id, "prep_requested": true}]}));
        assert_eq!(status, ToolCallStatus::Error, "{out}");
        assert!(out.contains("only they set it"), "{out}");
        let (status, _) = update(&mut state, json!({"updates": [{"id": id, "prep": false, "title": "Renamed"}]}));
        assert_eq!(status, ToolCallStatus::Error);
        let t = state.todo(&id).unwrap();
        assert!(!t.prep_requested && t.title == "Investor call", "nothing applied");
    }

    #[test]
    fn agent_time_deliver_and_ask_go_through_the_tool() {
        let mut state = PlannerState {
            agent_time_grant: model::AgentTimeGrant { enabled: true, daily_minutes: 45, spend_cap_usd: 2.0, ..Default::default() },
            ..Default::default()
        };
        let mut call = model::Todo::new("Investor call", 1.0);
        call.prep_requested = true;
        let call_id = call.id.clone();
        state.todos.push(call);
        state.open_agent_time_lease("agent-chat", today(), Utc::now()).unwrap();
        let at = |state: &mut PlannerState, args: Value| handle_agent_time(state, &args, "agent-chat", today(), false);

        let (status, out) = at(&mut state, json!({"action": "start", "intent": "Prep the investor call"}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("Prepare: the user asked you to prep these") && out.contains("Investor call"), "{out}");
        let (status, out) = at(&mut state, json!({"action": "deliver", "title": "x", "body": "y"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("'job'"), "{out}");
        let (status, out) = at(&mut state, json!({"action": "deliver", "job": "prepare", "kind": "poem", "title": "x", "body": "y", "todo_id": call_id}));
        assert_eq!(status, ToolCallStatus::Error, "{out}");
        let (status, out) = at(&mut state, json!({"action": "deliver", "job": "prepare", "kind": "research", "title": "Investor call brief", "body": "Who, what, asks.", "todo_id": call_id}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("Delivered") && out.contains("prepare"), "{out}");
        let (status, out) = at(&mut state, json!({"action": "deliver", "job": "tend", "title": "Correction", "body": "Was 12th; checked the invite: 14th.", "correction": true}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("(correction)"), "{out}");
        assert_eq!(state.trust_ledger.corrections_delivered, 1);
        let (status, out) = at(&mut state, json!({"action": "ask", "question": "Formal or casual?", "options": ["Formal", "Casual"]}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("carry on"), "{out}");
        assert_eq!(state.open_questions()[0].options, vec!["Formal".to_string(), "Casual".into()]);
        let (status, out) = at(&mut state, json!({"action": "end", "summary": "brief and a correction"}));
        assert_eq!(status, ToolCallStatus::Completed, "{out}");
        assert!(out.contains("2 result(s) and proposal(s)"), "{out}");
        let results: Vec<model::Job> = state.proposals.iter().map(|p| p.job).collect();
        assert_eq!(results, vec![model::Job::Prepare, model::Job::Tend]);
        let (status, out) = at(&mut state, json!({"action": "dance"}));
        assert_eq!(status, ToolCallStatus::Error);
        assert!(out.contains("deliver") && out.contains("ask"), "{out}");
    }
}
