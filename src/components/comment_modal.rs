use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};
use crate::components::chat::{Comment, Message};
use uuid::Uuid;

#[component]
pub fn CommentModal(
    message: Message,
    on_close: EventHandler<()>,
    on_save: EventHandler<Vec<Comment>>,
) -> Element {
    let mut new_comment_text = use_signal(|| String::new());
    let mut selected_text = use_signal(|| String::new());
    let mut comments = use_signal(|| message.comments.clone());
    
    // Extract text content from message
    let message_text = match &message.content {
        crate::components::shared::MessageContent::Text { content, .. } => content.clone(),
        crate::components::shared::MessageContent::Error { message } => format!("[Error: {}]", message),
        _ => String::new(),
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50",
            tabindex: "0",
            autofocus: true,
            onmounted: move |evt| {
                let mounted = evt.data();
                spawn(async move {
                    let _ = mounted.set_focus(true).await;
                });
            },
            onclick: move |_| on_close.call(()),
            onkeydown: {
                let on_close = on_close.clone();
                let on_save = on_save.clone();
                move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape {
                        on_close.call(());
                    } else if evt.key() == Key::Enter {
                        let modifiers = evt.modifiers();
                        if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                            evt.prevent_default();
                            on_save.call(comments());
                            on_close.call(());
                        }
                    }
                }
            },
            div {
                class: "bg-dark-card rounded-lg p-6 max-w-2xl w-full mx-4 max-h-[80vh] overflow-y-auto",
                tabindex: "0",
                onclick: move |e| e.stop_propagation(),
                onkeydown: {
                    let on_close = on_close.clone();
                    let on_save = on_save.clone();
                    move |evt: KeyboardEvent| {
                        if evt.key() == Key::Escape {
                            on_close.call(());
                        } else if evt.key() == Key::Enter {
                            let modifiers = evt.modifiers();
                            if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                                evt.prevent_default();
                                on_save.call(comments());
                                on_close.call(());
                            }
                        }
                    }
                },
                
                // Header
                div {
                    class: "flex justify-between items-center mb-4",
                    h2 {
                        class: "text-xl font-semibold text-dark-text",
                        "Add Comment"
                    }
                    button {
                        class: "text-gray-400 hover:text-white",
                        onclick: move |_| on_close.call(()),
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiX
                        }
                    }
                }
                
                // Message content (read-only)
                div {
                    class: "mb-4",
                    label {
                        class: "block text-sm font-medium text-gray-300 mb-2",
                        "Message Content"
                    }
                    div {
                        class: "bg-dark-bg p-3 rounded-lg text-sm text-gray-300 max-h-40 overflow-y-auto",
                        "{message_text}"
                    }
                }
                
                // Text selection input
                div {
                    class: "mb-4",
                    label {
                        class: "block text-sm font-medium text-gray-300 mb-2",
                        "Quoted Text (copy from above)"
                    }
                    input {
                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                        r#type: "text",
                        placeholder: "Paste the text you want to comment on...",
                        value: "{selected_text}",
                        oninput: move |e| selected_text.set(e.value())
                    }
                }
                
                // Comment input
                div {
                    class: "mb-4",
                    label {
                        class: "block text-sm font-medium text-gray-300 mb-2",
                        "Your Comment"
                    }
                    textarea {
                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                        rows: "3",
                        placeholder: "Add your comment...",
                        value: "{new_comment_text}",
                        oninput: move |e| new_comment_text.set(e.value())
                    }
                }
                
                // Add comment button
                button {
                    class: "w-full mb-4 px-4 py-2 bg-primary-500 hover:bg-primary-600 text-white rounded-md transition-colors",
                    onclick: move |_| {
                        if !selected_text().is_empty() && !new_comment_text().is_empty() {
                            let new_comment = Comment {
                                id: Uuid::new_v4().to_string(),
                                text_selection: selected_text(),
                                start_offset: 0, // Not used in simple approach
                                end_offset: 0,   // Not used in simple approach
                                comment: new_comment_text(),
                            };
                            comments.write().push(new_comment);
                            selected_text.set(String::new());
                            new_comment_text.set(String::new());
                        }
                    },
                    disabled: selected_text().is_empty() || new_comment_text().is_empty(),
                    "Add Comment"
                }
                
                // Existing comments list
                if !comments().is_empty() {
                    div {
                        class: "mb-4",
                        h3 {
                            class: "text-sm font-medium text-gray-300 mb-2",
                            "Comments ({comments().len()})"
                        }
                        for comment in comments().iter() {
                            div {
                                key: "{comment.id}",
                                class: "bg-dark-bg p-3 rounded-lg mb-2",
                                div {
                                    class: "flex justify-between items-start mb-2",
                                    div {
                                        class: "text-xs text-gray-400 italic",
                                        "\"{comment.text_selection}\""
                                    }
                                    button {
                                        class: "text-red-400 hover:text-red-300",
                                        onclick: {
                                            let comment_id = comment.id.clone();
                                            move |_| {
                                                comments.write().retain(|c| c.id != comment_id);
                                            }
                                        },
                                        Icon {
                                            width: 14,
                                            height: 14,
                                            icon: fi_icons::FiTrash2
                                        }
                                    }
                                }
                                div {
                                    class: "text-sm text-gray-200",
                                    "{comment.comment}"
                                }
                            }
                        }
                    }
                }
                
                // Action buttons
                div {
                    class: "flex justify-end space-x-2",
                    button {
                        class: "px-4 py-2 bg-gray-700 hover:bg-gray-600 text-white rounded-md transition-colors",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-primary-500 hover:bg-primary-600 text-white rounded-md transition-colors",
                        onclick: move |_| {
                            on_save.call(comments());
                            on_close.call(());
                        },
                        "Save & Close"
                    }
                }
            }
        }
    }
}
