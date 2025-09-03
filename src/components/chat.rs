use dioxus::prelude::*;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;
use dioxus_free_icons::{Icon, icons::fi_icons};
use std::rc::Rc;
use dioxus::html::geometry::euclid::Rect;
use std::time::Duration;
use tokio::time::sleep;
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
use pulldown_cmark::{Options, Parser, Event as CmarkEvent, Tag, TagEnd, html};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::settings::Settings;
use crate::mcp::manager::McpManager;
use crate::components::tool_call_display::{ToolCallDisplay, ToolCallStatus};


lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
    pub result: Option<Value>,
    #[serde(default)]
    pub status: ToolCallStatus,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageContent {
    Text { content: String },
    ToolCall { call: ToolCall },
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text { content: "".to_string() }
    }
}


#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: uuid::Uuid,
    pub author: String,
    #[serde(flatten)]
    pub content: MessageContent,
    #[serde(default = "default_visible")]
    pub visible: bool,
}

fn default_visible() -> bool {
    true
}

#[derive(Clone)]
enum ChatAction {
    SendMessage(Message),
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(on_content_resize: EventHandler<Rect<f64, f64>>, on_interaction: EventHandler<()>, on_toggle_sessions: EventHandler<()>, on_toggle_settings: EventHandler<()>) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<McpManager>>();
    let mut draft = use_signal(|| "".to_string());
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut has_interacted = use_signal(|| false);
    let is_sending = use_signal(|| false);
    let stream_manager = consume_context::<StreamManagerContext>();
    const INITIAL_MESSAGES_TO_SHOW: usize = 20;
    let mut show_scroll_button = use_signal(|| false);
    let mut is_initial_load = use_signal(|| true);
    let visible_message_count = use_signal(|| INITIAL_MESSAGES_TO_SHOW);
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

