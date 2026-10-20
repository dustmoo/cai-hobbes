#![allow(non_snake_case)]
//! Open tabs as a vertical list in a left sidebar — the alternative to the
//! top `TabBar` strip, chosen by `UiState::tabs_as_sidebar`.
//!
//! A second *view* onto the same open-tabs state: it takes the same select /
//! close / new-tab handlers `TabBar` does and never reorders `open_tabs`.
//! Its sort and grouping are the planner's lens (`tab_list::TabListOrder`,
//! driven by the shared `ListOrderControl`), persisted in
//! `UiState::tab_sidebar_order`.
//!
//! Resizable (180–400px) by the shared `ColumnDivider` that `main.rs` renders
//! on its right edge, and collapsible to a 48px emoji rail — by the user
//! (dragging below 140px, persisted) or by the layout solver when the window
//! runs out of room (temporary). See `layout_budget::solve_chat`.

use crate::components::chat_input::ChatCommand;
use crate::components::planner_rows::{ListOrderControl, SECTION_HEADER};
use crate::components::stream_manager::StreamManagerContext;
use crate::session::SessionState;
use crate::settings::{Settings, UiState, UiStateManager};
use crate::tab_list::{arrange_tabs, tab_group_options, TabFacts, TabListOrder, TabSortKey};
use crate::todo::PlannerState;
use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

/// What a tab shows in the 48px rail: its emoji, else the first letter of
/// its topic (AI summary or the user's rename), else a neutral chat glyph —
/// the bare date-stamp title of a fresh chat would make every letter "O"
/// in October, which tells tabs apart no better than nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailGlyph {
    Emoji(String),
    Letter(String),
    Blank,
}

pub fn rail_glyph(emoji: Option<&str>, topic: Option<&str>) -> RailGlyph {
    if let Some(e) = emoji.filter(|e| !e.trim().is_empty()) {
        return RailGlyph::Emoji(e.to_string());
    }
    match topic.and_then(|t| t.trim().chars().next()) {
        Some(c) => RailGlyph::Letter(c.to_uppercase().collect()),
        None => RailGlyph::Blank,
    }
}

fn is_middle_click(evt: &MouseEvent) -> bool {
    matches!(
        evt.trigger_button(),
        Some(dioxus::html::input_data::MouseButton::Auxiliary)
    )
}

/// One open chat tab, as `TabBar`'s per-tab props bundled into a row.
#[derive(Clone, PartialEq, Debug)]
pub struct TabSidebarEntry {
    pub session_id: String,
    /// `Session::display_title` — rename seed and the label before a summary.
    pub title: String,
    pub emoji: Option<String>,
    /// Short creation date ("Sep 16").
    pub date: String,
    /// `Session::tab_topic` — AI topic or the user's rename.
    pub topic: Option<String>,
    /// Resolved Composio profile display name.
    pub profile: Option<String>,
    pub project_id: Option<String>,
    pub last_updated: DateTime<Utc>,
    /// First message's timestamp, else `last_updated`.
    pub started: DateTime<Utc>,
}

#[derive(Props, PartialEq, Clone)]
pub struct TabSidebarProps {
    /// In `open_tabs` order; indices are what the handlers take.
    pub tabs: Vec<TabSidebarEntry>,
    pub active_tab_index: usize,
    pub on_select_tab: EventHandler<usize>,
    pub on_close_tab: EventHandler<usize>,
    pub on_new_tab: EventHandler<()>,
    pub planner_tab_open: bool,
    pub planner_active: bool,
    pub on_select_planner: EventHandler<()>,
    pub on_close_planner: EventHandler<()>,
    pub fleet_tab_open: bool,
    pub fleet_active: bool,
    pub fleet_attention: usize,
    pub on_select_fleet: EventHandler<()>,
    pub on_close_fleet: EventHandler<()>,
    /// Full-mode width (px), already resolved by the layout solver.
    pub width: f64,
    /// Show the 48px emoji rail (user's choice or the solver's).
    #[props(default)]
    pub rail: bool,
    /// Draw our own right border — when no divider sits on the edge (the
    /// solver's automatic rail isn't user-resizable).
    #[props(default)]
    pub edge_border: bool,
}

