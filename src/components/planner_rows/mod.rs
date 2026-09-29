//! Planner centre column: the Things-style todo list surfaces.
//!
//! Split out of `planner_view.rs` (P-split for concurrent editing) — keeps
//! `CentreColumn`, `QuickAdd`, `TodoList`, and `TodoRow`, plus the pure
//! helpers only they use. Root types (`PlannerSelection`, persistence
//! helpers, `today()`) and timeline geometry live in the sibling modules
//! `planner_view` and `planner_timeline`.
//!
//! The row is a three-column grid — WHO (status/actor), WHAT (title, project,
//! carrier session), WHEN (estimate, timebox, due, schedule) — so a scan down
//! the list answers the three questions in fixed places instead of reading a
//! ragged chip stream.

use std::collections::HashMap;

use chrono::{DateTime, Local, NaiveDate, Utc};
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::fleet::{FleetOrigin, FleetSession, FleetStatus};
use crate::settings::Settings;
use crate::todo::model::{
    self, BlockSource, FocusActor, FocusSession, Priority, TimeBlock, TimeOfDay, Todo, TodoBucket,
    TodoStatus,
};
use crate::todo::project_match::{self, MatchTier, ProjectIndex, ProjectResolution, Ranked, PENDING_PROJECT_ID};
use crate::todo::quick_add::{
    active_project_query, parse_quick_add_with, set_project_token, QuickAddParse,
};
use crate::todo::store;
use crate::todo::views::{self, ArrangeInputs, GroupBy, ListOrder, RowGroup, SortDir, SortKey, TodoView};
use crate::todo::PlannerState;

use super::planner_timeline::{fmt_hhmm, local_minutes_to_utc};
use super::planner_view::{
    mutate_todo, persist_block, persist_focus_sessions, persist_todo, today, PlannerSelection,
    SelectedBlockContext,
};

// ── Pure helpers (unit-tested at the bottom) ────────────────────────────────

fn cycle_estimate(current: Option<u32>) -> Option<u32> {
    const LADDER: [u32; 4] = [15, 30, 60, 120];
    match current {
        None => Some(15),
        Some(m) => match LADDER.iter().position(|&step| step == m) {
            Some(pos) => LADDER.get(pos + 1).copied(),
            // min_by_key breaks distance ties on Reverse(step): the larger
            // step wins, which is the ties-round-up rule.
            None => LADDER
                .iter()
                .min_by_key(|&&step| (m.abs_diff(step), std::cmp::Reverse(step)))
                .copied(),
        },
    }
}

/// The draft seeded into a fresh chat when a todo is activated. Speaks in the
/// user's voice (it is their message to edit), carries the id so the AI can
/// drive HOBBES_TODO_UPDATE, and ends mid-sentence so the caret invites the
/// details.
fn activation_seed(todo: &Todo) -> String {
    let mut meta: Vec<String> = Vec::new();
    if let Some(m) = todo.estimate_minutes {
        meta.push(model::format_minutes(m));
    }
    if let Some(d) = todo.deadline {
        meta.push(format!("due {}", d.format("%-d %b")));
    }
    for tag in &todo.tags {
        meta.push(format!("#{}", tag));
    }
    let meta = if meta.is_empty() {
        String::new()
    } else {
        format!(" ({})", meta.join(", "))
    };
    format!(
        "Let's work on my to-do \"{}\"{} — todo id {}.\n\nDetails: ",
        todo.title, meta, todo.id
    )
}

/// Who is on this todo right now — the WHO column's whole vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WhoState {
    /// Nobody: an open circle that completes on click.
    Open,
    /// You, in a person focus session.
    YouFocusing,
    /// A Hobbes chat tab, carrying its profile initial and swatch.
    Hobbes { initial: String, color: String },
    /// A coding-agent (non-Hobbes) fleet session.
    FleetAgent,
    /// The carrier session is blocked on an approval or a question.
    NeedsYou,
    /// Closed work.
    Done,
}

/// Resolve the WHO state from the three inputs the row can see.
///
/// Precedence, highest first: **done** (closed work is closed, whatever a
/// stale link says) → **needs you** (a blocked carrier outranks the work it
/// is blocked in) → **you focusing** (you pressed Start; that is the most
/// immediate truth about the row) → **Hobbes tab** → **fleet agent** →
/// **open**.
///
/// `profile` is the linked Hobbes tab's `(initial, bg-class)` swatch; when it
/// cannot be resolved the avatar falls back to the default profile colour.
fn who_state(
    todo: &Todo,
    focus: Option<&FocusSession>,
    fleet: Option<&FleetSession>,
    profile: Option<(String, String)>,
) -> WhoState {
    if todo.status.is_closed() {
        return WhoState::Done;
    }
    if fleet.is_some_and(|f| f.status.needs_attention()) {
        return WhoState::NeedsYou;
    }

    let live_focus = focus.filter(|f| f.is_open() && todo.status == TodoStatus::InProgress);
    if live_focus.is_some_and(|f| matches!(f.actor, FocusActor::Person)) {
        return WhoState::YouFocusing;
    }

    let fleet_working = fleet.is_some_and(|f| {
        matches!(
            f.status,
            FleetStatus::Working | FleetStatus::WorkingBackground
        )
    });
    let agent_focus = live_focus.is_some_and(|f| matches!(f.actor, FocusActor::Agent { .. }));
    let hobbes_carrier = fleet.is_some_and(|f| f.origin == FleetOrigin::Hobbes);

    if agent_focus || (fleet_working && hobbes_carrier) {
        let (initial, color) = profile
            .unwrap_or_else(|| ("H".to_string(), DEFAULT_PROFILE_COLOR.to_string()));
        return WhoState::Hobbes { initial, color };
    }
    if fleet_working {
        return WhoState::FleetAgent;
    }
    WhoState::Open
}

/// How loudly a deadline should read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DueTone {
    Neutral,
    /// Due today, tomorrow, or the day after.
    Soon,
    Overdue,
}

fn due_tone(due: NaiveDate, today: NaiveDate) -> DueTone {
    if due < today {
        DueTone::Overdue
    } else if (due - today).num_days() <= 2 {
        DueTone::Soon
    } else {
        DueTone::Neutral
    }
}

fn due_tone_class(tone: DueTone) -> &'static str {
    match tone {
        DueTone::Neutral => "border-faint text-fg-muted",
        DueTone::Soon => "border-yellow-700 text-yellow-300",
        DueTone::Overdue => "border-red-700 text-red-400",
    }
}

/// The date pill's text. `None` when the todo has no scheduled day — the pill
/// is the *date's* control, so an undated todo simply doesn't wear one.
///
/// "Evening" wins only on today: on any other day the day itself is the more
/// useful word, and the evening grouping is visible from its section header.
fn date_pill_label(scheduled_for: Option<NaiveDate>, evening: bool, today: NaiveDate) -> Option<String> {
    let d = scheduled_for?;
    if evening && d == today {
        return Some("Evening".to_string());
    }
    if d == today {
        return Some("Today".to_string());
    }
    if Some(d) == today.succ_opt() {
        return Some("Tomorrow".to_string());
    }
    Some(d.format("%a %-d").to_string())
}

/// One "when" the schedule popover (and the detail card's date field) can set.
///
/// Every one of these is a three-step mutation — patch the todo, prune the
/// blocks that no longer sit on its day, delete those block rows — which used
/// to be hand-rolled at each call site. [`apply_schedule_choice`] is the pure
/// patch; [`set_schedule`] is the whole sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleChoice {
    Today,
    ThisEvening,
    Tomorrow,
    On(NaiveDate),
    /// Drop the date without touching the bucket — the detail card's "Clear".
    ClearDate,
    /// Undated and triaged.
    Anytime,
    Someday,
}

pub(crate) fn apply_schedule_choice(t: &mut Todo, choice: ScheduleChoice, today: NaiveDate) {
    match choice {
        ScheduleChoice::Today => {
            t.scheduled_for = Some(today);
            t.time_of_day = None;
        }
        // Evening is a time-of-day, not a reschedule: a dated todo keeps its
        // day (yanking an Upcoming todo onto today was the U1.4 bug); only an
        // undated one gets scheduled today.
        ScheduleChoice::ThisEvening => {
            t.time_of_day = Some(TimeOfDay::Evening);
            if t.scheduled_for.is_none() {
                t.scheduled_for = Some(today);
            }
        }
        ScheduleChoice::Tomorrow => {
            t.scheduled_for = today.succ_opt();
            t.time_of_day = None;
        }
        // A picked day leaves the time-of-day grouping alone (the detail
        // card's date field has always behaved this way).
        ScheduleChoice::On(d) => t.scheduled_for = Some(d),
        // An undated todo has no day for a time-of-day group to belong to.
        ScheduleChoice::ClearDate => {
            t.scheduled_for = None;
            t.time_of_day = None;
        }
        ScheduleChoice::Anytime => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Anytime;
        }
        ScheduleChoice::Someday => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Someday;
        }
    }
}

/// Which popover entry wears the tick.
fn current_schedule_choice(todo: &Todo, today: NaiveDate) -> ScheduleChoice {
    if todo.bucket == TodoBucket::Someday {
        return ScheduleChoice::Someday;
    }
    match todo.scheduled_for {
        Some(d) if d == today && todo.time_of_day == Some(TimeOfDay::Evening) => {
            ScheduleChoice::ThisEvening
        }
        Some(d) if d == today => ScheduleChoice::Today,
        Some(d) if Some(d) == today.succ_opt() => ScheduleChoice::Tomorrow,
        Some(d) => ScheduleChoice::On(d),
        None => ScheduleChoice::Anytime,
    }
}

/// The whole schedule mutation: patch, prune the now-orphaned blocks, delete
/// their rows. The single owner of the sequence — the row popover, the detail
/// card and anything else that moves a todo's day all go through here (the
/// AI's update and plan-day handlers run the same rule server-side).
pub(crate) fn set_schedule(
    mut planner: Signal<PlannerState>,
    id: &str,
    choice: ScheduleChoice,
    today: NaiveDate,
) {
    mutate_todo(planner, id, |t| apply_schedule_choice(t, choice, today));
    let keep = planner.peek().todo(id).and_then(|t| t.scheduled_for);
    let pruned = planner.write().prune_blocks_for_todo(id, keep);
    for b in &pruned {
        if let Err(e) = store::delete_block(&b.id) {
            tracing::error!("planner: failed to delete rescheduled block {}: {}", b.id, e);
        }
    }
}

/// The left-edge stripe matching a profile swatch. Tailwind only sees class
/// literals, so the derived names are enumerated here for the scanner:
///
/// border-l-blue-600 border-l-purple-600 border-l-green-600 border-l-red-600
/// border-l-orange-600 border-l-pink-600 border-l-teal-600
///
/// ...and the matching pill borders (already emitted for the chat bar's
/// profile pill, repeated so this module stands on its own):
///
/// border-blue-600 border-purple-600 border-green-600 border-red-600
/// border-orange-600 border-pink-600 border-teal-600
fn profile_stripe_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-l-{}", rest),
        _ => "border-l-blue-600".to_string(),
    }
}

fn profile_pill_border_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-{}", rest),
        _ => "border-faint".to_string(),
    }
}

/// Minutes the carrier has been working, for the session pill's tail.
fn working_minutes(
    fleet: Option<&FleetSession>,
    focus: Option<&FocusSession>,
    now: DateTime<Utc>,
) -> Option<u32> {
    let started = focus
        .filter(|f| f.is_open())
        .map(|f| f.started_at)
        .or_else(|| fleet.and_then(|f| f.working_since))?;
    Some((now - started).num_minutes().max(0) as u32)
}

fn view_title(view: TodoView) -> &'static str {
    match view {
        TodoView::Inbox => "Inbox",
        TodoView::Today => "Today",
        TodoView::Upcoming => "Upcoming",
        TodoView::Anytime => "Anytime",
        TodoView::Someday => "Someday",
        TodoView::Logbook => "Logbook",
    }
}

fn empty_state_line(selection: &PlannerSelection) -> &'static str {
    match selection {
        PlannerSelection::View(TodoView::Inbox) => "Nothing to triage.",
        PlannerSelection::View(TodoView::Today) => "Nothing planned for today.",
        PlannerSelection::View(TodoView::Upcoming) => "Nothing scheduled ahead.",
        PlannerSelection::View(TodoView::Anytime) => "Nothing queued.",
        PlannerSelection::View(TodoView::Someday) => "Nothing on the back burner.",
        PlannerSelection::View(TodoView::Logbook) => "Nothing finished yet.",
        PlannerSelection::Project(_) => "No todos in this project yet.",
    }
}

/// Todos for the current selection, ordered for display.
fn todos_for_selection(state: &PlannerState, selection: &PlannerSelection, day: NaiveDate) -> Vec<Todo> {
    match selection {
        PlannerSelection::View(view) => views::in_view(&state.todos, *view, day)
            .into_iter()
            .cloned()
            .collect(),
        PlannerSelection::Project(id) => {
            let mut out: Vec<Todo> = state
                .todos
                .iter()
                .filter(|t| !t.status.is_closed() && t.project_id.as_deref() == Some(id))
                .cloned()
                .collect();
            out.sort_by(|a, b| {
                a.sort_order
                    .partial_cmp(&b.sort_order)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });
            out
        }
    }
}

/// A todo's earliest timeline block on its scheduled day. The one query
/// behind both the row's "@ 10:15" chip and the Time sort, so the chip and
/// the sort can't disagree.
fn first_block_on_day<'a>(blocks: &'a [TimeBlock], todo_id: &str, day: NaiveDate) -> Option<&'a TimeBlock> {
    blocks
        .iter()
        .filter(|b| {
            b.todo_id.as_deref() == Some(todo_id) && b.start.with_timezone(&Local).date_naive() == day
        })
        .min_by_key(|b| b.start)
}

/// todo id → local minutes since midnight of its first block on its
/// scheduled day. Built once per list render for the Time sort.
fn block_slots(todos: &[Todo], blocks: &[TimeBlock]) -> HashMap<String, u32> {
    use chrono::Timelike;
    todos
        .iter()
        .filter(|t| !t.status.is_closed())
        .filter_map(|t| {
            let day = t.scheduled_for?;
            let start = first_block_on_day(blocks, &t.id, day)?.start.with_timezone(&Local);
            Some((t.id.clone(), start.hour() * 60 + start.minute()))
        })
        .collect()
}

/// The quadrant badge's tone, from the colours rows already use: red for
/// Q1 (the overdue red), primary for Q2, yellow for Q3 (the due-soon
/// yellow), muted for Q4.
fn priority_tone_class(p: Priority) -> &'static str {
    match p {
        Priority::UrgentImportant => "border-red-700 text-red-400",
        Priority::Important => "border-primary-600 text-primary-300",
        Priority::Urgent => "border-yellow-700 text-yellow-300",
        Priority::Neither => "border-faint text-fg-muted",
    }
}

/// The toolbar's left-hand hint. `None` in the default state, which shows
/// no indicator at all.
fn order_hint(order: ListOrder) -> Option<String> {
    if order.is_manual() {
        return None;
    }
    Some(match order.key.dir_label(order.dir) {
        None => format!(
            "Grouped by {} — drag within a group to reorder",
            order.group.label().to_lowercase()
        ),
        Some(dir) => format!(
            "{} · {} — drag a row to make this your order",
            order.key.label(),
            dir
        ),
    })
}

/// Where a menu or keyboard move sends a row within its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reorder {
    Top,
    Bottom,
    Up,
    Down,
}