    let mcp_manager_for_debug = mcp_manager;
    let chat_coroutine = use_coroutine(
        move |mut rx: UnboundedReceiver<ChatAction>| {
            let mut session_state = session_state.clone();
            let settings = settings.clone();
            let mcp_manager = mcp_manager.clone();
            let stream_manager = stream_manager.clone();
            let mut is_sending = is_sending.clone();

            async move {
                while let Some(action) = rx.next().await {
                    match action {
                        ChatAction::SendMessage(mut message) => {
                            if *is_sending.read() {
                                tracing::warn!("'send_message' blocked: already sending.");
                                continue;
                            }
                            is_sending.set(true);

                            // This loop allows us to feed tool results back into the LLM without exiting the coroutine task
                            'action_loop: loop {
                                let hobbes_message_id = Uuid::new_v4();
                                
                                {
                                    let mut state = session_state.write();
                                    if state.active_session_id.is_empty() {
                                        state.create_session();
                                    }
                                    let session = state.get_active_session_mut().unwrap();
                                    session.messages.push(message.clone());
                                    session.messages.push(Message {
                                        id: hobbes_message_id,
                                        author: "Hobbes".to_string(),
                                        content: MessageContent::Text { content: "".to_string() },
                                        visible: true,
                                    });
                                }

                                let session_for_processing = session_state.read().get_active_session().cloned().unwrap();
                                let settings_clone = settings.read().clone();
                                let processor = ConversationProcessor::new();
                                if let Some(summary) = processor.generate_summary(&session_for_processing, &settings_clone).await {
                                    let mut state = session_state.write();
                                    if let Some(session) = state.get_active_session_mut() {
                                        session.active_context.conversation_summary = summary;
                                    }
                                }

                                let final_message = {
                                    let state = session_state.read();
                                    let session = state.get_active_session().unwrap();
                                    let builder = PromptBuilder::new();
                                    builder.build_context_string(session, &settings_clone, &mcp_manager.read()).await
                                };

                                if let Err(e) = session_state.read().save() {
                                    tracing::error!("Failed to save session state: {}", e);
                                }

                                let api_key = settings_clone.api_key.clone().unwrap_or_else(|| {
                                    std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set in settings or environment")
                                });

                                let (tx, mut rx_signal) = mpsc::unbounded_channel::<()>();
                                let on_complete = move || { let _ = tx.send(()); };
                                stream_manager.start_stream(
                                    api_key,
                                    settings_clone.chat_model,
                                    hobbes_message_id,
                                    final_message,
                                    on_complete,
                                );

                                rx_signal.recv().await;

                                let message_content = {
                                    let mut state = session_state.write();
                                    state.get_active_session_mut()
                                        .and_then(|s| s.messages.iter_mut().find(|m| m.id == hobbes_message_id))
                                        .map(|m| m.content.clone())
                                };

                                if let Some(MessageContent::ToolCall { call }) = message_content.clone() {
                                    let mcp_manager_clone = mcp_manager.read().clone();
                                    let mut call_clone = call.clone();
                                    
                                    let server_name = call_clone.name.split_once("::").map(|(s, _)| s).unwrap_or_default();
                                    let result = mcp_manager_clone.use_mcp_tool(server_name, &call_clone.name, call_clone.args.clone()).await;
                                    let result_value = serde_json::to_value(result).unwrap();
                                    call_clone.result = Some(result_value.clone());
                                    call_clone.status = ToolCallStatus::Completed;

                                    if let Some(msg_to_update) = session_state.write().get_active_session_mut().and_then(|s| s.messages.iter_mut().find(|m| m.id == hobbes_message_id)) {
                                        msg_to_update.content = MessageContent::ToolCall { call: call_clone.clone() };
                                    }
                                    
                                    let builder = PromptBuilder::new();
                                    let tool_result_context = builder.build_tool_result_context(&call_clone.name, &result_value);
                                    
                                    message = Message {
                                        id: Uuid::new_v4(),
                                        author: "User".to_string(),
                                        content: MessageContent::Text { content: tool_result_context },
                                        visible: false,
                                    };
                                    
                                    continue 'action_loop;
                                } else {
                                    break 'action_loop;
                                }
                            }
                            
                            is_sending.set(false);
                        }
                    }
                }
            }
        },
    );


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
                            if session.messages.is_empty() {
                                rsx! { WelcomeMessage {} }
                            } else {
                                rsx! {
                                    for message in session.messages.iter().filter(|m| m.visible) {
                                        MessageBubble {
                                            key: "{message.id}",
                                            message: message.clone()
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
                        placeholder: "Type your message...",
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
                                let user_message = draft.read().clone();
                                if !user_message.is_empty() {
                                    draft.set("".to_string());
                                    let _ = document::eval(r#"
                                        const el = document.getElementById('chat-textarea');
                                        if (el) { el.style.height = 'auto'; }
                                    "#);
                                    let message = Message {
                                        id: Uuid::new_v4(),
                                        author: "User".to_string(),
                                        content: MessageContent::Text { content: user_message },
                                        visible: true,
                                    };
                                    chat_coroutine.send(ChatAction::SendMessage(message));
                                }
                            }
                        },
                    }
                    button {
                        class: "p-2 rounded-full text-gray-400 hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-gray-600",
                        onclick: move |_| {
                            let state = session_state.read();
                            let settings = settings.read().clone();
                            let mcp_manager = mcp_manager_for_debug.clone();
                            if let Some(session) = state.sessions.get(&state.active_session_id).cloned() {
                                spawn(async move {
                                    let builder = PromptBuilder::new();
                                    let _context_string = builder.build_context_string(&session, &settings, &mcp_manager.read()).await;
                                });
                            } else {
                            }
                        },
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiCpu
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
                        class: "px-5 py-2 bg-purple-600 rounded-full text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors",
                        onclick: move |_| {
                            if !*has_interacted.read() {
                                on_interaction.call(());
                                has_interacted.set(true);
                            }
                            let user_message = draft.read().clone();
                            if !user_message.is_empty() {
                                draft.set("".to_string());
                                let _ = document::eval(r#"
                                    const el = document.getElementById('chat-textarea');
                                    if (el) { el.style.height = 'auto'; }
                                "#);
                                let message = Message {
                                    id: Uuid::new_v4(),
                                    author: "User".to_string(),
                                    content: MessageContent::Text { content: user_message },
                                    visible: true,
                                };
                                chat_coroutine.send(ChatAction::SendMessage(message));
                            }
                        },
                        "Send"
                    }
                }
            }
        }
    }
}