const ROW: &str = "group relative flex items-center gap-1.5 h-8 pl-3 pr-1 cursor-pointer select-none border-l-2";
const ROW_ACTIVE: &str = "bg-card border-l-primary-500";
const ROW_IDLE: &str = "border-transparent hover:bg-card";
const RAIL_ITEM: &str = "group relative flex items-center justify-center w-full h-9 shrink-0 cursor-pointer select-none border-l-2";
const RAIL_CLOSE: &str = "absolute top-0 right-0.5 p-px rounded-sm text-fg-muted bg-section hover:bg-red-500/20 hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity";
const CLOSE_BTN: &str = "p-0.5 rounded-md shrink-0 text-fg-muted hover:bg-red-500/20 hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity";

pub fn TabSidebar(props: TabSidebarProps) -> Element {
    let mut session_state = use_context::<Signal<SessionState>>();
    let stream_manager = consume_context::<StreamManagerContext>();
    let mut ui_state = use_context::<Signal<UiState>>();
    let ui_state_manager = use_context::<Signal<UiStateManager>>();
    let settings = use_context::<Signal<Settings>>();
    let planner = use_context::<Signal<PlannerState>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut editing_tab_id = use_signal(|| None::<String>);
    let mut temp_name = use_signal(String::new);

    // Streaming ids as a set (not per-index), so the memo stays right as tabs
    // open, close and re-sort.
    let streaming = use_memo(move || {
        let _activity = stream_manager.stream_activity.read();
        stream_manager.streaming_sessions.read().clone()
    });
    // The agent-time chat while it waits on the user's approval: a "needs
    // you" dot, in the fleet row's amber, so a background run is never
    // stuck unseen.
    let needs_you = use_memo(move || {
        crate::agent_time_runtime::held_approvals(&planner.read(), &session_state.read())
            .into_iter()
            .map(|h| h.chat_id)
            .collect::<std::collections::HashSet<String>>()
    });

    // Tabs between turns with a timer pending: a hollow, still dot, so a tab
    // waiting on itself doesn't read as idle (or as streaming).
    let timer_waits = use_memo(move || session_state.read().timer_waits());

    // ── Display order (a lens: never written to open_tabs) ─────────────────
    let group_options = move || {
        let s = settings.read();
        tab_group_options(s.planner_enabled, s.composio_profiles.len())
    };
    let mut order = use_signal(|| ui_state.peek().tab_sidebar_order);
    // Show only what's offered now (a Profile grouping after profiles drop to
    // one, say); the stored preference is left as-is until the user picks.
    use_effect(move || {
        let clean = order.peek().sanitized(&TabSortKey::ALL, &group_options());
        if clean != *order.peek() {
            order.set(clean);
        }
    });
    let set_order = move |next: TabListOrder| {
        order.set(next);
        ui_state.write().tab_sidebar_order = next;
        let state = ui_state.peek().clone();
        let manager = ui_state_manager.peek().clone();
        spawn(async move {
            let _ = manager.save(&state);
        });
    };

    let facts: Vec<TabFacts> = {
        let projects = &planner.read().projects;
        props
            .tabs
            .iter()
            .enumerate()
            .map(|(idx, t)| TabFacts {
                idx,
                title: t.title.clone(),
                last_updated: t.last_updated,
                started: t.started,
                project: t.project_id.as_ref().and_then(|pid| {
                    projects
                        .iter()
                        .find(|p| &p.id == pid)
                        .map(|p| (p.sort_order, p.id.clone(), p.title.clone()))
                }),
                profile: t.profile.clone(),
            })
            .collect()
    };
    let groups = arrange_tabs(&facts, *order.read());
    let any_view = props.planner_tab_open || props.fleet_tab_open;
    // The rail lists tabs in the same display order as the full list.
    let rail_order: Vec<usize> = groups.iter().flat_map(|g| g.rows.iter().copied()).collect();
    let nudge = {
        let ui = ui_state.read();
        ui.tidy_tabs_nudge && props.tabs.len() > ui.tidy_tabs_threshold && props.width >= 240.0
    };
    let width = if props.rail {
        crate::layout_budget::SIDEBAR_RAIL_WIDTH
    } else {
        props.width
    };
    let root_class = if props.edge_border {
        "flex flex-col h-full shrink-0 bg-section border-r border-primary-700/50"
    } else {
        "flex flex-col h-full shrink-0 bg-section"
    };

    if props.rail {
        return rsx! {
            div {
                class: "{root_class}",
                style: "width: {width}px;",
                div {
                    class: "flex items-center justify-center h-10 shrink-0 border-b border-faint",
                    crate::components::app_menu::AppMenuButton {
                        placement: crate::components::app_menu::AppMenuPlacement::Rail,
                    }
                }
                div {
                    class: "flex-1 min-h-0 overflow-y-auto overflow-x-hidden py-1 flex flex-col",
                    if props.planner_tab_open {
                        div {
                            class: if props.planner_active { "{RAIL_ITEM} {ROW_ACTIVE}" } else { "{RAIL_ITEM} {ROW_IDLE}" },
                            title: "Planner",
                            onclick: move |_| props.on_select_planner.call(()),
                            onmouseup: move |evt| if is_middle_click(&evt) { props.on_close_planner.call(()) },
                            Icon {
                                icon: fi_icons::FiCheckSquare,
                                width: 14,
                                height: 14,
                                class: if props.planner_active { "text-fg" } else { "text-fg-muted" },
                            }
                            button {
                                class: "{RAIL_CLOSE}",
                                title: "Close Planner",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    props.on_close_planner.call(());
                                },
                                Icon { icon: fi_icons::FiX, width: 10, height: 10 }
                            }
                        }
                    }
                    if props.fleet_tab_open {
                        div {
                            class: if props.fleet_active { "{RAIL_ITEM} {ROW_ACTIVE}" } else { "{RAIL_ITEM} {ROW_IDLE}" },
                            title: if props.fleet_attention > 0 { "Fleet · {props.fleet_attention} need you" } else { "Fleet" },
                            onclick: move |_| props.on_select_fleet.call(()),
                            onmouseup: move |evt| if is_middle_click(&evt) { props.on_close_fleet.call(()) },
                            Icon {
                                icon: crate::components::pixel_icons::HobbesTiger,
                                width: 16,
                                height: 16,
                                class: if props.fleet_active { "" } else { "opacity-70" },
                            }
                            if props.fleet_attention > 0 {
                                span {
                                    class: "absolute bottom-1 right-1.5 w-1.5 h-1.5 rounded-full",
                                    style: "background-color: #f59e0b;",
                                }
                            }
                            button {
                                class: "{RAIL_CLOSE}",
                                title: "Close Fleet",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    props.on_close_fleet.call(());
                                },
                                Icon { icon: fi_icons::FiX, width: 10, height: 10 }
                            }
                        }
                    }
                    if any_view {
                        div { class: "h-px mx-2 bg-[var(--color-border-muted)] my-1 shrink-0" }
                    }
                    for idx in rail_order.iter().copied() {
                        {
                            let tab = props.tabs[idx].clone();
                            let is_active = idx == props.active_tab_index
                                && !props.planner_active
                                && !props.fleet_active;
                            let is_streaming = streaming.read().contains(&tab.session_id);
                            let waits_on_you = needs_you.read().contains(&tab.session_id);
                            let timer_at = timer_waits.read().get(&tab.session_id).copied();
                            let label = tab.topic.clone().unwrap_or_else(|| tab.title.clone());
                            let glyph = rail_glyph(tab.emoji.as_deref(), tab.topic.as_deref());
                            let on_select = props.on_select_tab;
                            let on_close = props.on_close_tab;
                            rsx! {
                                div {
                                    key: "{tab.session_id}",
                                    class: if is_active { "{RAIL_ITEM} {ROW_ACTIVE}" } else { "{RAIL_ITEM} {ROW_IDLE}" },
                                    title: "{label}",
                                    onclick: move |_| on_select.call(idx),
                                    onmouseup: move |evt| if is_middle_click(&evt) { on_close.call(idx) },
                                    match glyph {
                                        RailGlyph::Emoji(e) => rsx! { span { class: "text-[15px] leading-none", "{e}" } },
                                        RailGlyph::Letter(l) => rsx! {
                                            span {
                                                class: if is_active { "text-xs font-medium text-fg" } else { "text-xs font-medium text-fg-muted" },
                                                "{l}"
                                            }
                                        },
                                        RailGlyph::Blank => rsx! {
                                            Icon {
                                                icon: fi_icons::FiMessageSquare,
                                                width: 13,
                                                height: 13,
                                                class: if is_active { "text-fg" } else { "text-fg-muted" },
                                            }
                                        },
                                    }
                                    if waits_on_you {
                                        span {
                                            class: "absolute bottom-1 right-1.5 w-1.5 h-1.5 rounded-full",
                                            style: "background-color: #f59e0b;",
                                            title: "Needs you: waiting for your approval",
                                        }
                                    } else if is_streaming {
                                        div { class: "absolute bottom-1 right-1.5 w-1.5 h-1.5 rounded-full bg-primary-400 animate-pulse" }
                                    } else if let Some(at) = timer_at {
                                        span {
                                            class: "absolute bottom-1 right-1.5 w-1.5 h-1.5 rounded-full border border-primary-400",
                                            title: "{crate::timers::waiting_title(at)}",
                                        }
                                    }
                                    button {
                                        class: "{RAIL_CLOSE}",
                                        title: "Close Tab",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            on_close.call(idx);
                                        },
                                        Icon { icon: fi_icons::FiX, width: 10, height: 10 }
                                    }
                                }
                            }
                        }
                    }
                }
                div {
                    class: "flex items-center justify-center h-10 shrink-0 border-t border-faint",
                    button {
                        class: "flex items-center justify-center w-8 h-8 rounded-md text-fg-muted hover:bg-card hover:text-fg transition-colors",
                        onclick: move |_| props.on_new_tab.call(()),
                        title: "New Tab (Cmd+Shift+N)",
                        Icon { icon: fi_icons::FiPlus, width: 14, height: 14 }
                    }
                }
            }
        };
    }

    rsx! {
        div {
            class: "{root_class}",
            style: "width: {width}px;",

            // Header: the Hobbes menu, title, the planner's sort/group pill,
            // new tab. The pill's popover is right-anchored and 224px wide,
            // which would hang off the window edge this far left, so
            // `.tab-sidebar-order` (tailwind.css) anchors it to the pill's
            // left edge instead and lets it overhang the chat column.
            div {
                class: "flex items-center gap-1.5 h-10 shrink-0 pl-1.5 pr-2 border-b border-faint",
                crate::components::app_menu::AppMenuButton {
                    placement: crate::components::app_menu::AppMenuPlacement::SidebarHeader,
                }
                span { class: "flex-1 min-w-0 truncate text-[11px] uppercase tracking-[0.08em] text-fg-muted", "Tabs" }
                div {
                    class: "tab-sidebar-order shrink-0",
                    ListOrderControl {
                        order,
                        keys: TabSortKey::ALL.to_vec(),
                        groups: group_options(),
                        on_change: set_order,
                    }
                }
                // Optional nudge: a quiet pill, only once the open-tab count
                // passes the user's threshold, and only where there's room.
                if nudge {
                    button {
                        class: "shrink-0 inline-flex items-center h-[22px] px-2 rounded-full border border-subtle text-[11px] text-fg hover:border-primary-500 hover:bg-card transition-colors",
                        title: "{props.tabs.len()} tabs open",
                        onclick: move |_| chat_command.set(Some(ChatCommand::TidyTabs)),
                        "Tidy tabs?"
                    }
                }
                button {
                    class: "flex items-center justify-center w-7 h-7 shrink-0 rounded-md text-fg-muted hover:bg-card hover:text-fg transition-colors",
                    onclick: move |_| chat_command.set(Some(ChatCommand::TidyTabs)),
                    title: "Tidy tabs",
                    "aria-label": "Tidy tabs",
                    svg {
                        width: "14",
                        height: "14",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        path { d: "M4 20l8-8" }
                        path { d: "M14 4l6 6-6 2-2-2z" }
                        path { d: "M3 21l3-1" }
                    }
                }
                button {
                    class: "flex items-center justify-center w-7 h-7 shrink-0 rounded-md text-fg-muted hover:bg-card hover:text-fg transition-colors",
                    onclick: move |_| props.on_new_tab.call(()),
                    title: "New Tab (Cmd+Shift+N)",
                    Icon { icon: fi_icons::FiPlus, width: 14, height: 14 }
                }
            }

            div {
                class: "flex-1 min-h-0 overflow-y-auto py-1",

                // App views stay pinned above the chats, outside the sort.
                if any_view {
                    if props.planner_tab_open {
                        div {
                            class: if props.planner_active { "{ROW} {ROW_ACTIVE}" } else { "{ROW} {ROW_IDLE}" },
                            onclick: move |_| props.on_select_planner.call(()),
                            onmouseup: move |evt| if is_middle_click(&evt) { props.on_close_planner.call(()) },
                            Icon {
                                icon: fi_icons::FiCheckSquare,
                                width: 12,
                                height: 12,
                                class: if props.planner_active { "text-fg shrink-0" } else { "text-fg-muted shrink-0" },
                            }
                            span {
                                class: if props.planner_active { "flex-1 truncate text-xs text-fg font-medium" } else { "flex-1 truncate text-xs text-fg-muted" },
                                "Planner"
                            }
                            button {
                                class: "{CLOSE_BTN}",
                                title: "Close Planner",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    props.on_close_planner.call(());
                                },
                                Icon { icon: fi_icons::FiX, width: 12, height: 12 }
                            }
                        }
                    }
                    if props.fleet_tab_open {
                        div {
                            class: if props.fleet_active { "{ROW} {ROW_ACTIVE}" } else { "{ROW} {ROW_IDLE}" },
                            onclick: move |_| props.on_select_fleet.call(()),
                            onmouseup: move |evt| if is_middle_click(&evt) { props.on_close_fleet.call(()) },
                            Icon {
                                icon: crate::components::pixel_icons::HobbesTiger,
                                width: 14,
                                height: 14,
                                class: if props.fleet_active { "shrink-0" } else { "shrink-0 opacity-70" },
                            }
                            span {
                                class: if props.fleet_active { "flex-1 truncate text-xs text-fg font-medium" } else { "flex-1 truncate text-xs text-fg-muted" },
                                "Fleet"
                            }
                            if props.fleet_attention > 0 {
                                span {
                                    class: "px-1.5 rounded-full text-[10px] font-bold shrink-0",
                                    style: "background-color: #f59e0b22; color: #f59e0b; border: 1px solid #f59e0b;",
                                    "{props.fleet_attention}"
                                }
                            }
                            button {
                                class: "{CLOSE_BTN}",
                                title: "Close Fleet",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    props.on_close_fleet.call(());
                                },
                                Icon { icon: fi_icons::FiX, width: 12, height: 12 }
                            }
                        }
                    }
                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                }

                for group in groups {
                    div {
                        key: "{group.id}",
                        if let Some(label) = &group.label {
                            h2 {
                                class: "{SECTION_HEADER} flex items-baseline gap-1.5",
                                span { class: "truncate", "{label}" }
                                span { class: "font-mono tabular-nums normal-case tracking-normal", "· {group.rows.len()}" }
                            }
                        }
                        for idx in group.rows.iter().copied() {
                            {
                                let tab = props.tabs[idx].clone();
                                let is_active = idx == props.active_tab_index
                                    && !props.planner_active
                                    && !props.fleet_active;
                                let is_streaming = streaming.read().contains(&tab.session_id);
                                let waits_on_you = needs_you.read().contains(&tab.session_id);
                                let timer_at = timer_waits.read().get(&tab.session_id).copied();
                                let is_editing = editing_tab_id.read().as_deref() == Some(tab.session_id.as_str());
                                // Same identity layers as the strip's tooltip.
                                let tooltip = {
                                    let mut parts: Vec<String> = Vec::new();
                                    if let Some(e) = &tab.emoji { parts.push(e.clone()); }
                                    parts.push(tab.topic.clone().unwrap_or_else(|| tab.title.clone()));
                                    parts.push(tab.date.clone());
                                    if let Some(p) = &tab.profile { parts.push(p.clone()); }
                                    parts.join(" · ")
                                };
                                // Before a summary the title *is* the date stamp,
                                // so the trailing date would just repeat it.
                                let label = tab.topic.clone().unwrap_or_else(|| tab.title.clone());
                                let show_date = tab.topic.is_some();
                                let on_select = props.on_select_tab;
                                let on_close = props.on_close_tab;
                                let id_edit = tab.session_id.clone();
                                let id_enter = tab.session_id.clone();
                                let id_blur = tab.session_id.clone();
                                let seed_name = tab.title.clone();

                                rsx! {
                                    div {
                                        key: "{tab.session_id}",
                                        class: if is_active { "{ROW} {ROW_ACTIVE}" } else { "{ROW} {ROW_IDLE}" },
                                        title: "{tooltip}",
                                        onclick: move |_| {
                                            if !is_editing {
                                                on_select.call(idx);
                                            }
                                        },
                                        onmouseup: move |evt| {
                                            if !is_editing && is_middle_click(&evt) {
                                                on_close.call(idx);
                                            }
                                        },
                                        if is_editing {
                                            input {
                                                class: "flex-1 min-w-0 bg-input text-fg text-xs rounded-sm px-1 py-0.5 focus:outline-none focus:ring-1 focus:ring-primary-500",
                                                value: "{temp_name}",
                                                onclick: move |evt| evt.stop_propagation(),
                                                oninput: move |evt| temp_name.set(evt.value()),
                                                onkeydown: move |evt| match evt.key() {
                                                    Key::Enter => {
                                                        session_state.write().update_session_name(&id_enter, temp_name.read().clone());
                                                        editing_tab_id.set(None);
                                                    }
                                                    Key::Escape => editing_tab_id.set(None),
                                                    _ => {}
                                                },
                                                onblur: move |_| {
                                                    if editing_tab_id.read().is_some() {
                                                        session_state.write().update_session_name(&id_blur, temp_name.read().clone());
                                                        editing_tab_id.set(None);
                                                    }
                                                },
                                                onmounted: move |evt| {
                                                    let mounted = evt.data();
                                                    spawn(async move {
                                                        let _ = mounted.set_focus(true).await;
                                                    });
                                                },
                                                autofocus: true,
                                            }
                                        } else {
                                            if is_streaming {
                                                div { class: "w-2 h-2 rounded-full bg-primary-400 animate-pulse shrink-0" }
                                            } else if let Some(at) = timer_at {
                                                span {
                                                    class: "w-2 h-2 rounded-full border border-primary-400 shrink-0",
                                                    title: "{crate::timers::waiting_title(at)}",
                                                }
                                            }
                                            if let Some(e) = &tab.emoji {
                                                span { class: "text-[13px] leading-none shrink-0", "{e}" }
                                            }
                                            span {
                                                class: if is_active { "flex-1 min-w-0 truncate text-xs text-fg font-medium" } else { "flex-1 min-w-0 truncate text-xs text-fg-muted" },
                                                // Clicking the active tab's label renames it,
                                                // as in the top strip.
                                                onclick: move |evt| {
                                                    if is_active {
                                                        evt.stop_propagation();
                                                        temp_name.set(seed_name.clone());
                                                        editing_tab_id.set(Some(id_edit.clone()));
                                                    }
                                                },
                                                "{label}"
                                            }
                                            if waits_on_you {
                                                span {
                                                    class: "shrink-0 rounded-full px-1.5 text-[10px] font-medium leading-4",
                                                    style: "color: #f59e0b; border: 1px solid #f59e0b;",
                                                    title: "Waiting for your approval",
                                                    "needs you"
                                                }
                                            } else if show_date {
                                                span { class: "font-mono text-[11px] text-fg-muted tabular-nums shrink-0", "{tab.date}" }
                                            }
                                            button {
                                                class: "{CLOSE_BTN}",
                                                title: "Close Tab",
                                                onclick: move |evt| {
                                                    evt.stop_propagation();
                                                    on_close.call(idx);
                                                },
                                                Icon { icon: fi_icons::FiX, width: 12, height: 12 }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_glyph_prefers_emoji_then_topic_letter_then_blank() {
        assert_eq!(rail_glyph(Some("🐯"), Some("tigers")), RailGlyph::Emoji("🐯".into()));
        assert_eq!(rail_glyph(None, Some("budget review")), RailGlyph::Letter("B".into()));
        assert_eq!(rail_glyph(Some("  "), Some(" écrire")), RailGlyph::Letter("É".into()));
        assert_eq!(rail_glyph(None, None), RailGlyph::Blank);
        assert_eq!(rail_glyph(None, Some("   ")), RailGlyph::Blank);
    }
}
