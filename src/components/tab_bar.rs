#![allow(non_snake_case)]
use crate::session::SessionState;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[derive(Props, PartialEq, Clone)]
pub struct TabBarProps {
    pub open_tabs: Vec<String>,
    pub active_tab_index: usize,
    pub on_select_tab: EventHandler<usize>,
    pub on_close_tab: EventHandler<usize>,
    pub on_new_tab: EventHandler<()>,
}

pub fn TabBar(props: TabBarProps) -> Element {
    let session_state = use_context::<Signal<SessionState>>();
    let sessions = session_state.read();

    rsx! {
        div {
            class: "flex items-center bg-section border-b border-primary-700/50 h-10 shrink-0 overflow-x-auto no-scrollbar",
            
            // Tab items
            for (idx, session_id) in props.open_tabs.iter().enumerate() {
                {
                    let is_active = idx == props.active_tab_index;
                    let session_name = sessions.sessions.get(session_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| "Unknown Session".to_string());
                    let session_id_key = session_id.clone();
                    let on_select = props.on_select_tab;
                    let on_close = props.on_close_tab;
                    
                    rsx! {
                        div {
                            key: "{session_id_key}",
                            class: if is_active {
                                "group flex items-center h-full px-3 cursor-pointer min-w-32 max-w-64 select-none bg-card border-b-2 border-b-primary-500"
                            } else {
                                "group flex items-center h-full px-3 cursor-pointer min-w-32 max-w-64 select-none hover:bg-card"
                            },
                            onclick: move |_| on_select.call(idx),
                            
                            span {
                                class: if is_active { "flex-1 truncate text-xs text-fg font-medium" } else { "flex-1 truncate text-xs text-fg-muted" },
                                "{session_name}"
                            }
                            
                            // Close button (visible on hover)
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