#[component]
fn CodeBlock(code: String, lang: String) -> Element {
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
                class: "p-4 overflow-x-auto text-sm",
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
    let stream_manager = consume_context::<StreamManagerContext>();
    let mut content = use_signal(|| message.content.clone());
    let is_user = message.author == "User";
    let mut is_hovered = use_signal(|| false);
    let mut copied = use_signal(|| false);

    use_effect({
        let message_id = message.id;
        let mut content = content.clone();
        let stream_manager = stream_manager.clone();

        move || {
            if !is_user {
                let mut rx = stream_manager.subscribe(message_id);
                let fut = async move {
                    while let Some(new_content) = rx.recv().await {
                        content.set(new_content);
                    }
                };
                spawn(fut);
            }
        }
    });

    let is_thinking = !is_user && match &*content.read() {
        MessageContent::Text { content } => content.is_empty(),
        _ => false,
    };

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

    let button_position_classes = if is_user {
        "absolute bottom-[-10px] left-[-10px]"
    } else {
        "absolute bottom-[-10px] right-[-10px]"
    };

    rsx! {
        div {
            class: "{container_classes}",
            div {
                class: "flex flex-col max-w-xs md:max-w-md",
                div {
                    class: "relative group px-4 py-2 rounded-2xl {bubble_classes}",
                    onmouseenter: move |_| is_hovered.set(true),
                    onmouseleave: move |_| is_hovered.set(false),
                    if is_thinking {
                        ThinkingIndicator {}
                    } else {
                        {
                            let is_streaming = stream_manager.is_streaming(&message.id);
                            match content.read().clone() {
                                MessageContent::Text{ content } => {
                                    if is_streaming {
                                        // While streaming, render raw text to avoid parsing incomplete markdown
                                        rsx! {
                                            div {
                                                class: "prose prose-sm dark:prose-invert max-w-none",
                                                "{content}"
                                            }
                                        }
                                    } else {
                                        // When not streaming, parse the complete markdown
                                        let elements = use_memo(move || {
                                            let mut options = Options::empty();
                                            options.insert(Options::ENABLE_STRIKETHROUGH);
        
                                            let parser = Parser::new_ext(&content, options);
                                            
                                            let mut elements: Vec<Element> = Vec::new();
                                            let mut current_events: Vec<CmarkEvent> = Vec::new();
                                            let mut in_code_block = false;
                                            let mut code_buffer = String::new();
                                            let mut lang = String::new();
            
                                            let flush_events = |events: &mut Vec<CmarkEvent>, elements: &mut Vec<Element>| {
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
                                                    CmarkEvent::Start(Tag::CodeBlock(kind)) => {
                                                        flush_events(&mut current_events, &mut elements);
                                                        in_code_block = true;
                                                        lang = match kind {
                                                            pulldown_cmark::CodeBlockKind::Fenced(l) => l.into_string(),
                                                            _ => String::new(),
                                                        };
                                                    }
                                                    CmarkEvent::End(TagEnd::CodeBlock) => {
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
                                                    CmarkEvent::Text(text) => {
                                                        if in_code_block {
                                                            code_buffer.push_str(&text);
                                                        } else {
                                                            current_events.push(CmarkEvent::Text(text));
                                                        }
                                                    }
                                                    CmarkEvent::SoftBreak | CmarkEvent::HardBreak => {
                                                        if in_code_block {
                                                            code_buffer.push('\n');
                                                        } else {
                                                            current_events.push(event);
                                                        }
                                                    }
                                                    e => {
                                                        if !in_code_block {
                                                            current_events.push(e);
                                                        }
                                                    }
                                                }
                                            }
                                            flush_events(&mut current_events, &mut elements);
        
                                            elements
                                        });
                                        rsx!{ for el in elements.read().iter() { {el} } }
                                    }
                                },
                                MessageContent::ToolCall{ call } => rsx! {
                                    ToolCallDisplay {
                                        tool_name: call.name.clone(),
                                        tool_arguments: call.args.clone(),
                                        status: call.status.clone(),
                                        result: call.result.clone(),
                                    }
                                }
                            }
                        }
                    }
                    if *is_hovered.read() {
                        if let MessageContent::Text { content: message_text } = &*content.read() {
                            if !message_text.is_empty() {
                                {
                                    let content_for_copy = message_text.clone();
                                    rsx! {
                                        button {
                                            class: "{button_position_classes} p-1 rounded-full text-gray-400 bg-gray-900 bg-opacity-75 hover:bg-gray-700 hover:text-white transition-all opacity-0 group-hover:opacity-100",
                                            onclick: move |_| {
                                                let content_to_copy = content_for_copy.clone();
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