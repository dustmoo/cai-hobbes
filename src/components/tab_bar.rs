#![allow(non_snake_case)]
use crate::components::stream_manager::StreamManagerContext;
use crate::session::SessionState;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[derive(Props, PartialEq, Clone)]
pub struct TabBarProps {
    pub open_tabs: Vec<String>,
    pub tab_names: Vec<String>,
    pub active_tab_index: usize,
    pub on_select_tab: EventHandler<usize>,
    pub on_close_tab: EventHandler<usize>,
    pub on_new_tab: EventHandler<()>,
    /// The planner renders as its own tab rather than replacing the active
    /// session's chat: open/active are separate so a chat tab can be selected
    /// (deactivating the planner) without closing the planner tab.
    pub planner_tab_open: bool,
    pub planner_active: bool,
    pub on_select_planner: EventHandler<()>,
    pub on_close_planner: EventHandler<()>,
    /// The fleet tab follows the planner-tab idiom: an app view pinned to
    /// the strip, open/active tracked separately.
    pub fleet_tab_open: bool,
    pub fleet_active: bool,
    /// Sessions needing the user right now — badges the fleet chip.
    pub fleet_attention: usize,
    pub on_select_fleet: EventHandler<()>,
    pub on_close_fleet: EventHandler<()>,
}

