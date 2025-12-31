use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

use crate::components::tool_call_display::{PermissionPrompt, ToolCallDisplay};

use super::chat::{MessageBubble, WelcomeMessage};
use super::shared::MessageContent;

const INITIAL_MESSAGES_TO_SHOW: usize = 20;

#[component]
pub fn MessageList(stream_update_trigger: Signal<i32>, show_scroll_button: Signal<bool>, on_delete: EventHandler<Uuid>, on_comment: EventHandler<()>) -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let mut visible_message_count = use_signal(|| INITIAL_MESSAGES_TO_SHOW);
    let _ = stream_update_trigger.read();

    rsx! {
        div {
            class: "relative flex-1 min-h-0",
            div {
                id: "message-list",
                class: "overflow-y-auto p-4 space-y-4 h-full",
                onscroll: move |_| {
                    let mut show_scroll_button = show_scroll_button;
                    let mut visible_message_count = visible_message_count;
                    let session_state = session_state;

                    spawn(async move {
                        let scroll_info = if let Ok(result) = document::eval(r#"
                            const el = document.getElementById('message-list');
                            if (el) {
                                const isAtTop = el.scrollTop === 0;
                                const threshold = 10;
                                const isAtBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
                                return { isAtTop, isAtBottom };
                            }
                            return { isAtTop: false, isAtBottom: true };
                        "#).await {
                            result
                        } else {
                            serde_json::from_str("{\"isAtTop\":false, \"isAtBottom\":true}").unwrap()
                        };

                        let is_at_top = scroll_info.get("isAtTop").unwrap().as_bool().unwrap_or(false);
                        let is_at_bottom = scroll_info.get("isAtBottom").unwrap().as_bool().unwrap_or(true);

                        // The parent now controls visibility on content change.
                        // This handler just hides the button if the user scrolls down manually.
                        if is_at_bottom {
                            show_scroll_button.set(false);
                        } else {
                            show_scroll_button.set(true);
                        }

                        if is_at_top {
                            let total_messages = session_state.read().get_active_session().map_or(0, |s| s.messages.len());
                            if *visible_message_count.read() < total_messages {
                                let _ = document::eval(r#"
                                    const el = document.getElementById('message-list');
                                    if (el) {
                                        window.prevScrollHeight = el.scrollHeight;
                                        window.prevScrollTop = el.scrollTop;
                                    }
                                "#).await;

                                let current_count = *visible_message_count.read();
                                visible_message_count.set(current_count + INITIAL_MESSAGES_TO_SHOW);

                                sleep(Duration::from_millis(20)).await;
                                let _ = document::eval(r#"
                                    const el = document.getElementById('message-list');
                                    if (el && window.prevScrollHeight) {
                                        const newScrollHeight = el.scrollHeight;
                                        const heightDifference = newScrollHeight - window.prevScrollHeight;
                                        el.scrollTop = window.prevScrollTop + heightDifference;
                                        delete window.prevScrollHeight;
                                        delete window.prevScrollTop;
                                    }
                                "#).await;
                            }
                        }
                    });
                },
                {
                    let state = session_state.read();
                    if let Some(session) = state.sessions.get(&state.active_session_id) {
                        let total_messages = session.messages.len();
                        
                        // Sort messages by created_at timestamp to ensure chronological order
                        let mut sorted_messages = session.messages.clone();
                        sorted_messages.sort_by_key(|m| m.created_at);
                        
                        let messages_to_render = sorted_messages.iter().skip(total_messages.saturating_sub(*visible_message_count.read())).collect::<Vec<_>>();

                        if session.messages.is_empty() {
                            rsx! { WelcomeMessage {} }
                        } else {
                            rsx! {
                                if total_messages > *visible_message_count.read() {
                                    div {
                                        class: "flex justify-center",
                                        button {
                                            class: "text-sm text-purple-400 hover:text-purple-300 focus:outline-none",
                                            onclick: move |_| {
                                                let current_count = *visible_message_count.read();
                                                visible_message_count.set(current_count + INITIAL_MESSAGES_TO_SHOW);
                                            },
                                            "Load More"
                                        }
                                    }
                                }
                                for message in messages_to_render {
                                    match &message.content {
                                        MessageContent::Text { content: text, .. } => {
                                            let stream_manager = consume_context::<crate::components::stream_manager::StreamManagerContext>();
                                            let is_generating = stream_manager.is_generating(&message.id);
                                            let should_render = !text.is_empty() || is_generating || !message.attachments.is_empty();

                                            if should_render {
                                                rsx! {
                                                    MessageBubble {
                                                        key: "{message.id}",
                                                        message: message.clone(),
                                                        on_content_update: move |_| stream_update_trigger += 1,
                                                        on_selection: move |data| tracing::debug!("Selection event: {:?}", data),
                                                        on_delete: {
                                                            let msg_id = message.id;
                                                            move |_| on_delete.call(msg_id)
                                                        },
                                                        on_comment: move |_| on_comment.call(()),
                                                    }
                                                }
                                            } else {
                                                rsx! {}
                                            }
                                        },
                                        MessageContent::ToolCall(tool_call) => {
                                            let bubble_classes = "bg-gray-700 text-gray-200 self-start mr-auto";
                                            let container_classes = "flex justify-start";
                                            let author_classes = format!(
                                                "text-xs text-gray-500 mt-1 px-2 {}",
                                                "text-left"
                                            );
                                            rsx! {
                                                div {
                                                    key: "{message.id}",
                                                    class: "{container_classes} w-full",
                                                    div {
                                                        class: "flex flex-col max-w-2/3 min-w-0",
                                                        div {
                                                            class: "relative group rounded-2xl {bubble_classes}",
                                                            ToolCallDisplay { tool_call: tool_call.clone() }
                                                        }
                                                        div {
                                                            class: "{author_classes}",
                                                            "{message.author}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        MessageContent::PermissionRequest(tool_call) => {
                                            let container_classes = "flex justify-start";
                                            let author_classes = "text-xs text-gray-500 mt-1 px-2 text-left";
                                            rsx! {
                                                div {
                                                    key: "{message.id}",
                                                    class: "{container_classes} w-full",
                                                    div {
                                                        class: "flex flex-col max-w-2/3 min-w-0",
                                                        PermissionPrompt { tool_call: tool_call.clone() },
                                                        div {
                                                            class: "{author_classes}",
                                                            "System"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        MessageContent::Error { .. } => {
                                            rsx! {
                                                MessageBubble {
                                                    key: "{message.id}",
                                                    message: message.clone(),
                                                    on_content_update: move |_| stream_update_trigger += 1,
                                                    on_selection: move |data| tracing::debug!("Selection event: {:?}", data),
                                                    on_delete: {
                                                        let msg_id = message.id;
                                                        move |_| on_delete.call(msg_id)
                                                    },
                                                    on_comment: move |_| on_comment.call(()),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        rsx! { WelcomeMessage {} }
                    }
                }
                if *show_scroll_button.read() {
                    button {
                        class: "absolute bottom-4 right-4 z-10 p-2 bg-primary-500 text-white rounded-full shadow-lg hover:bg-primary-600 focus:outline-none focus:ring-2 focus:ring-primary-500 transition-opacity duration-300 ease-in-out",
                        onclick: move |_| {
                            let _ = document::eval(r#"
                                const el = document.getElementById('message-list');
                                if (el) { el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' }); }
                            "#);
                        },
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiChevronDown
                        }
                    }
                }
            }
        }
    }
}