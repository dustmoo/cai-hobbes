//! The Planner view — the user surface of the built-in planner (P2+P3).
//!
//! Rendered in place of the chat column (`main.rs`), never as a sidebar.
//! Three columns: a Things-style left rail, the todo list for the selected
//! view, and — on Today only — a Sunsama-style capacity bar and day timeline.
//!
//! Every mutation updates the `Signal<PlannerState>` AND persists through
//! `crate::todo::store` immediately; planner writes are single-row and cheap.
//!
//! Split across three modules so they can be edited concurrently: this file
//! keeps `PlannerView`, `FocusBar`, the left rail, shared selection types,
//! and the persistence helpers; the todo-list surfaces live in
//! `planner_rows`, and the day timeline in `planner_timeline`.

use chrono::{Local, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::settings::Settings;
use crate::todo::model::{self, TimeBlock, Todo};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

use super::planner_rows::CentreColumn;
use super::planner_timeline::TodayRail;

/// What the centre column is showing: a built-in view or a single project.
/// (The fleet lives in its own top-level tab, not in this rail.)
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlannerSelection {
    View(TodoView),
    Project(String),
}

impl PlannerSelection {
    /// The key a list's display order is saved under in
    /// `UiState.planner_list_orders`.
    pub(crate) fn list_key(&self) -> String {
        match self {
            PlannerSelection::View(v) => v.as_str().to_string(),
            PlannerSelection::Project(id) => format!("project:{id}"),
        }
    }

    /// The built-in view, or `None` for a project list — the shape
    /// `views::order_options` takes.
    pub(crate) fn view(&self) -> Option<TodoView> {
        match self {
            PlannerSelection::View(v) => Some(*v),
            PlannerSelection::Project(_) => None,
        }
    }
}

/// The list the Planner is showing, mirrored to an app-root context so
/// surfaces mounted outside `PlannerView` (the ⌘K palette) can see it.
/// `None` while the Planner is unmounted. Readers should `peek()`: the
/// palette only samples it when it opens.
#[derive(Clone, Copy)]
pub(crate) struct PlannerFocusHint(pub(crate) Signal<Option<PlannerSelection>>);

/// Which timeline block is selected on the Today rail. Provided at the
/// PlannerView root (same pattern as TodoDetailContext) so a row's timebox
/// chip can jump to Today with its block already highlighted.
#[derive(Clone, Copy)]
pub(crate) struct SelectedBlockContext(pub(crate) Signal<Option<String>>);

pub(crate) fn today() -> NaiveDate {
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

// ── Focus bar ───────────────────────────────────────────────────────────────

/// `elapsed seconds` → `"7:04"` / `"1:02:30"`. The focus bar counts in
/// seconds because a sitting you just started should visibly be running —
/// a minutes-only readout sits on "0m" for a full minute and reads as broken.
fn fmt_elapsed_clock(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Sunsama-style focus strip: visible whenever one todo is in progress, with
/// a live elapsed-vs-estimate readout and the two exits (done / stop).
#[component]
fn FocusBar() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();

    // A one-second tick while the bar is mounted: the elapsed clock shows
    // seconds, so anything slower would stutter. The bar is one strip, and it
    // only exists while something is actually in progress.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
    // Seconds resolution for the clock: banked minutes plus the live tail.
    let elapsed_secs = i64::from(focus.actual_minutes) * 60
        + focus
            .started_at
            .map(|s| (now - s).num_seconds().max(0))
            .unwrap_or(0);
    let clock = fmt_elapsed_clock(elapsed_secs);
    let of_label = match focus.estimate_minutes {
        Some(est) => format!("of {} \u{b7} {}", model::format_minutes(est), focus.title),
        None => focus.title.clone(),
    };
    let over = focus.estimate_minutes.is_some_and(|est| elapsed > est);
    let focus_id = focus.id.clone();
    let done_id = focus.id.clone();

    // Who else is working while you are: the fleet's live count and today's
    // agent minutes, stated plainly beside your own clock.
    let alongside = {
        let fleet = fleet_state.read();
        let working = fleet
            .sessions
            .values()
            .filter(|s| {
                matches!(
                    s.status,
                    crate::fleet::FleetStatus::Working
                        | crate::fleet::FleetStatus::WorkingBackground
                )
            })
            .count();
        (working > 0).then(|| {
            format!(
                "{working} agents alongside \u{b7} {}",
                model::format_minutes(crate::fleet::fleet_agent_minutes_on(
                    &fleet,
                    today(),
                    now
                ))
            )
        })
    };

    rsx! {
        div {
            class: "h-9 flex items-center gap-2.5 px-4 bg-section border-b border-subtle border-l-[3px] border-l-primary-500 text-xs",
            span { class: "h-2 w-2 shrink-0 rounded-full bg-primary-300 animate-pulse" }
            span { class: "shrink-0 font-medium text-fg", "Focusing" }
            span {
                class: if over {
                    "shrink-0 font-mono text-[11px] tabular-nums text-red-300"
                } else {
                    "shrink-0 font-mono text-[11px] tabular-nums text-primary-300"
                },
                "{clock}"
            }
            span { class: "min-w-0 truncate text-fg-muted", "{of_label}" }
            if agent_driven {
                span {
                    class: "shrink-0 rounded-full border border-faint px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-fg-muted",
                    title: "This focus session was started by the assistant",
                    "\u{25c7} agent"
                }
            }
            div { class: "flex-1" }
            if let Some(alongside) = alongside {
                span {
                    class: "shrink-0 font-mono text-[11px] tabular-nums text-fg-muted",
                    "{alongside}"
                }
            }
            button {
                class: "shrink-0 rounded bg-btn-primary px-2 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
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
                class: "shrink-0 rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input",
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

    // Mirror the selection into the root hint. peek-and-set, so the effect
    // tracks only `selection` (P-012: no read-write loop).
    let mut focus_hint = use_context::<PlannerFocusHint>().0;
    use_effect(move || {
        let current = selection.read().clone();
        if focus_hint.peek().as_ref() != Some(&current) {
            focus_hint.set(Some(current));
        }
    });
    use_drop(move || focus_hint.set(None));

    // Which todo's detail card is open (U2.1) — consumed by TodoRow's info
    // button and by the card rendered at this root.
    let detail_open = use_signal(|| Option::<String>::None);
    use_context_provider(|| crate::components::todo_detail::TodoDetailContext(detail_open));

    // Timeline block selection lives at the root (not in TodayRail) so the
    // timebox chip on a row can select the block it navigates to.
    let selected_block = use_signal(|| Option::<String>::None);
    use_context_provider(|| SelectedBlockContext(selected_block));

    // Which day the right rail is showing. Owned here (not in `TodayRail`) so
    // it survives the rail unmounting when the centre column leaves Today.
    let mut viewing_day = use_signal(today);

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
            viewing_day.set(today());
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
                    TodayRail { viewing: viewing_day }
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