/// The destination index for a [`Reorder`], or `None` when it's a no-op.
fn reorder_target(from: usize, len: usize, r: Reorder) -> Option<usize> {
    if from >= len {
        return None;
    }
    let to = match r {
        Reorder::Top => 0,
        Reorder::Bottom => len - 1,
        Reorder::Up => from.checked_sub(1)?,
        Reorder::Down => (from + 1 < len).then_some(from + 1)?,
    };
    (to != from).then_some(to)
}

/// `ids` with the row at `from` moved to `to`.
fn moved(ids: &[String], from: usize, to: usize) -> Vec<String> {
    let mut out = ids.to_vec();
    if from < out.len() {
        let id = out.remove(from);
        out.insert(to.min(out.len()), id);
    }
    out
}

/// A row's height (`h-9`): a drag's insertion index is the grab index plus
/// whole rows travelled, so nothing is measured mid-drag.
const ROW_HEIGHT_PX: f64 = 36.0;

fn drag_over_index(from: usize, len: usize, dy: f64) -> usize {
    let steps = (dy / ROW_HEIGHT_PX).round() as i64;
    (from as i64 + steps).clamp(0, len.saturating_sub(1) as i64) as usize
}

/// The on-screen open order across every group, with one group's segment
/// replaced by its post-move order — what an adopt makes the manual order.
fn flatten_with(groups: &[(String, Vec<String>)], group_id: &str, replacement: &[String]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|(id, ids)| if id == group_id { replacement.to_vec() } else { ids.clone() })
        .collect()
}

// ── Shared class vocabulary ────────────────────────────────────────────────

/// Mirrors `default_profile_color()` in `settings.rs` (private there).
const DEFAULT_PROFILE_COLOR: &str = "bg-blue-600";

const PILL: &str = "h-[22px] inline-flex items-center rounded-full border border-faint px-2 text-[11px] text-fg-muted gap-1.5 shrink-0";
const META: &str = "font-mono text-[11px] tabular-nums text-fg-muted shrink-0";
const BTN_PRIMARY: &str =
    "text-[11px] rounded px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover shrink-0";
const BTN_SECONDARY: &str =
    "text-[11px] rounded px-2 py-0.5 border border-faint text-fg hover:bg-input shrink-0";
const POPOVER: &str = "absolute right-0 top-full mt-1 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 py-1";
const MENU_ITEM: &str =
    "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-fg text-left hover:bg-input";
const SECTION_HEADER: &str =
    "text-[11px] uppercase tracking-[0.08em] text-fg-muted px-2.5 pt-3.5 pb-1";

const MENU_LABEL: &str =
    "px-2.5 pt-1.5 pb-1 text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const MENU_RULE: &str = "h-px bg-[var(--color-border-muted)] my-1";
/// The faint "this is what the list is sorted by" ring on a row's chip.
const KEY_RING: &str = " ring-1 ring-primary-300/50";

/// Which of a row's popovers is open. Exactly one at a time, so a menu never
/// stacks on top of another.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowMenu {
    Project,
    Date,
    Hand,
    More,
    Priority,
}

// ── List order: drag and undo plumbing ──────────────────────────────────────

/// An armed or live row drag. Rows arm it on mousedown with the id and the
/// pointer's y; `TodoList` fills in the group once the pointer has moved
/// past the threshold.
#[derive(Clone, PartialEq, Debug, Default)]
pub(crate) struct ListDrag {
    pub todo_id: String,
    /// RowGroup id — drops are confined to the grab's group.
    pub group_id: String,
    /// The group's open rows, in on-screen order.
    pub ids: Vec<String>,
    pub grab_y: f64,
    pub from_index: usize,
    /// Current insertion index within the group's open rows.
    pub over_index: usize,
    /// Past the 4px threshold: the overlay is up.
    pub active: bool,
}

/// List-scoped drag state, provided by TodoList (same pattern as
/// SelectedBlockContext). The drag itself is a non-reactive `CopyValue`, so
/// mousemove never re-renders; only crossing the threshold does.
#[derive(Clone, Copy)]
pub(crate) struct ListDragContext {
    drag: CopyValue<Option<ListDrag>>,
    /// ⋯ menu "Move to top/bottom" and ⌥↑/⌥↓.
    reorder: Callback<(String, Reorder)>,
}

/// Undo for an adopt, rendered as a toast at the CentreColumn root.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct AdoptUndo {
    pub previous_order: ListOrder,
    pub previous_slots: Vec<(String, f64)>,
    /// Identifies this toast to its own 6s timer, so an older timer can't
    /// close a newer toast.
    generation: u64,
}

static UNDO_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Queue a FLIP play for after the next render (see `assets/list_flip.js`).
fn bump_flip(mut flip: Signal<(u64, u32)>, duration_ms: u32) {
    flip.with_mut(|f| {
        f.0 = f.0.wrapping_add(1);
        f.1 = duration_ms;
    });
}

fn flip_snap() {
    let _ = document::eval("window.hobbesFlip?.snap()");
}

/// Commit a move of `todo_id` to index `to` within its group.
///
/// In Manual this writes one row (or a renormalised group) through
/// `move_between`. In a sorted view it first adopts the on-screen order as
/// the manual order — permuting existing slots — then switches the list to
/// Manual (keeping its grouping) and offers Undo. Dragging is the choice.
#[allow(clippy::too_many_arguments)]
fn apply_move(
    mut planner: Signal<PlannerState>,
    order: Signal<ListOrder>,
    mut undo: Signal<Option<AdoptUndo>>,
    set_order: Callback<ListOrder>,
    groups: &[(String, Vec<String>)],
    todo_id: &str,
    group_id: &str,
    to: usize,
) {
    let Some(ids) = groups.iter().find(|g| g.0 == group_id).map(|g| &g.1) else {
        return;
    };
    let Some(from) = ids.iter().position(|id| id == todo_id) else {
        return;
    };
    if from == to {
        return;
    }
    let new_ids = moved(ids, from, to);
    let now = Utc::now();
    let current = *order.peek();

    if current.key == SortKey::Manual {
        let before = to.checked_sub(1).map(|i| new_ids[i].as_str());
        let after = new_ids.get(to + 1).map(String::as_str);
        let changed = { planner.write().move_between(todo_id, before, after, &new_ids, now) };
        for id in &changed {
            persist_todo(&planner, id);
        }
        return;
    }

    let flat = flatten_with(groups, group_id, &new_ids);
    let previous_slots = { planner.write().adopt_order(&flat, now) };
    for (id, _) in &previous_slots {
        persist_todo(&planner, id);
    }
    set_order.call(ListOrder {
        key: SortKey::Manual,
        dir: SortDir::Asc,
        group: current.group,
    });
    let generation = UNDO_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    undo.set(Some(AdoptUndo {
        previous_order: current,
        previous_slots,
        generation,
    }));
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        if undo.peek().as_ref().is_some_and(|u| u.generation == generation) {
            undo.set(None);
        }
    });
}

// ── Centre column ───────────────────────────────────────────────────────────

#[component]
pub(crate) fn CentreColumn(selection: Signal<PlannerSelection>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let sel = selection.read().clone();

    let title = match &sel {
        PlannerSelection::View(v) => view_title(*v).to_string(),
        PlannerSelection::Project(id) => planner
            .read()
            .projects
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.title.clone())
            .unwrap_or_else(|| "Project".to_string()),
    };
    let is_today_view = sel == PlannerSelection::View(TodoView::Today);
    let subtitle = is_today_view.then(|| Local::now().format("%A, %-d %B").to_string());

    // The day's numbers, in the same words the rail's capacity bar uses — the
    // centre column should not make you look sideways to know if today fits.
    let capacity_line = is_today_view.then(|| {
        let (default_capacity, meetings_count) = {
            let s = settings.read();
            (
                s.planner_daily_capacity_minutes,
                s.planner_calendar_counts_against_capacity,
            )
        };
        let state = planner.read();
        let day = today();
        model::measure_capacity_full(
            &state.todos,
            &state.blocks,
            &state.focus_sessions,
            day,
            state.capacity_for(day, default_capacity),
            meetings_count,
        )
        .summary()
    });

    // Quick-adding into the logbook would resurrect nothing — hide it there.
    let show_quick_add = sel != PlannerSelection::View(TodoView::Logbook);

    // ── List order (a lens: never written to the todos) ────────────────────
    // Saved per list in UiState, next to the column widths. The signal lives
    // here so the toolbar control and TodoList read the same value.
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    use_hook(|| {
        let _ = document::eval(include_str!("../../../assets/list_flip.js"));
    });
    let seed = move |sel: &PlannerSelection| -> ListOrder {
        let (keys, groups) = views::order_options(sel.view());
        ui_state
            .peek()
            .planner_list_orders
            .get(&sel.list_key())
            .copied()
            .unwrap_or_default()
            .sanitized(&keys, &groups)
    };
    let mut order = use_signal(|| seed(&selection.peek()));
    let mut undo = use_signal(|| Option::<AdoptUndo>::None);
    let flip = use_signal(|| (0u64, 220u32));
    // Re-seed when the list changes; a pending Undo belongs to the old list.
    use_effect(move || {
        let sel = selection.read().clone();
        let next = seed(&sel);
        if *order.peek() != next {
            order.set(next);
        }
        if undo.peek().is_some() {
            undo.set(None);
        }
    });
    // FLIP play: runs after the render that follows every order change,
    // drop and undo (each bumps `flip` after snapping).
    use_effect(move || {
        let (_, ms) = *flip.read();
        let _ = document::eval(&format!("window.hobbesFlip?.play({ms})"));
    });
    let set_order = use_callback(move |next: ListOrder| {
        order.set(next);
        undo.set(None);
        let key = selection.peek().list_key();
        let live: Vec<String> = planner.peek().projects.iter().map(|p| p.id.clone()).collect();
        {
            let mut ui = ui_state.write();
            if next == ListOrder::default() {
                ui.planner_list_orders.remove(&key);
            } else {
                ui.planner_list_orders.insert(key, next);
            }
            let live: Vec<&str> = live.iter().map(String::as_str).collect();
            ui.prune_planner_list_orders(&live);
        }
        let state = ui_state.peek().clone();
        let manager = ui_state_manager.peek().clone();
        spawn(async move {
            let _ = manager.save(&state);
        });
    });
    let (order_keys, order_groups) = views::order_options(sel.view());
    let hint = order_hint(*order.read());
    let undo_now = undo.read().clone();

    rsx! {
        div {
        class: "relative flex-1 min-w-0 min-h-0 flex flex-col",
        div {
            class: "flex-1 min-h-0 flex flex-col overflow-y-auto",
            div {
                class: "px-6 pt-6 pb-2",
                div {
                    class: "flex items-baseline gap-3",
                    h1 { class: "text-xl font-semibold", "{title}" }
                    if let Some(date_line) = subtitle {
                        span { class: "text-sm text-fg-muted", "{date_line}" }
                    }
                    div { class: "flex-1" }
                    if let Some(line) = capacity_line {
                        span { class: "{META} text-right", "{line}" }
                    }
                }
            }
            if show_quick_add {
                QuickAdd { selection }
            }
            if !order_keys.is_empty() {
                div {
                    class: "px-6 pb-1.5 flex items-center gap-1.5",
                    if let Some(hint) = hint {
                        span { class: "min-w-0 truncate text-[11px] text-fg-muted", "{hint}" }
                    }
                    div { class: "flex-1" }
                    ListOrderControl {
                        order,
                        keys: order_keys.clone(),
                        groups: order_groups.clone(),
                        on_change: move |next: ListOrder| {
                            flip_snap();
                            set_order.call(next);
                            bump_flip(flip, 220);
                        },
                    }
                }
            }
            TodoList { selection, order, undo, flip, set_order }
            // Breathing room so hover menus on the last row aren't clipped.
            div { class: "h-16 shrink-0" }
        }
        if let Some(u) = undo_now {
            div {
                class: "absolute inset-x-0 bottom-4 z-40 flex justify-center px-6 pointer-events-none",
                div {
                    class: "pointer-events-auto planner-pop-in flex items-center gap-3 rounded-lg border border-subtle bg-card px-3 py-2 shadow-xl text-[11px] text-fg",
                    span { "Now in manual order, kept the {u.previous_order.key.label()} arrangement" }
                    button {
                        class: "font-medium text-primary-300 hover:text-fg",
                        onclick: move |_| {
                            let Some(u) = undo.peek().clone() else { return };
                            flip_snap();
                            let ids = planner.write().restore_order(&u.previous_slots, Utc::now());
                            for id in &ids {
                                persist_todo(&planner, id);
                            }
                            set_order.call(u.previous_order);
                            bump_flip(flip, 220);
                        },
                        "Undo"
                    }
                }
            }
        }
        }
    }
}

