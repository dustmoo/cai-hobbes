//! The Planner view — the user surface of the built-in planner (P2+P3).
//!
//! Rendered in place of the chat column (`main.rs`), never as a sidebar.
//! Three columns: a Things-style left rail, the todo list for the selected
//! view, and — on Today only — a Sunsama-style capacity bar and day timeline.
//!
//! Every mutation updates the `Signal<PlannerState>` AND persists through
//! `crate::todo::store` immediately; planner writes are single-row and cheap.
//!
//! Timeline geometry uses inline `style:` attributes exclusively — Tailwind
//! purges computed class names, so positioned pixels must never ride in class
//! strings.

use chrono::{Local, NaiveDate, TimeZone, Timelike, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::settings::Settings;
use crate::todo::model::{
    self, BlockSource, FocusActor, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoStatus,
};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

/// Vertical scale of the day timeline.
const PX_PER_HOUR: f64 = 48.0;

/// Current-time indicator color: warm orange, complementary to the cool
/// blue/green block palette and distinct from the subscription palette's red.
const NOW_LINE_COLOR: &str = "#fb923c";

/// Pixel offset of the current-time line inside the ruler, or `None` when
/// "now" falls outside the rendered workday window.
fn now_line_offset(now_min: i64, day_start: u32, day_end: u32) -> Option<f64> {
    if now_min < day_start as i64 || now_min > day_end as i64 {
        return None;
    }
    Some((now_min - day_start as i64) as f64 / 60.0 * PX_PER_HOUR)
}

/// What the centre column is showing: a built-in view, a single project, or
/// the fleet panel (Pro).
#[derive(Clone, Debug, PartialEq)]
enum PlannerSelection {
    View(TodoView),
    Project(String),
    Fleet,
}

/// Which timeline block is selected on the Today rail. Provided at the
/// PlannerView root (same pattern as TodoDetailContext) so a row's timebox
/// chip can jump to Today with its block already highlighted.
#[derive(Clone, Copy)]
struct SelectedBlockContext(Signal<Option<String>>);

/// What a timeline drag is doing to a block.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockDragMode {
    /// Reposition the whole block, duration preserved.
    Move,
    /// Drag the bottom edge to change the end time.
    ResizeEnd,
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

// ── Persistence helpers ─────────────────────────────────────────────────────
//
// Store writes are deliberately synchronous: single-row SQLite upserts on a
// human-driven cadence. A failed write is logged, not surfaced — the in-memory
// state stays authoritative for the session either way.

pub(crate) fn persist_todo(planner: &Signal<PlannerState>, id: &str) {
    if let Some(t) = planner.peek().todo(id) {
        if let Err(e) = store::save_todo(t) {
            tracing::error!("planner: failed to save todo {}: {}", id, e);
        }
    }
}

pub(crate) fn persist_block(block: &TimeBlock) {
    if let Err(e) = store::save_block(block) {
        tracing::error!("planner: failed to save block {}: {}", block.id, e);
    }
}

/// Write through the focus-session rows the last state operation touched
/// (start/stop/complete/… open and close them at the PlannerState layer).
pub(crate) fn persist_focus_sessions(planner: &mut Signal<PlannerState>) {
    for s in planner.write().take_dirty_focus_sessions() {
        if let Err(e) = store::save_focus_session(&s) {
            tracing::error!("planner: failed to save focus session {}: {}", s.id, e);
        }
    }
}

/// Patch one todo in state, stamp `updated_at`, and persist it.
pub(crate) fn mutate_todo(mut planner: Signal<PlannerState>, id: &str, f: impl FnOnce(&mut Todo)) {
    let now = Utc::now();
    {
        let mut state = planner.write();
        if let Some(t) = state.todo_mut(id) {
            f(t);
            t.updated_at = now;
        }
    }
    persist_todo(&planner, id);
}

// ── Pure helpers (unit-tested at the bottom) ────────────────────────────────

/// `"09:30"` → minutes since midnight. Rejects malformed and out-of-range input.
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Timeline bounds from settings, falling back to 09:00–17:00 whenever the
/// stored strings are malformed or inverted.
fn workday_bounds(start: &str, end: &str) -> (u32, u32) {
    match (parse_hhmm(start), parse_hhmm(end)) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => (9 * 60, 17 * 60),
    }
}

/// Pixel geometry for a block on the ruler: `(top, height)` clamped into the
/// workday window. `None` when the block lies entirely outside it.
fn block_geometry(
    start_min: i64,
    end_min: i64,
    day_start: u32,
    day_end: u32,
) -> Option<(f64, f64)> {
    let start = start_min.max(day_start as i64);
    let end = end_min.min(day_end as i64);
    if end <= start {
        return None;
    }
    let top = (start - day_start as i64) as f64 / 60.0 * PX_PER_HOUR;
    // The floor keeps a one-line title readable: a 15-minute block is only
    // 12px at 48px/h, which clips its own label.
    let height = ((end - start) as f64 / 60.0 * PX_PER_HOUR).max(20.0);
    Some((top, height))
}

/// Which side of the workday window a fully-off-window block sits on.
///
/// The exact complement of [`block_geometry`]'s culling rule: every block on
/// the day is either drawn on the ruler or classified here for the off-window
/// strip — the store must never hold a block the Today rail doesn't show.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OffWindow {
    /// Ends at or before the workday starts.
    Early,
    /// Starts at or after the workday ends.
    After,
}

fn off_window(start_min: i64, end_min: i64, day_start: u32, day_end: u32) -> Option<OffWindow> {
    if end_min <= day_start as i64 {
        Some(OffWindow::Early)
    } else if start_min >= day_end as i64 {
        Some(OffWindow::After)
    } else {
        None
    }
}

/// [`block_geometry`] plus honesty about the clamp: external (calendar) blocks
/// that spill past a workday edge render clamped with a dashed cue on the
/// clamped side instead of silently looking shorter than the meeting is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ClampedGeometry {
    top: f64,
    height: f64,
    /// The block starts before the workday window opens.
    clamped_top: bool,
    /// The block ends after the workday window closes.
    clamped_bottom: bool,
}

/// Geometry for an external block: identical pixels to [`block_geometry`],
/// with flags for which edge (if either) was clamped to the window. `None` for
/// zero-length spans and blocks entirely outside the window — those are
/// tallied into the off-window count strip, never drawn.
fn clamped_block_geometry(
    start_min: i64,
    end_min: i64,
    day_start: u32,
    day_end: u32,
) -> Option<ClampedGeometry> {
    if end_min <= start_min {
        return None; // zero-length guard, mirrors block_geometry
    }
    let (top, height) = block_geometry(start_min, end_min, day_start, day_end)?;
    Some(ClampedGeometry {
        top,
        height,
        clamped_top: start_min < day_start as i64,
        clamped_bottom: end_min > day_end as i64,
    })
}

/// Label for external events the ruler can't show at all: `"2 earlier · 1
/// later"`. `None` when there is nothing outside the window.
fn fmt_outside_counts(earlier: usize, later: usize) -> Option<String> {
    match (earlier, later) {
        (0, 0) => None,
        (e, 0) => Some(format!("{} earlier", e)),
        (0, l) => Some(format!("{} later", l)),
        (e, l) => Some(format!("{} earlier · {} later", e, l)),
    }
}

/// Resolve a subscription id to its display color. `None` when the block has
/// no subscription or the subscription no longer exists — callers fall back
/// to the muted external style.
fn subscription_color(
    subs: &[crate::settings::CalendarSubscription],
    subscription_id: Option<&str>,
) -> Option<String> {
    let id = subscription_id?;
    subs.iter().find(|s| s.id == id).map(|s| s.color.clone())
}

/// Inline style for an external block: geometry, subscription tint (left color
/// bar + translucent fill — `26` is ~15% alpha appended to the hex), and
/// dashed cues on window-clamped edges. Non-busy (free / focus-time) events
/// get a subtler treatment — fainter fill (`14` ≈ 8% alpha), a dashed left
/// bar, and reduced opacity — so they read as non-blocking. Tentative busy
/// meetings keep the full busy tint (they still block time) but swap the
/// solid left bar for a dashed one — pencilled in, not inked. Inline because
/// Tailwind purges computed class names.
fn external_block_style(
    top: f64,
    height: f64,
    color: Option<&str>,
    clamped_top: bool,
    clamped_bottom: bool,
    busy: bool,
    tentative: bool,
) -> String {
    let mut style = format!("top: {top}px; height: {height}px;");
    if let Some(c) = color {
        if busy {
            let bar = if tentative { "dashed" } else { "solid" };
            style.push_str(&format!(
                " border-left: 3px {bar} {c}; background-color: {c}26;"
            ));
        } else {
            style.push_str(&format!(
                " border-left: 3px dashed {c}; background-color: {c}14;"
            ));
        }
    }
    if !busy {
        style.push_str(" opacity: 0.65;");
    }
    if clamped_top {
        style.push_str(" border-top-width: 2px; border-top-style: dashed;");
    }
    if clamped_bottom {
        style.push_str(" border-bottom-width: 2px; border-bottom-style: dashed;");
    }
    style
}

/// Today's all-day events for enabled subscriptions, for the banner row above
/// the timeline. All-day events are cached but never materialized as blocks
/// (a 24h block would wreck the ruler), so the banner reads the cache
/// directly. Multi-day spans show on every local day they cover; the end is
/// exclusive, so an event ending at local midnight does not bleed into the
/// next day.
fn all_day_events_for(
    events: &[model::CalendarEvent],
    enabled_subscriptions: &std::collections::HashSet<String>,
    day: NaiveDate,
) -> Vec<model::CalendarEvent> {
    // Shared with the planner_today context and HOBBES_CALENDAR_LIST — one
    // definition of "today's all-day events" for every surface.
    crate::todo::views::all_day_events_on(events, enabled_subscriptions, day)
}

/// Round down to the nearest quarter hour, so clicked blocks land on tidy edges.
fn snap_to_quarter(minutes: u32) -> u32 {
    minutes - minutes % 15
}

/// Where a moved block lands: the original span shifted by `delta_min`,
/// snapped to the quarter grid and clamped inside the workday, duration
/// preserved. Returns `(start, end)` in minutes since local midnight.
fn dragged_move(
    orig_start: u32,
    orig_end: u32,
    delta_min: i64,
    day_start: u32,
    day_end: u32,
) -> (u32, u32) {
    let duration = orig_end.saturating_sub(orig_start).max(15);
    let max_start = day_end.saturating_sub(duration).max(day_start);
    let raw = (orig_start as i64 + delta_min).clamp(day_start as i64, max_start as i64) as u32;
    let start = snap_to_quarter(raw).max(day_start);
    (start, start + duration)
}

/// Where a resized block's end lands: at least 15 minutes past the start, at
/// most the end of the workday, snapped to the quarter grid.
fn dragged_resize(orig_start: u32, orig_end: u32, delta_min: i64, day_end: u32) -> (u32, u32) {
    let min_end = orig_start + 15;
    let raw = (orig_end as i64 + delta_min).clamp(min_end as i64, day_end.max(min_end) as i64);
    let end = snap_to_quarter(raw as u32).max(min_end).min(day_end.max(min_end));
    (orig_start, end)
}

/// `570` → `"09:30"` — labels for previewed positions, where no DateTime
/// exists yet.
fn fmt_hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Naive first-fit: the earliest quarter-hour slot at or after `day_start`
/// where `duration` minutes don't collide with any busy interval. May run past
/// the end of the workday — an honest overflow beats silently dropping work.
fn first_free_slot(busy: &[(u32, u32)], day_start: u32, duration: u32) -> u32 {
    let mut intervals: Vec<(u32, u32)> = busy.to_vec();
    intervals.sort_unstable();
    let mut candidate = day_start;
    for (s, e) in intervals {
        if s < candidate + duration && candidate < e {
            candidate = e.div_ceil(15) * 15;
        }
    }
    candidate
}

/// Where automatic placement starts searching for a free slot.
///
/// For today, never before *now*: auto-placing work into the morning that has
/// already passed plans fiction. The current minute snaps FORWARD to the
/// quarter grid so placed blocks keep tidy edges, and the workday start still
/// applies as a floor. Future days search from the workday start as before.
/// Only automatic placement is clamped — manual drags and explicit AI times
/// stay allowed anywhere (user intent wins).
fn placement_search_start(workday_start_min: u32, now_min_local: u32, is_today: bool) -> u32 {
    if !is_today {
        return workday_start_min;
    }
    workday_start_min.max(now_min_local.div_ceil(15) * 15)
}

