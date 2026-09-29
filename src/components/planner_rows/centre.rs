use super::super::planner_view::{persist_todo, today, PlannerSelection};
use super::helpers::{order_hint, view_title};
use super::list::TodoList;
use super::quick_add::QuickAdd;
use super::reorder::{bump_flip, flip_snap, AdoptUndo};
use super::{MENU_ITEM, MENU_LABEL, MENU_RULE, META, PILL, POPOVER, SECTION_HEADER};
use crate::settings::Settings;
use crate::todo::model::{self};
use crate::todo::views::{self, GroupBy, ListOrder, SortDir, SortKey, TodoView};
use crate::todo::PlannerState;
use chrono::{Local, Utc};
use dioxus::prelude::*;

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
        let live: Vec<String> = planner
            .peek()
            .projects
            .iter()
            .map(|p| p.id.clone())
            .collect();
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
    let arrow = if current.dir == SortDir::Asc {
        "↑"
    } else {
        "↓"
    };
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
pub(super) fn GroupHeader(
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
