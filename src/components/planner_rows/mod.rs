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

use chrono::{Local, NaiveDate, Utc};
use dioxus_free_icons::{icons::fi_icons, Icon};
use crate::fleet::FleetSession;
use crate::todo::model::{self, FocusActor, Priority, TimeOfDay, Todo, TodoStatus};
use crate::todo::project_match::ProjectIndex;
use crate::todo::views::{self, ArrangeInputs, GroupBy, ListOrder, RowGroup, SortKey, TodoView};
use super::planner_view::{mutate_todo, persist_block, persist_focus_sessions, persist_todo, today, PlannerSelection, SelectedBlockContext};
use std::collections::HashMap;
use dioxus::html::input_data::MouseButton;
use crate::settings::Settings;
use crate::todo::store;
use crate::todo::PlannerState;
use dioxus::prelude::*;
use centre::{GroupHeader};
use helpers::{WhoState, activation_seed, block_slots, cycle_estimate, empty_state_line, first_block_on_day, priority_tone_class, profile_pill_border_class, profile_stripe_class, todos_for_selection, who_state, working_minutes};
use reorder::{AdoptUndo, ListDrag, ListDragContext, Reorder, apply_move, bump_flip, drag_over_index, flip_snap, reorder_target};
use schedule::{current_schedule_choice, date_pill_label, due_tone, due_tone_class};

mod centre;
mod helpers;
mod quick_add;
mod reorder;
mod schedule;
#[cfg(test)]
mod test_support;

pub(crate) use schedule::{ScheduleChoice, set_schedule};
pub(crate) use centre::{CentreColumn};
pub(crate) use quick_add::{BirthProject, Inherited, InheritedFrom, QuickAddDraft, QuickAddField, apply_project_pick, birth_project_title, commit_quick_add, landing_label, resolve_birth};

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
