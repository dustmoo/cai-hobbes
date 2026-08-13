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
    self, Area, BlockSource, ChecklistItem, DayPlan, Project, TimeBlock, TimeOfDay, Todo,
    TodoBucket, TodoOrigin, TodoStatus, SORT_STEP,
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

fn parse_status(raw: &str) -> Result<TodoStatus, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(TodoStatus::Open),
        "in_progress" | "in-progress" | "started" => Ok(TodoStatus::InProgress),
        "completed" => Ok(TodoStatus::Completed),
        "cancelled" => Ok(TodoStatus::Cancelled),
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

/// Convert a local wall-clock date + time to UTC for storage. DST gaps (a local
/// time that never occurs) are an error; ambiguous times (clock rollback) take
/// the earlier instant.
fn local_to_utc(date: NaiveDate, time: NaiveTime) -> Result<DateTime<Utc>, String> {
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&date.and_time(time)) {
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(earlier, _) => Ok(earlier.with_timezone(&Utc)),
        chrono::LocalResult::None => Err(format!(
            "local time {} {} does not exist (daylight-saving gap)",
            date, time
        )),
    }
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
    if let Some(project_id) = obj.get("project_id").and_then(|v| v.as_str()) {
        if !state.projects.iter().any(|p| p.id == project_id) {
            return Err(format!("unknown project_id '{}'", project_id));
        }
        todo.project_id = Some(project_id.to_string());
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
    Ok(todo)
}

fn new_checklist_item(title: &str) -> ChecklistItem {
    ChecklistItem {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        done: false,
    }
}

// ── HOBBES_TODO_UPDATE ──────────────────────────────────────────────────────

pub fn handle_todo_update(
    state: &mut PlannerState,
    args: &Value,
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
    for (i, item) in items.iter().enumerate() {
        match apply_todo_update(state, item, today, now) {
            Ok((summary, wants_focus)) => {
                let mut summary = summary;
                // The borrow of the patched todo has ended; re-fetch by id for
                // the store write.
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    if wants_focus {
                        // Single-focus: starting this one pauses any other.
                        for changed in state.start_focus(id, now) {
                            if let Some(t) = state.todo(&changed) {
                                persist_todo(t, persist);
                            }
                        }
                        // Re-summarise so the response shows ▶, not the
                        // pre-focus state.
                        if let Some(t) = state.todo(id) {
                            summary = t.summary();
                        }
                    }
                    if let Some(todo) = state.todo(id) {
                        persist_todo(todo, persist);
                    }
                    // Rescheduling moves the work's timebox with it.
                    if item.get("scheduled_for").is_some() {
                        pruned_blocks += prune_rescheduled_blocks(state, id, persist);
                    }
                }
                updated.push(summary);
            }
            Err(e) => errors.push(format!("updates[{}]: {}", i, e)),
        }
    }

    let (status, mut out) = compose_batch_response("Updated", &updated, &errors);
    if pruned_blocks > 0 {
        out.push_str(&format!(
            "
Removed {} timeline block(s) that no longer matched the schedule.",
            pruned_blocks
        ));
    }
    (status, out)
}

