use dioxus::prelude::*;
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


lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: uuid::Uuid,
    pub author: String,
    pub content: String,
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(on_content_resize: EventHandler<Rect<f64, f64>>, on_interaction: EventHandler<()>, on_toggle_sessions: EventHandler<()>) -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let mut draft = use_signal(|| "".to_string());
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut has_interacted = use_signal(|| false);
    let stream_manager = consume_context::<StreamManagerContext>();

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
                        const threshold = 50;
                        return el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
                    }
                    // Default to true if the element doesn't exist yet, so we scroll on the first load.
                    return true;
                "#).await {
                    result.as_bool().unwrap_or(true)
                } else {
                    true // Also default to true if the eval fails.
                };

                // Only scroll if the user was already near the bottom.
                if is_near_bottom {
                    let _ = document::eval(r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTop = el.scrollHeight; }
                    "#).await;
                }

                // Finally, notify the parent component of the new content size.
                if let Ok(rect) = element.get_client_rect().await {
                    on_content_resize.call(rect.cast_unit());
                }
            });
        }
    });

    // Reusable closure for sending a message
    let mut send_message = move || {
        let user_message = draft.read().clone();
        if user_message.is_empty() {
            return;
        }

        // Clear the draft immediately for a responsive UI
        draft.set("".to_string());
        // Reset textarea height
        let _ = document::eval(r#"
            const el = document.getElementById('chat-textarea');
            if (el) { el.style.height = 'auto'; }
        "#);

        // Clone necessary signals and data for the async task
        let mut session_state_clone = session_state.clone();
        let stream_manager_clone = stream_manager.clone();

        // Spawn a single async task to handle all state mutations and side effects
        spawn(async move {
            // 1. Prepare message and context
            let hobbes_message_id = Uuid::new_v4();
            // 2. Perform initial state mutations
            {
                let mut state = session_state_clone.write();
                if state.active_session_id.is_empty() {
                    state.create_session();
                }
                let active_id = state.active_session_id.clone();
                let session = state.sessions.get_mut(&active_id).unwrap();

                // Add user message
                session.messages.push(Message {
                    id: Uuid::new_v4(),
                    author: "User".to_string(),
                    content: user_message.clone(),
                });

                // Add bot placeholder
                session.messages.push(Message {
                    id: hobbes_message_id,
                    author: "Hobbes".to_string(),
                    content: "".to_string(),
                });
            } // Write lock is released here

            // 3. Process context and build the final prompt
            let final_message = {
                // Get a read lock to clone the session for processing
                let session_for_processing = session_state_clone.read().get_active_session().cloned().unwrap();
                
                // Generate the summary asynchronously without holding any locks
                let processor = ConversationProcessor::new();
                if let Some(summary) = processor.generate_summary(&session_for_processing).await {
                    // Re-acquire the write lock to update the context
                    let mut state = session_state_clone.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.active_context.insert("conversation_summary".to_string(), summary);
                    }
                }

                // Get a final read lock to build the prompt with the *updated* context
                let state = session_state_clone.read();
                let session = state.get_active_session().unwrap();
                let builder = PromptBuilder::new(session);
                let context_string = builder.build_context_string();
                format!("{}{}", context_string, user_message)
            };

            // 4. Save state after mutations
            if let Err(e) = session_state_clone.read().save() {
                tracing::error!("Failed to save session state: {}", e);
            }

            // 5. Start the stream using the manager
            stream_manager_clone.start_stream(hobbes_message_id, final_message);
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
                                    MessageBubble { key: "{message.id}", message: message.clone() }
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
                        onclick: move |_| {
                            let state = session_state.read();
                            let context_string = if let Some(session) = state.sessions.get(&state.active_session_id) {
                                let builder = PromptBuilder::new(session);
                                builder.build_context_string()
                            } else {
                                "[No active session]".to_string()
                            };
                            tracing::info!("---\n[DEBUG] Current Context:\n{}---", context_string);
                        },
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiCpu
                        }
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
    // This is currently unused after the refactor, but we'll keep it for potential future use.
    let _session_state = consume_context::<Signal<crate::session::SessionState>>();
    let mut content = use_signal(|| message.content.clone());
    let is_user = message.author == "User";
    let mut is_hovered = use_signal(|| false);
    let mut copied = use_signal(|| false);

    // This effect runs once when the component is created.
    // If it's a streaming Hobbes message, it takes the stream and updates its local state.
    use_effect(move || {
        if !is_user && stream_manager.is_streaming(&message.id) {
            spawn(async move {
                if let Some(mut rx) = stream_manager.take_stream(&message.id) {
                    // This component now ONLY updates its local content for display.
                    // The StreamManager is responsible for the final state update and save.
                    while let Some(chunk) = rx.recv().await {
                        content.write().push_str(&chunk);
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

        let parser = Parser::new_ext(&content_reader, options);
        
        let mut elements: Vec<Element> = Vec::new();
        let mut current_events: Vec<Event> = Vec::new();
        let mut in_code_block = false;
        let mut code_buffer = String::new();
        let mut lang = String::new();

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
                Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else {
                        current_events.push(Event::Text(text));
                    }
                }
                // Handle newlines inside code blocks, which pulldown-cmark
                // sends as SoftBreak or HardBreak events.
                Event::SoftBreak | Event::HardBreak => {
                    if in_code_block {
                        code_buffer.push('\n');
                    } else {
                        current_events.push(event);
                    }
                }
                e => {
                    // For any other event, only process it if we're outside a code block.
                    if !in_code_block {
                        current_events.push(e);
                    }
                }
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