use crate::fleet::FleetSession;
use crate::todo::model::{Priority, Todo};
use crate::todo::project_match::ProjectIndex;
use crate::todo::views::{self, ArrangeInputs, GroupBy, ListOrder, RowGroup, SortKey, TodoView};
use super::super::planner_view::{today, PlannerSelection};
use std::collections::HashMap;
use crate::settings::Settings;
use crate::todo::PlannerState;
use dioxus::prelude::*;
use super::{MENU_ITEM, MENU_LABEL, TodoRow};
use super::centre::{GroupHeader};
use super::helpers::{block_slots, empty_state_line, todos_for_selection};
use super::reorder::{AdoptUndo, ListDrag, ListDragContext, Reorder, apply_move, bump_flip, drag_over_index, flip_snap, reorder_target};

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
pub(super) fn TodoList(
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