/// Assign side-by-side columns to overlapping timeline blocks.
///
/// Input: `(id, start_min, end_min)` per block. Output: id → `(column_index,
/// column_count)`. Blocks are grouped into clusters of *transitively*
/// overlapping intervals (touching edges — end == next start — do NOT overlap,
/// consistent with `TimeBlock::overlaps`); within a cluster, a greedy sweep
/// places each block (sorted by start) into the first column whose last block
/// ends at or before its start. The column count is capped at 3: a block that
/// would land in column ≥ 3 shares column 2 instead — a slight visual overlap
/// beats unreadably thin slivers. Single-block clusters keep `(0, 1)`, i.e.
/// full width.
fn assign_overlap_columns(
    blocks: &[(String, u32, u32)],
) -> std::collections::HashMap<String, (usize, usize)> {
    const MAX_COLUMNS: usize = 3;
    let mut out = std::collections::HashMap::new();

    let mut sorted: Vec<&(String, u32, u32)> = blocks.iter().filter(|(_, s, e)| e > s).collect();
    sorted.sort_by_key(|(_, s, e)| (*s, *e));

    let mut i = 0;
    while i < sorted.len() {
        // Grow the cluster while the next block starts before the furthest
        // end seen so far (strict <: touching edges start a new cluster).
        let mut cluster_end = sorted[i].2;
        let mut j = i + 1;
        while j < sorted.len() && sorted[j].1 < cluster_end {
            cluster_end = cluster_end.max(sorted[j].2);
            j += 1;
        }

        // Greedy column assignment within the cluster.
        let mut column_ends: Vec<u32> = Vec::new(); // last end per column
        let mut assigned: Vec<(&str, usize)> = Vec::new();
        for (id, s, e) in &sorted[i..j] {
            let col = match column_ends.iter().position(|end| *end <= *s) {
                Some(col) => {
                    column_ends[col] = *e;
                    col
                }
                None => {
                    column_ends.push(*e);
                    column_ends.len() - 1
                }
            };
            assigned.push((id, col));
        }
        let count = column_ends.len().min(MAX_COLUMNS);
        for (id, col) in assigned {
            out.insert(id.to_string(), (col.min(MAX_COLUMNS - 1), count));
        }
        i = j;
    }
    out
}

/// The horizontal style fragment for a timeline block in its overlap column.
///
/// Column 0 of 1 (no overlap) keeps the classic full-width footprint
/// (44px hour-label gutter on the left, 4px margin on the right). Overlapping
/// blocks split the usable width evenly with a small gutter between columns.
/// Inline style, composed with the vertical geometry — Tailwind purges
/// computed class names.
fn overlap_column_style(col: usize, count: usize) -> String {
    if count <= 1 {
        return " left: 44px; right: 4px;".to_string();
    }
    let frac = 1.0 / count as f64;
    let left_frac = frac * col as f64;
    format!(
        " left: calc(44px + (100% - 48px) * {left_frac:.4}); width: calc((100% - 48px) * {frac:.4} - 3px);"
    )
}

/// A local wall-clock minute on `date`, as UTC. Clamped to the day; DST gaps
/// resolve to the earliest valid instant.
fn local_minutes_to_utc(date: NaiveDate, minutes: u32) -> chrono::DateTime<Utc> {
    let minutes = minutes.min(23 * 60 + 59);
    let naive = date
        .and_hms_opt(minutes / 60, minutes % 60, 0)
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).expect("midnight is always valid"));
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

/// Minutes since local midnight for a stored UTC instant.
fn minutes_in_local_day(dt: &chrono::DateTime<Utc>) -> i64 {
    let local = dt.with_timezone(&Local);
    (local.time().num_seconds_from_midnight() / 60) as i64
}

/// Estimate chip click-cycle: none → 15m → 30m → 1h → 2h → none.
///
/// Off-ladder values (AI- or free-entry-set, e.g. 45m) snap to the NEAREST
/// ladder step — ties round up, so 45 → 60 — and the next click walks the
/// ladder normally from there. Only an exact 2h advances to none: a click on
/// an off-ladder chip must never throw the estimate away.
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
        // The fleet renders its own empty state; no todo list exists there.
        PlannerSelection::Fleet => "",
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
        PlannerSelection::Fleet => Vec::new(),
    }
}

// ── Focus bar ───────────────────────────────────────────────────────────────

/// Sunsama-style focus strip: visible whenever one todo is in progress, with
/// a live elapsed-vs-estimate readout and the two exits (done / stop).
#[component]
fn FocusBar() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();

    // Elapsed is wall-clock; a ~20s ticker keeps the readout honest without
    // meaningful re-render cost (the bar is a single strip).
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            tick += 1;
        }
    });
    let _subscribe = *tick.read();

    let Some(focus) = planner.read().focused().cloned() else {
        return rsx! {};
    };
    // Who is driving this sitting: the live session row records the actor.
    let agent_driven = planner
        .read()
        .open_focus_session_for(&focus.id)
        .map(|s| s.actor.is_agent())
        .unwrap_or(false);
    let now = Utc::now();
    let elapsed = focus.elapsed_minutes(now);
    let readout = match focus.estimate_minutes {
        Some(est) => format!(
            "{} of {}",
            model::format_minutes(elapsed),
            model::format_minutes(est)
        ),
        None => model::format_minutes(elapsed),
    };
    let over = focus.estimate_minutes.is_some_and(|est| elapsed > est);
    let focus_id = focus.id.clone();
    let done_id = focus.id.clone();

    rsx! {
        div {
            class: "flex items-center gap-3 border-b border-subtle bg-primary-700/30 px-4 py-2",
            span { class: "h-2 w-2 shrink-0 rounded-full bg-primary-400 animate-pulse" }
            span { class: "min-w-0 truncate text-sm font-medium text-fg", "{focus.title}" }
            if agent_driven {
                span {
                    class: "shrink-0 rounded-full border border-subtle px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-fg-muted",
                    title: "This focus session was started by the assistant",
                    "◇ agent"
                }
            }
            span {
                class: if over { "shrink-0 text-xs text-red-400" } else { "shrink-0 text-xs text-fg-muted" },
                "{readout}"
            }
            div { class: "flex-1" }
            button {
                class: "shrink-0 rounded bg-btn-primary px-2 py-1 text-xs font-medium text-fg hover:bg-btn-primary-hover",
                onclick: move |_| {
                    // State-level complete: also closes the live focus-session
                    // row as 'completed'.
                    if planner.write().complete_todo(&done_id, Utc::now()) {
                        persist_todo(&planner, &done_id);
                        persist_focus_sessions(&mut planner);
                    }
                },
                "Done"
            }
            button {
                class: "shrink-0 rounded px-2 py-1 text-xs text-fg-muted hover:bg-input hover:text-fg",
                title: "Pause — elapsed time is kept",
                onclick: move |_| {
                    let _ = &focus_id;
                    if planner.write().stop_focus(Utc::now()).is_some() {
                        persist_todo(&planner, &focus_id);
                        persist_focus_sessions(&mut planner);
                    }
                },
                "Stop"
            }
        }
    }
}

// ── Root ────────────────────────────────────────────────────────────────────

#[component]
pub fn PlannerView() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let selection = use_signal(|| PlannerSelection::View(TodoView::Today));

    // Which todo's detail card is open (U2.1) — consumed by TodoRow's info
    // button and by the card rendered at this root.
    let detail_open = use_signal(|| Option::<String>::None);
    use_context_provider(|| crate::components::todo_detail::TodoDetailContext(detail_open));

    // Timeline block selection lives at the root (not in TodayRail) so the
    // timebox chip on a row can select the block it navigates to.
    let selected_block = use_signal(|| Option::<String>::None);
    use_context_provider(|| SelectedBlockContext(selected_block));

    // Menu-bar timer → planner reveal: the tray click handler (main.rs) parks
    // the focused todo's id in this global because selection/detail state is
    // local to this component and it may not even be mounted yet. Drain it:
    // jump to Today, open the todo's detail card, and highlight its timeline
    // block if it has one (the existing selection mechanism — no new pulse
    // machinery). Setting the global back to None re-runs the effect once,
    // which then no-ops.
    {
        let mut selection = selection;
        let mut detail_open = detail_open;
        let mut selected_block = selected_block;
        use_effect(move || {
            let pending = crate::focus_tray::PLANNER_REVEAL_TODO.read().clone();
            let Some(id) = pending else { return };
            selection.set(PlannerSelection::View(TodoView::Today));
            let block_id = planner
                .peek()
                .blocks_on(today())
                .iter()
                .find(|b| b.todo_id.as_deref() == Some(id.as_str()))
                .map(|b| b.id.clone());
            selected_block.set(block_id);
            detail_open.set(Some(id));
            *crate::focus_tray::PLANNER_REVEAL_TODO.write() = None;
        });
    }

    // Auto-rollover, once per mount: yesterday's unfinished work reappears on
    // today's plate. Guarded by a signal so re-renders never repeat it.
    let mut rolled_over = use_signal(|| false);
    use_effect(move || {
        if *rolled_over.peek() {
            return;
        }
        rolled_over.set(true);
        if !settings.peek().planner_auto_rollover {
            return;
        }
        let day = today();
        // Capture the movers first — rollover_unfinished only reports a count.
        let ids: Vec<String> = planner
            .peek()
            .todos
            .iter()
            .filter(|t| !t.status.is_closed() && t.scheduled_for.is_some_and(|d| d < day))
            .map(|t| t.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let moved = views::rollover_unfinished(&mut planner.write().todos, day, Utc::now());
        for id in &ids {
            persist_todo(&planner, id);
        }
        if moved > 0 {
            tracing::info!("planner: rolled {} unfinished todo(s) forward to {}", moved, day);
        }
    });

    if !settings.read().planner_enabled {
        return rsx! {
            div {
                class: "flex h-full items-center justify-center bg-app text-fg",
                p { class: "text-sm text-fg-muted", "The planner is disabled in Settings." }
            }
        };
    }

    let show_today_rail = *selection.read() == PlannerSelection::View(TodoView::Today);

    // Column resizing follows the main view's sidebar convention: a divider
    // strip arms a fullscreen overlay, mousemove writes the width straight to
    // the DOM via document::eval (no re-render churn), and mouseup commits the
    // final width to the signal and persists it in UiState.
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let mut left_width = use_signal(|| ui_state.peek().planner_left_width);
    let mut today_width = use_signal(|| ui_state.peek().planner_today_width);
    // (is_left_divider, drag start x, width at drag start)
    let mut drag: Signal<Option<(bool, f64, f64)>> = use_signal(|| None);
    let mut drag_last_width = use_signal(|| 0.0f64);

    let mut commit_drag = move || {
        let Some((is_left, _, _)) = *drag.peek() else {
            return;
        };
        let w = *drag_last_width.peek();
        drag.set(None);
        if w <= 0.0 {
            return;
        }
        if is_left {
            left_width.set(w);
            ui_state.write().planner_left_width = w;
        } else {
            today_width.set(w);
            ui_state.write().planner_today_width = w;
        }
        let state = (*ui_state.read()).clone();
        let manager = (*ui_state_manager.read()).clone();
        spawn(async move {
            let _ = manager.save(&state);
        });
    };

    rsx! {
        div {
            class: "flex h-full min-h-0 flex-col bg-app text-fg",
            FocusBar {}
            div {
            class: "relative flex flex-row flex-1 min-h-0",
            div {
                id: "planner-left-rail",
                class: "shrink-0 h-full min-h-0",
                style: "width: {left_width}px;",
                LeftRail { selection }
            }
            div {
                class: "w-1 shrink-0 cursor-col-resize bg-primary-700/40 hover:bg-primary-500 transition-colors",
                onmousedown: move |evt| {
                    drag_last_width.set(*left_width.peek());
                    drag.set(Some((true, evt.data.screen_coordinates().x, *left_width.peek())));
                },
            }
            CentreColumn { selection }
            if show_today_rail {
                div {
                    class: "w-1 shrink-0 cursor-col-resize bg-primary-700/40 hover:bg-primary-500 transition-colors",
                    onmousedown: move |evt| {
                        drag_last_width.set(*today_width.peek());
                        drag.set(Some((false, evt.data.screen_coordinates().x, *today_width.peek())));
                    },
                }
                div {
                    id: "planner-today-rail",
                    class: "shrink-0 h-full min-h-0",
                    style: "width: {today_width}px;",
                    TodayRail {}
                }
            }

            if drag.read().is_some() {
                div {
                    class: "fixed inset-0 z-50 cursor-col-resize",
                    onmousemove: move |evt| {
                        let Some((is_left, start_x, start_w)) = *drag.peek() else {
                            return;
                        };
                        let dx = evt.data.screen_coordinates().x - start_x;
                        // The right divider sits on the rail's left edge, so
                        // dragging left grows the rail.
                        let (id, new_w) = if is_left {
                            ("planner-left-rail", (start_w + dx).clamp(160.0, 340.0))
                        } else {
                            ("planner-today-rail", (start_w - dx).clamp(240.0, 560.0))
                        };
                        let js = format!(
                            "document.getElementById('{}').style.width = '{}px';",
                            id, new_w
                        );
                        let _ = document::eval(&js);
                        drag_last_width.set(new_w);
                    },
                    onmouseup: move |_| commit_drag(),
                    onmouseleave: move |_| commit_drag(),
                }
            }
            }
            crate::components::todo_detail::TodoDetailCard {}
        }
    }
}

// ── Left rail ───────────────────────────────────────────────────────────────

#[component]
fn RailItem<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    label: String,
    count: usize,
    selected: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        button {
            class: if selected {
                "flex w-full items-center gap-2 rounded px-2 py-1.5 text-sm bg-input text-fg"
            } else {
                "flex w-full items-center gap-2 rounded px-2 py-1.5 text-sm text-fg-muted hover:bg-input hover:text-fg transition-colors"
            },
            onclick: move |evt| onclick.call(evt),
            Icon { width: 16, height: 16, icon }
            span { class: "flex-1 truncate text-left", "{label}" }
            if count > 0 {
                span { class: "text-xs text-fg-muted", "{count}" }
            }
        }
    }
}

