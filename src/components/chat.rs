use dioxus::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;
use dioxus_free_icons::{Icon, icons::fi_icons};
use std::rc::Rc;
use dioxus::html::geometry::euclid::Rect;
use std::time::Duration;
use tokio::time::sleep;
use pulldown_cmark::{html, Options, Parser, Event, Tag, TagEnd};
use crate::components::stream_manager::StreamManagerContext;
use lazy_static::lazy_static;
use syntect::easy::HighlightLines;
use syntect::highlighting::{ThemeSet, Theme};
use syntect::parsing::SyntaxSet;
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use feature_clipboard::copy_to_clipboard;
use crate::context::prompt_builder::PromptBuilder;
use crate::processing::conversation_processor::ConversationProcessor;
// Define a simple `Message` struct
use serde::{Deserialize, Serialize};
use crate::settings::Settings;
use super::shared::{MessageContent};
use super::tool_call_display::ToolCallDisplay;
lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: uuid::Uuid,
    pub author: String,
    pub content: MessageContent,
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(on_content_resize: EventHandler<Rect<f64, f64>>, on_interaction: EventHandler<()>, on_toggle_sessions: EventHandler<()>, on_toggle_settings: EventHandler<()>) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut draft = use_signal(|| "".to_string());
    use_context_provider(|| draft);
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut has_interacted = use_signal(|| false);
    let is_sending = use_signal(|| false);
    let stream_manager = consume_context::<StreamManagerContext>();
    const INITIAL_MESSAGES_TO_SHOW: usize = 20;
    let mut show_scroll_button = use_signal(|| false);
    let mut is_initial_load = use_signal(|| true);
    let mut visible_message_count = use_signal(|| INITIAL_MESSAGES_TO_SHOW);
    // Effect to report content size changes and conditionally scroll to bottom
    use_effect(move || {
        // By reading the session state here, the effect becomes dependent on it.
        // Any change to messages will cause this to re-run.
        let _ = session_state.read();

        if let Some(element) = container_element.read().clone() {
            spawn(async move {
                // A short delay allows the DOM to render the new message before we measure/scroll.
                sleep(Duration::from_millis(20)).await;

                // First, check if the user is already near the bottom.
                let is_near_bottom = if let Ok(result) = document::eval(r#"
                    const el = document.getElementById('message-list');
                    if (el) {
                        // If the user is within 50px of the bottom, we consider them "at the bottom".
                        const threshold = el.clientHeight * 0.2; // 20% of the viewport height
                        return el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
                    }
                    // Default to true if the element doesn't exist yet, so we scroll on the first load.
                    return true;
                "#).await {
                    result.as_bool().unwrap_or(true)
                } else {
                    true // Also default to true if the eval fails.
                };

                // Set the visibility of the scroll button based on whether the user is near the bottom.
                show_scroll_button.set(!is_near_bottom);

                // On the very first load, we always scroll to the bottom.
                // On subsequent loads, we only scroll if the user was already near the bottom.
                if *is_initial_load.read() || is_near_bottom {
                    let _ = document::eval(r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTop = el.scrollHeight; }
                    "#).await;
                    if *is_initial_load.read() {
                        is_initial_load.set(false);
                    }
                }
                // Finally, notify the parent component of the new content size.
                if let Ok(rect) = element.get_client_rect().await {
                    on_content_resize.call(rect.cast_unit());
                }
            });
        }
    });

    // Effect to update session state with MCP context when it changes
    use_effect(move || {
        let mcp_context_reader = mcp_context.read();
        if !mcp_context_reader.servers.is_empty() {
            let mut state = session_state.write();
            if let Some(session) = state.get_active_session_mut() {
                session.active_context.mcp_tools = Some(mcp_context_reader.clone());
                tracing::info!("MCP context reactively loaded into session state.");
            }
        }
    });

    // Reusable closure for sending a message
    let send_prompt_to_llm = {
        // Capture signals which are all `Copy`
        let is_sending = is_sending;
        let session_state = session_state;
        let stream_manager = stream_manager;
        let settings = settings;

        move |prompt_data: crate::context::prompt_builder::LlmPrompt, mcp_context: Option<crate::mcp::manager::McpContext>| {
            spawn(async move {
                // Now clone/read them inside the async block
                let mut is_sending = is_sending;
                let mut session_state = session_state;
                let stream_manager = stream_manager;
                let settings = settings.read().clone();

                is_sending.set(true);
                tracing::info!("Lock ACQUIRED.");

                let (tx, mut rx) = mpsc::unbounded_channel::<()>();
                let hobbes_message_id = Uuid::new_v4();

                {
                    let mut state = session_state.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.messages.push(Message {
                            id: hobbes_message_id,
                            author: "Hobbes".to_string(),
                            content: MessageContent::Text("".to_string()),
                        });
                    }
                }

                let api_key = settings.api_key.clone().unwrap_or_else(|| {
                    std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set in settings or environment")
                });

                let on_complete = move || {
                    let _ = tx.send(());
                };

                stream_manager.start_stream(
                    api_key,
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

    // Effect to trigger LLM feedback loop after a tool call completes
    use_effect({
        let session_state = session_state.clone();
        let send_prompt_to_llm = send_prompt_to_llm.clone();
        let settings = settings.read().clone();
        move || {
            let messages = session_state.read().get_active_session().map_or(vec![], |s| s.messages.clone());
            if let Some(last_message) = messages.last() {
                if let MessageContent::ToolCall(tc) = &last_message.content {
                    if tc.status == super::shared::ToolCallStatus::Completed {
                        // A tool call just finished. Time to send the result back to the LLM.
                        let tool_response_prompt = format!(
                            "<tool_response>\n<server_name>{}</server_name>\n<tool_name>{}</tool_name>\n<response>{}</response>\n</tool_response>",
                            tc.server_name, tc.tool_name, tc.response
                        );
                        
                        let state = session_state.read();
                        if let Some(session) = state.get_active_session() {
                             let builder = PromptBuilder::new(session, &settings);
                             let prompt_data = builder.build_prompt(tool_response_prompt, None);
                             let mcp_context = session.active_context.mcp_tools.clone();
                             send_prompt_to_llm(prompt_data, mcp_context);
                        }
                    }
                }
            }
        }
    });

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

                {
                    let mut state = session_state.write();
                    if state.active_session_id.is_empty() {
                        state.create_session();
                    }
                    if let Some(session) = state.get_active_session_mut() {
                        session.messages.push(Message {
                            id: Uuid::new_v4(),
                            author: "User".to_string(),
                            content: MessageContent::Text(user_message.clone()),
                        });
                    }
                }

                let prompt_data = {
                    let mcp_context = mcp_manager.read().get_mcp_context().await;
                    let (user_prompt, conversation_summary) = {
                        let mut session_for_processing = session_state.read().get_active_session().cloned().unwrap();
                        let processor = ConversationProcessor::new();
                        let prompt = processor.process_and_respond(&mut session_for_processing, &settings).await;
                        (prompt, session_for_processing.active_context.conversation_summary)
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
                    let last_agent_message = session.messages.iter().filter(|m| m.author == "Hobbes").last().and_then(|m| match &m.content {
                        MessageContent::Text(text) => Some(text.clone()),
                        _ => None,
                    });

                    let builder = PromptBuilder::new(session, &settings);
                    builder.build_prompt(user_prompt, last_agent_message)
                };

                if let Err(e) = session_state.read().save() {
                    tracing::error!("Failed to save session state: {}", e);
                }

                let mcp_context = session_state.read().get_active_session().and_then(|s| s.active_context.mcp_tools.clone());
                send_prompt_to_llm(prompt_data, mcp_context);
            });
        }
    };


    let root_classes = "flex flex-col bg-gray-900 text-gray-100 rounded-lg shadow-2xl h-full w-full";

    rsx! {
        div {
            class: "{root_classes}",
            onmounted: move |cx| container_element.set(Some(cx.data())),
            div {
                class: "relative flex-1 min-h-0", // Container for positioning
                div {
                    id: "message-list",
                    class: "flex-1 overflow-y-auto p-4 space-y-4 min-h-0 h-full", // Ensure it fills the container
                    onscroll: move |_| {
                        let mut show_scroll_button = show_scroll_button.clone();
                        let mut visible_message_count = visible_message_count.clone();
                        let session_state = session_state.clone();

                        spawn(async move {
                            // Check scroll position for both top and bottom
                            let scroll_info = if let Ok(result) = document::eval(r#"
                                const el = document.getElementById('message-list');
                                if (el) {
                                    const isAtTop = el.scrollTop === 0;
                                    const threshold = 10;
                                    const isAtBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
                                    return { isAtTop, isAtBottom };
                                }
                                return { isAtTop: false, isAtBottom: true }; // Default state
                            "#).await {
                                result
                            } else {
                                serde_json::from_str("{\"isAtTop\":false, \"isAtBottom\":true}").unwrap()
                            };

                            let is_at_top = scroll_info.get("isAtTop").unwrap().as_bool().unwrap_or(false);
                            let is_at_bottom = scroll_info.get("isAtBottom").unwrap().as_bool().unwrap_or(true);

                            // Show the scroll-to-bottom button if the user is NOT at the bottom
                            show_scroll_button.set(!is_at_bottom);

                            // If user scrolls to the top, load more messages while preserving scroll position
                            if is_at_top {
                                let total_messages = session_state.read().get_active_session().map_or(0, |s| s.messages.len());
                                if *visible_message_count.read() < total_messages {
                                    // 1. Get current scroll state BEFORE adding new messages
                                    let _ = document::eval(r#"
                                        const el = document.getElementById('message-list');
                                        if (el) {
                                            window.prevScrollHeight = el.scrollHeight;
                                            window.prevScrollTop = el.scrollTop;
                                        }
                                    "#).await;

                                    // 2. Load more messages
                                    let current_count = *visible_message_count.read();
                                    visible_message_count.set(current_count + INITIAL_MESSAGES_TO_SHOW);

                                    // 3. After the next render, adjust scroll position
                                    sleep(Duration::from_millis(20)).await; // Give it a moment to render
                                    let _ = document::eval(r#"
                                        const el = document.getElementById('message-list');
                                        if (el && window.prevScrollHeight) {
                                            const newScrollHeight = el.scrollHeight;
                                            const heightDifference = newScrollHeight - window.prevScrollHeight;
                                            el.scrollTop = window.prevScrollTop + heightDifference;
                                            // Clean up global variables
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
                            let messages_to_render = session.messages.iter().skip(total_messages.saturating_sub(*visible_message_count.read())).collect::<Vec<_>>();

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
                                            MessageContent::Text(_) => rsx! {
                                                MessageBubble {
                                                    key: "{message.id}",
                                                    message: message.clone()
                                                }
                                            },
                                            MessageContent::ToolCall(tool_call) => {
                                                // Replicating the bubble structure here for tool calls
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
                                                            class: "flex flex-col max-w-2/3",
                                                            div {
                                                                class: "relative group px-4 py-2 rounded-2xl {bubble_classes}",
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
                                        }
                                    }
                                }
                            }
                        } else {
                            rsx! { WelcomeMessage {} }
                        }
                    }
                }
                if *show_scroll_button.read() {
                    button {
                        class: "absolute bottom-4 right-4 z-10 p-2 bg-purple-600 text-white rounded-full shadow-lg hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 transition-opacity duration-300 ease-in-out",
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
            div {
                class: "p-4 border-t border-gray-700 flex-shrink-0",
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
                        class: "flex-1 py-2 px-4 rounded-xl bg-gray-800 border border-gray-700 text-gray-100 placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-purple-500 resize-none overflow-y-hidden",
                        rows: "1",
                        placeholder: if !mcp_context.read().servers.is_empty() { "Type your message..." } else { "Initializing..." },
                        disabled: mcp_context.read().servers.is_empty(),
                        value: "{draft}",
                        oninput: move |event| {
                            draft.set(event.value());
                            let _ = document::eval(r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    el.style.height = 'auto';
                                    el.style.height = (el.scrollHeight) + 'px';
                                }
                            "#);
                        },
                        onkeydown: move |event| {
                            let modifiers = event.data.modifiers();

                            // Allow all Command, Control, and Alt-based shortcuts to pass through
                            if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::ALT) {
                                return;
                            }

                            // Handle plain Enter for submission, allowing Shift+Enter for newlines
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
                                                // Asynchronously fetch the MCP context first.
                                                let mcp_context = {
                                                    let mcp_manager_reader = mcp_manager.read();
                                                    mcp_manager_reader.get_mcp_context().await
                                                };

                                                // Now, read the state and build the context string for debugging.
                                                let context_string = {
                                                    let state = session_state.read();
                                                    if let Some(session) = state.get_active_session().cloned() {
                                                        let mut session_for_debug = session;

                                                        // Inject the fetched MCP context into the temporary clone.
                                                        if !mcp_context.servers.is_empty() {
                                                            session_for_debug.active_context.mcp_tools = Some(mcp_context);
                                                        }

                                                        // Build the prompt from the modified clone to show an accurate preview.
                                                        let settings_reader = settings.read();
                                                        let builder = PromptBuilder::new(&session_for_debug, &settings_reader);
                                                        // Note: This debug view might not be perfect after the refactor,
                                                        // but it's better to show the raw prompt struct than to crash.
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
}

#[component]
pub fn CodeBlock(code: String, lang: String) -> Element {
    let mut copied = use_signal(|| false);

    let code_to_copy = code.clone();
    let copy_onclick = move |_| {
        let code_to_copy = code_to_copy.clone();
        spawn(async move {
            match copy_to_clipboard(&code_to_copy) {
                Ok(_) => {
                    copied.set(true);
                    sleep(Duration::from_secs(2)).await;
                    copied.set(false);
                }
                Err(e) => {
                    // Log the error, but don't crash the app.
                    // The error is already logged inside the function,
                    // but we could add more context here if needed.
                    tracing::error!("CodeBlock copy failed from component: {}", e);
                }
            }
        });
    };

    let lang_for_memo = lang.clone();
    let highlighted_html = use_memo(move || {
        let syntax = SYNTAX_SET.find_syntax_by_token(&lang_for_memo).unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        let mut h = HighlightLines::new(syntax, &THEME);
        let mut html = String::new();
        for line in code.lines() {
            let regions = h.highlight_line(line, &SYNTAX_SET).unwrap();
            let html_line = styled_line_to_highlighted_html(&regions, IncludeBackground::No).unwrap();
            html.push_str(&html_line);
            html.push('\n');
        }
        if html.ends_with('\n') {
            html.pop();
        }
        html
    });

    rsx! {
        div {
            class: "code-block-wrapper relative bg-gray-800 rounded-lg my-2",
            button {
                class: "absolute top-2 right-2 p-1.5 rounded text-gray-400 hover:bg-gray-700 hover:text-white transition-colors",
                onclick: copy_onclick,
                if *copied.read() {
                    Icon {
                        width: 16,
                        height: 16,
                        icon: fi_icons::FiCheck
                    }
                } else {
                    Icon {
                        width: 16,
                        height: 16,
                        icon: fi_icons::FiClipboard
                    }
                }
            }
            pre {
                class: "p-4 text-sm whitespace-pre-wrap break-words",
                code {
                    class: "language-{lang}",
                    dangerous_inner_html: "{highlighted_html}"
                }
            }
        }
    }
}

// Sub-component for styling individual messages
#[component]
fn MessageBubble(message: Message) -> Element {
    let is_user = message.author == "User";

    // This component now specifically handles Text content.
    // The parent component (`ChatWindow`) is responsible for matching on `MessageContent`
    // and rendering the correct component (`MessageBubble` or `ToolCallDisplay`).
    if let MessageContent::Text(text_content) = &message.content {
        let stream_manager = consume_context::<StreamManagerContext>();
        let mut content = use_signal(|| text_content.clone());
        let mut is_hovered = use_signal(|| false);
        let mut copied = use_signal(|| false);

        // This effect runs once when the component is created.
        // If it's a streaming Hobbes message, it takes the stream and updates its local state.
        use_effect(move || {
            if !is_user && stream_manager.is_streaming(&message.id) {
                spawn(async move {
                    if let Some(mut rx) = stream_manager.take_stream(&message.id) {
                        while let Some(stream_msg) = rx.recv().await {
                            if let crate::components::shared::StreamMessage::Text(chunk) = stream_msg {
                                content.write().push_str(&chunk);
                            }
                        }
                    }
                });
            }
        });

        let is_thinking = !is_user && content.read().is_empty();

        let bubble_classes = if is_user {
            "bg-purple-600 text-white self-end ml-auto"
        } else {
            "bg-gray-700 text-gray-200 self-start mr-auto"
        };
        let container_classes = if is_user { "flex justify-end" } else { "flex justify-start" };
        let author_classes = format!(
            "text-xs text-gray-500 mt-1 px-2 {}",
            if is_user { "text-right" } else { "text-left" }
        );

        let elements = use_memo(move || {
            let content_reader = content.read();
            let mut options = Options::empty();
            options.insert(Options::ENABLE_STRIKETHROUGH);
            options.insert(Options::ENABLE_TASKLISTS);
            options.insert(Options::ENABLE_TABLES);

            let parser = Parser::new_ext(&content_reader, options);
            
            let mut elements: Vec<Element> = Vec::new();
            let mut current_events: Vec<Event> = Vec::new();
            
            let mut in_code_block = false;
            let mut code_buffer = String::new();
            let mut lang = String::new();

            let mut in_link = false;
            let mut link_url = String::new();
            let mut link_text_buffer: Vec<Event> = Vec::new();

            let flush_events = |events: &mut Vec<Event>, elements: &mut Vec<Element>| {
                if !events.is_empty() {
                    let mut html_output = String::new();
                    html::push_html(&mut html_output, events.drain(..));
                    if !html_output.trim().is_empty() {
                        elements.push(rsx! {
                            div {
                                class: "prose prose-sm dark:prose-invert max-w-none",
                                dangerous_inner_html: "{html_output}"
                            }
                        });
                    }
                }
            };

            for event in parser {
                match event {
                    Event::Start(Tag::CodeBlock(kind)) => {
                        flush_events(&mut current_events, &mut elements);
                        in_code_block = true;
                        lang = match kind {
                            pulldown_cmark::CodeBlockKind::Fenced(l) => l.into_string(),
                            _ => String::new(),
                        };
                    }
                    Event::End(TagEnd::CodeBlock) => {
                        in_code_block = false;
                        elements.push(rsx! {
                            CodeBlock {
                                code: code_buffer.clone(),
                                lang: lang.clone()
                            }
                        });
                        code_buffer.clear();
                        lang.clear();
                    }
                    Event::Start(Tag::Link { dest_url, .. }) => {
                        flush_events(&mut current_events, &mut elements);
                        in_link = true;
                        link_url = dest_url.into_string();
                    }
                    Event::End(TagEnd::Link) => {
                        in_link = false;
                        let mut text_html = String::new();
                        html::push_html(&mut text_html, link_text_buffer.drain(..));
                        
                        elements.push(rsx! {
                            LinkWithControls {
                                href: link_url.clone(),
                                text_html: text_html
                            }
                        });
                        link_url.clear();
                    }
                    Event::Text(text) => {
                        if in_code_block {
                            code_buffer.push_str(&text);
                        } else if in_link {
                            link_text_buffer.push(Event::Text(text));
                        } else {
                            current_events.push(Event::Text(text));
                        }
                    }
                    Event::SoftBreak | Event::HardBreak => {
                        if in_code_block {
                            code_buffer.push('\n');
                        } else if in_link {
                            link_text_buffer.push(event);
                        } else {
                            current_events.push(event);
                        }
                    }
                    // For other events like emphasis, strong, etc., inside a link
                    e @ Event::Start(_) | e @ Event::End(_) | e @ Event::Code(_) | e @ Event::Html(_) | e @ Event::FootnoteReference(_) | e @ Event::TaskListMarker(_) => {
                        if in_link {
                            link_text_buffer.push(e);
                        } else if !in_code_block {
                            current_events.push(e);
                        }
                    }
                    _ => {} // Ignore other event types for now
                }
            }
            flush_events(&mut current_events, &mut elements);

            elements
        });

        let button_position_classes = if is_user {
            "absolute bottom-[-10px] left-[-10px]"
        } else {
            "absolute bottom-[-10px] right-[-10px]"
        };

        rsx! {
            div {
                class: "{container_classes} w-full",
                div {
                    class: "flex flex-col max-w-2/3",
                    div {
                        class: "relative group px-4 py-2 rounded-2xl {bubble_classes}",
                        onmouseenter: move |_| is_hovered.set(true),
                        onmouseleave: move |_| is_hovered.set(false),
                        if is_thinking {
                            ThinkingIndicator {}
                        } else {
                            for el in elements.read().iter() {
                                {el}
                            }
                        }
                        if *is_hovered.read() && !content.read().is_empty() {
                            button {
                                class: "{button_position_classes} p-1 rounded-full text-gray-400 bg-gray-900 bg-opacity-75 hover:bg-gray-700 hover:text-white transition-all opacity-0 group-hover:opacity-100",
                                onclick: move |_| {
                                    let content_to_copy = content.read().clone();
                                    spawn(async move {
                                        if copy_to_clipboard(&content_to_copy).is_ok() {
                                            copied.set(true);
                                            sleep(Duration::from_secs(2)).await;
                                            copied.set(false);
                                        }
                                    });
                                },
                                if *copied.read() {
                                    Icon { width: 14, height: 14, icon: fi_icons::FiCheck }
                                } else {
                                    Icon { width: 14, height: 14, icon: fi_icons::FiClipboard }
                                }
                            }
                        }
                    }
                    div {
                        class: "{author_classes}",
                        "{message.author}"
                    }
                }
            }
        }
    } else {
        // This path should not be taken, but as a fallback, render nothing.
        rsx! {}
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

#[component]
fn LinkWithControls(href: String, text_html: String) -> Element {
    let mut copied = use_signal(|| false);
    let mut draft = consume_context::<Signal<String>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();

    let fetch_tool_available = use_memo(move || {
        mcp_context.read().servers.iter().any(|s| s.name == "fetch")
    });

    let href_clone_copy = href.clone();
    let copy_onclick = move |_| {
        let href_to_copy = href_clone_copy.clone();
        spawn(async move {
            if copy_to_clipboard(&href_to_copy).is_ok() {
                copied.set(true);
                sleep(Duration::from_secs(2)).await;
                copied.set(false);
            }
        });
    };

    let href_clone_summarize = href.clone();
    let summarize_onclick = move |_| {
        let summary_prompt = format!("Please fetch {} and summarize.", href_clone_summarize);
        draft.set(summary_prompt);
        // Focus the textarea after setting the draft
        let _ = document::eval(r#"
            const el = document.getElementById('chat-textarea');
            if (el) {
                el.focus();
                el.style.height = 'auto';
                el.style.height = (el.scrollHeight) + 'px';
            }
        "#);
    };

    rsx! {
        div {
            class: "relative inline-block group",
            a {
                href: "{href}",
                target: "_blank",
                rel: "noopener noreferrer",
                class: "text-purple-400 hover:underline",
                dangerous_inner_html: "{text_html}"
            }
            div {
                class: "absolute bottom-0 right-0 translate-x-1/2 translate-y-1/2 z-10 hidden group-hover:flex items-center bg-gray-900 bg-opacity-75 border border-gray-700 rounded-full shadow-lg p-1 space-x-1",
                
                // Copy Button
                button {
                    class: "p-1.5 rounded text-gray-400 hover:bg-gray-700 hover:text-white transition-colors",
                    onclick: copy_onclick,
                    if *copied.read() {
                        Icon { width: 16, height: 16, icon: fi_icons::FiCheck }
                    } else {
                        Icon { width: 16, height: 16, icon: fi_icons::FiClipboard }
                    }
                }

                // Summarize Button
                if *fetch_tool_available.read() {
                    button {
                        class: "p-1.5 rounded text-gray-400 hover:bg-gray-700 hover:text-white transition-colors",
                        onclick: summarize_onclick,
                        Icon { width: 16, height: 16, icon: fi_icons::FiFileText }
                    }
                }
            }
        }
    }
}