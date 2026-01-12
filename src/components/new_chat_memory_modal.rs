use dioxus::prelude::*;
use crate::session::ActiveContext;
use crate::components::syntax_highlighter::highlight_json;
use crate::components::focus_context::FocusContext;
use dioxus_free_icons::icons::fi_icons;
use dioxus_free_icons::Icon;

#[component]
pub fn NewChatMemoryModal(
    is_visible: Signal<bool>,
    initial_context: ActiveContext,
    on_start_chat: EventHandler<ActiveContext>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut json_content = use_signal(|| String::new());
    let mut error_message = use_signal(|| Option::<String>::None);
    let mut focus_context = use_context::<Signal<FocusContext>>();
    let mut chat_command = use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();

    // Initialize content when modal becomes visible or initial context changes
    use_effect(move || {
        if *is_visible.read() {
            // Claim focus ownership
            focus_context.set(FocusContext::NewChatMemoryModal);
            
            match serde_json::to_string_pretty(&initial_context) {
                Ok(json) => json_content.set(json),
                Err(e) => {
                    tracing::error!("Failed to serialize initial context: {}", e);
                    json_content.set("{}".to_string());
                }
            }
            error_message.set(None);
        } else {
            // Release focus ownership when modal closes
            focus_context.set(FocusContext::ChatInput);
        }
    });
    
    let mut submit_session = move || {
        let content = json_content.read().clone();
        tracing::error!("DEBUG: NewChatMemoryModal::submit_session called");
        match serde_json::from_str::<ActiveContext>(&content) {
            Ok(valid_context) => {
                tracing::info!("NewChatMemoryModal::submit_session - valid context parsed, calling on_start_chat");
                on_start_chat.call(valid_context);
            }
            Err(e) => {
                tracing::error!("NewChatMemoryModal::submit_session - failed to parse JSON: {}", e);
                error_message.set(Some(format!("Generic JSON error: {}", e)));
                 // Try to parse as generic Value to give better error if it's just schema mismatch vs syntax
                if let Err(syntax_err) = serde_json::from_str::<serde_json::Value>(&content) {
                     error_message.set(Some(format!("Syntax Error: {}", syntax_err)));
                } else {
                     // If syntax is valid but schema matches failed
                     error_message.set(Some(format!("Schema Mismatch: The JSON structure doesn't match the expected memory format. ({})", e)));
                }
            }
        }
    };

    // Listen for ChatCommands (e.g. from global hotkeys)
    use_effect(move || {
        let cmd_opt = chat_command.read().clone();
        if let Some(cmd) = cmd_opt {
            if *is_visible.read() {
                match cmd {
                    crate::components::chat_input::ChatCommand::SubmitModal => {
                        tracing::info!("NewChatMemoryModal received SubmitModal command");
                        submit_session();
                        chat_command.set(None); // Reset command
                    }
                    crate::components::chat_input::ChatCommand::CloseModal => {
                        tracing::info!("NewChatMemoryModal received CloseModal command");
                        on_cancel.call(());
                        chat_command.set(None); // Reset command
                    }
                     _ => {}
                }
            }
        }
    });

    if !*is_visible.read() {
        return rsx! {};
    }

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm",
            tabindex: "0",
            onkeydown: move |evt: KeyboardEvent| {
                tracing::debug!("NewChatMemoryModal (Outer) onkeydown - Key: {:?}, Modifiers: {:?}", evt.key(), evt.modifiers());
                if evt.key() == Key::Escape {
                    on_cancel.call(());
                } else if evt.key() == Key::Enter {
                    let modifiers = evt.modifiers();
                    if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                        tracing::info!("NewChatMemoryModal (Outer) submitting via Cmd+Enter");
                        evt.prevent_default();
                        submit_session();
                    }
                }
            },
            div {
                class: "bg-dark-card border border-primary-700 rounded-lg shadow-xl w-[800px] h-[80vh] flex flex-col overflow-hidden animate-in fade-in zoom-in duration-200",
                
                // Header
                div {
                    class: "p-4 border-b border-primary-700 flex justify-between items-center bg-dark-section",
                    div {
                        h2 { class: "text-lg font-semibold text-white flex items-center gap-2",
                            Icon { width: 20, height: 20, icon: fi_icons::FiCpu }
                            "New Chat with Memory"
                        }
                        p { class: "text-xs text-gray-400 mt-1", 
                            "Edit the short-term memory (persona, instructions, etc.) for the new session." 
                        }
                    }
                    button {
                        class: "text-gray-400 hover:text-white transition-colors",
                        onclick: move |_| on_cancel.call(()),
                        Icon { width: 24, height: 24, icon: fi_icons::FiX }
                    }
                }

                // Body
                div {
                    class: "flex-1 flex flex-col p-4 overflow-hidden bg-dark-bg",
                    
                    // Toolbar
                    div {
                        class: "flex justify-between items-center mb-2",
                        div {
                            class: "flex gap-2",
                            button {
                                class: "text-xs px-2 py-1 bg-dark-input hover:bg-primary-900 border border-primary-700 rounded text-gray-300 transition-colors",
                                onclick: move |_| {
                                    let content = json_content.read().clone();
                                    match serde_json::from_str::<serde_json::Value>(&content) {
                                        Ok(parsed) => {
                                            if let Ok(formatted) = serde_json::to_string_pretty(&parsed) {
                                                json_content.set(formatted);
                                                error_message.set(None);
                                            }
                                        }
                                        Err(e) => {
                                            error_message.set(Some(format!("Invalid JSON: {}", e)));
                                        }
                                    }
                                },
                                "Format JSON"
                            }
                            a {
                                class: "text-xs px-2 py-1 flex items-center gap-1 text-primary-400 hover:text-primary-300 transition-colors",
                                href: "https://www.json.org/json-en.html", // A generic useful link, or a specific doc link if preferred
                                target: "_blank",
                                Icon { width: 12, height: 12, icon: fi_icons::FiHelpCircle }
                                "JSON Guide"
                            }
                        }
                    }

                    // Editor
                    div {
                        class: "flex-1 relative bg-dark-section rounded-md border border-gray-700 overflow-hidden",
                        id: "memory-json-editor-container",
                        
                        // Highlighted layer
                        pre {
                            class: "absolute inset-0 p-4 text-sm font-mono pointer-events-none whitespace-pre-wrap break-words overflow-auto",
                            id: "memory-json-highlight",
                            code {
                                dangerous_inner_html: "{highlight_json(json_content.read().clone())}"
                            }
                        }
                        
                        // Editable layer
                        textarea {
                            class: "absolute inset-0 w-full h-full p-4 bg-transparent font-mono text-sm text-transparent caret-white border-0 focus:outline-none resize-none overflow-auto whitespace-pre-wrap break-words",
                            id: "memory-json-editor",
                            style: "color: transparent;",
                            value: "{json_content}",
                            spellcheck: false,
                            autofocus: true,
                            onmounted: move |evt| {
                                let mounted = evt.data();
                                spawn(async move {
                                    let _ = mounted.set_focus(true).await;
                                });
                            },
                            oninput: move |e| {
                                json_content.set(e.value());
                                error_message.set(None);
                            },
                            onscroll: move |_| {
                                let _ = document::eval(r#"
                                    const editor = document.getElementById('memory-json-editor');
                                    const highlight = document.getElementById('memory-json-highlight');
                                    if (editor && highlight) {
                                        highlight.scrollTop = editor.scrollTop;
                                        highlight.scrollLeft = editor.scrollLeft;
                                    }
                                "#);
                            },
                        }
                    }

                    // Error Message
                    if let Some(msg) = error_message.read().as_ref() {
                        div { 
                            class: "mt-2 p-2 bg-red-900/50 border border-red-700 text-red-200 rounded text-sm flex items-start gap-2 animate-in slide-in-from-bottom-2",
                            Icon { width: 16, height: 16, icon: fi_icons::FiAlertCircle, class: "mt-0.5 min-w-[16px]" }
                            span { "{msg}" }
                        }
                    }
                }

                // Footer
                div {
                    class: "p-4 border-t border-primary-700 bg-dark-section flex justify-end gap-3",
                    button {
                        class: "px-4 py-2 text-gray-300 hover:text-white font-medium transition-colors",
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        class: "px-6 py-2 bg-primary-600 hover:bg-primary-500 text-white rounded-md font-semibold shadow-lg shadow-primary-900/20 transition-all hover:scale-105 active:scale-95",
                        onclick: move |_| submit_session(),
                        "Start New Session"
                    }
                }
            }
        }
    }
}