#[component]
fn LeftRail(mut selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let day = today();

    let show_fleet = settings.read().fleet_enabled && crate::entitlement::pro_active();
    let fleet_attention = fleet_state.read().attention_count();

    let state = planner.read();
    let count = |view: TodoView| views::in_view(&state.todos, view, day).len();
    let project_count = |id: &str| {
        state
            .todos
            .iter()
            .filter(|t| !t.status.is_closed() && t.project_id.as_deref() == Some(id))
            .count()
    };

    let mut areas = state.areas.clone();
    areas.sort_by(|a, b| {
        a.sort_order
            .partial_cmp(&b.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let area_ids: Vec<&str> = areas.iter().map(|a| a.id.as_str()).collect();
    let mut projects = state.projects.clone();
    projects.retain(|p| !p.status.is_closed());
    projects.sort_by(|a, b| {
        a.sort_order
            .partial_cmp(&b.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // A project pointing at a deleted area still deserves a home in the tree.
    let top_level: Vec<_> = projects
        .iter()
        .filter(|p| p.area_id.as_deref().is_none_or(|id| !area_ids.contains(&id)))
        .cloned()
        .collect();
    let by_area: Vec<(String, Vec<_>)> = areas
        .iter()
        .map(|a| {
            (
                a.title.clone(),
                projects
                    .iter()
                    .filter(|p| p.area_id.as_deref() == Some(a.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let has_tree = !top_level.is_empty() || !by_area.is_empty();

    let sel = selection.read().clone();
    let is_view = |v: TodoView| sel == PlannerSelection::View(v);

    rsx! {
        div {
            class: "h-full w-full flex flex-col gap-0.5 overflow-y-auto border-r border-subtle bg-section p-2",
            RailItem {
                icon: fi_icons::FiInbox,
                label: "Inbox",
                count: count(TodoView::Inbox),
                selected: is_view(TodoView::Inbox),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Inbox)),
            }
            RailItem {
                icon: fi_icons::FiStar,
                label: "Today",
                count: count(TodoView::Today),
                selected: is_view(TodoView::Today),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Today)),
            }
            RailItem {
                icon: fi_icons::FiCalendar,
                label: "Upcoming",
                count: count(TodoView::Upcoming),
                selected: is_view(TodoView::Upcoming),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Upcoming)),
            }
            RailItem {
                icon: fi_icons::FiLayers,
                label: "Anytime",
                count: count(TodoView::Anytime),
                selected: is_view(TodoView::Anytime),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Anytime)),
            }
            RailItem {
                icon: fi_icons::FiArchive,
                label: "Someday",
                count: count(TodoView::Someday),
                selected: is_view(TodoView::Someday),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Someday)),
            }
            RailItem {
                icon: fi_icons::FiCheckCircle,
                label: "Logbook",
                count: count(TodoView::Logbook),
                selected: is_view(TodoView::Logbook),
                onclick: move |_| selection.set(PlannerSelection::View(TodoView::Logbook)),
            }

            // Fleet (Pro surface): only when the feature is on AND Pro is
            // active. The badge counts sessions waiting on the user.
            if show_fleet {
                RailItem {
                    icon: fi_icons::FiMonitor,
                    label: "Fleet",
                    count: fleet_attention,
                    selected: sel == PlannerSelection::Fleet,
                    onclick: move |_| selection.set(PlannerSelection::Fleet),
                }
            }

            if has_tree {
                div { class: "my-2 border-t border-faint" }
            }

            for project in top_level {
                RailItem {
                    key: "{project.id}",
                    icon: fi_icons::FiFolder,
                    label: project.title.clone(),
                    count: project_count(&project.id),
                    selected: sel == PlannerSelection::Project(project.id.clone()),
                    onclick: {
                        let id = project.id.clone();
                        move |_| selection.set(PlannerSelection::Project(id.clone()))
                    },
                }
            }
            for (area_title, area_projects) in by_area {
                div {
                    key: "{area_title}",
                    class: "mt-2",
                    p {
                        class: "px-2 pb-1 text-xs font-medium uppercase tracking-wide text-fg-muted",
                        "{area_title}"
                    }
                    for project in area_projects {
                        RailItem {
                            key: "{project.id}",
                            icon: fi_icons::FiFolder,
                            label: project.title.clone(),
                            count: project_count(&project.id),
                            selected: sel == PlannerSelection::Project(project.id.clone()),
                            onclick: {
                                let id = project.id.clone();
                                move |_| selection.set(PlannerSelection::Project(id.clone()))
                            },
                        }
                    }
                }
            }
        }
    }
}

// ── Centre column ───────────────────────────────────────────────────────────

#[component]
fn CentreColumn(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let sel = selection.read().clone();

    // The fleet panel replaces the todo column wholesale — it has its own
    // header, list, and empty state.
    if sel == PlannerSelection::Fleet {
        return rsx! { crate::components::fleet_view::FleetView {} };
    }

    let title = match &sel {
        PlannerSelection::View(v) => view_title(*v).to_string(),
        PlannerSelection::Fleet => unreachable!("handled above"),
        PlannerSelection::Project(id) => planner
            .read()
            .projects
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.title.clone())
            .unwrap_or_else(|| "Project".to_string()),
    };
    let subtitle = (sel == PlannerSelection::View(TodoView::Today))
        .then(|| Local::now().format("%A, %-d %B").to_string());
    // Quick-adding into the logbook would resurrect nothing — hide it there.
    let show_quick_add = sel != PlannerSelection::View(TodoView::Logbook);

    rsx! {
        div {
            class: "flex-1 min-w-0 flex flex-col overflow-y-auto",
            div {
                class: "px-6 pt-6 pb-2",
                div {
                    class: "flex items-baseline gap-3",
                    h1 { class: "text-xl font-semibold", "{title}" }
                    if let Some(date_line) = subtitle {
                        span { class: "text-sm text-fg-muted", "{date_line}" }
                    }
                }
            }
            if show_quick_add {
                QuickAdd { selection }
            }
            TodoList { selection }
            // Breathing room so hover menus on the last row aren't clipped.
            div { class: "h-16 shrink-0" }
        }
    }
}

#[component]
fn QuickAdd(selection: Signal<PlannerSelection>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut draft = use_signal(String::new);

    let mut submit = move |_| {
        let day = today();
        let parsed = crate::todo::quick_add::parse_quick_add(&draft.peek(), day);
        if parsed.title.is_empty() {
            return; // tokens without a title aren't a todo yet
        }

        let mut todo = Todo::new(parsed.title, planner.peek().next_sort_order());
        // New todos land in the context being looked at, so quick-add never
        // makes work vanish into a different list.
        match selection.peek().clone() {
            // Scheduled todos are born triaged (Anytime, like block-created
            // ones): clearing the date later must not dump them into Inbox.
            PlannerSelection::View(TodoView::Today) => {
                todo.scheduled_for = Some(day);
                todo.bucket = TodoBucket::Anytime;
            }
            PlannerSelection::View(TodoView::Upcoming) => {
                todo.scheduled_for = day.succ_opt();
                todo.bucket = TodoBucket::Anytime;
            }
            PlannerSelection::View(TodoView::Anytime) => todo.bucket = TodoBucket::Anytime,
            PlannerSelection::View(TodoView::Someday) => todo.bucket = TodoBucket::Someday,
            PlannerSelection::Project(id) => {
                todo.project_id = Some(id);
                todo.bucket = TodoBucket::Anytime;
            }
            _ => {} // Inbox (and Logbook, which hides quick-add)
        }

        // A *day token overrides the view-context date — the user said when.
        // Born triaged (Anytime) like every other scheduled birth, so clearing
        // the date later never dumps it into Inbox.
        if let Some(d) = parsed.scheduled {
            todo.scheduled_for = Some(d);
            todo.bucket = TodoBucket::Anytime;
        }

        // Tokens beat defaults; defaults beat nothing.
        let default_estimate = settings.peek().planner_default_estimate_minutes;
        todo.estimate_minutes = parsed
            .estimate_minutes
            .or((default_estimate > 0).then_some(default_estimate));
        todo.tags = parsed.tags;
        todo.deadline = parsed.deadline;
        todo.time_of_day = parsed.time_of_day;

        // An @clock token also implies a time-of-day group when none was given
        // explicitly, so "@7pm" lands in This Evening without a second token.
        if todo.time_of_day.is_none() {
            todo.time_of_day = parsed.block_start.map(|m| match m / 60 {
                0..=11 => TimeOfDay::Morning,
                12..=16 => TimeOfDay::Afternoon,
                _ => TimeOfDay::Evening,
            });
        }

        // An @clock token means "this happens at that time": it implies a
        // scheduled day (today unless the view already picked one) and puts a
        // linked block on that day's timeline.
        let block_date = parsed.block_start.map(|_| {
            let d = todo.scheduled_for.unwrap_or(day);
            todo.scheduled_for = Some(d);
            if todo.bucket == TodoBucket::Inbox {
                todo.bucket = TodoBucket::Anytime;
            }
            d
        });

        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save new todo: {}", e);
        }

        let block = match (block_date, parsed.block_start) {
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
            state.upsert_todo(todo);
            if let Some(b) = block {
                state.blocks.push(b);
            }
        }
        draft.set(String::new());
        // The input stays mounted, so focus is retained across submits.
    };

    // Parsed inline on every keystroke (P-012: draft is a Signal, so this
    // tracks). The chips show what Enter will actually do — the premium tell
    // of Things/Todoist quick entry is that tokens are confirmed before commit.
    let day = today();
    let preview = crate::todo::quick_add::parse_quick_add(&draft.read(), day);

    rsx! {
        div {
            class: "px-6 pb-3",
            input {
                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
                r#type: "text",
                placeholder: "Add a to-do — ~30m · #tag · @2pm · *mon · !fri — press Enter",
                value: "{draft}",
                oninput: move |evt| draft.set(evt.value()),
                onkeydown: move |evt| {
                    if evt.key() == Key::Enter {
                        evt.prevent_default();
                        submit(());
                    }
                },
            }
            if preview.has_tokens() {
                div {
                    class: "mt-1.5 flex flex-wrap items-center gap-1.5",
                    if let Some(m) = preview.estimate_minutes {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg", "{model::format_minutes(m)}" }
                    }
                    if let Some(start) = preview.block_start {
                        span { class: "rounded bg-btn-primary px-1.5 py-0.5 text-[10px] text-fg", "on timeline @{fmt_hhmm(start)}" }
                    }
                    if let Some(tod) = preview.time_of_day {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg",
                            match tod {
                                TimeOfDay::Morning => "morning",
                                TimeOfDay::Afternoon => "afternoon",
                                TimeOfDay::Evening => "this evening",
                            }
                        }
                    }
                    if let Some(d) = preview.scheduled {
                        span {
                            class: "flex items-center gap-1 rounded bg-input px-1.5 py-0.5 text-[10px] text-fg",
                            Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                            "→ {views::friendly_date(d, day)}"
                        }
                    }
                    if let Some(d) = preview.deadline {
                        span { class: "rounded bg-red-500/20 px-1.5 py-0.5 text-[10px] text-fg", "due {views::friendly_date(d, day)}" }
                    }
                    for tag in preview.tags.iter() {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg-muted", "#{tag}" }
                    }
                    if preview.title.is_empty() {
                        span { class: "text-[10px] text-fg-muted", "…needs a title" }
                    }
                }
            }
        }
    }
}

#[component]
fn TodoList(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let sel = selection.read().clone();
    let day = today();
    let state = planner.read();

    let is_today = sel == PlannerSelection::View(TodoView::Today);
    let is_upcoming = sel == PlannerSelection::View(TodoView::Upcoming);

    // Upcoming renders under day headers — the same grouped query the AI's
    // list tool flattens, so the two orderings can't drift.
    let upcoming_groups: Vec<(NaiveDate, Vec<Todo>)> = if is_upcoming {
        views::upcoming_grouped(&state.todos, day)
            .into_iter()
            .map(|(d, ts)| (d, ts.into_iter().cloned().collect()))
            .collect()
    } else {
        Vec::new()
    };

    // Today gets its "This Evening" split; every other selection is one flat
    // list. Completing a todo must keep its row visible for the rest of the
    // day (struck through, reopenable in place), so Today appends the day's
    // completions after the open rows in each section — a separate query from
    // the view membership, which the AI's list tool needs to stay "open work
    // owed".
    let (main_rows, evening_rows): (Vec<Todo>, Vec<Todo>) = if is_today {
        let sections = views::today_sections(&state.todos, day);
        let (done_evening, done_daytime): (Vec<&Todo>, Vec<&Todo>) =
            views::completed_today(&state.todos, day)
                .into_iter()
                .partition(|t| t.time_of_day == Some(TimeOfDay::Evening));
        (
            sections
                .daytime
                .into_iter()
                .chain(done_daytime)
                .cloned()
                .collect(),
            sections
                .evening
                .into_iter()
                .chain(done_evening)
                .cloned()
                .collect(),
        )
    } else if is_upcoming {
        (Vec::new(), Vec::new())
    } else {
        (todos_for_selection(&state, &sel, day), Vec::new())
    };
    let is_empty = main_rows.is_empty() && evening_rows.is_empty() && upcoming_groups.is_empty();

    rsx! {
        div {
            class: "flex flex-col px-4",
            if is_empty {
                p { class: "px-2 py-8 text-sm text-fg-muted", "{empty_state_line(&sel)}" }
            }
            for todo in main_rows {
                // Cloned because the generated key borrows `todo.id` after the
                // prop takes ownership.
                TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
            }
            for (date, rows) in upcoming_groups {
                div {
                    key: "day-{date}",
                    h2 {
                        class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                        "{views::friendly_date(date, day)}"
                    }
                    for todo in rows {
                        TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
                    }
                }
            }
            if !evening_rows.is_empty() {
                h2 {
                    class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                    "This Evening"
                }
                for todo in evening_rows {
                    TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
                }
            }
        }
    }
}

