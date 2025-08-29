#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};
use crate::session::SessionState;

#[derive(Props, PartialEq, Clone)]
pub struct SessionManagerProps {}

pub fn SessionManager(_props: SessionManagerProps) -> Element {
    let mut session_state = consume_context::<Signal<SessionState>>();
    let mut editing_session_id = use_signal(|| None::<String>);
    let mut temp_session_name = use_signal(String::new);

    let sessions = session_state.read();
    let active_id = sessions.active_session_id.clone();

    rsx! {
        div {
            class: "flex flex-col bg-gray-800 text-white h-full w-full p-4",
            h2 {
                class: "text-lg font-bold mb-4",
                "Sessions"
            }
            div {
                class: "flex-1 overflow-y-auto",
                ul {
                    class: "space-y-2",
                    {
                        let mut sorted_sessions: Vec<_> = sessions.sessions.values().collect();
                        sorted_sessions.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));

                        sorted_sessions.into_iter().map(|session| {
                            let id = &session.id;
                            let is_active = *id == active_id;
                            let is_editing = editing_session_id.read().as_ref() == Some(id);

                            let active_class = if is_active { "bg-purple-600" } else { "" };
                            let id_clone_for_click = id.clone();
                            let id_clone_for_delete = id.clone();
                            let id_clone_for_keydown = id.clone();
                            let id_clone_for_blur = id.clone();
                            let id_clone_for_edit_button = id.clone();
                            let session_name = session.name.clone();

                            rsx! {
                                li {
                                    class: "flex items-center justify-between p-2 rounded-md cursor-pointer hover:bg-gray-700 {active_class}",
                                    key: "{id}",
                                    onclick: move |_| {
                                        if editing_session_id.read().is_none() {
                                            session_state.write().set_active_session(id_clone_for_click.clone());
                                        }
                                    },
                                    if is_editing {
                                        input {
                                            class: "flex-grow bg-gray-700 text-white rounded-md p-1 focus:outline-none focus:ring-2 focus:ring-purple-500",
                                            value: "{temp_session_name.read()}",
                                            oninput: move |evt| temp_session_name.set(evt.value()),
                                            onkeydown: move |evt| {
                                                if evt.key() == Key::Enter {
                                                    session_state.write().update_session_name(&id_clone_for_keydown, temp_session_name.read().clone());
                                                    editing_session_id.set(None);
                                                } else if evt.key() == Key::Escape {
                                                    editing_session_id.set(None);
                                                }
                                            },
                                            onblur: move |_| {
                                                session_state.write().update_session_name(&id_clone_for_blur, temp_session_name.read().clone());
                                                editing_session_id.set(None);
                                            }
                                        }
                                    } else {
                                        span { class: "flex-grow select-none", "{session.name}" }
                                    },
                                    div {
                                        class: "flex items-center",
                                        button {
                                            class: "px-2 py-1 rounded-md text-xs font-bold text-gray-400 hover:bg-gray-600 hover:text-white",
                                            onclick: move |event| {
                                                event.stop_propagation();
                                                temp_session_name.set(session_name.clone());
                                                editing_session_id.set(Some(id_clone_for_edit_button.clone()));
                                            },
                                            Icon {
                                                icon: fi_icons::FiEdit2,
                                                width: 16,
                                                height: 16,
                                            }
                                        }
                                        button {
                                            class: "px-2 py-1 rounded-md text-xs font-bold text-gray-400 hover:bg-red-600 hover:text-white",
                                            onclick: move |event| {
                                                event.stop_propagation();
                                                session_state.write().delete_session(&id_clone_for_delete);
                                            },
                                            "X"
                                        }
                                    }
                                }
                            }
                        })
                    }
                }
            }
            div {
                class: "mt-4",
                button {
                    class: "w-full px-4 py-2 bg-purple-600 rounded-md text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500",
                    onclick: move |_| {
                        session_state.write().create_session();
                    },
                    "✨ New Chat"
                }
            }
        }
    }
}