use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::time::SystemTime;

use crate::{context::prompt_builder::PromptBuilder, settings::Settings};

#[component]
pub fn ChatInput(
    is_sending: Signal<bool>,
    on_send: EventHandler<String>,
    on_cancel: EventHandler<()>,
    on_interaction: EventHandler<()>,
    on_toggle_sessions: EventHandler<()>,
    on_toggle_settings: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut draft = use_context::<Signal<String>>();

    let mut send_message = move || {
        if *is_sending.read() {
            tracing::warn!("'send_message' blocked: already sending.");
            return;
        }
        let user_message = draft.read().clone();
        if user_message.is_empty() {
            return;
        }
        on_send.call(user_message);
        draft.set("".to_string());
        let _ = document::eval(r#"
            const el = document.getElementById('chat-textarea');
            if (el) { el.style.height = 'auto'; }
        "#);
    };

    rsx! {
        div {
            class: "bg-gray-900 p-4 border-t border-gray-700",
            onmousedown: |e| e.stop_propagation(),
            div {
                class: "flex items-center space-x-3",
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_sessions.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiMenu
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| on_toggle_settings.call(()),
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiSettings
                    }
                }
                textarea {
                    id: "chat-textarea",
                    class: "flex-1 py-2 px-4 rounded-xl bg-gray-800 border border-gray-700 text-gray-100 placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-purple-500 resize-none overflow-y-auto",
                    style: "max-height: 50vh;",
                    rows: "1",
                    placeholder: if !mcp_context.read().servers.is_empty() { "Type your message..." } else { "Initializing..." },
                    disabled: mcp_context.read().servers.is_empty(),
                    value: "{draft}",
                    oninput: move |event| {
                        let _ = document::eval(r#"
                            const el = document.getElementById('chat-textarea');
                            if (el) {
                                window.dioxusCursorPos = [el.selectionStart, el.selectionEnd];
                                el.style.height = 'auto';
                                el.style.height = (el.scrollHeight) + 'px';
                            }
                        "#);
                        draft.set(event.value());
                    },
                    onkeydown: move |event| {
                        let modifiers = event.data.modifiers();

                        if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::ALT) {
                            return;
                        }

                        if event.key() == Key::Tab {
                            event.prevent_default();
                            let script = if modifiers.contains(Modifiers::SHIFT) {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const value = el.value;
                                    let line_start = value.lastIndexOf('\n', start - 1) + 1;
                                    if (value.substring(line_start, line_start + 1) === '\t') {
                                        el.value = value.substring(0, line_start) + value.substring(line_start + 1);
                                        el.selectionStart = el.selectionEnd = Math.max(start - 1, line_start);
                                    }
                                }
                                "#
                            } else {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const end = el.selectionEnd;
                                    el.value = el.value.substring(0, start) + '\t' + el.value.substring(end);
                                    el.selectionStart = el.selectionEnd = start + 1;
                                }
                                "#
                            };
                            let _ = document::eval(script);
                            let _ = document::eval(r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    var event = new Event('input', { bubbles: true, cancelable: true });
                                    el.dispatchEvent(event);
                                }
                            "#);
                            return;
                        }

                        if event.key() == Key::Enter && !modifiers.contains(Modifiers::SHIFT) {
                            event.prevent_default();
                            on_interaction.call(());
                            send_message();
                        }
                    },
                }
                {
                    cfg_if::cfg_if! {
                        if #[cfg(debug_assertions)] {
                            rsx! {
                                button {
                                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                                    onclick: move |_| {
                                        let session_state = session_state.clone();
                                        let settings = settings.clone();
                                        let mcp_manager = mcp_manager.clone();
                                        spawn(async move {
                                            let mcp_context = {
                                                let mcp_manager_reader = mcp_manager.read();
                                                mcp_manager_reader.get_mcp_context().await
                                            };

                                            let context_string = {
                                                let state = session_state.read();
                                                if let Some(session) = state.get_active_session().cloned() {
                                                    let mut session_for_debug = session;

                                                    if !mcp_context.servers.is_empty() {
                                                        session_for_debug.active_context.mcp_tools = Some(mcp_context);
                                                    }

                                                    let settings_reader = settings.read();
                                                    let builder = PromptBuilder::new(&session_for_debug, &settings_reader, &state);
                                                    let prompt_data = builder.build_prompt("[DEBUG USER MESSAGE]".to_string(), None);
                                                    format!("{:#?}", prompt_data)
                                                } else {
                                                    "[No active session]".to_string()
                                                }
                                            };
                                            let timestamp = SystemTime::now()
                                                .duration_since(SystemTime::UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs();
                                            let file_name = format!("prompt_{}.log", timestamp);
                                            if let Err(e) = std::fs::write(&file_name, &context_string) {
                                                tracing::error!("Failed to write debug prompt to file: {}", e);
                                            } else {
                                                tracing::info!("Debug prompt written to {}", &file_name);
                                            }
                                        });
                                    },
                                    Icon {
                                        width: 20,
                                        height: 20,
                                        icon: fi_icons::FiCpu
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                    onclick: move |_| {
                        session_state.write().create_session();
                    },
                    Icon {
                        width: 20,
                        height: 20,
                        icon: fi_icons::FiPlus
                    }
                }
                if !*is_sending.read() {
                    button {
                        class: "px-5 py-2 bg-purple-600 rounded-full text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors disabled:bg-gray-500",
                        disabled: mcp_context.read().servers.is_empty(),
                        onclick: move |_| {
                            on_interaction.call(());
                            send_message();
                        },
                        "Send"
                    }
                } else {
                    button {
                        class: "px-4 py-2 bg-red-600 rounded-full text-white font-semibold hover:bg-red-700 focus:outline-none focus:ring-2 focus:ring-red-500 focus:ring-opacity-50 transition-colors flex items-center space-x-2",
                        onclick: move |_| on_cancel.call(()),
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiSquare
                        }
                        span { "Stop" }
                    }
                }
            }
        }
    }
}