#[component]
fn TodoRow(todo: Todo, mut selection: Signal<PlannerSelection>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);
    // U1.5: first click arms, second click deletes; leaving the row disarms.
    let mut delete_armed = use_signal(|| false);
    let mut detail_open = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    let mut selected_block = use_context::<SelectedBlockContext>().0;

    let sel = selection.read().clone();
    let id = todo.id.clone();
    let day = today();
    let is_done = todo.status.is_closed();
    let in_focus = todo.status == TodoStatus::InProgress;
    let is_overdue = todo.is_overdue(day);

    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);
    // A project mixes scheduled with unscheduled work, so its rows answer
    // "when?" inline. Upcoming no longer needs the chip: its day headers
    // (U3.3) already say the date, and repeating it per row is noise.
    let show_scheduled = matches!(sel, PlannerSelection::Project(_));

    // The hover cluster never offers the state the row is already in.
    // "Today" stays visible for an Evening row scheduled today — there it
    // means "back to the daytime list", not a no-op.
    let offer_today = !(sel == PlannerSelection::View(TodoView::Today)
        && todo.scheduled_for == Some(day)
        && todo.time_of_day.is_none());
    let offer_tomorrow = todo.scheduled_for != day.succ_opt();
    let offer_evening =
        !(todo.time_of_day == Some(TimeOfDay::Evening) && todo.scheduled_for.is_some());
    let offer_clear_date = todo.scheduled_for.is_some() || todo.time_of_day.is_some();
    let offer_someday = sel != PlannerSelection::View(TodoView::Someday);

    // The row mirrors the timeline: if this todo is timeboxed on its
    // scheduled day, its start time appears as a chip (with the block id so
    // clicking the chip can highlight it on arrival). Scheduling metadata
    // must be legible from every visual state, not just the ruler.
    let block_info: Option<(String, String)> = todo.scheduled_for.and_then(|d| {
        let state = planner.read();
        state
            .blocks
            .iter()
            .filter(|b| {
                b.todo_id.as_deref() == Some(todo.id.as_str())
                    && b.start.with_timezone(&Local).date_naive() == d
            })
            .min_by_key(|b| b.start)
            .map(|b| {
                (
                    b.start.with_timezone(&Local).format("%H:%M").to_string(),
                    b.id.clone(),
                )
            })
    });

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

    let schedule = |patch: fn(&mut Todo, NaiveDate)| {
        let id = id.clone();
        move |_| {
            mutate_todo(planner, &id, |t| patch(t, today()));
            // The timebox follows the schedule: blocks left on days that no
            // longer match the new date are removed everywhere (same rule as
            // the AI's update and plan-day handlers).
            let keep = planner.peek().todo(&id).and_then(|t| t.scheduled_for);
            let pruned = planner.write().prune_blocks_for_todo(&id, keep);
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("planner: failed to delete rescheduled block {}: {}", b.id, e);
                }
            }
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

    let completed_line = in_logbook.then(|| {
        todo.completed_at
            .map(|d| views::friendly_date(d.with_timezone(&Local).date_naive(), day))
            .unwrap_or_default()
    });
    // U2.2: the logbook surfaces estimate accuracy — "52m of 1h", or just the
    // actual when the work was never estimated.
    let actuals_line = (in_logbook && todo.actual_minutes > 0).then(|| match todo.estimate_minutes {
        Some(est) => format!(
            "{} of {}",
            model::format_minutes(todo.actual_minutes),
            model::format_minutes(est)
        ),
        None => model::format_minutes(todo.actual_minutes),
    });

    rsx! {
        div {
            class: "group flex items-center gap-2 rounded px-2 py-1.5 hover:bg-section",
            // A wandering pointer disarms the delete confirmation.
            onmouseleave: move |_| {
                if *delete_armed.peek() {
                    delete_armed.set(false);
                }
            },
            // Checkbox — a pulsing play glyph while the todo is in focus.
            button {
                class: if in_focus {
                    "shrink-0 text-primary-400 animate-pulse hover:text-fg transition-colors"
                } else {
                    "shrink-0 text-fg-muted hover:text-fg transition-colors"
                },
                title: if is_done { "Reopen" } else { "Complete" },
                onclick: toggle_done,
                if is_done {
                    Icon { width: 16, height: 16, icon: fi_icons::FiCheckCircle }
                } else if in_focus {
                    Icon { width: 16, height: 16, icon: fi_icons::FiPlayCircle }
                } else {
                    Icon { width: 16, height: 16, icon: fi_icons::FiCircle }
                }
            }
            // Title (click to edit inline)
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
                        "flex-1 min-w-0 truncate text-sm text-fg-muted line-through cursor-text"
                    } else {
                        "flex-1 min-w-0 truncate text-sm text-fg cursor-text"
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
            if let Some(date_line) = completed_line {
                span { class: "shrink-0 text-xs text-fg-muted", "{date_line}" }
            }
            if let Some(actuals) = actuals_line {
                span { class: "shrink-0 text-xs text-fg-muted", "{actuals}" }
            }
            // Chips
            if show_scheduled {
                if let Some(d) = todo.scheduled_for {
                    span {
                        class: "shrink-0 flex items-center gap-1 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted",
                        Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                        "{views::friendly_date(d, day)}"
                    }
                }
            }
            if !in_logbook {
                if let Some((t, block_id)) = block_info {
                    // A live affordance, not a dead label: the chip jumps to
                    // Today with its block selected, so "@ 09:00" is always
                    // one click from the timebox it names.
                    button {
                        class: "shrink-0 rounded-full bg-primary-700/50 border border-subtle px-2 py-0.5 text-xs text-fg hover:bg-primary-700",
                        title: "On the timeline — click to show it in Today",
                        onclick: move |_| {
                            selected_block.set(Some(block_id.clone()));
                            selection.set(PlannerSelection::View(TodoView::Today));
                        },
                        "@ {t}"
                    }
                }
            }
            if !in_logbook {
                button {
                    class: if todo.estimate_minutes.is_some() {
                        "shrink-0 rounded-full border border-subtle bg-card px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                    } else {
                        "shrink-0 rounded-full border border-faint px-2 py-0.5 text-xs text-fg-muted opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                    },
                    title: "Estimate — click to cycle 15m / 30m / 1h / 2h / none",
                    onclick: cycle,
                    if let Some(m) = todo.estimate_minutes {
                        "{model::format_minutes(m)}"
                    } else {
                        "est"
                    }
                }
            }
            if let Some(deadline) = todo.deadline {
                span {
                    class: if is_overdue {
                        "shrink-0 rounded-full border border-red-700 bg-red-900/30 px-2 py-0.5 text-xs text-red-400"
                    } else {
                        "shrink-0 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted"
                    },
                    "due {views::friendly_date(deadline, day)}"
                }
            }
            for tag in todo.tags.iter() {
                span {
                    class: "shrink-0 rounded-full bg-input px-2 py-0.5 text-xs text-fg-muted",
                    "#{tag}"
                }
            }
            // Hover cluster. Logbook rows get a reduced one (U3.5): Details
            // and Delete only — closed work can be inspected and pruned, but
            // scheduling or focusing it would mean reopening it first.
            div {
                class: "hidden shrink-0 items-center gap-1 group-hover:flex",
                // Detail card (U2.1): the full editor for this todo.
                button {
                    class: "rounded p-1 text-fg-muted hover:bg-input hover:text-fg",
                    title: "Details",
                    onclick: {
                        let id = id.clone();
                        move |_| detail_open.set(Some(id.clone()))
                    },
                    Icon { width: 14, height: 14, icon: fi_icons::FiInfo }
                }
                if !in_logbook {
                    // Start / stop focus on this todo.
                    button {
                        class: "rounded p-1 text-primary-300 hover:bg-input hover:text-primary-200",
                        title: if in_focus { "Stop focus — elapsed time is kept" } else { "Start focus (pauses any other)" },
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                let now = Utc::now();
                                if in_focus {
                                    if planner.write().stop_focus(now).is_some() {
                                        persist_todo(&planner, &id);
                                        persist_focus_sessions(&mut planner);
                                    }
                                } else {
                                    // Focus implies today: start_focus may
                                    // reschedule the todo and prune off-day
                                    // blocks, whose rows we must delete.
                                    let (changed, pruned) = planner
                                        .write()
                                        .start_focus(&id, now, FocusActor::Person);
                                    for cid in &changed {
                                        persist_todo(&planner, cid);
                                    }
                                    for b in &pruned {
                                        if let Err(e) = store::delete_block(&b.id) {
                                            tracing::error!(
                                                "planner: failed to delete stale block {}: {}",
                                                b.id, e
                                            );
                                        }
                                    }
                                    persist_focus_sessions(&mut planner);
                                }
                            }
                        },
                        if in_focus {
                            Icon { width: 14, height: 14, icon: fi_icons::FiPauseCircle }
                        } else {
                            Icon { width: 14, height: 14, icon: fi_icons::FiPlayCircle }
                        }
                    }
                    // Activate: focus the task AND open a fresh chat seeded
                    // with it — activating means working on it now.
                    button {
                        class: "rounded p-1 text-primary-300 hover:bg-input hover:text-primary-200",
                        title: "Work on this in a new chat",
                        onclick: {
                            let todo = todo.clone();
                            let id = id.clone();
                            move |_| {
                                let (changed, pruned) = planner
                                    .write()
                                    .start_focus(&id, Utc::now(), FocusActor::Person);
                                for cid in &changed {
                                    persist_todo(&planner, cid);
                                }
                                for b in &pruned {
                                    if let Err(e) = store::delete_block(&b.id) {
                                        tracing::error!(
                                            "planner: failed to delete stale block {}: {}",
                                            b.id, e
                                        );
                                    }
                                }
                                persist_focus_sessions(&mut planner);
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::StartTodoInChat(
                                        activation_seed(&todo),
                                    ),
                                ));
                            }
                        },
                        Icon { width: 14, height: 14, icon: fi_icons::FiMessageCircle }
                    }
                    if offer_today {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, day| {
                                t.scheduled_for = Some(day);
                                t.time_of_day = None;
                            }),
                            "Today"
                        }
                    }
                    if offer_tomorrow {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, day| {
                                t.scheduled_for = day.succ_opt();
                                t.time_of_day = None;
                            }),
                            "Tomorrow"
                        }
                    }
                    if offer_evening {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            title: "Move to the This Evening group",
                            // Evening is a time-of-day, not a reschedule: a
                            // dated todo keeps its day (yanking an Upcoming
                            // todo onto today was the U1.4 bug); only an
                            // undated one gets scheduled today.
                            onclick: schedule(|t, day| {
                                t.time_of_day = Some(TimeOfDay::Evening);
                                if t.scheduled_for.is_none() {
                                    t.scheduled_for = Some(day);
                                }
                            }),
                            "Evening"
                        }
                    }
                    if offer_clear_date {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, _| {
                                t.scheduled_for = None;
                                t.time_of_day = None;
                            }),
                            "Clear date"
                        }
                    }
                    if offer_someday {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, _| {
                                t.scheduled_for = None;
                                t.time_of_day = None;
                                t.bucket = TodoBucket::Someday;
                            }),
                            "Someday"
                        }
                    }
                }
                if *delete_armed.read() {
                        button {
                            class: "rounded border border-red-700 bg-red-900/30 px-1.5 py-0.5 text-xs font-medium text-red-400 hover:bg-input",
                            title: "Click again to delete",
                            onclick: delete.clone(),
                            "Sure?"
                        }
                    } else {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-red-400 hover:bg-input",
                            title: "Delete",
                            onclick: delete,
                            Icon { width: 14, height: 14, icon: fi_icons::FiTrash2 }
                        }
                }
            }
        }
    }
}

// ── Right rail: capacity + timeline (Today only) ────────────────────────────

