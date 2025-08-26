#![allow(non_snake_case)]
use dioxus::prelude::*;
use crate::session::SessionState;

#[derive(Props, PartialEq, Clone)]
pub struct SessionManagerProps {}

pub fn SessionManager(_props: SessionManagerProps) -> Element {
    let mut session_state = consume_context::<Signal<SessionState>>();
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
                        sessions.sessions.iter().map(|(id, session)| {
                            let is_active = *id == active_id;
                            let active_class = if is_active { "bg-purple-600" } else { "" };
                            let id_clone_for_click = id.clone();
                            let id_clone_for_delete = id.clone();
                            rsx! {
                                li {
                                    class: "flex items-center justify-between p-2 rounded-md cursor-pointer hover:bg-gray-700 {active_class}",
                                    key: "{id}",
                                    onclick: move |_| {
                                        session_state.write().set_active_session(id_clone_for_click.clone());
                                    },
                                    span { class: "flex-grow select-none", "{session.name}" }
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