/// The toolbar pill + popover. Reads "Manual ▾" when idle; lit primary with
/// its own direction arrow and a × when any lens is on. Owns no order state:
/// the order lives in CentreColumn so TodoList and the control agree.
#[component]
fn ListOrderControl(
    order: Signal<ListOrder>,
    /// Keys valid for this list (see `views::order_options`).
    keys: Vec<SortKey>,
    /// Group options valid for this list; empty hides the Group section.
    groups: Vec<GroupBy>,
    on_change: EventHandler<ListOrder>,
) -> Element {
    let mut open = use_signal(|| false);
    let current = *order.read();
    let pill_label = match (current.key, current.group) {
        (SortKey::Manual, GroupBy::None) => "Manual".to_string(),
        (SortKey::Manual, g) => format!("By {}", g.label().to_lowercase()),
        (k, GroupBy::None) => k.label().to_string(),
        (k, g) => format!("{} · {}", k.label(), g.label().to_lowercase()),
    };
    let arrow = if current.dir == SortDir::Asc { "↑" } else { "↓" };
    let flip_title = current
        .key
        .dir_label(current.dir.flipped())
        .map(|l| format!("Flip to {}", l.to_lowercase()))
        .unwrap_or_default();
    let mut pick = move |next: ListOrder| {
        open.set(false);
        if next != current {
            on_change.call(next);
        }
    };

    rsx! {
        div {
            class: "relative shrink-0",
            if current.is_manual() {
                button {
                    class: "{PILL} hover:text-fg hover:border-subtle",
                    title: "Sort and group this list",
                    onclick: move |_| open.toggle(),
                    "Manual ▾"
                }
            } else {
                span {
                    class: "{PILL} border-primary-500 text-primary-300 pr-1 gap-1",
                    button {
                        class: "hover:text-fg",
                        title: "Sort and group this list",
                        onclick: move |_| open.toggle(),
                        "{pill_label}"
                    }
                    if current.key != SortKey::Manual {
                        button {
                            class: "font-mono hover:text-fg",
                            title: "{flip_title}",
                            onclick: move |_| pick(ListOrder { dir: current.dir.flipped(), ..current }),
                            "{arrow}"
                        }
                    }
                    button {
                        class: "rounded-full px-1 hover:text-fg hover:bg-input",
                        title: "Back to manual order",
                        onclick: move |_| pick(ListOrder::default()),
                        "×"
                    }
                }
            }
            if *open.read() {
                div {
                    class: "fixed inset-0 z-40",
                    onclick: move |_| open.set(false),
                }
                div {
                    class: "{POPOVER} planner-pop-in",
                    div { class: "{MENU_LABEL}", "Sort by" }
                    for k in keys.iter().copied() {
                        button {
                            key: "{k.label()}",
                            class: "{MENU_ITEM}",
                            // A new key starts ascending; Manual has no direction.
                            onclick: move |_| pick(ListOrder { key: k, dir: SortDir::Asc, ..current }),
                            span { class: "w-3 shrink-0", if current.key == k { "✓" } }
                            span { class: "flex-1", "{k.label()}" }
                            if let Some(asc) = k.dir_label(SortDir::Asc) {
                                span { class: "text-fg-muted", "{asc}" }
                            }
                        }
                    }
                    if current.key != SortKey::Manual {
                        div { class: "{MENU_RULE}" }
                        div { class: "{MENU_LABEL}", "Direction" }
                        for d in [SortDir::Asc, SortDir::Desc] {
                            button {
                                key: "{d:?}",
                                class: "{MENU_ITEM}",
                                onclick: move |_| pick(ListOrder { dir: d, ..current }),
                                span { class: "w-3 shrink-0", if current.dir == d { "✓" } }
                                span { "{current.key.dir_label(d).unwrap_or_default()}" }
                            }
                        }
                    }
                    if !groups.is_empty() {
                        div { class: "{MENU_RULE}" }
                        div { class: "{MENU_LABEL}", "Group by" }
                        for g in groups.iter().copied() {
                            button {
                                key: "{g.label()}",
                                class: "{MENU_ITEM}",
                                onclick: move |_| pick(ListOrder { group: g, ..current }),
                                span { class: "w-3 shrink-0", if current.group == g { "✓" } }
                                span { "{g.label()}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One header row. Reuses SECTION_HEADER; `band` adds the hairline rule.
/// Group headers carry the count and Σ estimate; bands only the count.
#[component]
fn GroupHeader(
    id: String,
    label: String,
    count: Option<usize>,
    estimate_minutes: Option<u32>,
    band: bool,
    warn: bool,
) -> Element {
    let mut class = if warn {
        SECTION_HEADER.replace("text-fg-muted", "text-red-400")
    } else {
        SECTION_HEADER.to_string()
    };
    class.push_str(" flex items-baseline gap-1.5");
    if band {
        class.push_str(" mt-1 border-t border-[var(--color-border-muted)]");
    }
    rsx! {
        h2 {
            // FLIP fades a header in when a re-sort creates it.
            "data-row-id": "head-{id}",
            class: "{class}",
            span { "{label}" }
            if let Some(c) = count {
                span { class: "font-mono tabular-nums normal-case tracking-normal", "· {c}" }
            }
            if let Some(m) = estimate_minutes {
                span { class: "font-mono tabular-nums normal-case tracking-normal", "· {model::format_minutes(m)}" }
            }
        }
    }
}

// ── Quick add ───────────────────────────────────────────────────────────────

/// Where a quick-add surface's default project came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InheritedFrom {
    /// The Planner was showing this project when ⌘K opened.
    View,
    /// The palette's previous ⇧↵ capture went here.
    LastCapture,
}

/// A surface default project — not text, so it's shown as a dashed pill.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inherited {
    pub(crate) project_id: String,
    pub(crate) why: InheritedFrom,
}

/// Everything a quick-add surface knows beyond its text. The text stays the
/// source of truth: pins only disambiguate a picked token, and a pending
/// create only exists while its `+slug` is still typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QuickAddDraft {
    pub(crate) text: String,
    /// slug → project id, written by list picks (see `ProjectIndex::with_pins`).
    pub(crate) pins: HashMap<String, String>,
    /// Surface default (the view's project, or the last ⇧↵ capture's).
    pub(crate) inherited: Option<Inherited>,
    /// "Create project …" chosen from the list: created at commit, never before.
    pub(crate) create_project: Option<String>,
}

impl QuickAddDraft {
    pub(crate) fn with_inherited(inherited: Option<Inherited>) -> Self {
        Self { inherited, ..Default::default() }
    }

    fn index(&self, state: &PlannerState) -> ProjectIndex {
        ProjectIndex::from_state(state)
            .with_pins(&self.pins)
            .with_pending(self.create_project.as_deref())
    }

    pub(crate) fn parse(&self, state: &PlannerState, day: NaiveDate) -> QuickAddParse {
        parse_quick_add_with(&self.text, day, &self.index(state))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BirthProject {
    None,
    Existing(String),
    /// Create a project with this title at commit.
    Create(String),
}

/// Where a quick-added todo is born: the placement half of the commit,
/// pure so the precedence is testable without Signals.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Birth {
    pub(crate) project: BirthProject,
    pub(crate) bucket: TodoBucket,
    pub(crate) scheduled_for: Option<NaiveDate>,
}

/// Project: a resolved `+token` beats a surface's inherited project, which
/// beats the view's own project. Bucket: the view's default, then `*day` or
/// `@clock` triage it, then filing into a project triages an Inbox birth.
pub(crate) fn resolve_birth(
    parsed: &QuickAddParse,
    draft: &QuickAddDraft,
    selection: &PlannerSelection,
    day: NaiveDate,
) -> Birth {
    let mut bucket = TodoBucket::Inbox;
    let mut scheduled_for = None;
    let mut view_project = None;
    // New todos land in the context being looked at, so quick-add never
    // makes work vanish into a different list. Scheduled births are triaged
    // (Anytime): clearing the date later must not dump them into Inbox.
    match selection {
        PlannerSelection::View(TodoView::Today) => {
            scheduled_for = Some(day);
            bucket = TodoBucket::Anytime;
        }
        PlannerSelection::View(TodoView::Upcoming) => {
            scheduled_for = day.succ_opt();
            bucket = TodoBucket::Anytime;
        }
        PlannerSelection::View(TodoView::Anytime) => bucket = TodoBucket::Anytime,
        PlannerSelection::View(TodoView::Someday) => bucket = TodoBucket::Someday,
        PlannerSelection::Project(id) => {
            view_project = Some(id.clone());
            bucket = TodoBucket::Anytime;
        }
        _ => {} // Inbox (and Logbook, which hides quick-add)
    }

    // A *day token overrides the view-context date — the user said when.
    if let Some(d) = parsed.scheduled {
        scheduled_for = Some(d);
        bucket = TodoBucket::Anytime;
    }
    // An @clock token means "this happens at that time": it implies a day
    // (today unless something already picked one).
    if parsed.block_start.is_some() {
        scheduled_for.get_or_insert(day);
        if bucket == TodoBucket::Inbox {
            bucket = TodoBucket::Anytime;
        }
    }

    let project = match &parsed.project {
        Some(p) if p.id == PENDING_PROJECT_ID => {
            BirthProject::Create(draft.create_project.clone().unwrap_or_else(|| p.title.clone()))
        }
        Some(p) => BirthProject::Existing(p.id.clone()),
        None => match (&draft.inherited, view_project) {
            (Some(i), _) => BirthProject::Existing(i.project_id.clone()),
            (None, Some(id)) => BirthProject::Existing(id),
            (None, None) => BirthProject::None,
        },
    };
    // Choosing a project is a triage decision: the todo is born in Anytime.
    if project != BirthProject::None && bucket == TodoBucket::Inbox {
        bucket = TodoBucket::Anytime;
    }
    Birth { project, bucket, scheduled_for }
}

/// Where a todo ended up, in the words the Planner's sidebar uses, led by
/// its project: "Hobbes RC · Friday at 2:00 PM".
pub(crate) fn landing_label(
    project: Option<&str>,
    scheduled_for: Option<NaiveDate>,
    bucket: TodoBucket,
    block_start: Option<u32>,
    day: NaiveDate,
) -> String {
    let place = match scheduled_for {
        Some(d) if d == day => "Today".to_string(),
        Some(d) => views::friendly_date(d, day),
        None => match bucket {
            TodoBucket::Someday => "Someday".to_string(),
            TodoBucket::Anytime => "Anytime".to_string(),
            TodoBucket::Inbox => "Inbox".to_string(),
        },
    };
    let place = match block_start {
        Some(m) => format!("{place} at {}", fmt_hhmm(m)),
        None => place,
    };
    match project {
        // A project's Anytime is just "the project".
        Some(p) if scheduled_for.is_none() && bucket == TodoBucket::Anytime => p.to_string(),
        Some(p) => format!("{p} · {place}"),
        None => place,
    }
}

/// The project title a birth lands in, for labels.
pub(crate) fn birth_project_title(state: &PlannerState, project: &BirthProject) -> Option<String> {
    match project {
        BirthProject::None => None,
        BirthProject::Create(t) => Some(t.clone()),
        BirthProject::Existing(id) => state.projects.iter().find(|p| &p.id == id).map(|p| p.title.clone()),
    }
}

/// `"garden-shed"` → `"Garden shed"`: the title a "Create project" row offers.
fn title_from_query(q: &str) -> String {
    let words = q.replace('-', " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Splits `title` into runs marked by whether they match `query`'s
/// characters in order (the list's highlight).
fn highlight_runs(title: &str, query: &str) -> Vec<(String, bool)> {
    let mut want = query.chars().filter(|c| *c != '-').flat_map(char::to_lowercase).peekable();
    let mut runs: Vec<(String, bool)> = Vec::new();
    for c in title.chars() {
        let hit = want.peek().is_some_and(|w| c.to_lowercase().any(|l| l == *w));
        if hit {
            want.next();
        }
        match runs.last_mut() {
            Some((s, h)) if *h == hit => s.push(c),
            _ => runs.push((c.to_string(), hit)),
        }
    }
    runs
}

/// Picking from a project picker is a text edit: it writes (or removes) the
/// `+slug` token and pins it, so the pick survives colliding slugs.
pub(crate) fn apply_project_pick(mut draft: Signal<QuickAddDraft>, state: &PlannerState, pick: Option<String>) {
    let mut d = draft.write();
    d.create_project = None;
    match pick.and_then(|id| state.projects.iter().find(|p| p.id == id)) {
        Some(p) => {
            let s = project_match::slug(&p.title);
            d.text = set_project_token(&d.text, Some(&s));
            d.pins.insert(s, p.id.clone());
        }
        None => {
            d.text = set_project_token(&d.text, None);
            d.inherited = None;
        }
    }
    if !d.text.is_empty() && !d.text.ends_with(' ') {
        d.text.push(' ');
    }
}

fn create_project(mut planner: Signal<PlannerState>, title: &str) -> String {
    let now = Utc::now();
    let sort_order = planner
        .peek()
        .projects
        .iter()
        .map(|p| p.sort_order)
        .fold(0.0f64, f64::max)
        + model::SORT_STEP;
    let project = model::Project {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.trim().to_string(),
        notes: String::new(),
        area_id: None,
        status: TodoStatus::default(),
        deadline: None,
        sort_order,
        path: None,
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = store::save_project(&project) {
        tracing::error!("planner: failed to save new project: {}", e);
    }
    let id = project.id.clone();
    planner.write().projects.push(project);
    id
}

#[component]
fn QuickAdd(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut draft = use_signal(QuickAddDraft::default);
    // "Added … → Hobbes RC", only when the todo landed outside this list.
    // The counter lets a newer confirmation outlive an older one's timer.
    let mut confirmation = use_signal(|| None::<(u64, String, String)>);

    let submit = move |_keep_open: bool| {
        let sel = selection.peek().clone();
        let d = draft.peek().clone();
        let day = today();
        let block_start = d.parse(&planner.peek(), day).block_start;
        let Some(todo) = commit_quick_add(planner, settings, &d, &sel) else {
            return; // no title yet — the preview already says so
        };
        // The input stays mounted, so focus is retained across submits.
        draft.set(QuickAddDraft::default());
        let (visible, landing) = {
            let state = planner.peek();
            let visible = todos_for_selection(&state, &sel, day).iter().any(|t| t.id == todo.id);
            let project = todo
                .project_id
                .as_ref()
                .and_then(|id| state.projects.iter().find(|p| &p.id == id))
                .map(|p| p.title.clone());
            (
                visible,
                landing_label(project.as_deref(), todo.scheduled_for, todo.bucket, block_start, day),
            )
        };
        if visible {
            return;
        }
        let generation = confirmation.peek().as_ref().map_or(0, |c| c.0) + 1;
        confirmation.set(Some((generation, todo.title, landing)));
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            if confirmation.peek().as_ref().is_some_and(|c| c.0 == generation) {
                confirmation.set(None);
            }
        });
    };

    rsx! {
        div {
            class: "px-6 pb-3",
            // The composer idiom: one framed well that lights on focus, with
            // the input itself invisible inside it.
            div {
                class: "bg-input border border-subtle rounded-xl px-3.5 pt-2.5 pb-2 focus-within:ring-2 focus-within:ring-primary-500",
                QuickAddField {
                    draft,
                    selection: selection.read().clone(),
                    on_submit: submit,
                    on_escape: move |_| {
                        let _ = document::eval("document.activeElement?.blur()");
                    },
                    input_class: "w-full bg-transparent border-0 p-0 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:ring-0",
                    placeholder: "Add a to-do — ~30m · #tag · +project · @2pm · *mon · !fri · !!1 — press Enter",
                }
            }
            if let Some((_, title, landing)) = confirmation.read().clone() {
                div {
                    class: "mt-1.5 px-1 flex items-center gap-1.5 text-[11px] text-fg-muted",
                    Icon { width: 11, height: 11, icon: fi_icons::FiCheck }
                    span { class: "truncate", "Added “{title}” → {landing}" }
                }
            }
        }
    }
}

/// Turns a quick-add draft into a saved todo (plus a linked timeline block
/// for an `@clock` token, and the project a "Create project …" pick asked
/// for). `selection` is the context the todo is born into; tokens override
/// it (see [`resolve_birth`]). Returns the saved todo, or None — saving
/// nothing — when the text has no title. Shared by the Planner's inline
/// quick-add and the Cmd+K quick-capture palette.
pub(crate) fn commit_quick_add(
    mut planner: Signal<PlannerState>,
    settings: Signal<Settings>,
    draft: &QuickAddDraft,
    selection: &PlannerSelection,
) -> Option<Todo> {
    let day = today();
    let parsed = draft.parse(&planner.peek(), day);
    if parsed.title.is_empty() {
        return None; // tokens without a title aren't a todo yet
    }
    let birth = resolve_birth(&parsed, draft, selection, day);

    let mut todo = Todo::new(parsed.title.clone(), planner.peek().next_sort_order());
    todo.project_id = match birth.project {
        BirthProject::None => None,
        BirthProject::Existing(id) => Some(id),
        // Created only now, at commit: an abandoned draft creates nothing.
        BirthProject::Create(title) => Some(create_project(planner, &title)),
    };
    todo.bucket = birth.bucket;
    todo.scheduled_for = birth.scheduled_for;

    // Tokens beat defaults; defaults beat nothing.
    let default_estimate = settings.peek().planner_default_estimate_minutes;
    todo.estimate_minutes = parsed
        .estimate_minutes
        .or((default_estimate > 0).then_some(default_estimate));
    todo.tags = parsed.tags;
    todo.deadline = parsed.deadline;
    todo.time_of_day = parsed.time_of_day;
    todo.priority = parsed.priority;

    // An @clock token also implies a time-of-day group when none was given
    // explicitly, so "@7pm" lands in This Evening without a second token.
    if todo.time_of_day.is_none() {
        todo.time_of_day = parsed.block_start.map(|m| match m / 60 {
            0..=11 => TimeOfDay::Morning,
            12..=16 => TimeOfDay::Afternoon,
            _ => TimeOfDay::Evening,
        });
    }

    if let Err(e) = store::save_todo(&todo) {
        tracing::error!("planner: failed to save new todo: {}", e);
    }

    // resolve_birth already gave an @clock todo its day.
    let block = match (todo.scheduled_for, parsed.block_start) {
        (Some(d), Some(start)) => {
            let duration = todo
                .estimate_minutes
                .unwrap_or(settings.peek().planner_default_block_minutes)
                .max(15);
            let block = TimeBlock {
                id: uuid::Uuid::new_v4().to_string(),
                todo_id: Some(todo.id.clone()),
                title: todo.title.clone(),
                start: local_minutes_to_utc(d, start),
                end: local_minutes_to_utc(d, start + duration),
                source: BlockSource::Manual,
            };
            persist_block(&block);
            Some(block)
        }
        _ => None,
    };

    {
        let mut state = planner.write();
        state.upsert_todo(todo.clone());
        if let Some(b) = block {
            state.blocks.push(b);
        }
    }
    Some(todo)
}

/// One row of the `+project` autocomplete list.
#[derive(Clone, Debug, PartialEq)]
enum ProjectRow {
    Project(Ranked),
    Create(String),
}

/// Input + `+project` autocomplete + chip row, shared by the Planner's
/// inline add and the ⌘K palette. They differ only in classes and in what
/// Enter / Esc do.
#[component]
pub(crate) fn QuickAddField(
    draft: Signal<QuickAddDraft>,
    /// The context the todo is born into (the palette passes Inbox).
    selection: PlannerSelection,
    /// Enter (false) / ⇧Enter (true) with the list closed.
    on_submit: EventHandler<bool>,
    /// Esc with the list closed. The palette closes; the inline add blurs.
    on_escape: EventHandler<()>,
    input_class: String,
    placeholder: String,
    #[props(default)] autofocus: bool,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut hi = use_signal(|| 0usize);
    // Esc closes the list for this query only; typing on reopens it.
    let mut dismissed_for = use_signal(|| None::<String>);
    let mut input_el = use_signal(|| None::<std::rc::Rc<MountedData>>);

    let text = draft.read().text.clone();
    let query = active_project_query(&text).map(str::to_string);
    let rows: Vec<ProjectRow> = match &query {
        Some(q) if dismissed_for.read().as_deref() != Some(q.as_str()) => {
            let state = planner.read();
            let ix = ProjectIndex::from_state(&state).with_pins(&draft.read().pins);
            // An exact token needs no list: "… +hobbes-rc⏎" submits at once.
            let exact = matches!(
                ix.resolve(q),
                ProjectResolution::Resolved { tier: MatchTier::Exact, .. }
            );
            if exact {
                Vec::new()
            } else {
                let mut rows: Vec<ProjectRow> =
                    ix.rank(q).into_iter().take(8).map(ProjectRow::Project).collect();
                let title = title_from_query(q);
                if !title.is_empty() {
                    rows.push(ProjectRow::Create(title));
                }
                rows
            }
        }
        _ => Vec::new(),
    };
    let list_open = !rows.is_empty();
    let hi_now = (*hi.read()).min(rows.len().saturating_sub(1));

    let accept = move |row: ProjectRow| {
        {
            let mut d = draft.write();
            match row {
                ProjectRow::Project(r) => {
                    d.text = set_project_token(&d.text, Some(&r.slug));
                    d.pins.insert(r.slug, r.id);
                }
                ProjectRow::Create(title) => {
                    d.text = set_project_token(&d.text, Some(&title));
                    d.create_project = Some(title);
                }
            }
            // The trailing space closes the list; the next ↵ submits.
            d.text.push(' ');
        }
        hi.set(0);
    };

    let query_for_list = query.clone().unwrap_or_default();

    rsx! {
        div {
            class: "relative",
            input {
                class: "{input_class}",
                r#type: "text",
                placeholder: "{placeholder}",
                value: "{text}",
                autofocus,
                onmounted: move |evt| {
                    let mounted = evt.data();
                    input_el.set(Some(mounted.clone()));
                    if autofocus {
                        spawn(async move {
                            let _ = mounted.set_focus(true).await;
                        });
                    }
                },
                oninput: move |evt| {
                    draft.write().text = evt.value();
                    hi.set(0);
                },
                onkeydown: {
                    let rows = rows.clone();
                    let query = query.clone();
                    let mut accept = accept;
                    move |evt: KeyboardEvent| {
                        let key = evt.key();
                        let ctrl = evt.modifiers().ctrl();
                        // Every key the list claims is prevent_default()ed, so
                        // the global hotkey listener (which skips
                        // defaultPrevented) never sees ⌃N/⌃P or friends.
                        if list_open {
                            let step: Option<i64> = match &key {
                                Key::ArrowDown => Some(1),
                                Key::ArrowUp => Some(-1),
                                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("n") => Some(1),
                                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("p") => Some(-1),
                                _ => None,
                            };
                            if let Some(s) = step {
                                evt.prevent_default();
                                let n = rows.len() as i64;
                                hi.set((hi_now as i64 + s).rem_euclid(n) as usize);
                                return;
                            }
                            match key {
                                // ⇧↵ accepts too — it never submits past an open list.
                                Key::Enter | Key::Tab => {
                                    evt.prevent_default();
                                    if let Some(row) = rows.get(hi_now) {
                                        accept(row.clone());
                                    }
                                    return;
                                }
                                Key::Escape => {
                                    // Stop here so the palette card doesn't close.
                                    evt.prevent_default();
                                    evt.stop_propagation();
                                    dismissed_for.set(query.clone());
                                    return;
                                }
                                _ => {}
                            }
                        }
                        match key {
                            Key::Enter => {
                                evt.prevent_default();
                                on_submit.call(evt.modifiers().contains(Modifiers::SHIFT));
                            }
                            Key::Escape => {
                                evt.prevent_default();
                                evt.stop_propagation();
                                on_escape.call(());
                            }
                            // The tokenized-input gesture: ⌫ in an empty input
                            // clears the inherited project, never typed text.
                            Key::Backspace => {
                                let clear = {
                                    let d = draft.peek();
                                    d.text.is_empty() && d.inherited.is_some()
                                };
                                if clear {
                                    evt.prevent_default();
                                    draft.write().inherited = None;
                                }
                            }
                            _ => {}
                        }
                    }
                },
            }
            if list_open {
                div {
                    class: "{POPOVER} left-0 right-auto w-72",
                    div { class: "{MENU_LABEL}", "Project" }
                    for (i, row) in rows.iter().cloned().enumerate() {
                        button {
                            key: "{i}",
                            class: if i == hi_now { "{MENU_ITEM} bg-input" } else { "{MENU_ITEM}" },
                            // Keep focus (and the caret) in the input.
                            onmousedown: move |evt| evt.prevent_default(),
                            onmouseenter: move |_| hi.set(i),
                            onclick: {
                                let row = row.clone();
                                let mut accept = accept;
                                move |evt: MouseEvent| {
                                    evt.stop_propagation();
                                    accept(row.clone());
                                    if let Some(el) = input_el.peek().clone() {
                                        spawn(async move {
                                            let _ = el.set_focus(true).await;
                                        });
                                    }
                                }
                            },
                            match row.clone() {
                                ProjectRow::Project(r) => rsx! {
                                    Icon { width: 11, height: 11, icon: fi_icons::FiFolder }
                                    span {
                                        class: "flex-1 truncate",
                                        for (j, (run, hit)) in highlight_runs(&r.title, &query_for_list).into_iter().enumerate() {
                                            span { key: "{j}", class: if hit { "text-primary-300" } else { "" }, "{run}" }
                                        }
                                    }
                                    if let Some(area) = r.area.clone() {
                                        span { class: "{META}", "{area}" }
                                    }
                                },
                                ProjectRow::Create(title) => rsx! {
                                    Icon { width: 11, height: 11, icon: fi_icons::FiPlus }
                                    span { class: "flex-1 truncate", "Create project “{title}”" }
                                },
                            }
                        }
                    }
                    div {
                        class: "{MENU_RULE}",
                    }
                    div { class: "px-2.5 pb-1 text-[11px] text-fg-muted", "↑↓ choose · ↵ pick · esc close" }
                }
            }
        }
        QuickAddPreview { draft, selection, input_el }
    }
}

/// The project pill's state, first in the chip row.
#[derive(Clone, Debug, PartialEq)]
enum ProjectPill {
    /// A resolved `+token`. `not_view` names the view's project it overrides.
    Token { title: String, not_view: Option<String> },
    Creating { title: String },
    Inherited { title: String, why: InheritedFrom },
}

/// The token chips under a quick-add input: what Enter will actually do.
#[component]
fn QuickAddPreview(
    draft: Signal<QuickAddDraft>,
    selection: PlannerSelection,
    input_el: Signal<Option<std::rc::Rc<MountedData>>>,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut picker_open = use_signal(|| false);
    // Parsed inline on every keystroke (P-012: draft is a Signal, so this
    // tracks). The chips show what Enter will actually do — the premium tell
    // of Things/Todoist quick entry is that tokens are confirmed before commit.
    let day = today();
    let (preview, birth, pill) = {
        let state = planner.read();
        let d = draft.read();
        let preview = d.parse(&state, day);
        let birth = resolve_birth(&preview, &d, &selection, day);
        let title_of = |id: &str| state.projects.iter().find(|p| p.id == id).map(|p| p.title.clone());
        let pill = match &birth.project {
            BirthProject::Create(title) => Some(ProjectPill::Creating { title: title.clone() }),
            BirthProject::Existing(id) if preview.project.is_some() => {
                let not_view = match &selection {
                    PlannerSelection::Project(v) if v != id => title_of(v),
                    _ => None,
                };
                title_of(id).map(|title| ProjectPill::Token { title, not_view })
            }
            // The inline add in a project view shows no pill: the heading
            // already says which project.
            BirthProject::Existing(id) => d.inherited.as_ref().and_then(|i| {
                title_of(id).map(|title| ProjectPill::Inherited { title, why: i.why })
            }),
            BirthProject::None => None,
        };
        (preview, birth, pill)
    };
    let current_project = match &birth.project {
        BirthProject::Existing(id) => Some(id.clone()),
        _ => None,
    };
    let miss_label = match &preview.project_miss {
        Some(ProjectResolution::NoMatch { query }) => {
            Some(format!("+{query} · no such project, stays in title"))
        }
        Some(ProjectResolution::Ambiguous { query, candidates }) => {
            Some(format!("+{query} · {candidates} projects match, stays in title"))
        }
        _ => None,
    };
    let timeline_hint = preview
        .block_start
        .map(|m| format!("Enter to add · goes on the timeline at {}", fmt_hhmm(m)));
    let priority_display = settings.read().planner_priority.clone();

    let refocus = move || {
        if let Some(el) = input_el.peek().clone() {
            spawn(async move {
                let _ = el.set_focus(true).await;
            });
        }
    };

    rsx! {
        if preview.has_tokens() || pill.is_some() {
            div {
                class: "mt-2 flex flex-wrap items-center gap-1.5",
                if pill.is_some() || miss_label.is_some() {
                    div {
                        class: "relative flex items-center gap-1.5 shrink-0",
                        match pill.clone() {
                            Some(ProjectPill::Inherited { title, why }) => rsx! {
                                span {
                                    class: "{PILL} border-dashed border-primary-600 text-fg pr-1",
                                    button {
                                        class: "inline-flex items-center gap-1.5 max-w-48 hover:text-fg",
                                        title: "Change project",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            picker_open.toggle();
                                        },
                                        Icon { width: 10, height: 10, icon: fi_icons::FiFolder }
                                        span { class: "truncate", "{title}" }
                                        span { class: "text-fg-muted",
                                            match why {
                                                InheritedFrom::View => "· from view",
                                                InheritedFrom::LastCapture => "· from last",
                                            }
                                        }
                                    }
                                    button {
                                        class: "px-1 text-fg-muted hover:text-fg",
                                        title: "Don't file into this project (⌫ in an empty input)",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            draft.write().inherited = None;
                                        },
                                        "×"
                                    }
                                }
                            },
                            Some(ProjectPill::Token { title, not_view }) => rsx! {
                                button {
                                    class: "{PILL} border-primary-600 text-fg max-w-64 hover:brightness-125",
                                    title: "Change project",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        picker_open.toggle();
                                    },
                                    Icon { width: 10, height: 10, icon: fi_icons::FiFolder }
                                    span { class: "truncate", "{title}" }
                                    if let Some(v) = not_view {
                                        span { class: "text-fg-muted truncate", "· not {v}" }
                                    }
                                }
                            },
                            Some(ProjectPill::Creating { title }) => rsx! {
                                button {
                                    class: "{PILL} border-dashed border-primary-600 text-fg max-w-64",
                                    title: "Created when you add this to-do",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        picker_open.toggle();
                                    },
                                    Icon { width: 10, height: 10, icon: fi_icons::FiPlus }
                                    span { class: "truncate", "New project “{title}”" }
                                }
                            },
                            None => rsx! {},
                        }
                        if let Some(label) = miss_label.clone() {
                            button {
                                class: "{PILL} border-yellow-700 text-yellow-300 max-w-72",
                                title: "Not filed — pick a project",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    picker_open.toggle();
                                },
                                span { class: "truncate", "{label}" }
                            }
                        }
                        if *picker_open.read() {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    picker_open.set(false);
                                },
                            }
                            div {
                                class: "{POPOVER} left-0 right-auto",
                                onkeydown: {
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Escape {
                                            evt.prevent_default();
                                            evt.stop_propagation();
                                            picker_open.set(false);
                                            refocus();
                                        }
                                    }
                                },
                                ProjectPicker {
                                    current: current_project.clone(),
                                    heading: "File under…",
                                    on_pick: {
                                        move |pick: Option<String>| {
                                            apply_project_pick(draft, &planner.peek(), pick);
                                            picker_open.set(false);
                                            refocus();
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
                if let Some(m) = preview.estimate_minutes {
                    span { class: "{PILL}", "{model::format_minutes(m)}" }
                }
                if let Some(start) = preview.block_start {
                    span {
                        class: "{PILL} border-primary-600 text-fg",
                        "on timeline @{fmt_hhmm(start)}"
                    }
                }
                if let Some(tod) = preview.time_of_day {
                    span { class: "{PILL}",
                        match tod {
                            TimeOfDay::Morning => "morning",
                            TimeOfDay::Afternoon => "afternoon",
                            TimeOfDay::Evening => "this evening",
                        }
                    }
                }
                if let Some(d) = preview.scheduled {
                    span {
                        class: "{PILL}",
                        Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                        "→ {views::friendly_date(d, day)}"
                    }
                }
                if let Some(d) = preview.deadline {
                    span {
                        class: "{PILL} border-red-700 text-red-400",
                        "due {views::friendly_date(d, day)}"
                    }
                }
                if let Some(p) = preview.priority {
                    span {
                        class: "{PILL} {priority_tone_class(p)}",
                        "{priority_display.badge(p)} · {priority_display.name(p)}"
                    }
                }
                for tag in preview.tags.iter() {
                    span { class: "{PILL}", "#{tag}" }
                }
                if preview.title.is_empty() {
                    if preview.has_tokens() {
                        span { class: "text-[11px] text-fg-muted", "…needs a title" }
                    }
                } else if let Some(hint) = timeline_hint {
                    span { class: "text-[11px] text-fg-muted ml-auto", "{hint}" }
                }
            }
        }
    }
}

/// One rendered section of the list: a header (or none, for the lead
/// section) and its rows, owned so the render doesn't borrow planner state.
struct Section {
    id: String,
    label: Option<String>,
    warn: bool,
    band: bool,
    count: Option<usize>,
    estimate_minutes: Option<u32>,
    rows: Vec<Todo>,
}

impl Section {
    fn from_group(g: RowGroup<'_>, grouped: bool) -> Self {
        let open = g.rows.iter().filter(|t| !t.status.is_closed()).count();
        let counted = g.label.is_some() && (g.band || grouped);
        Section {
            count: counted.then_some(if open > 0 { open } else { g.rows.len() }),
            estimate_minutes: (grouped && g.estimate_minutes > 0).then_some(g.estimate_minutes),
            id: g.id,
            label: g.label,
            warn: g.warn,
            band: g.band,
            rows: g.rows.into_iter().cloned().collect(),
        }
    }
}

#[component]
fn TodoList(
    selection: Signal<PlannerSelection>,
    order: Signal<ListOrder>,
    undo: Signal<Option<AdoptUndo>>,
    flip: Signal<(u64, u32)>,
    set_order: Callback<ListOrder>,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let sel = selection.read().clone();
    let current = *order.read();
    let day = today();
    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);

    // ── Arrange ────────────────────────────────────────────────────────────
    // The lens runs over the same queries as before (`in_view`,
    // `completed_today`, `upcoming_grouped`); Manual + no grouping renders
    // exactly what those return.
    let sections: Vec<Section> = {
        let state = planner.read();
        let slots = block_slots(&state.todos, &state.blocks);
        let projects: HashMap<String, (f64, String)> = state
            .projects
            .iter()
            .map(|p| (p.id.clone(), (p.sort_order, p.title.clone())))
            .collect();
        let inputs = |split_evening: bool, bands: bool| ArrangeInputs {
            today: day,
            slots: &slots,
            projects: &projects,
            split_evening,
            bands,
        };
        let grouped = current.group != GroupBy::None;
        match &sel {
            PlannerSelection::View(TodoView::Logbook) => {
                let rows = todos_for_selection(&state, &sel, day);
                if rows.is_empty() {
                    Vec::new()
                } else {
                    vec![Section {
                        id: "main".into(),
                        label: None,
                        warn: false,
                        band: false,
                        count: None,
                        estimate_minutes: None,
                        rows,
                    }]
                }
            }
            // Day headers are Upcoming's grouping; the sort applies within
            // each day, with no bands (headers never nest).
            PlannerSelection::View(TodoView::Upcoming) => {
                let within = ListOrder { group: GroupBy::None, ..current };
                let inputs = inputs(false, false);
                views::upcoming_grouped(&state.todos, day)
                    .into_iter()
                    .map(|(d, ts)| Section {
                        id: format!("day-{d}"),
                        label: Some(views::friendly_date(d, day)),
                        warn: false,
                        band: false,
                        count: None,
                        estimate_minutes: None,
                        rows: views::arrange(ts, Vec::new(), within, &inputs)
                            .into_iter()
                            .flat_map(|g| g.rows)
                            .cloned()
                            .collect(),
                    })
                    .collect()
            }
            // Completing a todo keeps its row visible for the rest of the
            // day (struck through, reopenable in place): Today passes the
            // day's closures alongside the open work — a separate query from
            // view membership, which the AI's list tool needs to stay "open
            // work owed".
            PlannerSelection::View(TodoView::Today) => views::arrange(
                views::in_view(&state.todos, TodoView::Today, day),
                views::completed_today(&state.todos, day),
                current,
                &inputs(true, true),
            )
            .into_iter()
            .map(|g| Section::from_group(g, grouped))
            .collect(),
            _ => {
                let open = todos_for_selection(&state, &sel, day);
                views::arrange(open.iter().collect(), Vec::new(), current, &inputs(false, true))
                    .into_iter()
                    .map(|g| Section::from_group(g, grouped))
                    .collect()
            }
        }
    };
    // Priority bands speak the user's priority names (views stays pure and
    // names them by quadrant).
    let sections: Vec<Section> = if current.key == SortKey::Priority {
        let names = settings.read().planner_priority.clone();
        sections
            .into_iter()
            .map(|mut s| {
                if let Some(p) = s.id.strip_prefix("band-").and_then(Priority::parse) {
                    s.label = Some(names.name(p));
                }
                s
            })
            .collect()
    } else {
        sections
    };
    let is_empty = sections.is_empty();

    // Each group's open rows in on-screen order: what a drag or a menu move
    // reorders within, and what an adopt flattens.
    let drag_groups: Vec<(String, Vec<String>)> = if in_logbook {
        Vec::new()
    } else {
        sections
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.rows
                        .iter()
                        .filter(|t| !t.status.is_closed())
                        .map(|t| t.id.clone())
                        .collect(),
                )
            })
            .collect()
    };

    // ── Drag plumbing ──────────────────────────────────────────────────────
    let mut drag = use_hook(|| CopyValue::new(Option::<ListDrag>::None));
    let mut drag_active = use_signal(|| false);

    let reorder = use_callback({
        let groups = drag_groups.clone();
        move |(id, r): (String, Reorder)| {
            // ⌥↑/⌥↓ are Manual-only; top/bottom work from any mode (a sorted
            // list adopts, like a drag).
            if matches!(r, Reorder::Up | Reorder::Down) && order.peek().key != SortKey::Manual {
                return;
            }
            let Some((gid, ids)) = groups.iter().find(|g| g.1.contains(&id)) else {
                return;
            };
            let from = ids.iter().position(|x| *x == id).unwrap_or(0);
            let Some(to) = reorder_target(from, ids.len(), r) else {
                return;
            };
            flip_snap();
            apply_move(planner, order, undo, set_order, &groups, &id, gid, to);
            bump_flip(flip, 220);
        }
    });
    use_context_provider(|| ListDragContext { drag, reorder });

    let commit_drag = use_callback({
        let groups = drag_groups.clone();
        move |_: ()| {
            let taken = drag.write().take();
            drag_active.set(false);
            let Some(d) = taken else { return };
            if !d.active {
                return;
            }
            // Snap while the lifted row still sits under the pointer, so the
            // FLIP play settles it from there into its slot.
            flip_snap();
            if d.over_index != d.from_index {
                apply_move(planner, order, undo, set_order, &groups, &d.todo_id, &d.group_id, d.over_index);
            }
            bump_flip(flip, 180);
        }
    });

    // One read of the fleet mirror for the whole list: rows receive their own
    // carrier session as a prop rather than each reaching for shared state.
    let fleet = fleet_state.read();
    let carrier = |t: &Todo| -> Option<FleetSession> {
        t.linked_fleet_session
            .as_ref()
            .and_then(|id| fleet.sessions.get(id).cloned())
    };

    rsx! {
        div {
            class: "flex flex-col px-4",
            // Arming → live: the drag starts only after 4px of travel, so a
            // click on a row (title, pills, buttons) stays a click.
            onmousemove: {
                let groups = drag_groups.clone();
                move |evt: MouseEvent| {
                    let y = evt.client_coordinates().y;
                    let went_live = {
                        let mut w = drag.write();
                        match w.as_mut() {
                            Some(d) if !d.active && (y - d.grab_y).abs() > 4.0 => {
                                match groups.iter().find(|g| g.1.contains(&d.todo_id)) {
                                    Some((gid, ids)) => {
                                        let from = ids.iter().position(|x| *x == d.todo_id).unwrap_or(0);
                                        d.group_id = gid.clone();
                                        d.ids = ids.clone();
                                        d.from_index = from;
                                        d.over_index = from;
                                        d.active = true;
                                        true
                                    }
                                    None => {
                                        *w = None;
                                        false
                                    }
                                }
                            }
                            _ => false,
                        }
                    };
                    if went_live {
                        drag_active.set(true);
                    }
                }
            },
            onmouseup: move |_| {
                if !*drag_active.peek() {
                    drag.set(None);
                }
            },
            onmouseleave: move |_| {
                if !*drag_active.peek() {
                    drag.set(None);
                }
            },
            if is_empty {
                p { class: "px-2.5 py-8 text-sm text-fg-muted", "{empty_state_line(&sel)}" }
            }
            for section in sections {
                div {
                    key: "{section.id}",
                    if let Some(label) = section.label.clone() {
                        GroupHeader {
                            id: section.id.clone(),
                            label,
                            count: section.count,
                            estimate_minutes: section.estimate_minutes,
                            band: section.band,
                            warn: section.warn,
                        }
                    }
                    for todo in section.rows {
                        // Cloned because the generated key borrows `todo.id`
                        // after the prop takes ownership.
                        TodoRow {
                            key: "{todo.id}",
                            fleet: carrier(&todo),
                            draggable: !in_logbook && !todo.status.is_closed(),
                            sort_key: current.key,
                            todo: todo.clone(),
                            selection,
                        }
                    }
                }
            }
            // While a drag is live, a fullscreen overlay owns the pointer —
            // the column-resize pattern from PlannerView. Positions go straight
            // to the DOM; only the insertion index lives in Rust.
            if *drag_active.read() {
                div {
                    class: "fixed inset-0 z-50 cursor-grabbing",
                    onmousemove: move |evt: MouseEvent| {
                        let y = evt.client_coordinates().y;
                        let js = {
                            let mut w = drag.write();
                            let Some(d) = w.as_mut() else { return };
                            let dy = y - d.grab_y;
                            d.over_index = drag_over_index(d.from_index, d.ids.len(), dy);
                            format!(
                                "window.hobbesDrag?.move({}, {}, {}, {})",
                                serde_json::to_string(&d.ids).unwrap_or_else(|_| "[]".into()),
                                d.from_index,
                                d.over_index,
                                dy
                            )
                        };
                        let _ = document::eval(&js);
                    },
                    onmouseup: move |_| commit_drag.call(()),
                    onmouseleave: move |_| commit_drag.call(()),
                }
            }
        }
    }
}

/// The project list behind every project picker: the row's pill, the ⋯
/// menu's "Move to project…", and the quick-add pill and palette header. It
/// only reports the pick; each caller decides what a pick does.
#[component]
pub(crate) fn ProjectPicker(
    current: Option<String>,
    on_pick: EventHandler<Option<String>>,
    #[props(into, default = "Move to project…".to_string())] heading: String,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut filter = use_signal(String::new);
    let (projects, show_filter): (Vec<(String, String)>, bool) = {
        let state = planner.read();
        let mut ps: Vec<(String, String)> = state
            .projects
            .iter()
            .filter(|p| !p.status.is_closed())
            .map(|p| (p.id.clone(), p.title.clone()))
            .collect();
        ps.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        // A filter field once the list outgrows a glance.
        let show_filter = ps.len() > 6;
        let q = filter.read().trim().to_string();
        if !q.is_empty() {
            ps = ProjectIndex::from_state(&state)
                .rank(&q)
                .into_iter()
                .map(|r| (r.id, r.title))
                .collect();
        }
        (ps, show_filter)
    };
    let first = projects.first().map(|p| p.0.clone());

    rsx! {
        div { class: "{MENU_LABEL}", "{heading}" }
        if show_filter {
            div {
                class: "px-2.5 pb-1.5",
                input {
                    class: "w-full rounded border border-subtle bg-input px-1.5 py-0.5 text-[11px] text-fg focus:outline-none",
                    r#type: "text",
                    placeholder: "Filter projects…",
                    autofocus: true,
                    value: "{filter}",
                    onclick: move |evt| evt.stop_propagation(),
                    oninput: move |evt| filter.set(evt.value()),
                    onkeydown: move |evt: KeyboardEvent| {
                        if evt.key() == Key::Enter {
                            evt.prevent_default();
                            if let Some(id) = first.clone() {
                                on_pick.call(Some(id));
                            }
                        }
                    },
                }
            }
        }
        if projects.is_empty() && filter.read().is_empty() {
            div { class: "px-2.5 py-1.5 text-[11px] text-fg-muted", "No projects yet." }
        }
        button {
            class: "{MENU_ITEM}",
            onclick: move |evt: MouseEvent| {
                evt.stop_propagation();
                on_pick.call(None);
            },
            span { class: "w-3 shrink-0", if current.is_none() { "✓" } }
            span { class: "truncate text-fg-muted", "No project" }
        }
        for (pid, ptitle) in projects {
            button {
                key: "{pid}",
                class: "{MENU_ITEM}",
                onclick: {
                    let pid = pid.clone();
                    move |evt: MouseEvent| {
                        evt.stop_propagation();
                        on_pick.call(Some(pid.clone()));
                    }
                },
                span { class: "w-3 shrink-0", if current.as_deref() == Some(pid.as_str()) { "✓" } }
                span { class: "truncate", "{ptitle}" }
            }
        }
    }
}

#[component]
fn TodoRow(
    todo: Todo,
    /// The carrier fleet session, resolved once per list render (never locked
    /// from inside a row).
    fleet: Option<FleetSession>,
    mut selection: Signal<PlannerSelection>,
    /// Which chip gets the faint "this is the sort key" ring.
    sort_key: SortKey,
    /// Grip visible + mousedown arms a drag. False for done rows.
    draggable: bool,
) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let session_state = use_context::<Signal<crate::session::SessionState>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);
    // U1.5: first click arms, second click deletes; leaving the row disarms.
    let mut delete_armed = use_signal(|| false);
    let mut menu = use_signal(|| Option::<RowMenu>::None);
    let mut pick_day = use_signal(String::new);
    let mut detail_open = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    let mut selected_block = use_context::<SelectedBlockContext>().0;
    let drag_ctx = try_use_context::<ListDragContext>();

    let sel = selection.read().clone();
    let id = todo.id.clone();
    let day = today();
    let now = Utc::now();
    let is_done = todo.status.is_closed();
    let in_focus = todo.status == TodoStatus::InProgress;
    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);

    // ── WHO ────────────────────────────────────────────────────────────────
    let focus_session = planner.read().open_focus_session_for(&id).cloned();
    // The carrier's identity, when it is one of our own chat tabs.
    let carrier_session_id = todo.linked_fleet_session.clone().or_else(|| {
        focus_session.as_ref().and_then(|f| match &f.actor {
            FocusActor::Agent { session_id } => session_id.clone(),
            FocusActor::Person => None,
        })
    });
    let is_chat_carrier = carrier_session_id
        .as_ref()
        .is_some_and(|cid| session_state.read().sessions.contains_key(cid));
    let profile_badge: Option<(String, String)> = carrier_session_id.as_ref().and_then(|cid| {
        let s = session_state.read();
        let session = s.sessions.get(cid)?;
        let settings_read = settings.read();
        let profile = session
            .composio_profile
            .as_ref()
            .and_then(|pid| settings_read.composio_profiles.iter().find(|p| &p.id == pid))
            .or_else(|| settings_read.get_active_profile())?;
        Some((
            profile
                .name
                .chars()
                .next()
                .unwrap_or('H')
                .to_uppercase()
                .to_string(),
            profile.color.clone(),
        ))
    });
    let profile_name: Option<String> = {
        let settings_read = settings.read();
        settings_read.get_active_profile().map(|p| p.name.clone())
    };

    let who = who_state(
        &todo,
        focus_session.as_ref(),
        fleet.as_ref(),
        profile_badge.clone(),
    );

    // The stripe is the row's left edge; `pl-[7px]` keeps the 10px gutter the
    // un-striped rows have (7 + a 3px border).
    let stripe = match &who {
        WhoState::YouFocusing => " border-l-[3px] border-l-primary-500 pl-[7px]".to_string(),
        WhoState::Hobbes { color, .. } => {
            format!(" border-l-[3px] {} pl-[7px]", profile_stripe_class(color))
        }
        WhoState::FleetAgent => " border-l-[3px] border-l-accent-500 pl-[7px]".to_string(),
        WhoState::NeedsYou => " border-l-[3px] border-l-yellow-700 pl-[7px]".to_string(),
        _ => String::new(),
    };
    let row_class = format!(
        "group relative grid grid-cols-[20px_minmax(0,1fr)_auto] items-center gap-2.5 h-9 px-2.5 rounded-md hover:bg-section{}",
        stripe
    );

    // ── WHAT ───────────────────────────────────────────────────────────────
    let project_title = todo.project_id.as_ref().and_then(|pid| {
        planner
            .read()
            .projects
            .iter()
            .find(|p| &p.id == pid)
            .map(|p| p.title.clone())
    });
    let carrier_label = carrier_session_id.as_ref().and_then(|cid| {
        fleet
            .as_ref()
            .map(|f| f.display_name().to_string())
            .or_else(|| {
                session_state
                    .read()
                    .sessions
                    .get(cid)
                    .map(|s| s.display_title())
            })
    });
    let carrier_minutes = working_minutes(fleet.as_ref(), focus_session.as_ref(), now);
    let carrier_pill_class = match &who {
        WhoState::NeedsYou => format!("{} border-yellow-700 text-yellow-300", PILL),
        WhoState::FleetAgent => format!("{} border-accent-500 text-accent-300", PILL),
        WhoState::Hobbes { color, .. } => {
            format!("{} {} text-fg", PILL, profile_pill_border_class(color))
        }
        _ => PILL.to_string(),
    };

    // ── WHEN ───────────────────────────────────────────────────────────────
    // The row mirrors the timeline: if this todo is timeboxed on its
    // scheduled day, its start time appears as a chip (with the block id so
    // clicking the chip can highlight it on arrival). Scheduling metadata
    // must be legible from every visual state, not just the ruler.
    let block_info: Option<(String, String)> = todo.scheduled_for.and_then(|d| {
        let state = planner.read();
        first_block_on_day(&state.blocks, &todo.id, d).map(|b| {
                (
                    b.start.with_timezone(&Local).format("%H:%M").to_string(),
                    b.id.clone(),
                )
            })
    });
    let date_label = date_pill_label(
        todo.scheduled_for,
        todo.time_of_day == Some(TimeOfDay::Evening),
        day,
    );
    let current_choice = current_schedule_choice(&todo, day);
    // U2.2: closed work surfaces estimate accuracy — "52m of 1h", or just the
    // actual when the work was never estimated.
    let actuals_line = (is_done && todo.actual_minutes > 0).then(|| match todo.estimate_minutes {
        Some(est) => format!(
            "{} of {}",
            model::format_minutes(todo.actual_minutes),
            model::format_minutes(est)
        ),
        None => model::format_minutes(todo.actual_minutes),
    });
    let completed_line = in_logbook.then(|| {
        todo.completed_at
            .map(|d| views::friendly_date(d.with_timezone(&Local).date_naive(), day))
            .unwrap_or_default()
    });

    // ── Handlers ───────────────────────────────────────────────────────────
    let commit_title = {
        let id = id.clone();
        move || {
            let new_title = edit_buffer.peek().trim().to_string();
            editing.set(false);
            if new_title.is_empty() {
                return;
            }
            mutate_todo(planner, &id, |t| t.title = new_title);
        }
    };

    let toggle_done = {
        let id = id.clone();
        move |_| {
            let now = Utc::now();
            let currently_done = planner
                .peek()
                .todo(&id)
                .map(|t| t.status.is_closed())
                .unwrap_or(false);
            // State-level transitions: completing (or reopening an
            // in-progress todo) must also close its focus-session row.
            {
                let mut state = planner.write();
                if currently_done {
                    state.reopen_todo(&id, now);
                } else {
                    state.complete_todo(&id, now);
                }
            }
            persist_todo(&planner, &id);
            persist_focus_sessions(&mut planner);
        }
    };

    let delete = {
        let id = id.clone();
        move |_| {
            // Honour confirm-on-delete without a modal: the first click only
            // arms the button, the second (while still on the row) deletes.
            if settings.peek().confirm_on_delete && !*delete_armed.peek() {
                delete_armed.set(true);
                return;
            }
            // store::delete_todo cascades to the todo's block rows;
            // remove_todo mirrors that in memory.
            if let Err(e) = store::delete_todo(&id) {
                tracing::error!("planner: failed to delete todo {}: {}", id, e);
            }
            planner.write().remove_todo(&id);
            // Deleting mid-focus closed its session row; keep the history.
            persist_focus_sessions(&mut planner);
            menu.set(None);
        }
    };

    let cycle = {
        let id = id.clone();
        move |_| {
            mutate_todo(planner, &id, |t| {
                t.estimate_minutes = cycle_estimate(t.estimate_minutes)
            });
            // The estimate IS the timebox's length: a re-estimate resizes the
            // block on the ruler from its anchored start.
            for b in planner.write().resize_blocks_to_estimate(&id) {
                persist_block(&b);
            }
        }
    };

    let start_focus = {
        let id = id.clone();
        move || {
            // Focus implies today: start_focus may reschedule the todo and
            // prune off-day blocks, whose rows we must delete.
            let (changed, pruned) = planner
                .write()
                .start_focus(&id, Utc::now(), FocusActor::Person);
            for cid in &changed {
                persist_todo(&planner, cid);
            }
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("planner: failed to delete stale block {}: {}", b.id, e);
                }
            }
            persist_focus_sessions(&mut planner);
        }
    };

    let pick_schedule = {
        let id = id.clone();
        move |choice: ScheduleChoice| {
            set_schedule(planner, &id, choice, today());
            menu.set(None);
        }
    };

    // The faint ring on whichever chip the list is sorted by.
    let ring = |key: SortKey| if sort_key == key { KEY_RING } else { "" };
    // The Time sort reads the timebox; an unboxed row's date pill stands in.
    let time_ring_on_date = block_info.is_none();

    let priority_display = settings.read().planner_priority.clone();

    let set_priority = {
        let id = id.clone();
        move |p: Option<Priority>| {
            mutate_todo(planner, &id, |t| t.priority = p);
            menu.set(None);
        }
    };

    let menu_now = *menu.read();
    // An open popover must survive the pointer leaving the hover zone.
    let cluster_class = if menu_now.is_some() {
        "flex items-center gap-1.5"
    } else {
        "hidden group-hover:flex items-center gap-1.5"
    };

    rsx! {
        div {
            class: "{row_class} focus:outline-none focus-visible:bg-section",
            // FLIP and the drag find rows by this.
            "data-row-id": "{todo.id}",
            tabindex: "0",
            // A wandering pointer disarms the delete confirmation.
            onmouseleave: move |_| {
                if *delete_armed.peek() {
                    delete_armed.set(false);
                }
            },
            // Arms a drag; TodoList makes it live after 4px of travel. Not
            // while editing the title or with a popover open (their inputs
            // need the mouse).
            onmousedown: {
                let id = id.clone();
                move |evt: MouseEvent| {
                    if !draggable || *editing.peek() || menu.peek().is_some() {
                        return;
                    }
                    if evt.trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    if let Some(mut ctx) = drag_ctx {
                        ctx.drag.set(Some(ListDrag {
                            todo_id: id.clone(),
                            grab_y: evt.client_coordinates().y,
                            ..Default::default()
                        }));
                    }
                }
            },
            // ⌥↑ / ⌥↓ nudge a focused row (Manual only — the list enforces it).
            // 1–4 set the priority, 0 clears it, p cycles it. Keys typed
            // into the title editor or a popover's input bubble up here too,
            // hence the editing / open-menu guard.
            onkeydown: {
                let id = id.clone();
                let mut set_priority = set_priority.clone();
                move |evt: KeyboardEvent| {
                    if *editing.peek() || menu.peek().is_some() {
                        return;
                    }
                    let mods = evt.modifiers();
                    let bare = !(mods.alt() || mods.ctrl() || mods.meta() || mods.shift());
                    if bare && !is_done && !in_logbook {
                        if let Key::Character(c) = evt.key() {
                            let current = planner.peek().todo(&id).and_then(|t| t.priority);
                            let next = match c.as_str() {
                                "p" | "P" => Some(Priority::cycle(current)),
                                "0" => Some(None),
                                d => Priority::parse(d).map(Some),
                            };
                            if let Some(next) = next {
                                evt.prevent_default();
                                set_priority(next);
                                return;
                            }
                        }
                    }
                    if !draggable || !mods.alt() {
                        return;
                    }
                    let r = match evt.key() {
                        Key::ArrowUp => Reorder::Up,
                        Key::ArrowDown => Reorder::Down,
                        _ => return,
                    };
                    evt.prevent_default();
                    if let Some(ctx) = drag_ctx {
                        ctx.reorder.call((id.clone(), r));
                    }
                }
            },

            if draggable {
                span {
                    class: "absolute -left-3 top-1/2 -translate-y-1/2 select-none text-[11px] leading-none text-fg-muted opacity-0 group-hover:opacity-100 cursor-grab transition-opacity",
                    title: "Drag to reorder",
                    "⠿"
                }
            }

            // ── Column 1 — WHO ────────────────────────────────────────────
            div {
                class: "w-5 flex items-center justify-center",
                match &who {
                    WhoState::Hobbes { initial, color } => rsx! {
                        button {
                            class: "w-4 h-4 rounded-full {color} flex items-center justify-center text-[9px] font-bold text-fg",
                            title: "Hobbes is on this — click to complete",
                            onclick: toggle_done,
                            "{initial}"
                        }
                    },
                    WhoState::FleetAgent => rsx! {
                        button {
                            class: "text-accent-400 animate-pulse hover:text-fg transition-colors",
                            title: "A coding agent is on this — click to complete",
                            onclick: toggle_done,
                            Icon {
                                width: 16,
                                height: 16,
                                icon: crate::components::pixel_icons::HobbesTiger,
                            }
                        }
                    },
                    WhoState::NeedsYou => rsx! {
                        button {
                            class: "text-yellow-400 hover:text-fg transition-colors",
                            title: "Its session needs you",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiAlertCircle }
                        }
                    },
                    WhoState::YouFocusing => rsx! {
                        button {
                            class: "text-primary-300 animate-pulse hover:text-fg transition-colors",
                            title: "In focus — click to complete",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiPlayCircle }
                        }
                    },
                    WhoState::Done => rsx! {
                        button {
                            class: "text-[#86efac] hover:text-fg transition-colors",
                            title: "Reopen",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiCheckCircle }
                        }
                    },
                    WhoState::Open => rsx! {
                        button {
                            class: "text-fg-muted hover:text-fg transition-colors",
                            title: "Complete",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiCircle }
                        }
                    },
                }
            }

            // ── Column 2 — WHAT ───────────────────────────────────────────
            div {
                class: "min-w-0 flex items-center gap-2",
                if *editing.read() {
                    input {
                        class: "flex-1 min-w-0 rounded border border-subtle bg-input px-2 py-0.5 text-sm text-fg focus:outline-none",
                        r#type: "text",
                        autofocus: true,
                        value: "{edit_buffer}",
                        oninput: move |evt| edit_buffer.set(evt.value()),
                        onkeydown: {
                            let mut commit = commit_title.clone();
                            move |evt: KeyboardEvent| {
                                if evt.key() == Key::Enter {
                                    evt.prevent_default();
                                    commit();
                                } else if evt.key() == Key::Escape {
                                    editing.set(false);
                                }
                            }
                        },
                        onblur: {
                            let mut commit = commit_title.clone();
                            move |_| commit()
                        },
                    }
                } else {
                    span {
                        class: if is_done {
                            "min-w-0 truncate text-sm text-fg-muted line-through cursor-text"
                        } else {
                            "min-w-0 truncate text-sm text-fg cursor-text"
                        },
                        onclick: {
                            let title = todo.title.clone();
                            move |_| {
                                edit_buffer.set(title.clone());
                                editing.set(true);
                            }
                        },
                        "{todo.title}"
                    }
                }

                // Project pill — the door to this todo's tags and its project.
                // A todo with neither wears it only on hover, so untagged rows
                // stay quiet but never lose the affordance.
                div {
                    class: "relative shrink-0",
                    button {
                        class: if project_title.is_some() || !todo.tags.is_empty() {
                            "{PILL} hover:text-fg hover:border-subtle max-w-40"
                        } else {
                            "{PILL} max-w-40 opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                        },
                        title: "Project and tags",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            let next = (menu_now != Some(RowMenu::Project)).then_some(RowMenu::Project);
                            menu.set(next);
                        },
                        span {
                            class: "truncate",
                            if let Some(p) = project_title.clone() { "{p}" } else { "Project…" }
                        }
                    }
                    if menu_now == Some(RowMenu::Project) {
                        div {
                            class: "fixed inset-0 z-40",
                            onclick: move |_| menu.set(None),
                        }
                        div {
                            class: "{POPOVER} left-0 right-auto",
                            if !todo.tags.is_empty() {
                                div {
                                    class: "flex flex-wrap gap-1.5 px-2.5 pt-1.5 pb-2",
                                    for tag in todo.tags.iter() {
                                        span { key: "{tag}", class: "{PILL}", "#{tag}" }
                                    }
                                }
                                div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                            }
                            ProjectPicker {
                                current: todo.project_id.clone(),
                                on_pick: {
                                    let id = id.clone();
                                    move |pid: Option<String>| {
                                        mutate_todo(planner, &id, |t| t.set_project(pid));
                                        menu.set(None);
                                    }
                                },
                            }
                        }
                    }
                }

                // Carrier session pill — the assignment's door.
                if let Some(label) = carrier_label.clone() {
                    button {
                        class: "{carrier_pill_class} max-w-44 overflow-hidden hover:brightness-110",
                        title: if is_chat_carrier { "Carried by this chat — click to open" } else { "Carried by this agent session — click to open the Fleet" },
                        onclick: {
                            let carrier_session_id = carrier_session_id.clone();
                            move |evt: MouseEvent| {
                                evt.stop_propagation();
                                let cmd = if is_chat_carrier {
                                    carrier_session_id.clone().map(
                                        crate::components::chat_input::ChatCommand::SwitchToSession,
                                    )
                                } else {
                                    Some(crate::components::chat_input::ChatCommand::ToggleFleet)
                                };
                                chat_command.set(cmd);
                            }
                        },
                        if !is_chat_carrier {
                            // The tiny-chip stencil: a fleet carrier reads as
                            // an agent at a glance, without a second pill.
                            Icon {
                                width: 10,
                                height: 10,
                                icon: crate::components::pixel_icons::HobbesTigerStencil,
                            }
                        }
                        span { class: "truncate", "{label}" }
                        if let Some(m) = carrier_minutes {
                            // shrink-0 + whitespace-nowrap: without an
                            // overflow value of its own this span's
                            // automatic min-width is its full text, so once
                            // the pill's max-w-44 got tight this text wrapped
                            // onto a second line instead of shrinking — and
                            // since the pill has a fixed h-[22px], that
                            // second line spilled out below it instead of
                            // being clipped. Keeping it on one line (and
                            // never shrinking it) means the label span above
                            // — which already handles this correctly via
                            // `truncate` — is the only thing that gives.
                            span { class: "shrink-0 whitespace-nowrap font-mono tabular-nums opacity-80", "· working {model::format_minutes(m)}" }
                        }
                    }
                }
            }

            // ── Column 3 — WHEN (+ the hover cluster) ─────────────────────
            div {
                class: "flex items-center justify-end gap-1.5 shrink-0",

                // Hover cluster.
                div {
                    class: "{cluster_class}",
                    if !in_logbook && !is_done {
                        button {
                            class: "{BTN_PRIMARY}",
                            title: if in_focus { "Stop focus — elapsed time is kept" } else { "Start focus (pauses any other)" },
                            onclick: {
                                let id = id.clone();
                                let start_focus = start_focus.clone();
                                move |_| {
                                    if in_focus {
                                        if planner.write().stop_focus(Utc::now()).is_some() {
                                            persist_todo(&planner, &id);
                                            persist_focus_sessions(&mut planner);
                                        }
                                    } else {
                                        { let mut f = start_focus.clone(); f(); }
                                    }
                                }
                            },
                            if in_focus { "Stop" } else { "Start" }
                        }
                        // Hand to Hobbes — a chat tab, or a background agent.
                        div {
                            class: "relative",
                            button {
                                class: "{BTN_SECONDARY}",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    let next = (menu_now != Some(RowMenu::Hand)).then_some(RowMenu::Hand);
                                    menu.set(next);
                                },
                                "Hand to Hobbes ▾"
                            }
                            if menu_now == Some(RowMenu::Hand) {
                                div {
                                    class: "fixed inset-0 z-40",
                                    onclick: move |_| menu.set(None),
                                }
                                div {
                                    class: "{POPOVER} w-72",
                                    button {
                                        class: "{MENU_ITEM} justify-between",
                                        onclick: {
                                            let todo = todo.clone();
                                            let id = id.clone();
                                            let start_focus = start_focus.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                menu.set(None);
                                                { let mut f = start_focus.clone(); f(); }
                                                let _ = &id;
                                                chat_command.set(Some(
                                                    crate::components::chat_input::ChatCommand::StartTodoInChat(
                                                        activation_seed(&todo),
                                                    ),
                                                ));
                                            }
                                        },
                                        span { "Work on it in a new chat" }
                                        if let Some(name) = profile_name.clone() {
                                            span { class: "{META}", "{name}" }
                                        }
                                    }
                                    button {
                                        class: "{MENU_ITEM}",
                                        onclick: {
                                            let id = id.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                menu.set(None);
                                                // Routed through ChatCommand (handled in
                                                // main.rs) rather than calling
                                                // dispatch_todo_to_background directly: a
                                                // row dispatch now gets a review chat tab
                                                // first, which main.rs owns creating.
                                                chat_command.set(Some(
                                                    crate::components::chat_input::ChatCommand::DispatchTodoToAgent(
                                                        id.clone(),
                                                    ),
                                                ));
                                            }
                                        },
                                        span { "Dispatch to a background agent" }
                                    }
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    p {
                                        class: "px-2.5 py-1 text-[11px] leading-snug text-fg-muted",
                                        "Both open with the project, estimate and notes in the message. You can edit before sending."
                                    }
                                }
                            }
                        }
                    }
                    // ⋯ — details, project, delete.
                    div {
                        class: "relative",
                        button {
                            class: "rounded px-1.5 py-0.5 text-[11px] text-fg-muted hover:bg-input hover:text-fg",
                            title: "More",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                let next = (menu_now != Some(RowMenu::More)).then_some(RowMenu::More);
                                menu.set(next);
                            },
                            "⋯"
                        }
                        if menu_now == Some(RowMenu::More) {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |_| menu.set(None),
                            }
                            div {
                                class: "{POPOVER}",
                                button {
                                    class: "{MENU_ITEM}",
                                    onclick: {
                                        let id = id.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            menu.set(None);
                                            detail_open.set(Some(id.clone()));
                                        }
                                    },
                                    Icon { width: 12, height: 12, icon: fi_icons::FiInfo }
                                    span { "Details" }
                                }
                                if draggable {
                                    for (r, label) in [(Reorder::Top, "Move to top"), (Reorder::Bottom, "Move to bottom")] {
                                        button {
                                            key: "{label}",
                                            class: "{MENU_ITEM}",
                                            onclick: {
                                                let id = id.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    menu.set(None);
                                                    if let Some(ctx) = drag_ctx {
                                                        ctx.reorder.call((id.clone(), r));
                                                    }
                                                }
                                            },
                                            span { class: "w-3 shrink-0 text-center", if r == Reorder::Top { "↑" } else { "↓" } }
                                            span { "{label}" }
                                        }
                                    }
                                }
                                if !in_logbook && !is_done {
                                    div { class: "{MENU_RULE}" }
                                    div { class: "{MENU_LABEL}", "Priority" }
                                    div {
                                        class: "flex items-center gap-1 px-2.5 pb-1.5",
                                        for p in Priority::ALL {
                                            button {
                                                key: "{p.as_str()}",
                                                class: if todo.priority == Some(p) {
                                                    "{PILL} px-1.5 font-mono {priority_tone_class(p)} ring-1 ring-primary-300/50"
                                                } else {
                                                    "{PILL} px-1.5 font-mono {priority_tone_class(p)} hover:brightness-125"
                                                },
                                                title: "{priority_display.describe(p)}",
                                                onclick: {
                                                    let mut set = set_priority.clone();
                                                    move |evt: MouseEvent| {
                                                        evt.stop_propagation();
                                                        set(Some(p));
                                                    }
                                                },
                                                "{priority_display.badge(p)}"
                                            }
                                        }
                                        if todo.priority.is_some() {
                                            button {
                                                class: "ml-auto text-[11px] text-fg-muted hover:text-fg",
                                                title: "Clear priority",
                                                onclick: {
                                                    let mut set = set_priority.clone();
                                                    move |evt: MouseEvent| {
                                                        evt.stop_propagation();
                                                        set(None);
                                                    }
                                                },
                                                "Clear"
                                            }
                                        }
                                    }
                                }
                                if !in_logbook {
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    ProjectPicker {
                                        current: todo.project_id.clone(),
                                        on_pick: {
                                            let id = id.clone();
                                            move |pid: Option<String>| {
                                                mutate_todo(planner, &id, |t| t.set_project(pid));
                                                menu.set(None);
                                            }
                                        },
                                    }
                                }
                                div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                button {
                                    class: if *delete_armed.read() {
                                        "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-left text-red-400 font-medium bg-red-900/30 hover:bg-input"
                                    } else {
                                        "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-left text-red-400 hover:bg-input"
                                    },
                                    onclick: delete,
                                    Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                    if *delete_armed.read() { "Sure? Click again" } else { "Delete" }
                                }
                            }
                        }
                    }
                    div { class: "w-px h-4 bg-[var(--color-border-muted)] mx-0.5" }
                }

                // WHEN meta.
                if is_done {
                    if let Some(actuals) = actuals_line {
                        span { class: "{META}", "{actuals}" }
                    }
                    if let Some(date_line) = completed_line {
                        span { class: "{META}", "{date_line}" }
                    }
                } else {
                    if who == WhoState::NeedsYou {
                        button {
                            class: "{BTN_PRIMARY}",
                            title: "Open the Fleet to answer it",
                            onclick: move |_| {
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::ToggleFleet,
                                ));
                            },
                            "Open"
                        }
                    }
                    // Priority badge. Mirrors the estimate chip: an untriaged
                    // row wears a ghost only on hover, and a click cycles
                    // Q1 → Q2 → Q3 → Q4 → none. ⌥-click (or right-click)
                    // opens the named list; a focused row takes 1–4 / 0 / p.
                    div {
                        class: "relative shrink-0",
                        button {
                            class: match todo.priority {
                                Some(p) => format!("{PILL} px-1.5 font-mono {} hover:brightness-125{}", priority_tone_class(p), ring(SortKey::Priority)),
                                None => format!("{PILL} px-1.5 font-mono opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 hover:text-fg transition-opacity"),
                            },
                            title: match todo.priority {
                                Some(p) => format!("{}. Click to cycle, ⌥-click to choose · keys 1–4, 0 clears", priority_display.describe(p)),
                                None => "Priority — click to cycle, ⌥-click to choose · keys 1–4".to_string(),
                            },
                            onclick: {
                                let mut set = set_priority.clone();
                                let current = todo.priority;
                                move |evt: MouseEvent| {
                                    evt.stop_propagation();
                                    if evt.modifiers().alt() {
                                        let next = (menu_now != Some(RowMenu::Priority)).then_some(RowMenu::Priority);
                                        menu.set(next);
                                    } else {
                                        set(Priority::cycle(current));
                                    }
                                }
                            },
                            oncontextmenu: move |evt| {
                                evt.prevent_default();
                                evt.stop_propagation();
                                let next = (menu_now != Some(RowMenu::Priority)).then_some(RowMenu::Priority);
                                menu.set(next);
                            },
                            match todo.priority {
                                Some(p) => priority_display.badge(p),
                                None => priority_display.badge(Priority::UrgentImportant).replace('1', "–"),
                            }
                        }
                        if menu_now == Some(RowMenu::Priority) {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |_| menu.set(None),
                            }
                            div {
                                class: "{POPOVER}",
                                div { class: "{MENU_LABEL}", "Priority" }
                                for q in Priority::ALL {
                                    button {
                                        key: "{q.as_str()}",
                                        class: "{MENU_ITEM}",
                                        onclick: {
                                            let mut set = set_priority.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                set(Some(q));
                                            }
                                        },
                                        span { class: "w-3 shrink-0", if Some(q) == todo.priority { "✓" } }
                                        span { class: "{PILL} px-1.5 font-mono {priority_tone_class(q)}", "{priority_display.badge(q)}" }
                                        span { class: "flex-1", "{priority_display.name(q)}" }
                                        span { class: "{META}", "{q.quadrant()}" }
                                    }
                                }
                                div { class: "{MENU_RULE}" }
                                button {
                                    class: "{MENU_ITEM}",
                                    onclick: {
                                        let mut set = set_priority.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            set(None);
                                        }
                                    },
                                    span { class: "w-3 shrink-0" }
                                    span { class: "flex-1 text-fg-muted", "No priority" }
                                    span { class: "{META}", "0" }
                                }
                            }
                        }
                    }
                    button {
                        class: if todo.estimate_minutes.is_some() {
                            "{META} hover:text-fg{ring(SortKey::Estimate)}"
                        } else {
                            "{META} opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                        },
                        title: "Estimate — click to cycle 15m / 30m / 1h / 2h / none",
                        onclick: cycle,
                        if let Some(m) = todo.estimate_minutes {
                            "{model::format_minutes(m)}"
                        } else {
                            "est"
                        }
                    }
                    if let Some((t, block_id)) = block_info {
                        // A live affordance, not a dead label: the chip jumps
                        // to Today with its block selected, so "@ 09:00" is
                        // always one click from the timebox it names.
                        button {
                            class: "{META} hover:text-fg{ring(SortKey::Time)}",
                            title: "On the timeline — click to show it in Today",
                            onclick: move |_| {
                                selected_block.set(Some(block_id.clone()));
                                selection.set(PlannerSelection::View(TodoView::Today));
                            },
                            "@ {t}"
                        }
                    }
                    if let Some(deadline) = todo.deadline {
                        span {
                            class: "{PILL} {due_tone_class(due_tone(deadline, day))}{ring(SortKey::Deadline)}",
                            "due {views::friendly_date(deadline, day)}"
                        }
                    }
                    // The date pill: always present once a day is set, in every
                    // view (it used to hide outside Projects, which made the
                    // schedule invisible exactly where it mattered).
                    {
                        let scheduled = date_label.is_some();
                        let label = date_label.clone().unwrap_or_else(|| "When".to_string());
                        let date_ring = if time_ring_on_date && scheduled { ring(SortKey::Time) } else { "" };
                        rsx! {
                        div {
                            class: "relative shrink-0",
                            button {
                                class: if scheduled {
                                    "{PILL} hover:text-fg hover:border-subtle{date_ring}"
                                } else {
                                    "{PILL} opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                                },
                                title: "When — click to reschedule",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    let next = (menu_now != Some(RowMenu::Date)).then_some(RowMenu::Date);
                                    menu.set(next);
                                },
                                "{label} ▾"
                            }
                            if menu_now == Some(RowMenu::Date) {
                                div {
                                    class: "fixed inset-0 z-40",
                                    onclick: move |_| menu.set(None),
                                }
                                div {
                                    class: "{POPOVER}",
                                    for (choice, label) in [
                                        (ScheduleChoice::Today, "Today"),
                                        (ScheduleChoice::ThisEvening, "This evening"),
                                        (ScheduleChoice::Tomorrow, "Tomorrow"),
                                    ] {
                                        button {
                                            key: "{label}",
                                            class: "{MENU_ITEM}",
                                            onclick: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    pick(choice);
                                                }
                                            },
                                            span { class: "w-3 shrink-0", if current_choice == choice { "✓" } }
                                            span { "{label}" }
                                        }
                                    }
                                    div {
                                        class: "flex items-center gap-2 px-2.5 py-1.5",
                                        span { class: "w-3 shrink-0" }
                                        span { class: "text-[11px] text-fg-muted shrink-0", "Pick a day…" }
                                        input {
                                            class: "flex-1 min-w-0 rounded border border-subtle bg-input px-1.5 py-0.5 text-[11px] text-fg focus:outline-none",
                                            r#type: "date",
                                            value: "{pick_day}",
                                            onclick: move |evt| evt.stop_propagation(),
                                            oninput: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: FormEvent| {
                                                    let raw = evt.value();
                                                    pick_day.set(raw.clone());
                                                    if let Ok(d) = raw.parse::<NaiveDate>() {
                                                        pick(ScheduleChoice::On(d));
                                                    }
                                                }
                                            },
                                        }
                                    }
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    for (choice, label) in [
                                        (ScheduleChoice::Anytime, "Anytime"),
                                        (ScheduleChoice::Someday, "Someday"),
                                    ] {
                                        button {
                                            key: "{label}",
                                            class: "{MENU_ITEM}",
                                            onclick: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    pick(choice);
                                                }
                                            },
                                            span { class: "w-3 shrink-0", if current_choice == choice { "✓" } }
                                            span { "{label}" }
                                        }
                                    }
                                }
                            }
                        }
                        } // rsx!
                    }
                }
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::AttentionKind;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn fleet_session(origin: FleetOrigin, status: FleetStatus) -> FleetSession {
        let mut s = FleetSession {
            id: "fs_1".into(),
            cwd: "/tmp/repo".into(),
            name: "repo".into(),
            status,
            last_event_at: Utc::now(),
            working_since: None,
            day_minutes: Default::default(),
            day_seconds: Default::default(),
            auto_passthrough: false,
            ended_at: None,
            pending_gate: None,
            transcript_path: None,
            brief: None,
            brief_dirty_at: None,
            origin,
            session_title: None,
            active_prompt: None,
            stopped_prompts: Vec::new(),
            attention_at: None,
            dispatched_todo: None,
            dispatched_from: None,
            continuation_sent: false,
        };
        s.working_since = matches!(
            s.status,
            FleetStatus::Working | FleetStatus::WorkingBackground
        )
        .then(Utc::now);
        s
    }

    fn focus(todo_id: &str, actor: FocusActor) -> FocusSession {
        FocusSession::open(todo_id, Utc::now(), actor)
    }

    // ── who_state ──────────────────────────────────────────────────────────

    #[test]
    fn who_state_open_when_nobody_is_on_it() {
        let t = Todo::new("Write the brief", 0.0);
        assert_eq!(who_state(&t, None, None, None), WhoState::Open);
    }

    #[test]
    fn who_state_you_focusing_needs_an_open_person_session() {
        let mut t = Todo::new("Write the brief", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(&t.id, FocusActor::Person);
        assert_eq!(who_state(&t, Some(&f), None, None), WhoState::YouFocusing);

        // A closed session is history, not a live state.
        let mut closed = f.clone();
        closed.ended_at = Some(Utc::now());
        assert_eq!(who_state(&t, Some(&closed), None, None), WhoState::Open);
    }

    #[test]
    fn who_state_hobbes_from_an_agent_focus_session_or_a_hobbes_carrier() {
        let mut t = Todo::new("Draft the memo", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(
            &t.id,
            FocusActor::Agent {
                session_id: Some("sess_1".into()),
            },
        );
        let badge = Some(("W".to_string(), "bg-purple-600".to_string()));
        assert_eq!(
            who_state(&t, Some(&f), None, badge.clone()),
            WhoState::Hobbes {
                initial: "W".into(),
                color: "bg-purple-600".into()
            }
        );

        // ...and without a focus session, a working Hobbes-origin carrier.
        let mut open = Todo::new("Draft the memo", 0.0);
        open.linked_fleet_session = Some("fs_1".into());
        let fs = fleet_session(FleetOrigin::Hobbes, FleetStatus::WorkingBackground);
        assert_eq!(
            who_state(&open, None, Some(&fs), badge),
            WhoState::Hobbes {
                initial: "W".into(),
                color: "bg-purple-600".into()
            }
        );
    }

    #[test]
    fn who_state_hobbes_falls_back_to_the_default_swatch() {
        let mut t = Todo::new("Draft the memo", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(&t.id, FocusActor::Agent { session_id: None });
        assert_eq!(
            who_state(&t, Some(&f), None, None),
            WhoState::Hobbes {
                initial: "H".into(),
                color: DEFAULT_PROFILE_COLOR.to_string()
            }
        );
    }

    #[test]
    fn who_state_fleet_agent_for_a_working_external_carrier() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.linked_fleet_session = Some("fs_1".into());
        let fs = fleet_session(FleetOrigin::External, FleetStatus::Working);
        assert_eq!(who_state(&t, None, Some(&fs), None), WhoState::FleetAgent);

        // An idle carrier is not "on it".
        let idle = fleet_session(FleetOrigin::External, FleetStatus::Idle);
        assert_eq!(who_state(&t, None, Some(&idle), None), WhoState::Open);
    }

    #[test]
    fn who_state_needs_you_beats_working() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.status = TodoStatus::InProgress;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Agent { session_id: None });
        let fs = fleet_session(
            FleetOrigin::External,
            FleetStatus::NeedsAttention(AttentionKind::Gate),
        );
        assert_eq!(who_state(&t, Some(&f), Some(&fs), None), WhoState::NeedsYou);
    }

    #[test]
    fn who_state_done_beats_everything() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.status = TodoStatus::Completed;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Person);
        let fs = fleet_session(
            FleetOrigin::External,
            FleetStatus::NeedsAttention(AttentionKind::Gate),
        );
        assert_eq!(who_state(&t, Some(&f), Some(&fs), None), WhoState::Done);

        // Cancelled is closed too.
        t.status = TodoStatus::Cancelled;
        assert_eq!(who_state(&t, None, None, None), WhoState::Done);
    }

    #[test]
    fn who_state_you_beat_a_stale_working_link() {
        // You pressed Start; a linked session still reporting Working must not
        // steal the row's identity from you.
        let mut t = Todo::new("Write the brief", 0.0);
        t.status = TodoStatus::InProgress;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Person);
        let fs = fleet_session(FleetOrigin::External, FleetStatus::Working);
        assert_eq!(
            who_state(&t, Some(&f), Some(&fs), None),
            WhoState::YouFocusing
        );
    }

    // ── due_tone ───────────────────────────────────────────────────────────

    #[test]
    fn due_tone_reads_the_next_two_days_as_soon() {
        let today = d("2026-09-16");
        assert_eq!(due_tone(d("2026-09-15"), today), DueTone::Overdue);
        assert_eq!(due_tone(today, today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-17"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-18"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-19"), today), DueTone::Neutral);
    }

    // ── date_pill_label ────────────────────────────────────────────────────

    #[test]
    fn date_pill_label_names_the_day() {
        let today = d("2026-09-16"); // a Wednesday
        assert_eq!(date_pill_label(None, false, today), None);
        assert_eq!(
            date_pill_label(Some(today), false, today).as_deref(),
            Some("Today")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-17")), false, today).as_deref(),
            Some("Tomorrow")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), false, today).as_deref(),
            Some("Fri 18")
        );
        // Evening only rebrands today's pill; a future evening still says the day.
        assert_eq!(
            date_pill_label(Some(today), true, today).as_deref(),
            Some("Evening")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), true, today).as_deref(),
            Some("Fri 18")
        );
    }

    // ── the shared schedule helper ─────────────────────────────────────────

    #[test]
    fn apply_schedule_choice_pins_the_row_menu_behaviour() {
        let today = d("2026-09-16");
        let tomorrow = d("2026-09-17");

        let mut t = Todo::new("x", 0.0);
        t.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut t, ScheduleChoice::Today, today);
        assert_eq!(t.scheduled_for, Some(today));
        assert_eq!(t.time_of_day, None, "Today means the daytime list");

        apply_schedule_choice(&mut t, ScheduleChoice::Tomorrow, today);
        assert_eq!(t.scheduled_for, Some(tomorrow));
        assert_eq!(t.time_of_day, None);

        // Evening is a time-of-day, not a reschedule: a dated todo keeps its day.
        apply_schedule_choice(&mut t, ScheduleChoice::ThisEvening, today);
        assert_eq!(t.scheduled_for, Some(tomorrow), "the day is not yanked");
        assert_eq!(t.time_of_day, Some(TimeOfDay::Evening));

        // ...but an undated one lands on today.
        let mut u = Todo::new("y", 0.0);
        apply_schedule_choice(&mut u, ScheduleChoice::ThisEvening, today);
        assert_eq!(u.scheduled_for, Some(today));
        assert_eq!(u.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn apply_schedule_choice_buckets_and_clears() {
        let today = d("2026-09-16");

        // Someday clears the date and the grouping — and, with no date left,
        // `set_schedule` prunes every block (keep = None).
        let mut t = Todo::new("x", 0.0);
        t.scheduled_for = Some(today);
        t.time_of_day = Some(TimeOfDay::Evening);
        t.bucket = TodoBucket::Anytime;
        apply_schedule_choice(&mut t, ScheduleChoice::Someday, today);
        assert_eq!(t.scheduled_for, None);
        assert_eq!(t.time_of_day, None);
        assert_eq!(t.bucket, TodoBucket::Someday);

        // Anytime is the inverse: undated, but triaged rather than deferred.
        apply_schedule_choice(&mut t, ScheduleChoice::Anytime, today);
        assert_eq!(t.bucket, TodoBucket::Anytime);

        // ClearDate (the detail card's "Clear") leaves the bucket alone.
        let mut s = Todo::new("y", 0.0);
        s.bucket = TodoBucket::Someday;
        s.scheduled_for = Some(today);
        s.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut s, ScheduleChoice::ClearDate, today);
        assert_eq!(s.scheduled_for, None);
        assert_eq!(s.time_of_day, None);
        assert_eq!(s.bucket, TodoBucket::Someday);

        // A picked day sets the date without touching the grouping.
        let mut p = Todo::new("z", 0.0);
        p.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut p, ScheduleChoice::On(d("2026-10-01")), today);
        assert_eq!(p.scheduled_for, Some(d("2026-10-01")));
        assert_eq!(p.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn current_schedule_choice_ticks_the_todays_state() {
        let today = d("2026-09-16");
        let mut t = Todo::new("x", 0.0);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Anytime);

        t.bucket = TodoBucket::Someday;
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Someday);

        t.bucket = TodoBucket::Anytime;
        t.scheduled_for = Some(today);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Today);

        t.time_of_day = Some(TimeOfDay::Evening);
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::ThisEvening
        );

        t.time_of_day = None;
        t.scheduled_for = Some(d("2026-09-17"));
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Tomorrow);

        t.scheduled_for = Some(d("2026-10-01"));
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::On(d("2026-10-01"))
        );
    }

    // ── swatch derivation ──────────────────────────────────────────────────

    #[test]
    fn profile_classes_derive_from_the_swatch() {
        assert_eq!(profile_stripe_class("bg-purple-600"), "border-l-purple-600");
        assert_eq!(profile_pill_border_class("bg-teal-600"), "border-teal-600");
        // Anything that isn't a bg utility falls back rather than emitting a
        // class Tailwind never generated.
        assert_eq!(profile_stripe_class("nonsense"), "border-l-blue-600");
        assert_eq!(profile_pill_border_class("nonsense"), "border-faint");
    }

    // ── list order helpers ─────────────────────────────────────────────────

    fn ids(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_targets_stay_inside_the_group() {
        assert_eq!(reorder_target(2, 4, Reorder::Top), Some(0));
        assert_eq!(reorder_target(2, 4, Reorder::Bottom), Some(3));
        assert_eq!(reorder_target(2, 4, Reorder::Up), Some(1));
        assert_eq!(reorder_target(2, 4, Reorder::Down), Some(3));
        // Already there → no-op, never a wasted write.
        assert_eq!(reorder_target(0, 4, Reorder::Top), None);
        assert_eq!(reorder_target(0, 4, Reorder::Up), None);
        assert_eq!(reorder_target(3, 4, Reorder::Down), None);
        assert_eq!(reorder_target(3, 4, Reorder::Bottom), None);
        assert_eq!(reorder_target(5, 4, Reorder::Top), None);
    }

    #[test]
    fn moved_lifts_and_reinserts() {
        let list = ids(&["a", "b", "c", "d"]);
        assert_eq!(moved(&list, 0, 2), ids(&["b", "c", "a", "d"]));
        assert_eq!(moved(&list, 3, 0), ids(&["d", "a", "b", "c"]));
        assert_eq!(moved(&list, 1, 1), list);
    }

    #[test]
    fn drag_over_index_counts_whole_rows_and_clamps() {
        assert_eq!(drag_over_index(1, 4, 0.0), 1);
        assert_eq!(drag_over_index(1, 4, 17.0), 1, "under half a row stays put");
        assert_eq!(drag_over_index(1, 4, 19.0), 2);
        assert_eq!(drag_over_index(1, 4, 36.0 * 10.0), 3);
        assert_eq!(drag_over_index(1, 4, -36.0 * 10.0), 0);
        assert_eq!(drag_over_index(0, 0, 50.0), 0);
    }

    #[test]
    fn flatten_with_replaces_only_the_dragged_group() {
        let groups = vec![
            ("band-overdue".to_string(), ids(&["a", "b"])),
            ("band-later".to_string(), ids(&["c", "d"])),
        ];
        assert_eq!(
            flatten_with(&groups, "band-later", &ids(&["d", "c"])),
            ids(&["a", "b", "d", "c"])
        );
    }

    #[test]
    fn order_hint_is_silent_in_the_default_state() {
        assert_eq!(order_hint(ListOrder::default()), None);
        let sorted = ListOrder { key: SortKey::Deadline, dir: SortDir::Asc, group: GroupBy::None };
        assert_eq!(
            order_hint(sorted).as_deref(),
            Some("Due date · Soonest first — drag a row to make this your order")
        );
        let grouped = ListOrder { group: GroupBy::Project, ..ListOrder::default() };
        assert_eq!(
            order_hint(grouped).as_deref(),
            Some("Grouped by project — drag within a group to reorder")
        );
    }

    #[test]
    fn priority_tones_reuse_existing_row_colours() {
        assert_eq!(priority_tone_class(Priority::UrgentImportant), due_tone_class(DueTone::Overdue));
        assert_eq!(priority_tone_class(Priority::Urgent), due_tone_class(DueTone::Soon));
        assert_eq!(priority_tone_class(Priority::Neither), due_tone_class(DueTone::Neutral));
    }

    // ── pre-existing helpers ───────────────────────────────────────────────

    #[test]
    fn activation_seed_carries_the_task_and_invites_details() {
        let mut t = Todo::new("Finalize Case Study", 0.0);
        t.id = "td_1a2b".into();
        t.estimate_minutes = Some(60);
        t.deadline = Some("2026-08-13".parse().unwrap());
        t.tags = vec!["writing".into()];

        let seed = activation_seed(&t);
        assert_eq!(
            seed,
            "Let's work on my to-do \"Finalize Case Study\" (1h, due 13 Aug, #writing) — todo id td_1a2b.\n\nDetails: "
        );

        // Bare todos skip the empty parenthetical.
        let bare = Todo::new("Email", 0.0);
        let seed = activation_seed(&bare);
        assert!(seed.starts_with("Let's work on my to-do \"Email\" — todo id"));
        assert!(seed.ends_with("Details: "), "caret parks after the prompt");
    }

    #[test]
    fn cycle_estimate_walks_the_bucket_ladder() {
        assert_eq!(cycle_estimate(None), Some(15));
        assert_eq!(cycle_estimate(Some(15)), Some(30));
        assert_eq!(cycle_estimate(Some(30)), Some(60));
        assert_eq!(cycle_estimate(Some(60)), Some(120));
        // Only an exact 2h cycles to none.
        assert_eq!(cycle_estimate(Some(120)), None);
    }

    #[test]
    fn cycle_estimate_snaps_off_ladder_values_to_the_nearest_step() {
        // 45 is equidistant from 30 and 60 — ties round up.
        assert_eq!(cycle_estimate(Some(45)), Some(60));
        // 90 is equidistant from 60 and 120 — same rule.
        assert_eq!(cycle_estimate(Some(90)), Some(120));
        // Plain nearest when there's no tie.
        assert_eq!(cycle_estimate(Some(20)), Some(15));
        assert_eq!(cycle_estimate(Some(50)), Some(60));
        // Past the top of the ladder snaps back to 2h, never to none.
        assert_eq!(cycle_estimate(Some(300)), Some(120));
        // ...and the next click continues the ladder normally.
        assert_eq!(cycle_estimate(cycle_estimate(Some(45))), Some(120));
    }

    // ── Quick-add birth ─────────────────────────────────────────────────────

    fn planner_with_projects() -> PlannerState {
        let p = |id: &str, title: &str, sort_order: f64| model::Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::default(),
            deadline: None,
            sort_order,
            path: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut state = PlannerState::default();
        state.projects = vec![p("rc", "Hobbes RC", 1.0), p("reno", "Home renovation", 2.0)];
        state
    }

    fn birth(draft: &QuickAddDraft, sel: &PlannerSelection) -> Birth {
        let day = d("2026-09-24");
        let parsed = draft.parse(&planner_with_projects(), day);
        resolve_birth(&parsed, draft, sel, day)
    }

    fn text(t: &str) -> QuickAddDraft {
        QuickAddDraft { text: t.into(), ..Default::default() }
    }

    fn inherited(id: &str) -> Option<Inherited> {
        Some(Inherited { project_id: id.into(), why: InheritedFrom::View })
    }

    #[test]
    fn token_overrides_view_project() {
        let b = birth(&text("tiles +reno"), &PlannerSelection::Project("rc".into()));
        assert_eq!(b.project, BirthProject::Existing("reno".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);

        let mut with_inherited = text("tiles +reno");
        with_inherited.inherited = inherited("rc");
        let b = birth(&with_inherited, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("reno".into()));
    }

    #[test]
    fn inherited_used_when_no_token() {
        let mut draft = text("ship notes");
        draft.inherited = inherited("rc");
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime, "filed into a project = triaged");
    }

    #[test]
    fn cleared_inherited_means_no_project() {
        let b = birth(&text("ship notes"), &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::None);
        assert_eq!(b.bucket, TodoBucket::Inbox);
    }

    #[test]
    fn view_project_is_the_inline_default() {
        let b = birth(&text("ship notes"), &PlannerSelection::Project("rc".into()));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
    }

    #[test]
    fn project_in_inbox_view_is_born_triaged() {
        let b = birth(&text("ship +rc"), &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);
        assert_eq!(b.scheduled_for, None);
    }

    #[test]
    fn project_with_someday_view_stays_someday() {
        let b = birth(&text("ship +rc"), &PlannerSelection::View(TodoView::Someday));
        assert_eq!(b.bucket, TodoBucket::Someday);
        let today = birth(&text("ship +rc"), &PlannerSelection::View(TodoView::Today));
        assert_eq!(today.scheduled_for, Some(d("2026-09-24")));
    }

    #[test]
    fn create_project_only_when_token_unchanged() {
        let mut draft = text("sand it +garden-shed ");
        draft.create_project = Some("Garden shed".into());
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Create("Garden shed".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);

        // Token edited after the pick: the pending create is dropped.
        draft.text = "sand it +garden".into();
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::None);
        assert_eq!(b.bucket, TodoBucket::Inbox);
    }

    #[test]
    fn create_row_titles_and_list_highlights() {
        assert_eq!(title_from_query("garden-shed"), "Garden shed");
        assert_eq!(title_from_query("x"), "X");
        let runs = highlight_runs("Home renovation", "reno");
        assert_eq!(
            runs,
            vec![
                ("Home ".to_string(), false),
                ("reno".to_string(), true),
                ("vation".to_string(), false)
            ]
        );
    }
}