pub fn TabBar(props: TabBarProps) -> Element {
    // Write-only: used only for update_session_name in editing handlers
    let mut session_state = use_context::<Signal<SessionState>>();
    let stream_manager = consume_context::<StreamManagerContext>();
    let mut editing_tab_id = use_signal(|| None::<String>);
    let mut temp_name = use_signal(String::new);

    // Use a memo to create a proper reactive subscription to the streaming state.
    // This ensures the component re-renders when stream_activity changes,
    // even though the component's Props haven't changed.
    let open_tabs_for_memo = props.open_tabs.clone();
    let streaming_states = use_memo(move || {
        // Read stream_activity to subscribe to stream start/end events
        let _activity = stream_manager.stream_activity.read();
        // Also directly subscribe to streaming_sessions for immediate updates
        let _sessions = stream_manager.streaming_sessions.read();
        let states: Vec<bool> = open_tabs_for_memo.iter()
            .map(|id| stream_manager.is_session_streaming(id))
            .collect();
        states
    });

    rsx! {
        div {
            class: "flex items-center bg-section border-b border-primary-700/50 h-10 shrink-0 overflow-hidden",

            // Tab items
            for (idx, session_id) in props.open_tabs.iter().enumerate() {
                {
                    // A session tab is never shown active while the planner tab
                    // is — the session stays *selected* underneath, but exactly
                    // one tab may carry the active styling.
                    let is_active = idx == props.active_tab_index
                        && !props.planner_active
                        && !props.fleet_active;
                    let session_name = props.tab_names.get(idx)
                        .cloned()
                        .unwrap_or_else(|| "Unknown Session".to_string());
                    let is_streaming = streaming_states.read().get(idx).cloned().unwrap_or(false);
                    let session_id_key = session_id.clone();
                    let id_edit = session_id.clone();
                    let id_save_keydown = session_id.clone();
                    let id_save_click = session_id.clone();
                    let id_blur = session_id.clone();
                    let on_select = props.on_select_tab;
                    let on_close = props.on_close_tab;
                    let is_editing = editing_tab_id.read().as_ref() == Some(session_id);

                    rsx! {
                        div {
                            key: "{session_id_key}",
                            class: if is_active {
                                "group relative flex items-center h-full px-3 cursor-pointer min-w-0 max-w-64 shrink select-none bg-card border-b-2 border-b-primary-500"
                            } else {
                                "group relative flex items-center h-full px-3 cursor-pointer min-w-0 max-w-64 shrink select-none hover:bg-card"
                            },
                            onclick: move |_| {
                                if !is_editing {
                                    on_select.call(idx);
                                }
                            },

                            // Glowing base line while this tab's chat is actively streaming
                            if is_streaming {
                                div {
                                    class: "absolute bottom-0 left-0 right-0 h-[2px] bg-primary-400 shadow-[0_0_8px_2px_rgba(91,134,193,0.75)] animate-pulse pointer-events-none",
                                }
                            }

                            if is_editing {
                                div {
                                    class: "flex items-center w-full",
                                    input {
                                        class: "flex-1 bg-input text-fg text-xs rounded-sm px-1 py-0.5 focus:outline-none focus:ring-1 focus:ring-primary-500",
                                        value: "{temp_name}",
                                        onclick: move |evt| evt.stop_propagation(),
                                        oninput: move |evt| temp_name.set(evt.value()),
                                        onkeydown: move |evt| {
                                            match evt.key() {
                                                Key::Enter => {
                                                    session_state.write().update_session_name(&id_save_keydown, temp_name.read().clone());
                                                    editing_tab_id.set(None);
                                                }
                                                Key::Escape => {
                                                    editing_tab_id.set(None);
                                                }
                                                _ => {}
                                            }
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
                                    // Save Button
                                    button {
                                        class: "ml-1 p-0.5 text-green-500 hover:bg-green-500/10 rounded",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            session_state.write().update_session_name(&id_save_click, temp_name.read().clone());
                                            editing_tab_id.set(None);
                                        },
                                        title: "Save",
                                        Icon {
                                            icon: fi_icons::FiCheck,
                                            width: 12,
                                            height: 12,
                                        }
                                    }
                                    // Cancel Button
                                    button {
                                        class: "ml-0.5 p-0.5 text-fg-muted hover:bg-red-500/10 hover:text-red-500 rounded",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            editing_tab_id.set(None);
                                        },
                                        title: "Cancel",
                                        Icon {
                                            icon: fi_icons::FiX,
                                            width: 12,
                                            height: 12,
                                        }
                                    }
                                }
                            } else {
                                if is_streaming {
                                    div {
                                        class: "w-2 h-2 rounded-full bg-primary-400 animate-pulse mr-1.5 shrink-0",
                                    }
                                }
                                span {
                                    class: if is_active { "flex-1 truncate text-xs text-fg font-medium" } else { "flex-1 truncate text-xs text-fg-muted" },
                                    onclick: move |evt| {
                                        if is_active {
                                            evt.stop_propagation();
                                            temp_name.set(session_name.clone());
                                            editing_tab_id.set(Some(id_edit.clone()));
                                        }
                                    },
                                    "{session_name}"
                                }
                            }

                            // Close button (visible on hover)
                            if !is_editing {
                                button {
                                    class: "ml-2 p-0.5 rounded-md hover:bg-red-500/20 hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        on_close.call(idx);
                                    },
                                    Icon {
                                        icon: fi_icons::FiX,
                                        width: 12,
                                        height: 12,
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Planner tab — a pinned app view, not a session: no rename, and
            // closing it never touches session state.
            if props.planner_tab_open {
                div {
                    class: if props.planner_active {
                        "group relative flex items-center h-full px-3 cursor-pointer shrink-0 select-none bg-card border-b-2 border-b-primary-500"
                    } else {
                        "group relative flex items-center h-full px-3 cursor-pointer shrink-0 select-none hover:bg-card"
                    },
                    onclick: move |_| props.on_select_planner.call(()),
                    Icon {
                        icon: fi_icons::FiCheckSquare,
                        width: 12,
                        height: 12,
                        class: if props.planner_active { "text-fg mr-1.5" } else { "text-fg-muted mr-1.5" },
                    }
                    span {
                        class: if props.planner_active { "text-xs text-fg font-medium" } else { "text-xs text-fg-muted" },
                        "Planner"
                    }
                    button {
                        class: "ml-2 p-0.5 rounded-md hover:bg-red-500/20 hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            props.on_close_planner.call(());
                        },
                        Icon {
                            icon: fi_icons::FiX,
                            width: 12,
                            height: 12,
                        }
                    }
                }
            }

            // Fleet tab — same pinned-app-view idiom as the planner, plus an
            // attention badge (sessions waiting on the user).
            if props.fleet_tab_open {
                div {
                    class: if props.fleet_active {
                        "group relative flex items-center h-full px-3 cursor-pointer shrink-0 select-none bg-card border-b-2 border-b-primary-500"
                    } else {
                        "group relative flex items-center h-full px-3 cursor-pointer shrink-0 select-none hover:bg-card"
                    },
                    onclick: move |_| props.on_select_fleet.call(()),
                    Icon {
                        icon: fi_icons::FiMonitor,
                        width: 12,
                        height: 12,
                        class: if props.fleet_active { "text-fg mr-1.5" } else { "text-fg-muted mr-1.5" },
                    }
                    span {
                        class: if props.fleet_active { "text-xs text-fg font-medium" } else { "text-xs text-fg-muted" },
                        "Fleet"
                    }
                    if props.fleet_attention > 0 {
                        span {
                            class: "ml-1.5 px-1.5 rounded-full text-[10px] font-bold",
                            style: "background-color: #f59e0b22; color: #f59e0b; border: 1px solid #f59e0b;",
                            "{props.fleet_attention}"
                        }
                    }
                    button {
                        class: "ml-2 p-0.5 rounded-md hover:bg-red-500/20 hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            props.on_close_fleet.call(());
                        },
                        Icon {
                            icon: fi_icons::FiX,
                            width: 12,
                            height: 12,
                        }
                    }
                }
            }

            // New tab button
            button {
                class: "flex items-center justify-center h-full px-3 shrink-0 text-fg-muted hover:bg-card hover:text-fg transition-colors",
                onclick: move |_| props.on_new_tab.call(()),
                title: "New Tab (Cmd+Shift+N)",
                Icon {
                    icon: fi_icons::FiPlus,
                    width: 16,
                    height: 16,
                }
            }
        }
    }
}
