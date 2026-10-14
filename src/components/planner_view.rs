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

use crate::layout_budget;
use crate::settings::Settings;
use crate::todo::model::{self, TimeBlock, Todo};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

use super::column_divider::{ColumnDivider, ColumnSide, ColumnWidth};
use super::planner_rows::CentreColumn;
use super::planner_timeline::TodayRail;

// Symbiotic planner surfaces (SYMBIOTIC_PLANNER.md): the agent lane and its
// readouts, the decision queue, your-time goals, the weekly review and the
// agent-time panel.
pub(crate) mod agent_lane;
pub(crate) mod agent_review;
pub(crate) mod agent_review_chat;
pub(crate) mod agent_time;
pub(crate) mod decisions;
pub(crate) mod manage_projects;
pub(crate) mod review;
pub(crate) mod your_time;

/// What the centre column is showing: a built-in view or a single project.
/// (The fleet lives in its own top-level tab, not in this rail.)
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlannerSelection {
    View(TodoView),
    Project(String),
    /// A full-width Planner panel (no to-do list, no timeline).
    Panel(PlannerPanel),
}

/// The Planner's non-list panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlannerPanel {
    AgentTime,
    WeeklyReview,
    /// Rename, aliases, status, and merging possible duplicates.
    ManageProjects,
}

impl PlannerPanel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PlannerPanel::AgentTime => "agent_time",
            PlannerPanel::WeeklyReview => "weekly_review",
            PlannerPanel::ManageProjects => "manage_projects",
        }
    }
}

/// Whether the decision-queue drawer is open. Provided at the Planner root
/// so the nav, the header readout and the agent lane can all open it.
#[derive(Clone, Copy)]
pub(crate) struct DecisionsDrawerContext(pub(crate) Signal<bool>);

/// Agent-lane column width: the lane only becomes its own column when the
/// task list can keep its 520px minimum beside it.
pub(crate) const AGENT_LANE_MIN_WIDTH: f64 = 320.0;
pub(crate) const AGENT_LANE_MAX_WIDTH: f64 = 480.0;
pub(crate) const AGENT_LANE_DEFAULT_WIDTH: f64 = 400.0;

/// Where the agent lane goes on Today, given the Planner row's measured
/// width and the solver's nav/timeline widths: `Some(width)` for its own
/// column (taken only from space beyond the task list's 520px minimum —
/// the layout solver's guarantee is never eaten into), `None` to embed it
/// as a section in the right column. An unmeasured row (0) embeds.
pub(crate) fn agent_lane_width(row_width: f64, nav_width: f64, timeline_width: f64, requested: f64) -> Option<f64> {
    if row_width <= 0.0 {
        return None;
    }
    let spare = row_width - nav_width - timeline_width - layout_budget::PLANNER_MAIN_MIN_WIDTH;
    let want = requested.clamp(AGENT_LANE_MIN_WIDTH, AGENT_LANE_MAX_WIDTH);
    if spare >= want {
        Some(want)
    } else if spare >= AGENT_LANE_MIN_WIDTH {
        Some(spare.floor())
    } else {
        None
    }
}

impl PlannerSelection {
    /// The key a list's display order is saved under in
    /// `UiState.planner_list_orders`.
    pub(crate) fn list_key(&self) -> String {
        match self {
            PlannerSelection::View(v) => v.as_str().to_string(),
            PlannerSelection::Project(id) => format!("project:{id}"),
            PlannerSelection::Panel(p) => format!("panel:{}", p.as_str()),
        }
    }

    /// The built-in view, or `None` for a project list — the shape
    /// `views::order_options` takes.
    pub(crate) fn view(&self) -> Option<TodoView> {
        match self {
            PlannerSelection::View(v) => Some(*v),
            PlannerSelection::Project(_) | PlannerSelection::Panel(_) => None,
        }
    }
}

/// The list the Planner is showing, mirrored to an app-root context so
/// surfaces mounted outside `PlannerView` (the ⌘K palette) can see it.
/// `None` while the Planner is unmounted. Readers should `peek()`: the
/// palette only samples it when it opens.
#[derive(Clone, Copy)]
pub(crate) struct PlannerFocusHint(pub(crate) Signal<Option<PlannerSelection>>);

