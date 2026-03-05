#![allow(non_snake_case)]
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
}

pub fn TabBar(props: TabBarProps) -> Element {
    // Write-only: used only for update_session_name in editing handlers
    let mut session_state = use_context::<Signal<SessionState>>();
    let mut editing_tab_id = use_signal(|| None::<String>);
    let mut temp_name = use_signal(String::new);

    rsx! {
        div {
            class: "flex items-center bg-section border-b border-primary-700/50 h-10 shrink-0 overflow-x-auto no-scrollbar",

            // Tab items
            for (idx, session_id) in props.open_tabs.iter().enumerate() {
                {
                    let is_active = idx == props.active_tab_index;
                    let session_name = props.tab_names.get(idx)
                        .cloned()
                        .unwrap_or_else(|| "Unknown Session".to_string());
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
                                "group flex items-center h-full px-3 cursor-pointer min-w-32 max-w-64 select-none bg-card border-b-2 border-b-primary-500"
                            } else {
                                "group flex items-center h-full px-3 cursor-pointer min-w-32 max-w-64 select-none hover:bg-card"
                            },
                            onclick: move |_| {
                                if !is_editing {
                                    on_select.call(idx);
                                }
                            },

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

            // New tab button
            button {
                class: "flex items-center justify-center h-full px-3 text-fg-muted hover:bg-card hover:text-fg transition-colors",
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
