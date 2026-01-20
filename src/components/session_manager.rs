#![allow(non_snake_case)]
use crate::{
    components::chat_input::ChatCommand, context::permissions::PermissionManager,
    session::SessionState, settings::Settings,
};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[derive(Props, PartialEq, Clone)]
pub struct SessionManagerProps {}

pub fn SessionManager(_props: SessionManagerProps) -> Element {
    let mut session_state = use_context::<Signal<SessionState>>();
    let mut permission_manager = use_context::<Signal<PermissionManager>>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut editing_session_id = use_signal(|| None::<String>);
    let mut temp_session_name = use_signal(String::new);
    let mut show_confirm_modal = use_context::<Signal<bool>>();
    let mut session_to_delete = use_context::<Signal<String>>();

    // Pagination and Filtering State
    let mut filter_query = use_signal(String::new);
    let mut current_page = use_signal(|| 0);
    let mut items_per_page = use_signal(|| 10);

    // Listen for Global Commands (e.g., Delete Session Hotkey)
    use_effect(move || {
        let should_listen = {
            let read = chat_command.read();
            matches!(read.as_ref(), Some(ChatCommand::DeleteSession))
        };

        if should_listen {
            // Clone active ID to avoid borrow issues
            let active_id = session_state.read().active_session_id.clone();
            if !active_id.is_empty() {
                tracing::info!("Delete Session Hotkey Triggered for: {}", active_id);
                if settings.read().confirm_on_delete {
                    session_to_delete.set(active_id);
                    show_confirm_modal.set(true);
                } else {
                    session_state.write().delete_session(&active_id);
                }
            }
            // Reset command
            chat_command.set(None);
        }
    });

    let sessions = session_state.read();
    let active_id = sessions.active_session_id.clone();

    let mut sorted_sessions: Vec<_> = sessions.sessions.values().collect();
    sorted_sessions.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));

    // Filter sessions
    let query = filter_query.read().to_lowercase();
    let filtered_sessions: Vec<_> = sorted_sessions
        .into_iter()
        .filter(|s| s.name.to_lowercase().contains(&query))
        .collect();

    let total_items = filtered_sessions.len();
    let limit = *items_per_page.read();
    let total_pages = (total_items as f64 / limit as f64).ceil() as usize;
    let total_pages = if total_pages == 0 { 1 } else { total_pages };

    // Ensure current page is valid
    if *current_page.read() >= total_pages {
        current_page.set(total_pages.saturating_sub(1));
    }

    let start_index = *current_page.read() * limit;
    let end_index = (start_index + limit).min(total_items);

    let paginated_sessions = if start_index < total_items {
        &filtered_sessions[start_index..end_index]
    } else {
        &[]
    };

    rsx! {
    // Main content of the session manager
    div {
        class: "flex flex-col bg-dark-bg text-white h-full w-full p-4",
            h2 { class: "text-lg font-bold mb-4", "Sessions" }

            // Search Input
            div {
                class: "mb-4 relative",
                input {
                    class: "w-full bg-dark-input text-white border border-primary-600 rounded-md py-2 pl-10 pr-4 text-sm focus:outline-none focus:ring-2 focus:ring-primary-500 placeholder-gray-400",
                    placeholder: "Search chats...",
                    value: "{filter_query}",
                    oninput: move |evt| {
                        filter_query.set(evt.value());
                        current_page.set(0); // Reset to first page on search
                    }
                }
                div {
                    class: "absolute left-2.5 top-2.5 text-gray-400",
                    Icon {
                        icon: fi_icons::FiSearch,
                        width: 14,
                        height: 14,
                    }
                }
            }
            div {
                class: "flex-1 overflow-y-auto min-h-0",
                ul {
                    class: "space-y-2",
                    {paginated_sessions.iter().map(|session| {
                        let active_class = if session.id == active_id { "bg-primary-500" } else { "" };
                        let session_id = session.id.clone();
                        let session_name = session.name.clone();

                        let id_click = session_id.clone();
                        let id_edit = session_id.clone();
                        let id_delete = session_id.clone();
                        let id_keydown = session_id.clone();
                        let id_blur = session_id.clone();
                        let name_edit = session_name.clone();

                        rsx! {
                            li {
                                class: "flex items-center justify-between p-2 rounded-md cursor-pointer hover:bg-dark-card {active_class}",
                                key: "{session_id}",
                                onclick: move |_| {
                                    if editing_session_id.read().is_none() {
                                        session_state.write().set_active_session(id_click.clone());
                                        permission_manager.write().reset_turn_count();
                                    }
                                },
                                if editing_session_id.read().as_ref() == Some(&session_id) {
                                    input {
                                        class: "flex-grow bg-dark-input text-white rounded-md p-1 focus:outline-none focus:ring-2 focus:ring-primary-500",
                                        value: "{temp_session_name.read()}",
                                        oninput: move |evt| temp_session_name.set(evt.value()),
                                        onkeydown: move |evt| {
                                            if evt.key() == Key::Enter {
                                                session_state.write().update_session_name(&id_keydown, temp_session_name.read().clone());
                                                editing_session_id.set(None);
                                            } else if evt.key() == Key::Escape {
                                                editing_session_id.set(None);
                                            }
                                        },
                                        onblur: move |_| {
                                            session_state.write().update_session_name(&id_blur, temp_session_name.read().clone());
                                            editing_session_id.set(None);
                                        }
                                    }
                                } else {
                                    span { class: "flex-grow select-none truncate", "{session_name}" }
                                },
                                div {
                                    class: "flex items-center ml-2",
                                    button {
                                        class: "p-1 rounded-md text-gray-400 hover:bg-gray-600 hover:text-white mr-1",
                                        onclick: move |event| {
                                            event.stop_propagation();
                                            temp_session_name.set(name_edit.clone());
                                            editing_session_id.set(Some(id_edit.clone()));
                                        },
                                        Icon {
                                            icon: fi_icons::FiEdit2,
                                            width: 14,
                                            height: 14,
                                        }
                                    }
                                    button {
                                        class: "p-1 rounded-md text-gray-400 hover:bg-red-600 hover:text-white",
                                        onclick: move |event| {
                                            event.stop_propagation();
                                            if settings.read().confirm_on_delete {
                                                session_to_delete.set(id_delete.clone());
                                                show_confirm_modal.set(true);
                                            } else {
                                                session_state.write().delete_session(&id_delete);
                                            }
                                        },
                                        "X"
                                    }
                                }
                            }
                        }
                    })}
                }
            }

            // Footer Section: Pagination + New Chat
            div {
                class: "mt-auto pt-4 border-t border-gray-700",

                // Controls Container
                div {
                    class: "flex flex-col gap-3 mb-4",

                    // Pagination Controls
                    div {
                        class: "flex justify-between items-center px-2 text-sm text-gray-400",
                        button {
                            class: "p-2 rounded hover:bg-dark-card disabled:opacity-50 disabled:cursor-not-allowed transition-colors",
                            disabled: *current_page.read() == 0,
                            onclick: move |_| {
                                let p = *current_page.read();
                                current_page.set(p - 1);
                            },
                            Icon { icon: fi_icons::FiChevronLeft, width: 18, height: 18 }
                        }
                        span {
                            class: "font-medium select-none",
                            "Page {*current_page.read() + 1} of {total_pages}"
                        }
                        button {
                            class: "p-2 rounded hover:bg-dark-card disabled:opacity-50 disabled:cursor-not-allowed transition-colors",
                            disabled: *current_page.read() >= total_pages - 1,
                            onclick: move |_| {
                                let p = *current_page.read();
                                current_page.set(p + 1);
                            },
                            Icon { icon: fi_icons::FiChevronRight, width: 18, height: 18 }
                        }
                    }

                    // Items Per Page Selector
                    div {
                        class: "flex justify-between items-center px-2 text-xs text-gray-400",
                        span { "Sessions per page:" }
                        select {
                            class: "bg-dark-input border border-primary-600 rounded px-2 py-1 text-white text-xs focus:outline-none focus:ring-1 focus:ring-primary-500 cursor-pointer",
                            value: "{items_per_page.read()}",
                            onchange: move |evt| {
                                if let Ok(val) = evt.value().parse::<usize>() {
                                    items_per_page.set(val);
                                    current_page.set(0);
                                }
                            },
                            option { value: "10", "10" }
                            option { value: "20", "20" }
                            option { value: "50", "50" }
                            option { value: "100", "100" }
                        }
                    }
                }

                button {
                    class: "w-full px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600 focus:outline-none focus:ring-2 focus:ring-primary-500 transition-colors",
                    onclick: move |_| {
                        session_state.write().create_session();
                        permission_manager.write().reset_turn_count();
                    },
                    "✨ New Chat"
                }
            }
        }
    }
}
