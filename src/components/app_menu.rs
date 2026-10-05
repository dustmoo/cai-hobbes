#![allow(non_snake_case)]
//! The Hobbes (tiger) app menu: the mascot button plus its dropdown of app
//! views and drawers. One implementation shared by the top `TabBar` strip
//! and the `TabSidebar` header — it's the only way back to Planner/Fleet
//! once their pinned tabs have been closed.

use crate::components::chat_input::ChatCommand;
use crate::settings::{Settings, UiState};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

/// Where the menu button sits, which only changes its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMenuPlacement {
    /// Full-height 44px cell anchoring the top strip's left corner.
    Strip,
    /// 28px rounded button in the tab sidebar header.
    SidebarHeader,
    /// 32px button at the top of the 48px sidebar rail.
    Rail,
}

#[component]
pub fn AppMenuButton(placement: AppMenuPlacement) -> Element {
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let ui_state = use_context::<Signal<UiState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut show_app_menu = use_signal(|| false);
    // One-shot "pounce" reaction on the mascot when it's clicked; its idle
    // sprite loop otherwise runs continuously (see `AnimatedTiger`).
    let mut tiger_pouncing = use_signal(|| false);

    let (wrapper, button) = match placement {
        AppMenuPlacement::Strip => (
            "relative flex items-center h-full shrink-0 border-r border-faint",
            "flex items-center justify-center w-11 h-full text-fg-muted hover:bg-card hover:text-fg focus:outline-none transition-colors",
        ),
        AppMenuPlacement::SidebarHeader => (
            "relative flex items-center shrink-0",
            "flex items-center justify-center w-7 h-7 rounded-md text-fg-muted hover:bg-card hover:text-fg focus:outline-none transition-colors",
        ),
        AppMenuPlacement::Rail => (
            "relative flex items-center justify-center shrink-0",
            "flex items-center justify-center w-8 h-8 rounded-md text-fg-muted hover:bg-card hover:text-fg focus:outline-none transition-colors",
        ),
    };

    rsx! {
        div {
            class: "{wrapper}",
            button {
                class: "{button}",
                title: "Hobbes menu",
                onclick: move |_| {
                    show_app_menu.set(!show_app_menu());
                    tiger_pouncing.set(true);
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(320)).await;
                        tiger_pouncing.set(false);
                    });
                },
                crate::components::pixel_icons::AnimatedTiger {
                    pouncing: tiger_pouncing,
                    width: 18,
                    height: 18,
                }
            }

            if show_app_menu() {
                // `fixed` (not `absolute`) so the menu escapes the strip's or
                // sidebar's own `overflow-hidden`; it anchors to the window's
                // top-left corner below the 40px header either way.
                div {
                    class: "fixed top-10 left-0 w-60 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1.5",
                    AppMenuRow {
                        icon: fi_icons::FiCheckSquare,
                        label: "Planner",
                        hotkey: settings.read().hotkeys.toggle_planner.clone(),
                        visible: ui_state.read().show_planner_icon,
                        onclick: move |_| {
                            chat_command.set(Some(ChatCommand::TogglePlanner));
                            show_app_menu.set(false);
                        },
                    }
                    AppMenuRow {
                        icon: crate::components::pixel_icons::HobbesTiger,
                        label: "Fleet",
                        hotkey: settings.read().hotkeys.toggle_fleet.clone(),
                        visible: ui_state.read().show_fleet_icon,
                        onclick: move |_| {
                            chat_command.set(Some(ChatCommand::ToggleFleet));
                            show_app_menu.set(false);
                        },
                    }
                    div { class: "border-t border-faint my-1" }
                    AppMenuRow {
                        icon: fi_icons::FiClock,
                        label: "History",
                        hotkey: settings.read().hotkeys.toggle_history.clone(),
                        visible: ui_state.read().show_history_icon,
                        onclick: move |_| {
                            chat_command.set(Some(ChatCommand::ToggleHistory));
                            show_app_menu.set(false);
                        },
                    }
                    AppMenuRow {
                        icon: fi_icons::FiPackage,
                        label: "MCP Tools",
                        hotkey: settings.read().hotkeys.toggle_mcp.clone(),
                        visible: ui_state.read().show_mcp_icon,
                        onclick: move |_| {
                            chat_command.set(Some(ChatCommand::ToggleMcp));
                            show_app_menu.set(false);
                        },
                    }
                    AppMenuRow {
                        icon: fi_icons::FiSettings,
                        label: "Settings",
                        hotkey: settings.read().hotkeys.toggle_settings.clone(),
                        visible: true,
                        onclick: move |_| {
                            chat_command.set(Some(ChatCommand::ToggleSettings));
                            show_app_menu.set(false);
                        },
                    }
                }
                // Click-outside backdrop to close the menu.
                div {
                    class: "fixed inset-0 z-40",
                    onclick: move |_| show_app_menu.set(false),
                }
            }
        }
    }
}

/// One row in the tiger's app menu — an icon, a label, and (optionally) the
/// user's configured hotkey for the command it dispatches. `visible` mirrors
/// the same `ui_state.show_*_icon` flags the old chat-bar buttons honoured;
/// an empty `hotkey` renders no hint rather than a hardcoded guess.
#[component]
fn AppMenuRow<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    label: String,
    hotkey: String,
    onclick: EventHandler<MouseEvent>,
    #[props(default = true)] visible: bool,
) -> Element {
    if !visible {
        return rsx! {};
    }

    let display_hotkey = crate::hotkey::display_combo(&hotkey);

    rsx! {
        button {
            class: "w-full flex items-center gap-3 px-3 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg whitespace-nowrap",
            onclick: move |evt| onclick.call(evt),
            Icon { icon: icon, width: 16, height: 16 }
            span { "{label}" }
            div { class: "flex-1" }
            if !display_hotkey.is_empty() {
                span {
                    class: "text-[11px] font-mono text-fg-muted/70 tabular-nums tracking-wide pl-6",
                    "{display_hotkey}"
                }
            }
        }
    }
}