#[component]
fn CapacityBar(planned: u32, done: u32, capacity: u32, agent: u32, summary: String) -> Element {
    // The bar's full width represents whichever is larger, so overcommitment
    // shows as a red tail instead of silently clipping at 100%. `planned`
    // includes `done`: finished work fills from the left with its own tint so
    // completing a todo moves time between segments instead of shrinking the
    // bar.
    let denom = planned.max(capacity).max(1) as f64;
    let within = planned.min(capacity);
    let done_pct = (done.min(within) as f64 / denom) * 100.0;
    let open_pct = (within.saturating_sub(done) as f64 / denom) * 100.0;
    let over_pct = (planned.saturating_sub(capacity) as f64 / denom) * 100.0;
    // The agent lane shares the person bar's scale but never its space:
    // agent minutes consume no capacity, so they render as their own thin
    // strip below rather than a segment of the day.
    let agent_pct = ((agent as f64 / denom) * 100.0).min(100.0);
    let agent_label = format!("{} agent focus today", model::format_minutes(agent));

    rsx! {
        div {
            class: "flex flex-col gap-1.5",
            div {
                class: "h-2 w-full overflow-hidden rounded-full bg-input border border-faint flex",
                if done_pct > 0.0 {
                    div {
                        class: "h-full bg-primary-400",
                        style: "width: {done_pct}%;",
                    }
                }
                div {
                    class: "h-full bg-btn-primary",
                    style: "width: {open_pct}%;",
                }
                if over_pct > 0.0 {
                    div {
                        class: "h-full bg-red-700",
                        style: "width: {over_pct}%;",
                    }
                }
            }
            if agent > 0 {
                div {
                    class: "w-full overflow-hidden rounded-full bg-input border border-faint flex",
                    style: "height: 4px;",
                    title: "{agent_label}",
                    div {
                        class: "h-full",
                        style: "width: {agent_pct}%; background-color: #a78bfa;",
                    }
                }
            }
            p { class: "text-xs text-fg-muted", "{summary}" }
        }
    }
}

