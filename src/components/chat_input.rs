use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use uuid::Uuid;
use tokio::sync::mpsc;

use crate::{
    components::stream_manager::StreamManagerContext,
    context::prompt_builder::{LlmPrompt, PromptBuilder},
    processing::conversation_processor::ConversationProcessor,
    settings::Settings,
};

use super::{chat::Message, shared::MessageContent};

#[component]
pub fn ChatInput(
    on_interaction: EventHandler<()>,
    on_toggle_sessions: EventHandler<()>,
    on_toggle_settings: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut draft = use_context::<Signal<String>>();
    let mut has_interacted = use_signal(|| false);
    let is_sending = use_signal(|| false);
    let stream_manager = consume_context::<StreamManagerContext>();

    // Reusable closure for sending a message
    let send_prompt_to_llm = {
        // Capture signals which are all `Copy`
        let is_sending = is_sending;
        let stream_manager = stream_manager;
        let settings = settings;

        move |prompt_data: LlmPrompt, mcp_context: Option<crate::mcp::manager::McpContext>, hobbes_message_id: Uuid| {
            spawn(async move {
                // Now clone/read them inside the async block
                let mut is_sending = is_sending;
                let stream_manager = stream_manager;
                let settings = settings.read().clone();

                is_sending.set(true);
                tracing::info!("Lock ACQUIRED.");

                let (tx, mut rx) = mpsc::unbounded_channel::<()>();

                let on_complete = move || {
                    let _ = tx.send(());
                };

                stream_manager.start_stream(
                    settings.chat_model,
                    hobbes_message_id,
                    prompt_data,
                    on_complete,
                    mcp_context,
                );

                rx.recv().await;
                tracing::info!(message_id = %hobbes_message_id, "Stream completion signal RECEIVED.");

                is_sending.set(false);
                tracing::info!("Lock RELEASED.");
            });
        }
    };

    let mut send_message = {
        // Capture signals which are all `Copy`
        let is_sending = is_sending;
        let mut draft = draft;
        let session_state = session_state;
        let settings = settings;
        let mcp_manager = mcp_manager;
        let send_prompt_to_llm = send_prompt_to_llm;

        move || {
            if *is_sending.read() {
                tracing::warn!("'send_message' blocked: already sending.");
                return;
            }
            let user_message = draft.read().clone();
            if user_message.is_empty() {
                return;
            }
            draft.set("".to_string());
            let _ = document::eval(r#"
                const el = document.getElementById('chat-textarea');
                if (el) { el.style.height = 'auto'; }
            "#);

            spawn(async move {
                // Clone/read signals inside the async block
                let mut session_state = session_state;
                let settings = settings.read().clone();
                let mcp_manager = mcp_manager;
                let send_prompt_to_llm = send_prompt_to_llm;

                let hobbes_message_id = Uuid::new_v4();
                {
                    let mut state = session_state.write();
                    if state.active_session_id.is_empty() {
                        state.create_session();
                    }
                    if let Some(session) = state.get_active_session_mut() {
                        // Push the user's message
                        session.messages.push(Message {
                            id: Uuid::new_v4(),
                            author: "User".to_string(),
                            content: MessageContent::Text(user_message.clone()),
                        });
                        // Immediately push the empty "Hobbes" message to show the thinking indicator
                        session.messages.push(Message {
                            id: hobbes_message_id,
                            author: "Hobbes".to_string(),
                            content: MessageContent::Text("".to_string()),
                        });
                    }
                }

                let prompt_data = {
                    let mcp_context = mcp_manager.read().get_mcp_context().await;
                    let (user_prompt, conversation_summary) = {
                        let mut session_for_processing =
                            session_state.read().get_active_session().cloned().unwrap();
                        let processor = ConversationProcessor::new();
                        let prompt = processor
                            .process_and_respond(&mut session_for_processing, &settings)
                            .await;
                        (
                            prompt,
                            session_for_processing
                                .active_context
                                .conversation_summary,
                        )
                    };

                    {
                        let mut state = session_state.write();
                        if let Some(session) = state.get_active_session_mut() {
                            session.active_context.conversation_summary = conversation_summary;
                            if !mcp_context.servers.is_empty() {
                                session.active_context.mcp_tools = Some(mcp_context);
                            }
                        }
                    }

                    let state = session_state.read();
                    let session = state.get_active_session().unwrap();
                    let last_agent_message = session
                        .messages
                        .iter()
                        .filter(|m| m.author == "Hobbes")
                        .last()
                        .and_then(|m| match &m.content {
                            MessageContent::Text(text) => Some(text.clone()),
                            _ => None,
                        });

                    let builder = PromptBuilder::new(session, &settings, &state);
                    builder.build_prompt(user_prompt, last_agent_message)
                };

                if let Err(e) = session_state.read().save() {
                    tracing::error!("Failed to save session state: {}", e);
                }

                let mcp_context = session_state
                    .read()
                    .get_active_session()
                    .and_then(|s| s.active_context.mcp_tools.clone());
                send_prompt_to_llm(prompt_data, mcp_context, hobbes_message_id);
            });
        }
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

                        if event.key() == Key::Enter && !modifiers.contains(Modifiers::SHIFT) {
                            event.prevent_default();
                            if !*has_interacted.read() {
                                on_interaction.call(());
                                has_interacted.set(true);
                            }
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
                                            tracing::info!("---\n[DEBUG] Current Context:\n{}---", context_string);
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
                button {
                    class: "px-5 py-2 bg-purple-600 rounded-full text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors disabled:bg-gray-500",
                    disabled: mcp_context.read().servers.is_empty(),
                    onclick: move |_| {
                        if !*has_interacted.read() {
                            on_interaction.call(());
                            has_interacted.set(true);
                        }
                        send_message()
                    },
                    "Send"
                }
            }
        }
    }
}