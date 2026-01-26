use crate::components::focus_context::FocusContext;
use crate::components::syntax_highlighter::highlight_json;
use crate::hotkey::matches_hotkey;
use crate::session::ActiveContext;
use crate::settings::Settings;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fi_icons;
use dioxus_free_icons::Icon;

#[component]
pub fn NewChatMemoryModal(
    is_visible: Signal<bool>,
    initial_context: ActiveContext,
    optimization_summary: Signal<Option<String>>,
    on_start_chat: EventHandler<ActiveContext>,
    on_optimize_memory: EventHandler<ActiveContext>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut json_content = use_signal(String::new);
    let mut error_message = use_signal(|| Option::<String>::None);
    let mut focus_context = use_context::<Signal<FocusContext>>();
    let settings = use_context::<Signal<Settings>>();

    // Track previous context to detect external updates (like from optimization)
    let mut last_processed_context = use_signal(String::new);

    // Initialize content when modal becomes visible or initial context
    let initial_context_effect = initial_context.clone();
    use_effect(move || {
        if *is_visible.read() {
            tracing::info!("NewChatMemoryModal became visible");
            // Claim focus ownership
            focus_context.set(FocusContext::NewChatMemoryModal);

            // Check if context has changed externally (e.g. returning from optimization)
            // Create a view-only context that excludes tool definitions for the JSON editor
            let mut display_context = initial_context_effect.clone();
            display_context.mcp_tools = None;
            display_context.tools = None;

            let new_json = serde_json::to_string_pretty(&display_context).unwrap_or_default();
            if *last_processed_context.read() != new_json {
                json_content.set(new_json.clone());
                last_processed_context.set(new_json);
                error_message.set(None);
            }
        } else {
            // Release focus ownership when modal closes
            focus_context.set(FocusContext::ChatInput);
        }
    });

    let mut trigger_optimize = move || {
        let content = json_content.read().clone();
        match serde_json::from_str::<ActiveContext>(&content) {
            Ok(ctx) => on_optimize_memory.call(ctx),
            Err(e) => error_message.set(Some(format!("Cannot optimize invalid JSON: {}", e))),
        }
    };

    let mut submit_session = move || {
        let content = json_content.read().clone();
        tracing::debug!("NewChatMemoryModal::submit_session called");
        match serde_json::from_str::<ActiveContext>(&content) {
            Ok(mut valid_context) => {
                tracing::info!("NewChatMemoryModal::submit_session - valid context parsed, restoring tools");
                
                // Restore logic: usage state (mcp_tools/tools) must optionally persist from source
                // if not present in the editor (which we explicitly stripped above).
                // If the user *manually* added tools in the JSON (unlikely but possible), we accept them.
                if valid_context.mcp_tools.is_none() {
                    valid_context.mcp_tools = initial_context.mcp_tools.clone();
                }
                if valid_context.tools.is_none() {
                    valid_context.tools = initial_context.tools.clone();
                }

                on_start_chat.call(valid_context);
            }
            Err(e) => {
                tracing::error!(
                    "NewChatMemoryModal::submit_session - failed to parse JSON: {}",
                    e
                );
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
    
    // Clone for the onclick handler
    let mut submit_session_click = submit_session.clone();

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
                } else if matches_hotkey(&evt, &settings.read().hotkeys.submit_chat) {
                    tracing::info!("NewChatMemoryModal (Outer) submitting via Hotkey");
                    evt.prevent_default();
                    submit_session();
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
                        {
                            let context_size = json_content.read().len();
                            let estimated_tokens = context_size / 4;
                            rsx! {
                                p {
                                    class: "text-xs text-gray-500 mt-1",
                                    "Context size: ~{estimated_tokens} tokens ({context_size} chars)"
                                }
                            }
                        }
                    }
                    button {
                        class: "text-gray-400 hover:text-white transition-colors",
                        onclick: move |_| on_cancel.call(()),
                        Icon { width: 24, height: 24, icon: fi_icons::FiX }
                    }
                }

                // Optimization Summary Banner (if present)
                if let Some(summary) = optimization_summary.read().as_ref() {
                    div {
                        class: "mx-4 my-3 p-3 bg-primary-900/30 border border-primary-700/50 rounded-lg flex items-start gap-2 animate-in slide-in-from-top-2",
                        div { class: "text-primary-400 mt-0.5", "✨" }
                        div {
                            class: "flex-1",
                            p { class: "text-xs font-semibold text-primary-400 mb-0.5", "Memory Optimization Result" }
                            p { class: "text-sm text-gray-200", "{summary}" }
                        }
                        button {
                            class: "text-gray-400 hover:text-white transition-colors p-1",
                            onclick: move |_| optimization_summary.set(None),
                            Icon { width: 14, height: 14, icon: fi_icons::FiX }
                        }
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
                    class: "p-4 border-t border-primary-700 bg-dark-section flex justify-between items-center",

                    // Left Side: Optimization Control
                    button {
                        class: "flex items-center gap-2 px-3 py-2 text-yellow-500 hover:text-yellow-400 hover:bg-yellow-500/10 rounded transition-colors text-sm font-medium",
                        onclick: move |_| trigger_optimize(),
                        "⚡ Optimize Memory"
                    }

                    // Right Side: Action Buttons
                    div {
                        class: "flex gap-3",
                        button {
                            class: "px-4 py-2 text-gray-300 hover:text-white font-medium transition-colors",
                            onclick: move |_| on_cancel.call(()),
                            "Cancel"
                        }
                        button {
                            class: "px-6 py-2 bg-primary-600 hover:bg-primary-500 text-white rounded-md font-semibold shadow-lg shadow-primary-900/20 transition-all hover:scale-105 active:scale-95",
                            onclick: move |_| submit_session_click(),
                            "Start New Session"
                        }
                    }
                }
            }
        }
    }
}