#[component]
fn TodayRail() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let mut selected_block = use_context::<SelectedBlockContext>().0;

    // Current-time indicator: a once-a-minute tick is enough for a line that
    // moves ~0.8 px/min; subscribing to it re-renders the rail so the line
    // crawls without any other state changing.
    let mut now_tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            now_tick += 1;
        }
    });
    let _subscribe_now = *now_tick.read();

    let day = today();
    let settings_read = settings.read();
    let (day_start, day_end) = workday_bounds(
        &settings_read.planner_workday_start,
        &settings_read.planner_workday_end,
    );
    let default_capacity = settings_read.planner_daily_capacity_minutes;
    let meetings_count = settings_read.planner_calendar_counts_against_capacity;
    let calendar_subs = settings_read.planner_calendar_subscriptions.clone();
    drop(settings_read);

    // All-day events render as a banner, not blocks — they exist only in the
    // calendar cache, never as TimeBlocks. The memo re-reads the cache when
    // the planner state or the subscription set changes (sync passes write the
    // planner signal), keeping per-render cost at a clone, not a SQLite query.
    let all_day_memo = use_memo(move || {
        let enabled: std::collections::HashSet<String> = settings
            .read()
            .planner_calendar_subscriptions
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect();
        let _ = planner.read().blocks.len(); // re-read after a sync pass lands
        let events = store::load_calendar_events(None).unwrap_or_else(|e| {
            tracing::error!("planner: failed to load calendar cache: {}", e);
            Vec::new()
        });
        all_day_events_for(&events, &enabled, today())
    });
    let all_day_events: Vec<model::CalendarEvent> = all_day_memo.read().clone();

    let state = planner.read();
    let mut capacity = model::measure_capacity_full(
        &state.todos,
        &state.blocks,
        &state.focus_sessions,
        day,
        state.capacity_for(day, default_capacity),
        meetings_count,
    );
    // Pro surfaces: the agent lane (+ its summary suffix) and the strategic
    // readout. Recording is never gated — the rows exist either way; a Free
    // build simply doesn't display the split.
    let pro = crate::entitlement::pro_active();
    let strategic_line: Option<String> = if pro {
        let person_today = model::person_minutes_on(&state.focus_sessions, day);
        let agent_today = model::agent_minutes_on(&state.focus_sessions, day);
        let (gap_count, gap_minutes) =
            model::strategic_gaps(&state.focus_sessions, (day_start, day_end), day);
        (person_today > 0 || agent_today > 0).then(|| {
            format!(
                "{} person · {} agent · {}x strategic gaps · {}",
                model::format_minutes(person_today),
                model::format_minutes(agent_today),
                gap_count,
                model::format_minutes(gap_minutes),
            )
        })
    } else {
        None
    };
    // Fleet time joins the agent lane: external Claude Code sessions'
    // active minutes today, from the fleet's own tables/state — a distinct
    // source from in-app agent focus sessions, summed only for display.
    // `agent_lane_minutes` zeroes the whole lane for Free builds (the lane
    // is a Pro surface; recording is never gated).
    let fleet_today = if pro {
        crate::fleet::fleet_agent_minutes_on(&fleet_state.read(), day, Utc::now())
    } else {
        0
    };
    capacity.agent_minutes =
        crate::fleet::agent_lane_minutes(pro, capacity.agent_minutes, fleet_today);
    let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
    // Open but not yet on the ruler — the "still to place" pool. Unestimated
    // todos belong here too (U2.3): placing one uses the default block length
    // and stamps it as the estimate, so lacking an estimate must not hide the
    // work from the timeline.
    let unblocked: Vec<Todo> = {
        let blocked_todo_ids: Vec<&str> =
            blocks.iter().filter_map(|b| b.todo_id.as_deref()).collect();
        views::in_view(&state.todos, TodoView::Today, day)
            .into_iter()
            .filter(|t| !blocked_todo_ids.contains(&t.id.as_str()))
            .cloned()
            .collect()
    };
    // Linked blocks render their todo's *current* title (the copy stored on
    // the block goes stale the moment the todo is renamed) and show a done
    // treatment once the todo is closed.
    let linked_info: std::collections::HashMap<String, (String, bool, bool)> = blocks
        .iter()
        .filter_map(|b| {
            let tid = b.todo_id.as_ref()?;
            let t = state.todo(tid)?;
            Some((
                b.id.clone(),
                (
                    t.title.clone(),
                    t.status.is_closed(),
                    t.status == TodoStatus::InProgress,
                ),
            ))
        })
        .collect();
    drop(state);

    // Blocks the ruler culls (fully outside the workday window) still exist
    // and still count — first-fit overflow is deliberately honest — so they
    // get a strip of their own instead of rendering nowhere. External blocks
    // are excluded: the strip carries select/delete affordances that must not
    // exist for read-only mirrored events, so they get a compact count
    // indicator on the ruler instead.
    let off_window_blocks: Vec<(TimeBlock, OffWindow)> = blocks
        .iter()
        .filter(|b| !matches!(b.source, BlockSource::External { .. }))
        .filter_map(|b| {
            off_window(
                minutes_in_local_day(&b.start),
                minutes_in_local_day(&b.end),
                day_start,
                day_end,
            )
            .map(|side| (b.clone(), side))
        })
        .collect();

    // External events entirely outside the workday window: counted, not drawn.
    let (external_earlier, external_later) = blocks
        .iter()
        .filter(|b| matches!(b.source, BlockSource::External { .. }))
        .fold((0usize, 0usize), |(e, l), b| {
            match off_window(
                minutes_in_local_day(&b.start),
                minutes_in_local_day(&b.end),
                day_start,
                day_end,
            ) {
                Some(OffWindow::Early) => (e + 1, l),
                Some(OffWindow::After) => (e, l + 1),
                None => (e, l),
            }
        });
    let outside_label = fmt_outside_counts(external_earlier, external_later);

    let timeline_height = (day_end - day_start) as f64 / 60.0 * PX_PER_HOUR;
    let now_line_top = now_line_offset(minutes_in_local_day(&Utc::now()), day_start, day_end);
    let hours: Vec<u32> = (day_start / 60..=day_end / 60)
        .filter(|h| h * 60 >= day_start && h * 60 <= day_end)
        .collect();

    // A block dropped on the calendar IS planned work: it creates a todo
    // scheduled for the day (estimate = block length) plus the linked block.
    // A bare, todo-less block would be invisible to the Today list and the
    // capacity math — the timeline and the list must stay one model.
    let block_minutes = settings.peek().planner_default_block_minutes.max(15);
    let mut create_block_at = move |minutes: u32| {
        let start_min = snap_to_quarter(minutes.clamp(day_start, day_end.saturating_sub(15)));

        let mut todo = Todo::new("Focus", planner.peek().next_sort_order());
        // Anytime, not Inbox: this todo is born triaged — clearing its date
        // later should not dump it back into the capture queue.
        todo.bucket = TodoBucket::Anytime;
        todo.scheduled_for = Some(day);
        todo.estimate_minutes = Some(block_minutes);

        let block = TimeBlock {
            id: uuid::Uuid::new_v4().to_string(),
            todo_id: Some(todo.id.clone()),
            title: todo.title.clone(),
            start: local_minutes_to_utc(day, start_min),
            end: local_minutes_to_utc(day, start_min + block_minutes),
            source: BlockSource::Manual,
        };

        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save block todo {}: {}", todo.id, e);
        }
        persist_block(&block);
        selected_block.set(Some(block.id.clone()));
        {
            let mut state = planner.write();
            state.upsert_todo(todo);
            state.blocks.push(block);
        }
    };

    // Auto-placement's busy set. Non-busy external blocks (free / focus-time
    // mirrors) are NOT busy — tasks may be placed over them.
    let busy: Vec<(u32, u32)> = blocks
        .iter()
        .filter(|b| !matches!(b.source, BlockSource::External { busy: false, .. }))
        .map(|b| {
            (
                minutes_in_local_day(&b.start).max(0) as u32,
                minutes_in_local_day(&b.end).max(0) as u32,
            )
        })
        .collect();

    // Overlap columns for the ruler: computed from the *stored* positions of
    // every block on the day (a mid-drag preview keeps its column — width
    // reflow mid-drag would jitter). Keyed by block id.
    let overlap_columns = assign_overlap_columns(
        &blocks
            .iter()
            .map(|b| {
                (
                    b.id.clone(),
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect::<Vec<_>>(),
    );

    // Timeline drag: (block id, mode, anchor screen-y, orig start, orig end).
    // The preview holds the snapped (start, end) under the cursor; because it
    // only changes when the drag crosses a quarter-hour boundary, re-renders
    // are throttled to grid crossings for free.
    let mut block_drag =
        use_signal(|| Option::<(String, BlockDragMode, f64, u32, u32)>::None);
    let mut drag_preview = use_signal(|| Option::<(u32, u32)>::None);

    let mut commit_block_drag = move || {
        let Some((id, mode, _, orig_s, orig_e)) = block_drag.peek().clone() else {
            return;
        };
        let preview = *drag_preview.peek();
        block_drag.set(None);
        drag_preview.set(None);

        match preview {
            // The mouse never left the starting quarter: that's a click, and a
            // click on a block means select/deselect (the pre-drag behaviour).
            None => {
                if mode == BlockDragMode::Move {
                    let current = selected_block.peek().clone();
                    selected_block.set(if current.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id)
                    });
                }
            }
            Some((s, e)) if (s, e) == (orig_s, orig_e) => {}
            Some((s, e)) => {
                let mut linked_todo: Option<String> = None;
                {
                    let mut state = planner.write();
                    if let Some(b) = state.blocks.iter_mut().find(|b| b.id == id) {
                        b.start = local_minutes_to_utc(day, s);
                        b.end = local_minutes_to_utc(day, e);
                        if mode == BlockDragMode::ResizeEnd {
                            linked_todo = b.todo_id.clone();
                        }
                    }
                }
                if let Some(b) = planner.peek().blocks.iter().find(|b| b.id == id) {
                    persist_block(b);
                }
                // Resizing the block IS re-estimating the work: the capacity
                // bar follows the calendar, not a stale estimate.
                if let Some(tid) = linked_todo {
                    let minutes = e.saturating_sub(s).max(15);
                    mutate_todo(planner, &tid, |t| t.estimate_minutes = Some(minutes));
                }
                selected_block.set(Some(id));
            }
        }
    };

    // Snapshot for the render pass: which block is previewing where.
    let preview_for: Option<(String, u32, u32)> = block_drag
        .read()
        .as_ref()
        .and_then(|(id, _, _, _, _)| (*drag_preview.read()).map(|(s, e)| (id.clone(), s, e)));

    rsx! {
        div {
            class: "h-full w-full flex flex-col gap-4 overflow-y-auto border-l border-subtle bg-section p-4",
            CapacityBar {
                planned: capacity.planned_minutes,
                done: capacity.done_minutes,
                capacity: capacity.capacity_minutes,
                agent: capacity.agent_minutes,
                summary: capacity.summary(),
            }
            // Strategic-time readout v1 (Pro): person vs agent focus totals
            // and the usable gaps between person sittings in the workday.
            if let Some(line) = strategic_line {
                p { class: "text-xs text-fg-muted", "{line}" }
            }

            // All-day events: a thin banner row above the timeline. These are
            // cached calendar events, never TimeBlocks — a 24h block would
            // wreck the ruler geometry.
            if !all_day_events.is_empty() {
                div {
                    class: "flex flex-wrap gap-1",
                    for ev in all_day_events {
                        {
                            let color = subscription_color(&calendar_subs, Some(&ev.subscription_id))
                                .unwrap_or_else(|| "#94a3b8".to_string());
                            let url = ev.url.clone();
                            let has_url = url.is_some();
                            rsx! {
                                div {
                                    key: "allday-{ev.subscription_id}-{ev.uid}",
                                    class: if has_url {
                                        "flex min-w-0 items-center gap-1.5 rounded-full bg-input px-2 py-0.5 cursor-pointer hover:bg-card"
                                    } else {
                                        "flex min-w-0 items-center gap-1.5 rounded-full bg-input px-2 py-0.5"
                                    },
                                    title: if has_url { "All-day event — click to open" } else { "All-day event" },
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        if let Some(url) = &url {
                                            if let Err(e) = open::that_detached(url) {
                                                tracing::error!("planner: failed to open event link: {}", e);
                                            }
                                        }
                                    },
                                    span {
                                        class: "h-2 w-2 shrink-0 rounded-full",
                                        style: "background-color: {color};",
                                    }
                                    span { class: "min-w-0 truncate text-[11px] text-fg-muted", "{ev.title}" }
                                    if has_url {
                                        Icon { width: 10, height: 10, icon: fi_icons::FiExternalLink }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Hour-ruled timeline. All geometry is inline style — Tailwind
            // purges computed class names, so pixels never ride in classes.
            div {
                class: "relative rounded border border-faint bg-app",
                style: "height: {timeline_height}px;",
                onclick: move |evt| {
                    let y = evt.data.element_coordinates().y;
                    let minutes = day_start + (y / PX_PER_HOUR * 60.0).max(0.0) as u32;
                    create_block_at(minutes);
                },
                // External events the window can't show at all, as an honest
                // count. Non-interactive so it never eats a create-block click.
                if let Some(label) = outside_label {
                    span {
                        class: "pointer-events-none absolute right-1 top-0.5 z-10 rounded-full bg-input px-2 py-0.5 text-[10px] text-fg-muted",
                        "{label}"
                    }
                }
                for hour in hours {
                    {
                        let top = (hour * 60 - day_start) as f64 / 60.0 * PX_PER_HOUR;
                        rsx! {
                            div {
                                key: "hr-{hour}",
                                class: "pointer-events-none absolute left-0 right-0 border-t border-faint",
                                style: "top: {top}px;",
                                span {
                                    class: "pointer-events-none absolute left-1 -top-0.5 text-[10px] leading-none text-fg-muted",
                                    "{hour:02}:00"
                                }
                            }
                        }
                    }
                }
                // Current-time line (this rail IS today). Complementary warm
                // orange against the cool block palette; hidden outside the
                // workday window. Non-interactive so create-block clicks and
                // drags pass straight through it.
                if let Some(top) = now_line_top {
                    div {
                        class: "pointer-events-none absolute left-0 right-0 z-20",
                        style: "top: {top}px;",
                        div {
                            style: "position: absolute; left: 40px; right: 0; border-top: 2px solid {NOW_LINE_COLOR};",
                        }
                        div {
                            style: "position: absolute; left: 36px; top: -3px; width: 8px; height: 8px; border-radius: 9999px; background: {NOW_LINE_COLOR};",
                        }
                    }
                }
                // Blocks fully outside the workday window are filtered before
                // rsx so the loop body positions unconditionally. External
                // blocks carry clamp flags for the dashed-edge cue; manual and
                // auto blocks keep the plain geometry (their off-window home
                // is the strip below the ruler).
                for (block, s_min, e_min, geo) in blocks.iter().filter_map(|b| {
                    // A block mid-drag renders at its previewed position, not
                    // its stored one.
                    let (s_min, e_min) = match &preview_for {
                        Some((pid, ps, pe)) if *pid == b.id => (*ps as i64, *pe as i64),
                        _ => (minutes_in_local_day(&b.start), minutes_in_local_day(&b.end)),
                    };
                    let geo = if matches!(b.source, BlockSource::External { .. }) {
                        clamped_block_geometry(s_min, e_min, day_start, day_end)
                    } else {
                        block_geometry(s_min, e_min, day_start, day_end).map(|(top, height)| {
                            ClampedGeometry { top, height, clamped_top: false, clamped_bottom: false }
                        })
                    };
                    geo.map(|geo| (b, s_min, e_min, geo))
                }) {
                    {
                        let is_external = matches!(block.source, BlockSource::External { .. });
                        // Subscription tint + deep link for mirrored calendar
                        // events. A block whose subscription is unknown (legacy
                        // or just removed) keeps the muted fallback style.
                        let (ext_color, ext_url, ext_busy, ext_tentative) = match &block.source {
                            BlockSource::External { subscription_id, url, busy, tentative, .. } => (
                                subscription_color(&calendar_subs, subscription_id.as_deref()),
                                url.clone(),
                                *busy,
                                *tentative,
                            ),
                            _ => (None, None, true, false),
                        };
                        let has_url = ext_url.is_some();
                        let click_url = ext_url.clone();
                        let ClampedGeometry { top, height, clamped_top, clamped_bottom } = geo;
                        let mut block_style = if is_external {
                            external_block_style(top, height, ext_color.as_deref(), clamped_top, clamped_bottom, ext_busy, ext_tentative)
                        } else {
                            format!("top: {top}px; height: {height}px;")
                        };
                        // Overlapping blocks share the rail width side by side;
                        // a lone block keeps the classic full-width footprint.
                        let (col, count) = overlap_columns
                            .get(&block.id)
                            .copied()
                            .unwrap_or((0, 1));
                        block_style.push_str(&overlap_column_style(col, count));
                        let is_selected = selected_block.read().as_deref() == Some(block.id.as_str());
                        let is_dragging_this = preview_for.as_ref().is_some_and(|(pid, _, _)| *pid == block.id);
                        let (display_title, is_done, is_focus_block) = linked_info
                            .get(&block.id)
                            .map(|(t, c, f)| (t.clone(), *c, *f))
                            .unwrap_or_else(|| (block.title.clone(), false, false));
                        let delete_id = block.id.clone();
                        let drag_id = block.id.clone();
                        let resize_id = block.id.clone();
                        // Original minutes anchor the drag math; labels follow
                        // the (possibly previewed) rendered position.
                        let orig_s = minutes_in_local_day(&block.start).max(0) as u32;
                        let orig_e = minutes_in_local_day(&block.end).max(0) as u32;
                        let start_label = fmt_hhmm(s_min.max(0) as u32);
                        let end_label = fmt_hhmm(e_min.max(0) as u32);
                        // Three-way conditional lives outside rsx: the macro's
                        // conditional-attribute form only supports if/else.
                        // Horizontal placement (left/width) arrives via the
                        // inline overlap-column style, never via classes.
                        let block_class = if is_external {
                            if ext_color.is_some() {
                                // Fill and left bar arrive via inline style.
                                "absolute overflow-hidden rounded border border-faint px-1.5 py-0.5 text-fg-muted"
                            } else {
                                "absolute overflow-hidden rounded border border-faint bg-input px-1.5 py-0.5 text-fg-muted"
                            }
                        } else if is_selected {
                            "absolute overflow-hidden rounded border border-subtle bg-btn-primary-hover px-1.5 py-0.5 text-fg cursor-grab"
                        } else {
                            "absolute overflow-hidden rounded border border-subtle bg-btn-primary px-1.5 py-0.5 text-fg cursor-grab"
                        };
                        rsx! {
                                div {
                                    key: "{block.id}",
                                    class: "{block_class}",
                                    class: if is_dragging_this { "z-10 ring-1 ring-primary-400" },
                                    class: if is_focus_block { "ring-1 ring-primary-300" },
                                    class: if is_external && has_url { "cursor-pointer" },
                                    // Tentative invitations announce themselves in
                                    // the hover tooltip; the dashed left bar is the
                                    // at-a-glance cue.
                                    title: if is_external && ext_tentative { "(tentative) {display_title}" },
                                    style: "{block_style}",
                                    // Selection also runs through the drag: a
                                    // press that never leaves its quarter-hour
                                    // commits as a click in commit_block_drag.
                                    onmousedown: move |evt| {
                                        // Mirrored calendar events are read-only:
                                        // never start a drag, and never let the
                                        // press reach the timeline surface.
                                        evt.stop_propagation();
                                        if is_external {
                                            return;
                                        }
                                        block_drag.set(Some((
                                            drag_id.clone(),
                                            BlockDragMode::Move,
                                            evt.data.screen_coordinates().y,
                                            orig_s,
                                            orig_e,
                                        )));
                                    },
                                    // Only external blocks handle click (deep
                                    // link); stopping propagation also keeps
                                    // the timeline's create-block onclick from
                                    // minting a phantom block under the event.
                                    onclick: move |evt| {
                                        if is_external {
                                            evt.stop_propagation();
                                            if let Some(url) = &click_url {
                                                if let Err(e) = open::that_detached(url) {
                                                    tracing::error!("planner: failed to open event link: {}", e);
                                                }
                                            }
                                        }
                                    },
                                    div {
                                        class: "flex items-start justify-between gap-1",
                                        p {
                                            class: "truncate text-xs font-medium leading-4",
                                            class: if is_done { "line-through opacity-60" },
                                            "{display_title}"
                                        }
                                        if is_external && has_url {
                                            span {
                                                class: "shrink-0 opacity-70",
                                                title: "Open in calendar",
                                                Icon { width: 10, height: 10, icon: fi_icons::FiExternalLink }
                                            }
                                        }
                                        if is_selected && !is_external {
                                            button {
                                                class: "shrink-0 text-red-400 hover:text-red-300",
                                                title: "Delete block",
                                                onmousedown: move |evt| evt.stop_propagation(),
                                                onclick: move |evt| {
                                                    evt.stop_propagation();
                                                    if let Err(e) = store::delete_block(&delete_id) {
                                                        tracing::error!(
                                                            "planner: failed to delete block {}: {}",
                                                            delete_id, e
                                                        );
                                                    }
                                                    planner.write().blocks.retain(|b| b.id != delete_id);
                                                    selected_block.set(None);
                                                },
                                                Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                            }
                                        }
                                    }
                                    if height >= 30.0 {
                                        p { class: "text-[10px] opacity-80", "{start_label}–{end_label}" }
                                    }
                                    if !is_external {
                                        div {
                                            class: "absolute inset-x-0 bottom-0 h-1.5 cursor-ns-resize",
                                            onmousedown: move |evt| {
                                                evt.stop_propagation();
                                                block_drag.set(Some((
                                                    resize_id.clone(),
                                                    BlockDragMode::ResizeEnd,
                                                    evt.data.screen_coordinates().y,
                                                    orig_s,
                                                    orig_e,
                                                )));
                                            },
                                        }
                                    }
                                }
                        }
                    }
                }
            }

            // Off-window strip: the visible home for blocks the ruler culls.
            // Same select/delete affordances as ruler blocks, minus dragging.
            if !off_window_blocks.is_empty() {
                div {
                    class: "flex flex-col gap-1",
                    for (block, side) in off_window_blocks {
                        {
                            let is_selected =
                                selected_block.read().as_deref() == Some(block.id.as_str());
                            let display_title = linked_info
                                .get(&block.id)
                                .map(|(t, _, _)| t.clone())
                                .unwrap_or_else(|| block.title.clone());
                            let start_label =
                                fmt_hhmm(minutes_in_local_day(&block.start).max(0) as u32);
                            let end_label =
                                fmt_hhmm(minutes_in_local_day(&block.end).max(0) as u32);
                            let side_label = match side {
                                OffWindow::Early => "Early",
                                OffWindow::After => "After hours",
                            };
                            let select_id = block.id.clone();
                            let delete_id = block.id.clone();
                            rsx! {
                                div {
                                    key: "{block.id}",
                                    class: if is_selected {
                                        "flex items-center gap-2 rounded bg-card px-2 py-1.5 ring-1 ring-primary-400 cursor-pointer"
                                    } else {
                                        "flex items-center gap-2 rounded bg-card px-2 py-1.5 cursor-pointer"
                                    },
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        let current = selected_block.peek().clone();
                                        selected_block.set(
                                            if current.as_deref() == Some(select_id.as_str()) {
                                                None
                                            } else {
                                                Some(select_id.clone())
                                            },
                                        );
                                    },
                                    span {
                                        class: "shrink-0 rounded-full bg-input px-2 py-0.5 text-[10px] text-fg-muted",
                                        "{side_label}"
                                    }
                                    span {
                                        class: "shrink-0 text-[10px] text-fg-muted",
                                        "{start_label}–{end_label}"
                                    }
                                    span { class: "flex-1 min-w-0 truncate text-xs text-fg", "{display_title}" }
                                    if is_selected {
                                        button {
                                            class: "shrink-0 text-red-400 hover:text-red-300",
                                            title: "Delete block",
                                            onclick: move |evt| {
                                                evt.stop_propagation();
                                                if let Err(e) = store::delete_block(&delete_id) {
                                                    tracing::error!(
                                                        "planner: failed to delete block {}: {}",
                                                        delete_id, e
                                                    );
                                                }
                                                planner.write().blocks.retain(|b| b.id != delete_id);
                                                selected_block.set(None);
                                            },
                                            Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Fullscreen capture surface while a block drag is live — the same
            // convention as the column resizers. Snapping happens in the pure
            // helpers; commit persists exactly once on release.
            //
            // This MUST be a sibling of the timeline container, never a child:
            // after mousedown-on-block + mouseup-on-overlay the browser
            // synthesizes a click on their nearest common ancestor. As a child,
            // that ancestor was the container itself, whose onclick is
            // create_block_at — every drag also spawned a phantom "Focus"
            // block at the release point. As a sibling, the common ancestor is
            // this rail's root, which has no click handler.
            if block_drag.read().is_some() {
                div {
                    class: if block_drag.read().as_ref().is_some_and(|(_, m, _, _, _)| *m == BlockDragMode::ResizeEnd) {
                        "fixed inset-0 z-50 cursor-ns-resize"
                    } else {
                        "fixed inset-0 z-50 cursor-grabbing"
                    },
                    onmousemove: move |evt| {
                        let Some((_, mode, anchor_y, orig_s, orig_e)) = block_drag.peek().clone() else {
                            return;
                        };
                        let dy = evt.data.screen_coordinates().y - anchor_y;
                        let delta_min = (dy / PX_PER_HOUR * 60.0).round() as i64;
                        let next = match mode {
                            BlockDragMode::Move => {
                                dragged_move(orig_s, orig_e, delta_min, day_start, day_end)
                            }
                            BlockDragMode::ResizeEnd => {
                                dragged_resize(orig_s, orig_e, delta_min, day_end)
                            }
                        };
                        // Don't promote an unmoved press into a drag: the
                        // first preview is only set once the position
                        // actually differs, so commit can tell click from
                        // drag by preview presence.
                        let current = *drag_preview.peek();
                        if current.is_none() && next == (orig_s, orig_e) {
                            return;
                        }
                        if current != Some(next) {
                            drag_preview.set(Some(next));
                        }
                    },
                    onmouseup: move |_| commit_block_drag(),
                    onmouseleave: move |_| commit_block_drag(),
                }
            }
            p { class: "text-[11px] text-fg-muted", "Click an empty slot for a new block · drag a block to move it · drag its bottom edge to resize." }

            if !unblocked.is_empty() {
                div {
                    class: "flex flex-col gap-1",
                    h3 { class: "text-xs font-medium uppercase tracking-wide text-fg-muted", "Not on the timeline" }
                    for todo in unblocked {
                        {
                            let todo_id = todo.id.clone();
                            let title = todo.title.clone();
                            let estimate = todo.estimate_minutes;
                            // Unestimated work is placed at the default block
                            // length, which then becomes its estimate below.
                            let duration = estimate.unwrap_or(block_minutes).max(15);
                            let busy = busy.clone();
                            rsx! {
                                div {
                                    key: "{todo.id}",
                                    class: "flex items-center gap-2 rounded bg-card px-2 py-1.5",
                                    span { class: "flex-1 min-w-0 truncate text-xs text-fg", "{todo.title}" }
                                    if let Some(m) = estimate {
                                        span { class: "shrink-0 text-[10px] text-fg-muted", "{model::format_minutes(m)}" }
                                    } else {
                                        span {
                                            class: "shrink-0 text-[10px] text-amber-400/80",
                                            title: "No estimate — placing uses the default block length and sets it as the estimate",
                                            "{model::format_minutes(duration)}?"
                                        }
                                    }
                                    button {
                                        class: "shrink-0 rounded p-0.5 text-fg-muted hover:bg-input hover:text-fg",
                                        title: "Add to timeline at the first free slot",
                                        onclick: move |_| {
                                            // Auto-placement never lands before now on today
                                            // (this rail IS today): search from the current
                                            // minute, snapped forward to the quarter grid.
                                            let now_min =
                                                minutes_in_local_day(&Utc::now()).max(0) as u32;
                                            let search_from = placement_search_start(
                                                day_start,
                                                now_min,
                                                day == today(),
                                            );
                                            let start_min = first_free_slot(&busy, search_from, duration);
                                            let block = TimeBlock {
                                                id: uuid::Uuid::new_v4().to_string(),
                                                todo_id: Some(todo_id.clone()),
                                                title: title.clone(),
                                                start: local_minutes_to_utc(day, start_min),
                                                end: local_minutes_to_utc(day, start_min + duration),
                                                source: BlockSource::Auto,
                                            };
                                            persist_block(&block);
                                            // Timeboxing the work today IS scheduling it
                                            // today: a todo rolled in from an earlier day
                                            // must not keep its stale date once placed.
                                            let (changed, pruned) = {
                                                let mut state = planner.write();
                                                state.blocks.push(block);
                                                state.schedule_todo_on(&todo_id, day, Utc::now())
                                            };
                                            if changed {
                                                persist_todo(&planner, &todo_id);
                                            }
                                            for b in &pruned {
                                                if let Err(e) = store::delete_block(&b.id) {
                                                    tracing::error!(
                                                        "planner: failed to delete stale block {}: {}",
                                                        b.id, e
                                                    );
                                                }
                                            }
                                            // Estimate ↔ block sync: the block was
                                            // born at `duration`, so stamping it as
                                            // the estimate needs no resize.
                                            if estimate.is_none() {
                                                mutate_todo(planner, &todo_id, |t| {
                                                    t.estimate_minutes = Some(duration)
                                                });
                                            }
                                        },
                                        Icon { width: 14, height: 14, icon: fi_icons::FiPlus }
                                    }
                                }
                            }
                        }
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
    use crate::settings::{CalendarSource, CalendarSubscription};
    use chrono::{DateTime, Duration, TimeZone};
    use std::collections::HashSet;

    fn sub(id: &str, color: &str) -> CalendarSubscription {
        CalendarSubscription {
            id: id.into(),
            name: format!("Sub {}", id),
            color: color.into(),
            enabled: true,
            source: CalendarSource::Ics {},
        }
    }

    /// A UTC instant at `hour`:00 **local** time on `day` — matches how the
    /// planner builds instants, so tests stay timezone-independent.
    fn local_instant(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cal_event(sub: &str, uid: &str, start: DateTime<Utc>, end: DateTime<Utc>, all_day: bool) -> model::CalendarEvent {
        model::CalendarEvent {
            uid: uid.into(),
            subscription_id: sub.into(),
            title: format!("Event {}", uid),
            start,
            end,
            all_day,
            url: None,
            location: None,
            busy: true,
            tentative: false,
        }
    }

    #[test]
    fn clamped_geometry_matches_block_geometry_when_fully_inside() {
        // 09:30–10:15 in a 09:00–17:00 window: same pixels, no clamp flags.
        let geo = clamped_block_geometry(570, 615, 540, 1020).unwrap();
        assert_eq!((geo.top, geo.height), block_geometry(570, 615, 540, 1020).unwrap());
        assert!(!geo.clamped_top && !geo.clamped_bottom);
    }

    #[test]
    fn clamped_geometry_flags_the_clamped_side() {
        // 08:00–10:00 against a 09:00 open: clamped at the top only.
        let geo = clamped_block_geometry(480, 600, 540, 1020).unwrap();
        assert_eq!((geo.top, geo.height), block_geometry(480, 600, 540, 1020).unwrap());
        assert!(geo.clamped_top && !geo.clamped_bottom);
        assert_eq!(geo.top, 0.0);

        // 16:30–18:00 against a 17:00 close: clamped at the bottom only.
        let geo = clamped_block_geometry(990, 1080, 540, 1020).unwrap();
        assert!(!geo.clamped_top && geo.clamped_bottom);

        // 08:00–18:00 spans the whole window: clamped on both sides.
        let geo = clamped_block_geometry(480, 1080, 540, 1020).unwrap();
        assert!(geo.clamped_top && geo.clamped_bottom);
        assert_eq!(geo.top, 0.0);
        assert_eq!(geo.height, 8.0 * PX_PER_HOUR);
    }

    #[test]
    fn clamped_geometry_excludes_fully_outside_and_zero_length() {
        // Entirely before the window / entirely after: not drawn (counted
        // instead, via off_window).
        assert_eq!(clamped_block_geometry(420, 540, 540, 1020), None);
        assert_eq!(clamped_block_geometry(1020, 1080, 540, 1020), None);
        assert_eq!(off_window(420, 540, 540, 1020), Some(OffWindow::Early));
        assert_eq!(off_window(1020, 1080, 540, 1020), Some(OffWindow::After));
        // Zero- and negative-length guard.
        assert_eq!(clamped_block_geometry(600, 600, 540, 1020), None);
        assert_eq!(clamped_block_geometry(600, 570, 540, 1020), None);
    }

    #[test]
    fn fmt_outside_counts_reads_naturally() {
        assert_eq!(fmt_outside_counts(0, 0), None);
        assert_eq!(fmt_outside_counts(2, 0), Some("2 earlier".into()));
        assert_eq!(fmt_outside_counts(0, 1), Some("1 later".into()));
        assert_eq!(fmt_outside_counts(2, 1), Some("2 earlier · 1 later".into()));
    }

    #[test]
    fn subscription_color_resolves_known_ids_and_falls_back() {
        let subs = vec![sub("s1", "#4f9cf9"), sub("s2", "#34d399")];
        assert_eq!(subscription_color(&subs, Some("s2")), Some("#34d399".into()));
        assert_eq!(subscription_color(&subs, Some("gone")), None);
        assert_eq!(subscription_color(&subs, None), None);
        assert_eq!(subscription_color(&[], Some("s1")), None);
    }

    #[test]
    fn external_block_style_carries_tint_and_clamp_cues() {
        let plain = external_block_style(24.0, 36.0, None, false, false, true, false);
        assert_eq!(plain, "top: 24px; height: 36px;");

        let tinted = external_block_style(0.0, 48.0, Some("#34d399"), true, false, true, false);
        assert!(tinted.contains("border-left: 3px solid #34d399;"));
        assert!(tinted.contains("background-color: #34d39926;"), "{}", tinted);
        assert!(tinted.contains("border-top-style: dashed;"));
        assert!(!tinted.contains("border-bottom-style"));
        assert!(!tinted.contains("opacity"), "busy blocks keep full opacity");

        let bottom = external_block_style(0.0, 48.0, None, false, true, true, false);
        assert!(bottom.contains("border-bottom-style: dashed;"));
        assert!(!bottom.contains("border-top-style"));
    }

    #[test]
    fn non_busy_external_block_style_reads_as_non_blocking() {
        let free = external_block_style(0.0, 48.0, Some("#34d399"), false, false, false, false);
        assert!(free.contains("border-left: 3px dashed #34d399;"), "{}", free);
        assert!(free.contains("background-color: #34d39914;"), "fainter fill: {}", free);
        assert!(free.contains("opacity: 0.65;"), "{}", free);

        // Colorless free block still gets the opacity cue.
        let plain_free = external_block_style(0.0, 48.0, None, false, false, false, false);
        assert!(plain_free.contains("opacity: 0.65;"), "{}", plain_free);
    }

    #[test]
    fn tentative_external_block_style_dashes_the_bar_but_keeps_the_busy_tint() {
        let maybe = external_block_style(0.0, 48.0, Some("#34d399"), false, false, true, true);
        assert!(maybe.contains("border-left: 3px dashed #34d399;"), "{}", maybe);
        assert!(
            maybe.contains("background-color: #34d39926;"),
            "tentative keeps the full busy fill: {}",
            maybe
        );
        assert!(
            !maybe.contains("opacity"),
            "tentative still blocks time — no free-style fade: {}",
            maybe
        );
    }

    #[test]
    fn all_day_partition_filters_by_flag_subscription_and_day() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let enabled: HashSet<String> = ["s1".to_string()].into();
        let midnight = local_instant(day, 0);

        let events = vec![
            // Today's all-day event on an enabled subscription: kept.
            cal_event("s1", "offsite", midnight, midnight + Duration::days(1), true),
            // Timed event: not an all-day banner.
            cal_event("s1", "standup", local_instant(day, 9), local_instant(day, 10), false),
            // Disabled subscription: dropped.
            cal_event("s2", "other", midnight, midnight + Duration::days(1), true),
            // Yesterday only (end is exclusive — it closes at today's
            // midnight and must not bleed into today).
            cal_event("s1", "yesterday", midnight - Duration::days(1), midnight, true),
            // Multi-day span covering today: kept.
            cal_event("s1", "conf", midnight - Duration::days(1), midnight + Duration::days(2), true),
        ];

        let uids: Vec<String> = all_day_events_for(&events, &enabled, day)
            .into_iter()
            .map(|e| e.uid)
            .collect();
        assert_eq!(uids, vec!["conf".to_string(), "offsite".to_string()]);
    }

    #[test]
    fn parse_hhmm_accepts_valid_and_rejects_junk() {
        assert_eq!(parse_hhmm("09:00"), Some(540));
        assert_eq!(parse_hhmm(" 17:30 "), Some(1050));
        assert_eq!(parse_hhmm("0:05"), Some(5));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("09:60"), None);
        assert_eq!(parse_hhmm("nine"), None);
        assert_eq!(parse_hhmm("09"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn now_line_appears_only_inside_the_workday_window() {
        // 09:00–17:00 window: before, at start, mid-day, at end, after.
        assert_eq!(now_line_offset(8 * 60, 540, 1020), None);
        assert_eq!(now_line_offset(540, 540, 1020), Some(0.0));
        assert_eq!(now_line_offset(12 * 60 + 30, 540, 1020), Some(3.5 * PX_PER_HOUR));
        assert_eq!(now_line_offset(1020, 540, 1020), Some(8.0 * PX_PER_HOUR));
        assert_eq!(now_line_offset(1021, 540, 1020), None);
    }

    #[test]
    fn workday_bounds_falls_back_on_bad_or_inverted_input() {
        assert_eq!(workday_bounds("08:00", "16:00"), (480, 960));
        assert_eq!(workday_bounds("garbage", "16:00"), (540, 1020));
        // End before start makes no timeline — fall back rather than invert.
        assert_eq!(workday_bounds("17:00", "09:00"), (540, 1020));
        assert_eq!(workday_bounds("09:00", "09:00"), (540, 1020));
    }

    #[test]
    fn block_geometry_positions_and_clamps() {
        // 09:30–10:15 in a 09:00–17:00 window at 48px/h.
        assert_eq!(block_geometry(570, 615, 540, 1020), Some((24.0, 36.0)));
        // Starting before the window clamps to its top.
        assert_eq!(block_geometry(480, 600, 540, 1020), Some((0.0, 48.0)));
        // Fully outside the window renders nothing.
        assert_eq!(block_geometry(300, 480, 540, 1020), None);
        assert_eq!(block_geometry(1080, 1140, 540, 1020), None);
        // Very short blocks keep a height that fits a one-line title.
        let (_, h) = block_geometry(540, 545, 540, 1020).unwrap();
        assert_eq!(h, 20.0);
    }

    #[test]
    fn off_window_classifies_exactly_what_the_ruler_culls() {
        let (ds, de) = (540, 1020); // 09:00–17:00

        // Fully before / fully after the window, edges touching included.
        assert_eq!(off_window(300, 480, ds, de), Some(OffWindow::Early));
        assert_eq!(off_window(480, 540, ds, de), Some(OffWindow::Early));
        assert_eq!(off_window(1020, 1080, ds, de), Some(OffWindow::After));
        assert_eq!(off_window(1080, 1140, ds, de), Some(OffWindow::After));

        // Anything the ruler draws (even partially) is not off-window.
        assert_eq!(off_window(570, 615, ds, de), None);
        assert_eq!(off_window(480, 600, ds, de), None);
        assert_eq!(off_window(1000, 1080, ds, de), None);

        // The complement of block_geometry: every block is drawn or stripped.
        for (s, e) in [(300, 480), (480, 600), (570, 615), (1000, 1080), (1020, 1080)] {
            assert_eq!(
                block_geometry(s, e, ds, de).is_none(),
                off_window(s, e, ds, de).is_some(),
                "({}, {}) must render in exactly one place",
                s,
                e
            );
        }
    }

    #[test]
    fn dragged_move_snaps_and_clamps() {
        // 09:30–10:15 dragged down 40min lands on the 10:00 grid line, 45m kept.
        assert_eq!(dragged_move(570, 615, 40, 540, 1020), (600, 645));
        // Dragging above the workday pins to its start.
        assert_eq!(dragged_move(570, 615, -600, 540, 1020), (540, 585));
        // Dragging below pins so the block still ends inside the day.
        assert_eq!(dragged_move(570, 615, 600, 540, 1020), (975, 1020));
        // Zero delta is the identity — a click must not look like a drag.
        assert_eq!(dragged_move(570, 615, 0, 540, 1020), (570, 615));
        // A zero-length block is treated as the 15m minimum, not a panic.
        assert_eq!(dragged_move(600, 600, 0, 540, 1020), (600, 615));
    }

    #[test]
    fn dragged_resize_keeps_a_minimum_and_stays_in_day() {
        // 09:30–10:15 pulled 30min longer.
        assert_eq!(dragged_resize(570, 615, 30, 1020), (570, 645));
        // Shrinking below 15 minutes stops at 15.
        assert_eq!(dragged_resize(570, 615, -600, 1020), (570, 585));
        // Growing past the workday stops at its end.
        assert_eq!(dragged_resize(570, 615, 600, 1020), (570, 1020));
        // Snap floors to the quarter grid.
        assert_eq!(dragged_resize(570, 615, 10, 1020), (570, 615));
    }

    #[test]
    fn fmt_hhmm_pads() {
        assert_eq!(fmt_hhmm(570), "09:30");
        assert_eq!(fmt_hhmm(0), "00:00");
        assert_eq!(fmt_hhmm(1020), "17:00");
    }

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

    // Date labels are covered by views::friendly_date's tests (U3.6): one
    // formatter, one test home.

    #[test]
    fn snap_to_quarter_rounds_down() {
        assert_eq!(snap_to_quarter(540), 540);
        assert_eq!(snap_to_quarter(554), 540);
        assert_eq!(snap_to_quarter(555), 555);
        assert_eq!(snap_to_quarter(569), 555);
    }

    #[test]
    fn first_free_slot_finds_gaps_in_order() {
        // Empty day: right at the start.
        assert_eq!(first_free_slot(&[], 540, 60), 540);
        // 09:00–10:00 busy: an hour-long task starts at 10:00.
        assert_eq!(first_free_slot(&[(540, 600)], 540, 60), 600);
        // A gap big enough between blocks is used.
        assert_eq!(first_free_slot(&[(540, 600), (660, 720)], 540, 60), 600);
        // A gap too small is skipped.
        assert_eq!(first_free_slot(&[(540, 600), (630, 720)], 540, 60), 720);
        // Unsorted input still resolves earliest-first.
        assert_eq!(first_free_slot(&[(630, 720), (540, 600)], 540, 30), 600);
        // Busy ends snap up to the next quarter hour.
        assert_eq!(first_free_slot(&[(540, 610)], 540, 30), 615);
    }

    /// Regression for Phase 5's busy-time verification: the timeline's
    /// auto-place ("Add to timeline") builds its busy set from `blocks_on`,
    /// which must include external calendar blocks (`todo_id: None`,
    /// `BlockSource::External`) — a meeting is not free time.
    #[test]
    fn auto_placement_treats_external_meetings_as_busy() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let mut state = PlannerState::default();
        state.blocks.push(TimeBlock {
            id: "own".into(),
            todo_id: None,
            title: "Deep work".into(),
            start: local_instant(day, 8),
            end: local_instant(day, 9),
            source: BlockSource::Manual,
        });
        state.blocks.push(TimeBlock {
            id: "ext".into(),
            todo_id: None,
            title: "Board meeting".into(),
            start: local_instant(day, 9),
            end: local_instant(day, 11),
            source: BlockSource::External {
                uid: "mtg-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        });

        // Exactly the component's construction (blocks_on → minute spans).
        let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
        let busy: Vec<(u32, u32)> = blocks
            .iter()
            .map(|b| {
                (
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect();
        // A 60-minute todo placed from 08:00 must skip both the own block and
        // the meeting, landing at 11:00 — not 09:00, which is mid-meeting.
        assert_eq!(first_free_slot(&busy, 8 * 60, 60), 11 * 60);
    }

    /// Mirror of the component's busy-set construction: non-busy external
    /// blocks (free / focus time) are excluded, so auto-placement may place
    /// tasks over them.
    #[test]
    fn auto_placement_ignores_non_busy_external_blocks() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let mut state = PlannerState::default();
        state.blocks.push(TimeBlock {
            id: "focus-hold".into(),
            todo_id: None,
            title: "Focus time".into(),
            start: local_instant(day, 9),
            end: local_instant(day, 11),
            source: BlockSource::External {
                uid: "ft-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: false,
                tentative: false,
            },
        });
        state.blocks.push(TimeBlock {
            id: "mtg".into(),
            todo_id: None,
            title: "Board meeting".into(),
            start: local_instant(day, 11),
            end: local_instant(day, 12),
            source: BlockSource::External {
                uid: "mtg-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        });

        let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
        let busy: Vec<(u32, u32)> = blocks
            .iter()
            .filter(|b| !matches!(b.source, BlockSource::External { busy: false, .. }))
            .map(|b| {
                (
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect();
        // The free 9–11 hold doesn't block: a 60m task from 09:00 lands right
        // at 09:00. Only the real meeting (11–12) is busy.
        assert_eq!(first_free_slot(&busy, 9 * 60, 60), 9 * 60);
        assert_eq!(first_free_slot(&busy, 11 * 60, 60), 12 * 60);
    }

    #[test]
    fn placement_search_start_clamps_today_to_now() {
        let day_start = 540; // 09:00

        // Before the workday opens: the workday start wins.
        assert_eq!(placement_search_start(day_start, 7 * 60, true), 540);
        // Mid-day: now wins, snapped FORWARD to the quarter grid (13:07 → 13:15).
        assert_eq!(placement_search_start(day_start, 13 * 60 + 7, true), 13 * 60 + 15);
        // Already on the grid: no over-snap (13:15 stays 13:15).
        assert_eq!(placement_search_start(day_start, 13 * 60 + 15, true), 13 * 60 + 15);
        // One minute past a grid line still snaps forward, never back.
        assert_eq!(placement_search_start(day_start, 541, true), 555);
        // After the workday ends: the search starts past the day end — the
        // caller's first-fit overflows honestly rather than planning fiction.
        assert_eq!(placement_search_start(day_start, 18 * 60, true), 18 * 60);
        // Future days are untouched.
        assert_eq!(placement_search_start(day_start, 18 * 60, false), 540);
        assert_eq!(placement_search_start(day_start, 0, false), 540);
    }

    // ── Overlap columns ─────────────────────────────────────────────────────

    fn cols(entries: &[(&str, u32, u32)]) -> std::collections::HashMap<String, (usize, usize)> {
        let owned: Vec<(String, u32, u32)> = entries
            .iter()
            .map(|(id, s, e)| (id.to_string(), *s, *e))
            .collect();
        assign_overlap_columns(&owned)
    }

    #[test]
    fn disjoint_blocks_keep_full_width() {
        let map = cols(&[("a", 540, 600), ("b", 660, 720), ("c", 780, 840)]);
        for id in ["a", "b", "c"] {
            assert_eq!(map[id], (0, 1), "{id} keeps the classic footprint");
        }
    }

    #[test]
    fn two_overlapping_blocks_split_in_half() {
        let map = cols(&[("a", 540, 600), ("b", 570, 630)]);
        assert_eq!(map["a"], (0, 2));
        assert_eq!(map["b"], (1, 2));
    }

    #[test]
    fn staggered_chain_shares_one_cluster() {
        // A–B overlap and B–C overlap but A–C don't: transitively one cluster.
        // A and C never coexist, so greedy reuses column 0 for C.
        let map = cols(&[("a", 540, 600), ("b", 570, 660), ("c", 620, 700)]);
        assert_eq!(map["a"], (0, 2));
        assert_eq!(map["b"], (1, 2));
        assert_eq!(map["c"], (0, 2), "C reuses A's column; the cluster needs 2");
    }

    #[test]
    fn four_concurrent_blocks_cap_at_three_columns() {
        let map = cols(&[
            ("a", 540, 660),
            ("b", 540, 660),
            ("c", 540, 660),
            ("d", 540, 660),
        ]);
        assert_eq!(map["a"], (0, 3));
        assert_eq!(map["b"], (1, 3));
        assert_eq!(map["c"], (2, 3));
        assert_eq!(map["d"], (2, 3), "overflow shares column 2 (0-based)");
    }

    #[test]
    fn touching_edges_stay_separate_clusters() {
        // end == next start does not overlap (TimeBlock::overlaps parity).
        let map = cols(&[("a", 540, 600), ("b", 600, 660)]);
        assert_eq!(map["a"], (0, 1));
        assert_eq!(map["b"], (0, 1));

        // ...even inside a mixed set: the meeting overlapping a timebox splits,
        // the back-to-back one doesn't.
        let map = cols(&[("box", 540, 600), ("mtg", 570, 630), ("next", 630, 690)]);
        assert_eq!(map["box"], (0, 2));
        assert_eq!(map["mtg"], (1, 2));
        assert_eq!(map["next"], (0, 1));
    }

    #[test]
    fn overlap_columns_skip_degenerate_spans() {
        let map = cols(&[("zero", 600, 600), ("real", 540, 660)]);
        assert!(!map.contains_key("zero"));
        assert_eq!(map["real"], (0, 1));
    }

    #[test]
    fn overlap_column_style_composes_inline_geometry() {
        // Single column: the classic full-width footprint, unchanged look.
        assert_eq!(overlap_column_style(0, 1), " left: 44px; right: 4px;");

        // Two columns: halves with a gutter, offset by the column index.
        let left = overlap_column_style(0, 2);
        assert!(left.contains("left: calc(44px + (100% - 48px) * 0.0000)"), "{left}");
        assert!(left.contains("width: calc((100% - 48px) * 0.5000 - 3px)"), "{left}");
        let right = overlap_column_style(1, 2);
        assert!(right.contains("left: calc(44px + (100% - 48px) * 0.5000)"), "{right}");

        // Three columns: thirds.
        let mid = overlap_column_style(1, 3);
        assert!(mid.contains("* 0.3333)"), "{mid}");
        assert!(mid.contains("* 0.3333 - 3px)"), "{mid}");
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

    #[test]
    fn local_minutes_round_trip() {
        let date: NaiveDate = "2026-08-12".parse().unwrap();
        let dt = local_minutes_to_utc(date, 570);
        assert_eq!(minutes_in_local_day(&dt), 570);
        // Out-of-range minutes clamp inside the day instead of panicking.
        let clamped = local_minutes_to_utc(date, 30_000);
        assert_eq!(minutes_in_local_day(&clamped), 23 * 60 + 59);
    }
}