/// The user's *requested* Planner column widths, owned by `main.rs` so its
/// layout solver can see them. The widths the Planner actually renders are
/// the solver's outcome, passed to `PlannerView` as props; automatic
/// collapsing never writes these.
#[derive(Clone, Copy)]
pub(crate) struct PlannerWidths {
    pub(crate) nav: Signal<f64>,
    pub(crate) timeline: Signal<f64>,
}

/// A list's count as the rail badge shows it: hidden at zero, "99+" above 99.
pub(crate) fn badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_string()),
    }
}

/// Tooltip / accessible name for a rail button: the list name and count.
pub(crate) fn rail_label(name: &str, count: usize) -> String {
    if count == 0 {
        name.to_string()
    } else {
        format!("{name}, {count}")
    }
}

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
pub fn PlannerView(
    /// The Planner nav shows its 52px icon rail (solver outcome).
    nav_railed: bool,
    /// Effective nav width (the rail width when railed).
    nav_width: f64,
    /// Effective timeline width.
    timeline_width: f64,
) -> Element {
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

    // The decision queue drawer (over Today).
    let decisions_drawer = use_signal(|| false);
    use_context_provider(|| DecisionsDrawerContext(decisions_drawer));
    // The Planner row's measured width and the agent lane's requested width
    // (see `agent_lane_width`). Declared before any early return.
    let mut row_width = use_signal(|| 0.0f64);
    let mut lane_requested = use_signal(|| AGENT_LANE_DEFAULT_WIDTH);

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

    let sel_now = selection.read().clone();
    let show_today_rail = sel_now == PlannerSelection::View(TodoView::Today);
    let panel = match sel_now {
        PlannerSelection::Panel(p) => Some(p),
        _ => None,
    };

    // Requested widths live in `PlannerWidths` (main.rs reads them for the
    // give-way solver); the dividers update them live and persist on commit.
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let PlannerWidths { nav: mut nav_requested, timeline: mut timeline_requested } =
        use_context::<PlannerWidths>();

    // The agent lane takes its own column only from space the solver left
    // beyond the task list's minimum; otherwise it's a section in the right
    // column. The row's width is measured (onresize) rather than solved, so
    // main.rs's solver stays the single owner of the 520px guarantee.
    let lane_column = if show_today_rail {
        agent_lane_width(*row_width.read(), nav_width, timeline_width, *lane_requested.read())
    } else {
        None
    };
    // Release gate (`Settings::agent_time_enabled`): Agent time and the
    // decision queue are hidden unless the experimental switch is on.
    let agent_time_on = settings.read().agent_time_enabled();
    let decisions_open = *decisions_drawer.read() && agent_time_on;

    rsx! {
        div {
            class: "flex h-full min-h-0 flex-col bg-app text-fg",
            FocusBar {}
            agent_lane::AgentStateSync {}
            div {
                class: "relative flex flex-row flex-1 min-h-0",
                onresize: move |evt| {
                    if let Ok(size) = evt.data().get_border_box_size() {
                        if (size.width - *row_width.peek()).abs() >= 1.0 {
                            row_width.set(size.width);
                        }
                    }
                },
                div {
                    id: "planner-left-rail",
                    class: "shrink-0 h-full min-h-0",
                    style: "width: {nav_width}px;",
                    LeftRail { selection, railed: nav_railed }
                }
                // The solver's automatic rail is temporary and not
                // user-resizable; otherwise the divider is always there.
                if !nav_railed {
                    ColumnDivider {
                        width: nav_width,
                        min: layout_budget::PLANNER_NAV_MIN_WIDTH,
                        max: layout_budget::PLANNER_NAV_MAX_WIDTH,
                        default: layout_budget::PLANNER_NAV_DEFAULT_WIDTH,
                        side: ColumnSide::Left,
                        label: "Resize Planner list nav".to_string(),
                        on_resize: move |w: ColumnWidth| nav_requested.set(w.width),
                        on_commit: move |w: ColumnWidth| {
                            nav_requested.set(w.width);
                            let mut ui = ui_state.write();
                            if ui.planner_left_width != w.width {
                                ui.planner_left_width = w.width;
                                ui_state_manager.read().save_async(ui.clone(), None);
                            }
                        },
                    }
                }
                match panel {
                    Some(PlannerPanel::AgentTime) if agent_time_on => rsx! { agent_time::AgentTimeView {} },
                    Some(PlannerPanel::AgentTime) => rsx! { CentreColumn { selection } },
                    Some(PlannerPanel::WeeklyReview) => rsx! { review::WeeklyReview {} },
                    Some(PlannerPanel::ManageProjects) => rsx! { manage_projects::ManageProjects {} },
                    None => rsx! { CentreColumn { selection } },
                }
                if show_today_rail {
                    ColumnDivider {
                        width: timeline_width,
                        min: layout_budget::PLANNER_TIMELINE_MIN_WIDTH,
                        max: layout_budget::PLANNER_TIMELINE_MAX_WIDTH,
                        default: layout_budget::PLANNER_TIMELINE_DEFAULT_WIDTH,
                        side: ColumnSide::Right,
                        label: "Resize day timeline".to_string(),
                        on_resize: move |w: ColumnWidth| timeline_requested.set(w.width),
                        on_commit: move |w: ColumnWidth| {
                            timeline_requested.set(w.width);
                            let mut ui = ui_state.write();
                            if ui.planner_today_width != w.width {
                                ui.planner_today_width = w.width;
                                ui_state_manager.read().save_async(ui.clone(), None);
                            }
                        },
                    }
                    div {
                        id: "planner-today-rail",
                        class: "shrink-0 h-full min-h-0",
                        style: "width: {timeline_width}px;",
                        TodayRail { viewing: viewing_day, embed_agent_lane: lane_column.is_none() }
                    }
                    if let Some(w) = lane_column {
                        ColumnDivider {
                            width: w,
                            min: AGENT_LANE_MIN_WIDTH,
                            max: AGENT_LANE_MAX_WIDTH,
                            default: AGENT_LANE_DEFAULT_WIDTH,
                            side: ColumnSide::Right,
                            label: "Resize agent lane".to_string(),
                            on_resize: move |w: ColumnWidth| lane_requested.set(w.width),
                            on_commit: move |w: ColumnWidth| lane_requested.set(w.width),
                        }
                        div {
                            id: "planner-agent-lane",
                            class: "shrink-0 h-full min-h-0",
                            style: "width: {w}px;",
                            agent_lane::AgentLaneColumn {}
                        }
                    }
                }
                if decisions_open {
                    decisions::DecisionDrawer { open: decisions_drawer }
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

/// A rail row whose right-hand tag is text ("off", "18m") or, with `warn`,
/// the yellow count badge the decision queue uses.
#[component]
fn RailItemTagged<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    label: String,
    tag: Option<String>,
    warn: bool,
    selected: bool,
    onclick: EventHandler<MouseEvent>,
    /// Show the "Experimental" pill after the label.
    #[props(default)]
    experimental: bool,
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
            if experimental {
                ExperimentalPill {}
            }
            if let Some(t) = tag {
                if warn {
                    span { class: "rounded-full bg-yellow-800 px-1.5 text-[10px] font-bold leading-4 text-yellow-300", "{t}" }
                } else {
                    span { class: "text-xs text-fg-muted", "{t}" }
                }
            }
        }
    }
}

#[component]
fn LeftRail(mut selection: Signal<PlannerSelection>, railed: bool) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    // Release gate: "Decisions" and "Agent time" only when the experimental
    // switch is on (`Settings::agent_time_enabled`).
    let agent_time_on = settings.read().agent_time_enabled();
    // Rail mode's projects popover (hook declared before the rail early-return).
    let mut projects_open = use_signal(|| false);
    // Calls the agent-time run is waiting on the user for (a memo, so chat
    // streaming doesn't re-render the rail).
    let session_state = use_context::<Signal<crate::session::SessionState>>();
    let held_count = use_memo(move || {
        crate::agent_time_runtime::held_approvals(&planner.read(), &session_state.read()).len()
    });
    let held_count = *held_count.read();
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
    // The projects header (and its Manage link) shows whenever any project
    // exists — closed ones are managed there too.
    let has_projects = !state.projects.is_empty();
    let duplicate_groups = crate::todo::project_identity::duplicate_groups(&state.projects).len();

    let sel = selection.read().clone();
    let is_view = |v: TodoView| sel == PlannerSelection::View(v);
    let mut drawer = use_context::<DecisionsDrawerContext>().0;
    let decisions_count = crate::todo::lanes::needs_you(&state.todos).len() + held_count;
    let agent_time_label = {
        let b = state.agent_time_budget(day, Utc::now());
        if held_count > 0 {
            Some("needs you".to_string())
        } else if !b.enabled {
            Some("off".to_string())
        } else if b.minutes_used > 0 {
            Some(format!("{}m", b.minutes_used))
        } else {
            None
        }
    };
    let is_panel = |p: PlannerPanel| sel == PlannerSelection::Panel(p);
    let mut open_decisions = move || {
        if *selection.peek() != PlannerSelection::View(TodoView::Today) {
            selection.set(PlannerSelection::View(TodoView::Today));
        }
        drawer.set(true);
    };

    if railed {
        let smart: [(TodoView, &'static str, usize); 6] = [
            (TodoView::Inbox, "Inbox", count(TodoView::Inbox)),
            (TodoView::Today, "Today", count(TodoView::Today)),
            (TodoView::Upcoming, "Upcoming", count(TodoView::Upcoming)),
            (TodoView::Anytime, "Anytime", count(TodoView::Anytime)),
            (TodoView::Someday, "Someday", count(TodoView::Someday)),
            (TodoView::Logbook, "Logbook", count(TodoView::Logbook)),
        ];
        let project_selected = matches!(sel, PlannerSelection::Project(_));
        let project_rows: Vec<RailProjectRow> = top_level
            .iter()
            .map(|p| (p.id.clone(), p.title.clone(), project_count(&p.id)))
            .collect();
        let area_rows: Vec<RailAreaRows> = by_area
            .iter()
            .map(|(title, ps)| {
                (
                    title.clone(),
                    ps.iter().map(|p| (p.id.clone(), p.title.clone(), project_count(&p.id))).collect(),
                )
            })
            .collect();
        return rsx! {
            div {
                class: "relative h-full w-full border-r border-subtle bg-section",
                div {
                    class: "flex h-full w-full flex-col items-stretch gap-1 overflow-y-auto py-2",
                RailIconButton {
                    icon: fi_icons::FiInbox,
                    label: rail_label(smart[0].1, smart[0].2),
                    badge: smart[0].2,
                    selected: is_view(smart[0].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[0].0)),
                }
                RailIconButton {
                    icon: fi_icons::FiStar,
                    label: rail_label(smart[1].1, smart[1].2),
                    badge: smart[1].2,
                    selected: is_view(smart[1].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[1].0)),
                }
                RailIconButton {
                    icon: fi_icons::FiCalendar,
                    label: rail_label(smart[2].1, smart[2].2),
                    badge: smart[2].2,
                    selected: is_view(smart[2].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[2].0)),
                }
                RailIconButton {
                    icon: fi_icons::FiLayers,
                    label: rail_label(smart[3].1, smart[3].2),
                    badge: smart[3].2,
                    selected: is_view(smart[3].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[3].0)),
                }
                RailIconButton {
                    icon: fi_icons::FiArchive,
                    label: rail_label(smart[4].1, smart[4].2),
                    badge: smart[4].2,
                    selected: is_view(smart[4].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[4].0)),
                }
                RailIconButton {
                    icon: fi_icons::FiCheckCircle,
                    label: rail_label(smart[5].1, smart[5].2),
                    badge: smart[5].2,
                    selected: is_view(smart[5].0),
                    onclick: move |_| selection.set(PlannerSelection::View(smart[5].0)),
                }
                if agent_time_on {
                    RailIconButton {
                        icon: fi_icons::FiGitMerge,
                        label: rail_label("Decisions", decisions_count),
                        badge: decisions_count,
                        selected: *drawer.read(),
                        onclick: move |_| open_decisions(),
                    }
                    RailIconButton {
                        icon: fi_icons::FiClock,
                        label: "Agent time".to_string(),
                        badge: 0usize,
                        selected: is_panel(PlannerPanel::AgentTime),
                        onclick: move |_| selection.set(PlannerSelection::Panel(PlannerPanel::AgentTime)),
                    }
                }
                RailIconButton {
                    icon: fi_icons::FiBarChart2,
                    label: "Weekly review (experimental)".to_string(),
                    badge: 0usize,
                    selected: is_panel(PlannerPanel::WeeklyReview),
                    onclick: move |_| selection.set(PlannerSelection::Panel(PlannerPanel::WeeklyReview)),
                }
                if has_projects {
                    div { class: "mx-3 my-1 border-t border-faint" }
                    RailIconButton {
                        icon: fi_icons::FiFolder,
                        label: "Projects".to_string(),
                        badge: duplicate_groups,
                        selected: project_selected || *projects_open.read() || is_panel(PlannerPanel::ManageProjects),
                        onclick: move |_| {
                            let now = *projects_open.peek();
                            projects_open.set(!now);
                        },
                    }
                }
                }
                if *projects_open.read() {
                    RailProjectsPopover {
                        selection,
                        open: projects_open,
                        top_level: project_rows,
                        by_area: area_rows,
                        duplicate_groups,
                    }
                }
            }
        };
    }

    rsx! {
        div {
            class: "h-full w-full flex flex-col gap-0.5 overflow-y-auto bg-section p-2",
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
            if agent_time_on {
                RailItemTagged {
                    icon: fi_icons::FiGitMerge,
                    label: "Decisions",
                    tag: badge_text(decisions_count),
                    warn: true,
                    selected: *drawer.read(),
                    onclick: move |_| open_decisions(),
                }
                RailItemTagged {
                    icon: fi_icons::FiClock,
                    label: "Agent time",
                    tag: agent_time_label,
                    warn: held_count > 0,
                    selected: is_panel(PlannerPanel::AgentTime),
                    onclick: move |_| selection.set(PlannerSelection::Panel(PlannerPanel::AgentTime)),
                }
            }
            RailItemTagged {
                icon: fi_icons::FiBarChart2,
                label: "Weekly review",
                tag: None,
                warn: false,
                selected: is_panel(PlannerPanel::WeeklyReview),
                onclick: move |_| selection.set(PlannerSelection::Panel(PlannerPanel::WeeklyReview)),
                experimental: true,
            }

            if has_projects {
                div { class: "my-2 border-t border-faint" }
                div {
                    class: "flex items-center gap-2 px-2 pb-1",
                    p {
                        class: "flex-1 text-xs font-medium uppercase tracking-wide text-fg-muted",
                        "Projects"
                    }
                    button {
                        class: if is_panel(PlannerPanel::ManageProjects) {
                            "inline-flex items-center gap-1 text-[11px] text-fg"
                        } else {
                            "inline-flex items-center gap-1 text-[11px] text-fg-muted hover:text-fg"
                        },
                        title: if duplicate_groups > 0 {
                            format!("Manage projects \u{b7} {} possible duplicate group(s)", duplicate_groups)
                        } else {
                            "Manage projects".to_string()
                        },
                        onclick: move |_| selection.set(PlannerSelection::Panel(PlannerPanel::ManageProjects)),
                        "Manage"
                        if duplicate_groups > 0 {
                            span { class: "rounded-full bg-yellow-800 px-1.5 text-[10px] font-bold leading-4 text-yellow-300", "{duplicate_groups}" }
                        }
                    }
                }
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

/// (id, title, open-todo count) for one project in the rail popover.
type RailProjectRow = (String, String, usize);
/// An area heading and its projects.
type RailAreaRows = (String, Vec<RailProjectRow>);

// ── Icon rail (52px) ────────────────────────────────────────────────────────

/// One icon button in the railed nav: tooltip + aria-label carry the list
/// name and count, a 10px badge shows the count (hidden at zero).
/// The small "Experimental" pill for surfaces that ship but aren't settled
/// (the weekly review). Same outline-pill vocabulary as the review's
/// "Correction" chip.
#[component]
pub(crate) fn ExperimentalPill() -> Element {
    rsx! {
        span {
            class: "shrink-0 rounded-full border border-yellow-600 px-1.5 text-[10px] uppercase leading-4 tracking-[0.08em] text-yellow-200",
            title: "Experimental: still taking shape, and may change",
            "Experimental"
        }
    }
}

#[component]
fn RailIconButton<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    label: String,
    badge: usize,
    selected: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let badge = badge_text(badge);
    rsx! {
        button {
            class: if selected {
                "relative flex h-9 w-full shrink-0 items-center justify-center border-l-2 border-l-primary-500 bg-card text-fg"
            } else {
                "relative flex h-9 w-full shrink-0 items-center justify-center border-l-2 border-transparent text-fg-muted hover:bg-card hover:text-fg transition-colors"
            },
            title: "{label}",
            "aria-label": "{label}",
            "aria-current": if selected { "true" } else { "false" },
            onclick: move |evt| onclick.call(evt),
            Icon { width: 16, height: 16, icon }
            if let Some(text) = badge {
                span {
                    class: "absolute right-1.5 top-0.5 min-w-[14px] rounded-full bg-input px-1 text-center text-[10px] leading-[14px] text-fg-muted",
                    "{text}"
                }
            }
        }
    }
}

/// The rail's projects popover (opened by its folder button): projects
/// grouped by area, using the same `RailItem` rows as the full nav. Picking
/// one selects it and closes the popover; a click outside closes it too.
/// Rendered outside the rail's scroll container so it isn't clipped.
#[component]
fn RailProjectsPopover(
    mut selection: Signal<PlannerSelection>,
    mut open: Signal<bool>,
    top_level: Vec<RailProjectRow>,
    by_area: Vec<RailAreaRows>,
    duplicate_groups: usize,
) -> Element {
    let manage_label = if duplicate_groups > 0 {
        format!("Manage projects \u{b7} {duplicate_groups} to review")
    } else {
        "Manage projects".to_string()
    };
    rsx! {
            div {
                class: "fixed inset-0 z-40",
                onclick: move |_| open.set(false),
            }
            div {
                class: "absolute left-full top-0 z-50 ml-1 max-h-[70vh] w-60 overflow-y-auto rounded-md border border-subtle bg-card p-2 shadow-lg",
                role: "menu",
                "aria-label": "Projects",
                RailItem {
                    icon: fi_icons::FiSliders,
                    label: manage_label,
                    count: 0usize,
                    selected: *selection.read() == PlannerSelection::Panel(PlannerPanel::ManageProjects),
                    onclick: move |_| {
                        selection.set(PlannerSelection::Panel(PlannerPanel::ManageProjects));
                        open.set(false);
                    },
                }
                div { class: "my-1 border-t border-faint" }
                for (id, title, n) in top_level {
                    RailItem {
                        key: "{id}",
                        icon: fi_icons::FiFolder,
                        label: title,
                        count: n,
                        selected: *selection.read() == PlannerSelection::Project(id.clone()),
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                selection.set(PlannerSelection::Project(id.clone()));
                                open.set(false);
                            }
                        },
                    }
                }
                for (area_title, projects) in by_area {
                    div {
                        key: "{area_title}",
                        class: "mt-2",
                        p {
                            class: "px-2 pb-1 text-xs font-medium uppercase tracking-wide text-fg-muted",
                            "{area_title}"
                        }
                        for (id, title, n) in projects {
                            RailItem {
                                key: "{id}",
                                icon: fi_icons::FiFolder,
                                label: title,
                                count: n,
                                selected: *selection.read() == PlannerSelection::Project(id.clone()),
                                onclick: {
                                    let id = id.clone();
                                    move |_| {
                                        selection.set(PlannerSelection::Project(id.clone()));
                                        open.set(false);
                                    }
                                },
                            }
                        }
                    }
                }
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_hides_zero_and_caps_at_99_plus() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(99).as_deref(), Some("99"));
        assert_eq!(badge_text(100).as_deref(), Some("99+"));
        assert_eq!(badge_text(5000).as_deref(), Some("99+"));
    }

    #[test]
    fn agent_lane_takes_only_space_beyond_the_task_list_minimum() {
        // 1600 row, 220 nav, 380 timeline → 1000 for main; 480 spare.
        assert_eq!(agent_lane_width(1600.0, 220.0, 380.0, 400.0), Some(400.0));
        assert_eq!(agent_lane_width(1600.0, 220.0, 380.0, 900.0), Some(AGENT_LANE_MAX_WIDTH));
        // 1450 → 330 spare: shrinks to fit rather than eat the list's 520.
        assert_eq!(agent_lane_width(1450.0, 220.0, 380.0, 400.0), Some(330.0));
        // Too narrow (or unmeasured): a section in the right column.
        assert_eq!(agent_lane_width(1300.0, 220.0, 380.0, 400.0), None);
        assert_eq!(agent_lane_width(0.0, 220.0, 380.0, 400.0), None);
    }

    #[test]
    fn rail_label_names_the_list_and_its_count() {
        assert_eq!(rail_label("Inbox", 0), "Inbox");
        assert_eq!(rail_label("Today", 7), "Today, 7");
    }
}
