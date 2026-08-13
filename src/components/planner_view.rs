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
use crate::todo::model::{self, BlockSource, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoStatus};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

/// Vertical scale of the day timeline.
const PX_PER_HOUR: f64 = 48.0;

/// What the centre column is showing: a built-in view or a single project.
#[derive(Clone, Debug, PartialEq)]
enum PlannerSelection {
    View(TodoView),
    Project(String),
}

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

fn persist_todo(planner: &Signal<PlannerState>, id: &str) {
    if let Some(t) = planner.peek().todo(id) {
        if let Err(e) = store::save_todo(t) {
            tracing::error!("planner: failed to save todo {}: {}", id, e);
        }
    }
}

fn persist_block(block: &TimeBlock) {
    if let Err(e) = store::save_block(block) {
        tracing::error!("planner: failed to save block {}: {}", block.id, e);
    }
}

/// Patch one todo in state, stamp `updated_at`, and persist it.
fn mutate_todo(mut planner: Signal<PlannerState>, id: &str, f: impl FnOnce(&mut Todo)) {
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
fn cycle_estimate(current: Option<u32>) -> Option<u32> {
    match current {
        None => Some(15),
        Some(15) => Some(30),
        Some(30) => Some(60),
        Some(60) => Some(120),
        _ => None,
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

/// Human scheduled-date label: "Today", "Tomorrow", then "Fri 15 Aug".
fn scheduled_label(d: NaiveDate, today: NaiveDate) -> String {
    if d == today {
        "Today".to_string()
    } else if Some(d) == today.succ_opt() {
        "Tomorrow".to_string()
    } else {
        d.format("%a %-d %b").to_string()
    }
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
            span {
                class: if over { "shrink-0 text-xs text-red-400" } else { "shrink-0 text-xs text-fg-muted" },
                "{readout}"
            }
            div { class: "flex-1" }
            button {
                class: "shrink-0 rounded bg-btn-primary px-2 py-1 text-xs font-medium text-fg hover:bg-btn-primary-hover",
                onclick: move |_| {
                    mutate_todo(planner, &done_id, |t| t.mark_completed(Utc::now()));
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
    let day = today();

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
    let preview = crate::todo::quick_add::parse_quick_add(&draft.read(), today());

    rsx! {
        div {
            class: "px-6 pb-3",
            input {
                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
                r#type: "text",
                placeholder: "Add a to-do — ~30m · #tag · @2pm · !fri — press Enter",
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
                    if let Some(d) = preview.deadline {
                        span { class: "rounded bg-red-500/20 px-1.5 py-0.5 text-[10px] text-fg", "due {d.format(\"%-d %b\")}" }
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

    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);
    let is_today = sel == PlannerSelection::View(TodoView::Today);
    // Views whose membership doesn't already answer "when?" show the
    // scheduled date on each row: Upcoming spans arbitrary future days and a
    // project mixes scheduled with unscheduled work.
    let show_scheduled = matches!(
        sel,
        PlannerSelection::View(TodoView::Upcoming) | PlannerSelection::Project(_)
    );

    // Today gets its "This Evening" split; every other selection is one flat list.
    let (main_rows, evening_rows): (Vec<Todo>, Vec<Todo>) = if is_today {
        let sections = views::today_sections(&state.todos, day);
        (
            sections.daytime.into_iter().cloned().collect(),
            sections.evening.into_iter().cloned().collect(),
        )
    } else {
        (todos_for_selection(&state, &sel, day), Vec::new())
    };
    let is_empty = main_rows.is_empty() && evening_rows.is_empty();

    rsx! {
        div {
            class: "flex flex-col px-4",
            if is_empty {
                p { class: "px-2 py-8 text-sm text-fg-muted", "{empty_state_line(&sel)}" }
            }
            for todo in main_rows {
                // Cloned because the generated key borrows `todo.id` after the
                // prop takes ownership.
                TodoRow { key: "{todo.id}", todo: todo.clone(), in_logbook, show_scheduled }
            }
            if !evening_rows.is_empty() {
                h2 {
                    class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                    "This Evening"
                }
                for todo in evening_rows {
                    TodoRow { key: "{todo.id}", todo: todo.clone(), in_logbook: false, show_scheduled: false }
                }
            }
        }
    }
}

#[component]
fn TodoRow(todo: Todo, in_logbook: bool, show_scheduled: bool) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);

    let id = todo.id.clone();
    let day = today();
    let is_done = todo.status.is_closed();
    let in_focus = todo.status == TodoStatus::InProgress;
    let is_overdue = todo.is_overdue(day);

    // The row mirrors the timeline: if this todo is timeboxed on its
    // scheduled day, its start time appears as a chip. Scheduling metadata
    // must be legible from every visual state, not just the ruler.
    let block_time: Option<String> = todo.scheduled_for.and_then(|d| {
        let state = planner.read();
        state
            .blocks
            .iter()
            .filter(|b| {
                b.todo_id.as_deref() == Some(todo.id.as_str())
                    && b.start.with_timezone(&Local).date_naive() == d
            })
            .map(|b| b.start)
            .min()
            .map(|start| start.with_timezone(&Local).format("%H:%M").to_string())
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
            mutate_todo(planner, &id, |t| {
                if currently_done {
                    t.reopen(now);
                } else {
                    t.mark_completed(now);
                }
            });
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
            // store::delete_todo cascades to the todo's block rows;
            // remove_todo mirrors that in memory.
            if let Err(e) = store::delete_todo(&id) {
                tracing::error!("planner: failed to delete todo {}: {}", id, e);
            }
            planner.write().remove_todo(&id);
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
            .map(|d| d.with_timezone(&Local).format("%-d %b %Y").to_string())
            .unwrap_or_default()
    });

    rsx! {
        div {
            class: "group flex items-center gap-2 rounded px-2 py-1.5 hover:bg-section",
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
            // Chips
            if show_scheduled {
                if let Some(d) = todo.scheduled_for {
                    span {
                        class: "shrink-0 flex items-center gap-1 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted",
                        Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                        "{scheduled_label(d, day)}"
                    }
                }
            }
            if !in_logbook {
                if let Some(t) = block_time {
                    span {
                        class: "shrink-0 rounded-full bg-primary-700/50 border border-subtle px-2 py-0.5 text-xs text-fg",
                        title: "On the timeline",
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
                    "due {deadline.format(\"%-d %b\")}"
                }
            }
            for tag in todo.tags.iter() {
                span {
                    class: "shrink-0 rounded-full bg-input px-2 py-0.5 text-xs text-fg-muted",
                    "#{tag}"
                }
            }
            // Hover schedule menu
            if !in_logbook {
                div {
                    class: "hidden shrink-0 items-center gap-1 group-hover:flex",
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
                                    }
                                } else {
                                    let changed = planner.write().start_focus(&id, now);
                                    for cid in &changed {
                                        persist_todo(&planner, cid);
                                    }
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
                                let changed = planner.write().start_focus(&id, Utc::now());
                                for cid in &changed {
                                    persist_todo(&planner, cid);
                                }
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::StartTodoInChat(
                                        activation_seed(&todo),
                                    ),
                                ));
                            }
                        },
                        Icon { width: 14, height: 14, icon: fi_icons::FiMessageCircle }
                    }
                    button {
                        class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                        onclick: schedule(|t, day| {
                            t.scheduled_for = Some(day);
                            t.time_of_day = None;
                        }),
                        "Today"
                    }
                    button {
                        class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                        onclick: schedule(|t, day| {
                            t.scheduled_for = day.succ_opt();
                            t.time_of_day = None;
                        }),
                        "Tomorrow"
                    }
                    button {
                        class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                        title: "Move to today's This Evening group",
                        onclick: schedule(|t, day| {
                            t.scheduled_for = Some(day);
                            t.time_of_day = Some(TimeOfDay::Evening);
                        }),
                        "Evening"
                    }
                    button {
                        class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                        onclick: schedule(|t, _| {
                            t.scheduled_for = None;
                            t.time_of_day = None;
                        }),
                        "Clear date"
                    }
                    button {
                        class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                        onclick: schedule(|t, _| {
                            t.scheduled_for = None;
                            t.time_of_day = None;
                            t.bucket = TodoBucket::Someday;
                        }),
                        "Someday"
                    }
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
fn CapacityBar(planned: u32, capacity: u32, summary: String) -> Element {
    // The bar's full width represents whichever is larger, so overcommitment
    // shows as a red tail instead of silently clipping at 100%.
    let denom = planned.max(capacity).max(1) as f64;
    let within_pct = (planned.min(capacity) as f64 / denom) * 100.0;
    let over_pct = (planned.saturating_sub(capacity) as f64 / denom) * 100.0;

    rsx! {
        div {
            class: "flex flex-col gap-1.5",
            div {
                class: "h-2 w-full overflow-hidden rounded-full bg-input border border-faint flex",
                div {
                    class: "h-full bg-btn-primary",
                    style: "width: {within_pct}%;",
                }
                if over_pct > 0.0 {
                    div {
                        class: "h-full bg-red-700",
                        style: "width: {over_pct}%;",
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
    let mut selected_block = use_signal(|| Option::<String>::None);

    let day = today();
    let settings_read = settings.read();
    let (day_start, day_end) = workday_bounds(
        &settings_read.planner_workday_start,
        &settings_read.planner_workday_end,
    );
    let default_capacity = settings_read.planner_daily_capacity_minutes;
    drop(settings_read);

    let state = planner.read();
    let capacity = model::measure_capacity(
        &state.todos,
        day,
        state.capacity_for(day, default_capacity),
    );
    let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
    // Open, estimated, but not yet on the ruler — the "still to place" pool.
    let unblocked: Vec<Todo> = {
        let blocked_todo_ids: Vec<&str> =
            blocks.iter().filter_map(|b| b.todo_id.as_deref()).collect();
        views::in_view(&state.todos, TodoView::Today, day)
            .into_iter()
            .filter(|t| t.estimate_minutes.is_some() && !blocked_todo_ids.contains(&t.id.as_str()))
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

    let timeline_height = (day_end - day_start) as f64 / 60.0 * PX_PER_HOUR;
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

    let busy: Vec<(u32, u32)> = blocks
        .iter()
        .map(|b| {
            (
                minutes_in_local_day(&b.start).max(0) as u32,
                minutes_in_local_day(&b.end).max(0) as u32,
            )
        })
        .collect();

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
                capacity: capacity.capacity_minutes,
                summary: capacity.summary(),
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
                // Blocks fully outside the workday window are filtered before
                // rsx so the loop body positions unconditionally.
                for (block, s_min, e_min, top, height) in blocks.iter().filter_map(|b| {
                    // A block mid-drag renders at its previewed position, not
                    // its stored one.
                    let (s_min, e_min) = match &preview_for {
                        Some((pid, ps, pe)) if *pid == b.id => (*ps as i64, *pe as i64),
                        _ => (minutes_in_local_day(&b.start), minutes_in_local_day(&b.end)),
                    };
                    block_geometry(s_min, e_min, day_start, day_end)
                        .map(|(top, height)| (b, s_min, e_min, top, height))
                }) {
                    {
                        let is_external = matches!(block.source, BlockSource::External { .. });
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
                        let block_class = if is_external {
                            "absolute left-11 right-1 overflow-hidden rounded border border-faint bg-input px-1.5 py-0.5 text-fg-muted"
                        } else if is_selected {
                            "absolute left-11 right-1 overflow-hidden rounded border border-subtle bg-btn-primary-hover px-1.5 py-0.5 text-fg cursor-grab"
                        } else {
                            "absolute left-11 right-1 overflow-hidden rounded border border-subtle bg-btn-primary px-1.5 py-0.5 text-fg cursor-grab"
                        };
                        rsx! {
                                div {
                                    key: "{block.id}",
                                    class: "{block_class}",
                                    class: if is_dragging_this { "z-10 ring-1 ring-primary-400" },
                                    class: if is_focus_block { "ring-1 ring-primary-300" },
                                    style: "top: {top}px; height: {height}px;",
                                    // Selection also runs through the drag: a
                                    // press that never leaves its quarter-hour
                                    // commits as a click in commit_block_drag.
                                    onmousedown: move |evt| {
                                        if is_external {
                                            return; // mirrored calendar events are read-only
                                        }
                                        evt.stop_propagation();
                                        block_drag.set(Some((
                                            drag_id.clone(),
                                            BlockDragMode::Move,
                                            evt.data.screen_coordinates().y,
                                            orig_s,
                                            orig_e,
                                        )));
                                    },
                                    div {
                                        class: "flex items-start justify-between gap-1",
                                        p {
                                            class: "truncate text-xs font-medium leading-4",
                                            class: if is_done { "line-through opacity-60" },
                                            "{display_title}"
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
                            let estimate = todo.estimate_minutes.unwrap_or(30);
                            let busy = busy.clone();
                            rsx! {
                                div {
                                    key: "{todo.id}",
                                    class: "flex items-center gap-2 rounded bg-card px-2 py-1.5",
                                    span { class: "flex-1 min-w-0 truncate text-xs text-fg", "{todo.title}" }
                                    span { class: "shrink-0 text-[10px] text-fg-muted", "{model::format_minutes(estimate)}" }
                                    button {
                                        class: "shrink-0 rounded p-0.5 text-fg-muted hover:bg-input hover:text-fg",
                                        title: "Add to timeline at the first free slot",
                                        onclick: move |_| {
                                            let start_min = first_free_slot(&busy, day_start, estimate);
                                            let block = TimeBlock {
                                                id: uuid::Uuid::new_v4().to_string(),
                                                todo_id: Some(todo_id.clone()),
                                                title: title.clone(),
                                                start: local_minutes_to_utc(day, start_min),
                                                end: local_minutes_to_utc(day, start_min + estimate),
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

    #[test]
    fn scheduled_labels_read_naturally() {
        let today: NaiveDate = "2026-08-13".parse().unwrap();
        assert_eq!(scheduled_label(today, today), "Today");
        assert_eq!(scheduled_label("2026-08-14".parse().unwrap(), today), "Tomorrow");
        assert_eq!(scheduled_label("2026-08-21".parse().unwrap(), today), "Fri 21 Aug");
    }

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

    #[test]
    fn cycle_estimate_walks_the_bucket_ladder() {
        assert_eq!(cycle_estimate(None), Some(15));
        assert_eq!(cycle_estimate(Some(15)), Some(30));
        assert_eq!(cycle_estimate(Some(30)), Some(60));
        assert_eq!(cycle_estimate(Some(60)), Some(120));
        assert_eq!(cycle_estimate(Some(120)), None);
        // An off-ladder estimate (e.g. AI-set 45m) cycles back to none.
        assert_eq!(cycle_estimate(Some(45)), None);
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
