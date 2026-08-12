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
use crate::todo::model::{self, BlockSource, TimeBlock, TimeOfDay, Todo, TodoBucket};
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
            class: "relative flex flex-row h-full min-h-0 bg-app text-fg",
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
    let mut draft = use_signal(String::new);

    let mut submit = move |_| {
        let title = draft.peek().trim().to_string();
        if title.is_empty() {
            return;
        }
        let day = today();
        let mut todo = Todo::new(title, planner.peek().next_sort_order());
        // New todos land in the context being looked at, so quick-add never
        // makes work vanish into a different list.
        match selection.peek().clone() {
            PlannerSelection::View(TodoView::Today) => todo.scheduled_for = Some(day),
            PlannerSelection::View(TodoView::Upcoming) => {
                todo.scheduled_for = day.succ_opt();
            }
            PlannerSelection::View(TodoView::Anytime) => todo.bucket = TodoBucket::Anytime,
            PlannerSelection::View(TodoView::Someday) => todo.bucket = TodoBucket::Someday,
            PlannerSelection::Project(id) => {
                todo.project_id = Some(id);
                todo.bucket = TodoBucket::Anytime;
            }
            _ => {} // Inbox (and Logbook, which hides quick-add)
        }
        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save new todo: {}", e);
        }
        planner.write().upsert_todo(todo);
        draft.set(String::new());
        // The input stays mounted, so focus is retained across submits.
    };

    rsx! {
        div {
            class: "px-6 pb-3",
            input {
                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
                r#type: "text",
                placeholder: "Add a to-do — press Enter",
                value: "{draft}",
                oninput: move |evt| draft.set(evt.value()),
                onkeydown: move |evt| {
                    if evt.key() == Key::Enter {
                        evt.prevent_default();
                        submit(());
                    }
                },
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
                TodoRow { key: "{todo.id}", todo: todo.clone(), in_logbook }
            }
            if !evening_rows.is_empty() {
                h2 {
                    class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                    "This Evening"
                }
                for todo in evening_rows {
                    TodoRow { key: "{todo.id}", todo: todo.clone(), in_logbook: false }
                }
            }
        }
    }
}

#[component]
fn TodoRow(todo: Todo, in_logbook: bool) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);

    let id = todo.id.clone();
    let day = today();
    let is_done = todo.status.is_closed();
    let is_overdue = todo.is_overdue(day);

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
        move |_| mutate_todo(planner, &id, |t| patch(t, today()))
    };

    let delete = {
        let id = id.clone();
        move |_| {
            if let Err(e) = store::delete_todo(&id) {
                tracing::error!("planner: failed to delete todo {}: {}", id, e);
            }
            // Orphaned timeline blocks are removed by remove_todo; drop their
            // rows too so the store doesn't resurrect them at next launch.
            let block_ids: Vec<String> = planner
                .peek()
                .blocks
                .iter()
                .filter(|b| b.todo_id.as_deref() == Some(id.as_str()))
                .map(|b| b.id.clone())
                .collect();
            for bid in block_ids {
                if let Err(e) = store::delete_block(&bid) {
                    tracing::error!("planner: failed to delete block {}: {}", bid, e);
                }
            }
            planner.write().remove_todo(&id);
        }
    };

    let cycle = {
        let id = id.clone();
        move |_| mutate_todo(planner, &id, |t| t.estimate_minutes = cycle_estimate(t.estimate_minutes))
    };

    let completed_line = in_logbook.then(|| {
        todo.completed_at
            .map(|d| d.with_timezone(&Local).format("%-d %b %Y").to_string())
            .unwrap_or_default()
    });

    rsx! {
        div {
            class: "group flex items-center gap-2 rounded px-2 py-1.5 hover:bg-section",
            // Checkbox
            button {
                class: "shrink-0 text-fg-muted hover:text-fg transition-colors",
                title: if is_done { "Reopen" } else { "Complete" },
                onclick: toggle_done,
                if is_done {
                    Icon { width: 16, height: 16, icon: fi_icons::FiCheckCircle }
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
    drop(state);

    let timeline_height = (day_end - day_start) as f64 / 60.0 * PX_PER_HOUR;
    let hours: Vec<u32> = (day_start / 60..=day_end / 60)
        .filter(|h| h * 60 >= day_start && h * 60 <= day_end)
        .collect();

    let mut create_block_at = move |minutes: u32| {
        let start_min = snap_to_quarter(minutes.clamp(day_start, day_end.saturating_sub(15)));
        let block = TimeBlock {
            id: uuid::Uuid::new_v4().to_string(),
            todo_id: None,
            title: "Focus".to_string(),
            start: local_minutes_to_utc(day, start_min),
            end: local_minutes_to_utc(day, start_min + 30),
            source: BlockSource::Manual,
        };
        persist_block(&block);
        selected_block.set(Some(block.id.clone()));
        planner.write().blocks.push(block);
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
                for (block, top, height) in blocks.iter().filter_map(|b| {
                    block_geometry(
                        minutes_in_local_day(&b.start),
                        minutes_in_local_day(&b.end),
                        day_start,
                        day_end,
                    )
                    .map(|(top, height)| (b, top, height))
                }) {
                    {
                        let is_external = matches!(block.source, BlockSource::External { .. });
                        let is_selected = selected_block.read().as_deref() == Some(block.id.as_str());
                        let block_id = block.id.clone();
                        let delete_id = block.id.clone();
                        let start_label = block.start.with_timezone(&Local).format("%H:%M");
                        let end_label = block.end.with_timezone(&Local).format("%H:%M");
                        // Three-way conditional lives outside rsx: the macro's
                        // conditional-attribute form only supports if/else.
                        let block_class = if is_external {
                            "absolute left-11 right-1 overflow-hidden rounded border border-faint bg-input px-1.5 py-0.5 text-fg-muted"
                        } else if is_selected {
                            "absolute left-11 right-1 overflow-hidden rounded border border-subtle bg-btn-primary-hover px-1.5 py-0.5 text-fg cursor-pointer"
                        } else {
                            "absolute left-11 right-1 overflow-hidden rounded border border-subtle bg-btn-primary px-1.5 py-0.5 text-fg cursor-pointer"
                        };
                        rsx! {
                                div {
                                    key: "{block.id}",
                                    class: block_class,
                                    style: "top: {top}px; height: {height}px;",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        if is_external {
                                            return; // mirrored calendar events are read-only
                                        }
                                        let current = selected_block.peek().clone();
                                        selected_block.set(
                                            if current.as_deref() == Some(block_id.as_str()) {
                                                None
                                            } else {
                                                Some(block_id.clone())
                                            },
                                        );
                                    },
                                    div {
                                        class: "flex items-start justify-between gap-1",
                                        p { class: "truncate text-xs font-medium leading-4", "{block.title}" }
                                        if is_selected && !is_external {
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
                                    if height >= 30.0 {
                                        p { class: "text-[10px] opacity-80", "{start_label}–{end_label}" }
                                    }
                                }
                        }
                    }
                }
            }
            p { class: "text-[11px] text-fg-muted", "Click an empty slot to add a 30-minute block." }

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
                                            planner.write().blocks.push(block);
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