/// Returns the patched todo's summary plus whether the item asked for focus
/// (`status: in_progress`), which the caller applies via `start_focus` once
/// this borrow has ended.
fn apply_todo_update(
    state: &mut PlannerState,
    item: &Value,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<(String, bool), String> {
    let obj = item.as_object().ok_or("expected an object")?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing required 'id'")?;

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
    let project_id = patch_from(obj, "project_id", |v| {
        as_str_value(v).map(str::to_string)
    })?;
    if let Patch::Set(ref pid) = project_id {
        if !state.projects.iter().any(|p| p.id == *pid) {
            return Err(format!("unknown project_id '{}'", pid));
        }
    }
    let bucket = match obj.get("bucket").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_bucket(raw)?),
        None => None,
    };
    let status = match obj.get("status").and_then(|v| v.as_str()) {
        Some(raw) => Some(parse_status(raw)?),
        None => None,
    };

    let todo = state
        .todo_mut(id)
        .ok_or_else(|| format!("unknown id '{}'", id))?;

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
    project_id.apply(&mut todo.project_id);
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
    let mut wants_focus = false;
    match status {
        Some(TodoStatus::Completed) => todo.mark_completed(now),
        Some(TodoStatus::Open) => todo.reopen(now),
        Some(TodoStatus::Cancelled) => {
            // Cancelling mid-focus still banks the session time.
            todo.fold_elapsed(now);
            todo.status = TodoStatus::Cancelled;
            // The logbook orders by completed_at; a cancelled todo's "closed
            // moment" is when it was abandoned.
            todo.completed_at = Some(now);
        }
        // Focus is a whole-state operation (it pauses the previous focus), so
        // it can't run inside this single-todo borrow — the caller applies it
        // through PlannerState::start_focus.
        Some(TodoStatus::InProgress) => wants_focus = true,
        None => {}
    }
    todo.updated_at = now;
    Ok((todo.summary(), wants_focus))
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
        let matches = views::in_view(&state.todos, view, today);
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
        // Planning a todo onto `date` moves its timebox with it.
        pruned_blocks += prune_rescheduled_blocks(state, id, persist);
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

    let cap = model::measure_capacity(&state.todos, date, capacity);
    let mut out = String::new();
    // The product opinion: overcommitment leads the response, never a footnote.
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
        _ => (
            ToolCallStatus::Error,
            "Provide 'action': one of create, move, delete.".to_string(),
        ),
    }
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
/// the honest move is to flag it, not forbid it.
fn overlap_warning(state: &PlannerState, block: &TimeBlock) -> Option<String> {
    let clashes: Vec<String> = state
        .blocks
        .iter()
        .filter(|b| b.id != block.id && b.overlaps(block))
        .map(|b| block_line(state, b))
        .collect();
    if clashes.is_empty() {
        None
    } else {
        Some(format!("Warning — overlaps: {}", clashes.join("; ")))
    }
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
    let Some(title) = title else {
        return (
            ToolCallStatus::Error,
            "Provide 'title' (or a 'todo_id' to take the title from).".to_string(),
        );
    };

    let block = TimeBlock {
        id: format!("blk_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        todo_id,
        title,
        start,
        end,
        source: BlockSource::Auto,
    };
    let warning = overlap_warning(state, &block);
    persist_block(&block, persist);
    let mut out = format!("Created {}", block_line(state, &block));
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
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        updated.title = title.to_string();
    }

    let warning = overlap_warning(state, &updated);
    persist_block(&updated, persist);
    let mut out = format!("Moved {}", block_line(state, &updated));
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
            Ok(line) => {
                applied.push(line);
                if persist {
                    if let Some(area) = last_touched_area(state, item) {
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
            Ok(line) => {
                applied.push(line);
                if persist {
                    if let Some(project) = last_touched_project(state, item) {
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

fn upsert_area(state: &mut PlannerState, item: &Value, now: DateTime<Utc>) -> Result<String, String> {
    let obj = item.as_object().ok_or("expected an object")?;
    let title = obj.get("title").and_then(|v| v.as_str()).map(str::trim);

    match obj.get("id").and_then(|v| v.as_str()) {
        Some(id) => {
            let area = state
                .areas
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| format!("unknown area id '{}'", id))?;
            if let Some(t) = title.filter(|t| !t.is_empty()) {
                area.title = t.to_string();
            }
            area.updated_at = now;
            Ok(format!("[{}] area: {}", area.id, area.title))
        }
        None => {
            let title = title
                .filter(|t| !t.is_empty())
                .ok_or("missing required 'title' for a new area")?;
            let sort_order = state
                .areas
                .iter()
                .map(|a| a.sort_order)
                .fold(0.0f64, f64::max)
                + SORT_STEP;
            let area = Area {
                id: uuid::Uuid::new_v4().to_string(),
                title: title.to_string(),
                sort_order,
                created_at: now,
                updated_at: now,
            };
            let line = format!("[{}] area: {}", area.id, area.title);
            state.areas.push(area);
            Ok(line)
        }
    }
}

fn upsert_project(
    state: &mut PlannerState,
    item: &Value,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<String, String> {
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

    match obj.get("id").and_then(|v| v.as_str()) {
        Some(id) => {
            let project = state
                .projects
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or_else(|| format!("unknown project id '{}'", id))?;
            if let Some(t) = title.filter(|t| !t.is_empty()) {
                project.title = t.to_string();
            }
            if let Some(notes) = obj.get("notes").and_then(|v| v.as_str()) {
                project.notes = notes.to_string();
            }
            if let Some(aid) = area_id {
                project.area_id = Some(aid);
            }
            if let Some(d) = deadline {
                project.deadline = Some(d);
            }
            if let Some(s) = status {
                project.status = s;
            }
            project.updated_at = now;
            Ok(format!("[{}] project: {}", project.id, project.title))
        }
        None => {
            let title = title
                .filter(|t| !t.is_empty())
                .ok_or("missing required 'title' for a new project")?;
            let sort_order = state
                .projects
                .iter()
                .map(|p| p.sort_order)
                .fold(0.0f64, f64::max)
                + SORT_STEP;
            let project = Project {
                id: uuid::Uuid::new_v4().to_string(),
                title: title.to_string(),
                notes: obj
                    .get("notes")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                area_id,
                status: status.unwrap_or_default(),
                deadline,
                sort_order,
                created_at: now,
                updated_at: now,
            };
            let line = format!("[{}] project: {}", project.id, project.title);
            state.projects.push(project);
            Ok(line)
        }
    }
}

/// Re-fetch the record an upsert just touched so the store write sees the final
/// state. Keyed by the item's id when updating; falls back to the newest record
/// (just pushed) when creating.
fn last_touched_area<'a>(state: &'a PlannerState, item: &Value) -> Option<&'a Area> {
    match item.get("id").and_then(|v| v.as_str()) {
        Some(id) => state.areas.iter().find(|a| a.id == id),
        None => state.areas.last(),
    }
}

fn last_touched_project<'a>(state: &'a PlannerState, item: &Value) -> Option<&'a Project> {
    match item.get("id").and_then(|v| v.as_str()) {
        Some(id) => state.projects.iter().find(|p| p.id == id),
        None => state.projects.last(),
    }
}

// ── planner_today context injection ─────────────────────────────────────────

/// Hard caps for the `planner_today` system-context block. The block rides on
/// *every* prompt, so it is budgeted like a header, not a report — the model
/// can always call HOBBES_TODO_LIST for the rest.
const CTX_MAX_TODOS: usize = 20;
const CTX_MAX_BLOCKS: usize = 10;
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
pub fn planner_today_context(
    planner: &PlannerState,
    settings: &crate::settings::Settings,
    today: NaiveDate,
) -> Option<serde_json::Value> {
    if !settings.planner_enabled || !settings.planner_inject_today_context {
        return None;
    }

    let capacity = planner.capacity_for(today, settings.planner_daily_capacity_minutes);
    let cap = model::measure_capacity(&planner.todos, today, capacity);

    let todos: Vec<String> = views::in_view(&planner.todos, TodoView::Today, today)
        .into_iter()
        .take(CTX_MAX_TODOS)
        .map(|t| {
            let mut clipped = t.clone();
            clipped.title = truncate_title(&t.title);
            clipped.summary()
        })
        .collect();

    let blocks: Vec<String> = planner
        .blocks_on(today)
        .into_iter()
        .take(CTX_MAX_BLOCKS)
        .map(|b| {
            format!(
                "{}–{} {}",
                b.start.with_timezone(&chrono::Local).format("%H:%M"),
                b.end.with_timezone(&chrono::Local).format("%H:%M"),
                truncate_title(&planner.block_display_title(b))
            )
        })
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
            model::format_minutes(t.elapsed_minutes(now)),
            t.estimate_minutes
                .map(|m| format!(" of {} estimated", model::format_minutes(m)))
                .unwrap_or_default()
        )
    });

    Some(serde_json::json!({
        "date": today.to_string(),
        "capacity": cap.summary(),
        "in_focus": focus,
        "todos": todos,
        "blocks": blocks,
        "overdue": overdue,
        "instruction": "The user's shared to-do list for today. Manage it with the HOBBES_TODO_* / HOBBES_PLAN_DAY / HOBBES_TIME_BLOCK tools. Setting a todo's status to in_progress starts focus mode on it (only one at a time); completing or reopening it ends the session.",
    }))
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
        handle_todo_update(state, &args, today(), false)
    }

    fn list(state: &PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_todo_list(state, &args, today())
    }

    fn plan(state: &mut PlannerState, args: Value) -> (ToolCallStatus, String) {
        handle_plan_day(state, &args, 360, today(), false)
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
        assert!(resp.contains("3h 15m planned of 6h capacity"), "{resp}");
        assert!(resp.contains("2h 45m free"), "{resp}");

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
        assert_eq!(ctx["capacity"], "3h 25m planned of 6h capacity — 2h 35m free");
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
}
