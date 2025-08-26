use dioxus::prelude::*;
use futures_util::StreamExt;
use dioxus_free_icons::{Icon, icons::fi_icons};
use std::rc::Rc;
use dioxus::html::geometry::euclid::Rect;
use std::time::Duration;
use tokio::time::sleep;
use crate::components::llm;
use tokio::sync::mpsc;
use pulldown_cmark::{html, Options, Parser};

// Define a simple `Message` struct
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Message {
    pub author: String,
    pub content: String,
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(on_content_resize: EventHandler<Rect<f64, f64>>, on_interaction: EventHandler<()>, on_toggle_sessions: EventHandler<()>) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let mut draft = use_signal(|| "".to_string());
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut has_interacted = use_signal(|| false);

    // Effect to report content size changes and scroll to bottom
    use_effect(move || {
        if let Some(element) = container_element.read().clone() {
            spawn(async move {
                // Wait a tick for the DOM to update
                sleep(Duration::from_millis(10)).await;
                if let Ok(rect) = element.get_client_rect().await {
                    on_content_resize.call(rect.cast_unit());
                }
                // Scroll to bottom
                let _ = document::eval(r#"
                    const el = document.getElementById('message-list');
                    if (el) { el.scrollTop = el.scrollHeight; }
                "#);
            });
        }
    });

    // Coroutine for handling streaming responses
    let response_stream_handler = use_coroutine(move |mut rx: UnboundedReceiver<(mpsc::UnboundedReceiver<String>, usize)>| {
        async move {
            while let Some((mut stream, index)) = rx.next().await {
                while let Some(chunk) = stream.recv().await {
                    let mut state = session_state.write();
                    let active_id = state.active_session_id.clone();
                    if let Some(session) = state.sessions.get_mut(&active_id) {
                        if let Some(message) = session.messages.get_mut(index) {
                            message.content.push_str(&chunk);
                        }
                    }
                }
                // After the stream is finished, save the session
                let state = session_state.read();
                if let Err(e) = state.save() {
                    tracing::error!("Failed to save session state after stream: {}", e);
                }
            }
        }
    });

    // Reusable closure for sending a message
    let mut send_message = move || {
        let user_message = draft.read().clone();
        if user_message.is_empty() {
            return;
        }

        let hobbes_message_index = {
            let mut state = session_state.write();
            if state.active_session_id.is_empty() {
                state.create_session();
            }
            let active_id = state.active_session_id.clone();
            let session = state.sessions.get_mut(&active_id).unwrap();
            session.messages.push(Message {
                author: "User".to_string(),
                content: user_message.clone(),
            });
            session.messages.push(Message {
                author: "Hobbes".to_string(),
                content: "".to_string(),
            });
            session.messages.len() - 1
        };

        if let Err(e) = session_state.read().save() {
            tracing::error!("Failed to save session state after sending message: {}", e);
        }

        draft.set("".to_string());

        let (tx, rx) = mpsc::unbounded_channel::<String>();
        response_stream_handler.send((rx, hobbes_message_index));

        spawn(async move {
            llm::generate_content_stream(user_message, tx).await;
        });
    };


    let root_classes = "flex flex-col bg-gray-900 text-gray-100 rounded-lg shadow-2xl h-full w-full";

    rsx! {
        div {
            class: "{root_classes}",
            onmounted: move |cx| container_element.set(Some(cx.data())),
            div {
                id: "message-list",
                class: "flex-1 overflow-y-auto p-4 space-y-4 min-h-0",
                {
                    let state = session_state.read();
                    if let Some(session) = state.sessions.get(&state.active_session_id) {
                        if session.messages.is_empty() {
                            rsx! { WelcomeMessage {} }
                        } else {
                            rsx! {
                                for message in &session.messages {
                                    MessageBubble { message: message.clone() }
                                }
                            }
                        }
                    } else {
                        rsx! { WelcomeMessage {} }
                    }
                }
            }
            div {
                class: "p-4 border-t border-gray-700 flex-shrink-0",
                onmousedown: |e| e.stop_propagation(),
                div {
                    class: "flex items-center space-x-3",
                    textarea {
                        class: "flex-1 py-2 px-4 rounded-xl bg-gray-800 border border-gray-700 text-gray-100 placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-purple-500 resize-none",
                        rows: "1",
                        placeholder: "Type your message...",
                        value: "{draft}",
                        oninput: move |event| draft.set(event.value()),
                        onkeydown: move |event| {
                            if event.key() == Key::Enter && !event.data.modifiers().contains(Modifiers::META) {
                                event.prevent_default();
                                if !*has_interacted.read() {
                                    on_interaction.call(());
                                    has_interacted.set(true);
                                }
                                send_message();
                            }
                        },
                    }
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
                        class: "px-5 py-2 bg-purple-600 rounded-full text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors",
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
}

// Sub-component for styling individual messages
#[component]
fn MessageBubble(message: Message) -> Element {
    let is_user = message.author == "User";
    let is_thinking = message.author == "Hobbes" && message.content.is_empty();

    let bubble_classes = if is_user {
        "bg-purple-600 text-white self-end ml-auto"
    } else {
        "bg-gray-700 text-gray-200 self-start mr-auto"
    };
    let container_classes = if is_user { "flex justify-end" } else { "flex justify-start" };

    // Parse markdown to HTML
    let parsed_html = use_memo(move || {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        let parser = Parser::new_ext(&message.content, options);
        let mut html_output = String::new();
        html::push_html(&mut html_output, parser);
        html_output
    });

    rsx! {
        div {
            class: "{container_classes}",
            div {
                class: "flex flex-col max-w-xs md:max-w-md",
                div {
                    class: "px-4 py-2 rounded-2xl {bubble_classes}",
                    if is_thinking {
                        ThinkingIndicator {}
                    } else {
                        div {
                            class: "prose prose-sm dark:prose-invert max-w-none",
                            dangerous_inner_html: "{parsed_html}"
                        }
                    }
                }
                div {
                    class: "text-xs text-gray-500 mt-1 px-2",
                    class: if is_user { "text-right" } else { "text-left" },
                    "{message.author}"
                }
            }
        }
    }
}

#[component]
fn ThinkingIndicator() -> Element {
    rsx! {
        div {
            class: "flex items-center justify-center space-x-1",
            span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-fast" },
            span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-medium" },
            span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-slow" },
        }
    }
}

#[component]
fn WelcomeMessage() -> Element {
    rsx! {
        div {
            class: "flex flex-col items-center justify-center h-full text-gray-500",
            svg {
                class: "w-24 h-24 mb-4",
                fill: "none",
                stroke: "currentColor",
                view_box: "0 0 24 24",
                xmlns: "http://www.w3.org/2000/svg",
                path {
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    stroke_width: "2",
                    d: "M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z"
                }
            }
            p {
                class: "text-lg",
                "Start a new conversation"
            }
        }
    }
}