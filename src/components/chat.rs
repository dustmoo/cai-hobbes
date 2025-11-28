use dioxus::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;
use dioxus_free_icons::{Icon, icons::fi_icons};
use std::rc::Rc;
use dioxus::html::geometry::euclid::Rect;
use std::time::Duration;
use tokio::time::sleep;
use crate::{components::stream_manager::StreamManagerContext};
use lazy_static::lazy_static;
use syntect::easy::HighlightLines;
use syntect::highlighting::{ThemeSet, Theme};
use syntect::parsing::SyntaxSet;
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use feature_clipboard::copy_to_clipboard;
use crate::context::prompt_builder::PromptBuilder;
use serde::{Deserialize, Serialize};
use crate::{settings::Settings};
use hobbes_core::models::Attachment;
use super::shared::{MessageContent, StreamMessage};
use super::continuation_controller::ContinuationController;
use super::chat_input::ChatInput;
use super::message_list::MessageList;
use crate::context::permissions::PermissionManager;
use crate::components::markdown_renderer::MarkdownRenderer;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SelectionData {
    text: String,
    top: f64,
    left: f64,
}

lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub text_selection: String,
    pub start_offset: usize,
    pub end_offset: usize,
    pub comment: String,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: uuid::Uuid,
    pub author: String,
    pub content: MessageContent,
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(on_content_resize: EventHandler<Rect<f64, f64>>, on_interaction: EventHandler<()>, on_toggle_sessions: EventHandler<()>, on_toggle_settings: EventHandler<()>) -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let permission_manager = use_context::<Signal<PermissionManager>>();
    let draft = use_signal(|| "".to_string());
    use_context_provider(|| draft);
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let stream_manager = consume_context::<StreamManagerContext>();
    let active_message_id = use_signal(|| None::<Uuid>);
    let mut continuation_controller = consume_context::<Signal<ContinuationController>>();
    let mut is_initial_load = use_signal(|| true);
    let mut last_session_id = use_signal(|| session_state.read().active_session_id.clone());
    let stream_update_trigger = use_signal(|| 0);
    let mut show_scroll_button = use_signal(|| false);


    // Effect to report content size changes, scroll, and attach JS controls
    use_effect(move || {
        // By reading the session state here, the effect becomes dependent on it.
        // Any change to messages will cause this to re-run.
        let _ = stream_update_trigger.read();
        let current_session_id = session_state.read().active_session_id.clone();
        let mut is_session_switch = false;
        last_session_id.with_mut(|last_id| {
            if current_session_id != *last_id {
                is_session_switch = true;
                *last_id = current_session_id;
            }
        });

        if let Some(element) = container_element.read().clone() {
            spawn(async move {
                // A short delay allows the DOM to render the new message before we measure/scroll.
                sleep(Duration::from_millis(50)).await;

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

                // On the very first load, we always scroll to the bottom.
                // On subsequent loads, we only scroll if the user was already near the bottom.
                if is_session_switch || *is_initial_load.read() || is_near_bottom {
                    let _ = document::eval(r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTop = el.scrollHeight; }
                    "#).await;
                    if *is_initial_load.read() {
                        is_initial_load.set(false);
                    }
                }

                // After scrolling, check if the scroll button should be visible.
                let show_button = if let Ok(result) = document::eval(r#"
                    const el = document.getElementById('message-list');
                    if (el) {
                        // Show button if not at the bottom (with a small threshold)
                        return el.scrollHeight - el.scrollTop - el.clientHeight > 10;
                    }
                    return false; // Don't show if element doesn't exist
                "#).await {
                    result.as_bool().unwrap_or(false)
                } else {
                    false
                };
                show_scroll_button.set(show_button);

                // Finally, notify the parent component of the new content size.
                if let Ok(rect) = element.get_client_rect().await {
                    on_content_resize.call(rect.cast_unit());
                }
            });
        }

    });


    // Effect to restore cursor position after re-renders
    use_effect(move || {
        // Read the draft to create a dependency on it
        let _ = draft.read();

        spawn(async move {
            // A minimal delay to allow Dioxus to commit the new value to the DOM
            sleep(Duration::from_millis(1)).await;
            let _ = document::eval(r#"
                const el = document.getElementById('chat-textarea');
                // Check for the global cursor variable and that the element is focused
                // to avoid moving cursor on background updates.
                if (el && window.dioxusCursorPos && document.activeElement === el) {
                    el.setSelectionRange(window.dioxusCursorPos[0], window.dioxusCursorPos[1]);
                    // Clean up the global variable to prevent stale positions
                    delete window.dioxusCursorPos;
                }
            "#).await;
        });
    });

    // Reusable closure for sending a message
    let send_prompt_to_llm = {
        // Capture signals which are all `Copy`
        let stream_manager = stream_manager;
        let settings = settings;
        let active_message_id = active_message_id;

        move |prompt_data: crate::context::prompt_builder::LlmPrompt, mcp_context: Option<crate::mcp::manager::McpContext>, hobbes_message_id: Uuid| {
            spawn(async move {
                // Now clone/read them inside the async block
                let stream_manager = stream_manager;
                let _settings = settings.read().clone();
                let mut active_message_id = active_message_id;

                active_message_id.set(Some(hobbes_message_id));
                tracing::info!("Lock ACQUIRED.");

                let (tx, mut rx) = mpsc::unbounded_channel::<()>();

                let on_complete = {
                    let mut active_message_id = active_message_id;
                    move || {
                        active_message_id.set(None);
                        let _ = tx.send(());
                    }
                };

                stream_manager.start_stream(
                    hobbes_message_id,
                    prompt_data,
                    on_complete,
                    mcp_context,
                );

                rx.recv().await;
                tracing::info!(message_id = %hobbes_message_id, "Stream completion signal RECEIVED.");

                tracing::info!("Lock RELEASED.");
            });
        }
    };

    let send_message = move |(user_message, attachments): (String, Vec<Attachment>)| {
        spawn(async move {
            let mut session_state = session_state;
            let settings = settings.read().clone();
            let mcp_manager = mcp_manager;
            let send_prompt_to_llm = send_prompt_to_llm;
            let mut permission_manager = permission_manager;

            // Reset the AI turn count every time the user sends a message.
            permission_manager.write().reset_turn_count();

            // Clear the tool call history to ensure a fresh start for the new turn.
            session_state.write().tool_call_history.clear();

            // Check if the last message was the turn limit warning.
            let last_message_was_warning = session_state.read().get_active_session()
                .and_then(|s| s.messages.last())
                .map_or(false, |m| {
                    if let MessageContent::Text { content: text, .. } = &m.content {
                        text.starts_with("Pardon, I have reached the 'Max Turn Limit' currently set to X in settings")
                    } else {
                        false
                    }
                });

            if last_message_was_warning {
                permission_manager.write().reset_turn_count();
            }

            if permission_manager.read().is_turn_limit_reached() {
                let mut state = session_state.write();
                if let Some(session) = state.get_active_session_mut() {
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "User".to_string(),
                        content: MessageContent::Text { content: user_message.clone(), thought_signature: None },
                        attachments,
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                    });
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "Hobbes".to_string(),
                        content: MessageContent::Text { content: format!("Pardon, I have reached the 'Max Turn Limit' currently set to {} in settings and need permission to continue.", settings.permission_settings.max_ai_turns), thought_signature: None },
                        attachments: Vec::new(),
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                    });
                }
                return;
            }

            let hobbes_message_id = Uuid::new_v4();
            {
                let mut state = session_state.write();
                if state.active_session_id.is_empty() {
                    state.create_session();
                }
                if let Some(session) = state.get_active_session_mut() {
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "User".to_string(),
                        content: MessageContent::Text { content: user_message.clone(), thought_signature: None },
                        attachments,
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                    });
                    session.messages.push(Message {
                        id: hobbes_message_id,
                        author: "Hobbes".to_string(),
                        content: MessageContent::Text { content: "".to_string(), thought_signature: None },
                        attachments: Vec::new(),
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                    });
                }
            }

            let prompt_data = {
                let mcp_context = mcp_manager.read().get_mcp_context().await;
                let user_prompt = user_message.clone();
                let conversation_summary = session_state.read().get_active_session().unwrap().active_context.conversation_summary.clone();

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
                let builder = PromptBuilder::new(session, &settings, &state);
                builder.build_prompt(user_prompt, None)
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
    };

    let cancel_message = move || {
        if let Some(id) = *active_message_id.read() {
            stream_manager.cancel_stream(&id);
        }
    };

    let continue_prompt_flow = {
        let session_state = session_state;
        let settings = settings;
        let send_prompt_to_llm = send_prompt_to_llm;

        Rc::new(move || {
            tracing::info!("continue_prompt_flow callback INVOKED.");
            spawn(async move {
                tracing::info!("continue_prompt_flow task SPAWNED.");
                let mut session_state = session_state;
                let settings = settings.read().clone();
                let send_prompt_to_llm = send_prompt_to_llm;

                let hobbes_message_id = Uuid::new_v4();
                {
                    let mut state = session_state.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.messages.push(Message {
                            id: hobbes_message_id,
                            author: "Hobbes".to_string(),
                            content: MessageContent::Text { content: "".to_string(), thought_signature: None },
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                        });
                    }
                }

                let prompt_data = {
                    let state = session_state.read();
                    let session = state.get_active_session().unwrap();
                    let builder = PromptBuilder::new(session, &settings, &state);
                    builder.build_prompt("".to_string(), None)
                };

                if let Err(e) = session_state.read().save() {
                    tracing::error!("Failed to save session state before continuation: {}", e);
                }

                let mcp_context = session_state.read().get_active_session().and_then(|s| s.active_context.mcp_tools.clone());
                tracing::info!("Sending continuation prompt to LLM.");
                send_prompt_to_llm(prompt_data, mcp_context, hobbes_message_id);
            });
        })
    };

    use_effect(move || {
        continuation_controller.write().register_callback(continue_prompt_flow.clone());
    });
    
    let root_classes = "relative flex flex-col bg-dark-bg text-dark-text rounded-lg shadow-2xl h-full w-full flex-1 min-h-0";

    rsx! {
        div {
            class: "{root_classes}",
            onmounted: move |cx| container_element.set(Some(cx.data())),
            MessageList {
                stream_update_trigger: stream_update_trigger,
                show_scroll_button: show_scroll_button,
            },
            ChatInput {
                is_sending: Signal::new(stream_manager.is_sending.read().clone() || stream_manager.is_any_generating()),
                on_send: move |(msg, attachments)| send_message((msg, attachments)),
                on_cancel: move |_| cancel_message(),
                on_interaction: on_interaction,
                on_toggle_sessions: on_toggle_sessions,
                on_toggle_settings: on_toggle_settings,
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
            class: "code-block-wrapper relative bg-dark-section rounded-lg my-2",
            button {
                class: "absolute top-2 right-2 p-1.5 rounded text-gray-400 hover:bg-dark-card hover:text-white transition-colors",
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
                class: "w-full max-w-none p-4 text-sm whitespace-pre-wrap break-words overflow-x-auto",
                code {
                    class: "language-{lang}",
                    dangerous_inner_html: "{highlighted_html}"
                }
            }
        }
    }
}

// Sub-component for styling individual messages
use crate::components::selection_toolbar::SelectionToolbar;

#[derive(PartialEq, Clone, Copy)]
enum SelectionMode {
    None,
    Toolbar,
    CommentInput,
}

#[component]
pub fn MessageBubble(message: Message, on_content_update: EventHandler<()>, on_selection: EventHandler<(String, f64, f64)>) -> Element {
    let is_user = message.author == "User";
    
    // Get necessary contexts
    let settings = consume_context::<Signal<Settings>>();
    let stream_manager = consume_context::<StreamManagerContext>();
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    
    let mut is_thinking = false;
    let mut thought_signature: Option<String> = None;

    if let MessageContent::Text { thought_signature: ts, .. } = &message.content {
        if stream_manager.is_generating(&message.id) {
             is_thinking = true;
        }
        thought_signature = ts.clone();
    }

    match &message.content {
        MessageContent::Text { content: text_content, .. } => {
            let mut content = use_signal(|| text_content.clone());
            let mut copied = use_signal(|| false);
            let mut show_thinking = use_signal(|| false);
            
            // Inline comment state
            let mut selection_mode = use_signal(|| SelectionMode::None);
            let mut selection_data = use_signal(|| (String::new(), 0.0, 0.0)); // text, top, left
            
            // Setup eval for text selection
            let message_id_str = message.id.to_string();
            
            use_effect(move || {
                let message_id_clone = message_id_str.clone();
                spawn(async move {
                    let mut eval = document::eval(&format!(r#"
                        const bubble = document.getElementById('message-bubble-{}');
                        if (bubble) {{
                            bubble.addEventListener('mouseup', (e) => {{
                                const selection = window.getSelection();
                                if (!selection.isCollapsed && bubble.contains(selection.anchorNode)) {{
                                    const range = selection.getRangeAt(0);
                                    const rect = range.getBoundingClientRect();
                                    const text = selection.toString();
                                    dioxus.send({{ text: text, top: rect.bottom + window.scrollY, left: rect.left + window.scrollX }});
                                }}
                            }});
                        }}
                    "#, message_id_clone));

                    while let Ok(msg) = eval.recv().await {
                        if let Ok(data) = serde_json::from_value::<SelectionData>(msg) {
                            if !data.text.trim().is_empty() {
                                selection_data.set((data.text.clone(), data.top, data.left));
                                selection_mode.set(SelectionMode::Toolbar);
                            }
                        }
                    }
                });
            });

            // This effect runs once when the component is created.
            // If it's a streaming Hobbes message, it takes the stream and updates its local state.
            use_effect(move || {
                let _stream_activity = stream_manager.stream_activity;
                if !is_user && stream_manager.is_streaming(&message.id) {
                    spawn(async move {
                        if let Some(mut rx) = stream_manager.take_stream(&message.id) {
                            while let Some(stream_msg) = rx.recv().await {
                                if let StreamMessage::Text { content: chunk, .. } = stream_msg {
                                    tracing::debug!("CHUNK RECEIVED: '{}'", &chunk);
                                    content.write().push_str(&chunk);
                                    on_content_update.call(());
                                }
                            }
                        }
                    });
                }
            });

            let is_thinking = !is_user && content.read().is_empty();
            let thinking_mode_enabled = settings.read().gemini_config.thinking_enabled;

            let bubble_classes = if is_user {
                "bg-primary-500 text-white self-end ml-auto"
            } else {
                "bg-dark-card text-dark-text self-start mr-auto"
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
                class: "{container_classes} w-full",
                div {
                    class: "flex flex-col max-w-2/3 min-w-0",
                    div {
                        id: "message-bubble-{message.id}",
                        class: "relative group rounded-2xl {bubble_classes} max-w-full overflow-hidden",
                        div {
                            class: "px-4 py-3 text-sm leading-relaxed break-words",
                            if is_thinking {
                                ThinkingIndicator { thinking_mode_enabled }
                            } else {
                                MarkdownRenderer { 
                                    content: content(), 
                                    comments: message.comments.clone(),
                                    pending_highlight: if *selection_mode.read() != SelectionMode::None {
                                        Some(selection_data.read().0.clone())
                                    } else {
                                        None
                                    }
                                }
                                if !message.attachments.is_empty() {
                                    div {
                                        class: "flex flex-col space-y-2 mt-2",
                                        for attachment in &message.attachments {
                                            img {
                                                src: format!("data:{};base64,{}", attachment.mime_type, attachment.data),
                                                class: "max-w-full rounded-lg",
                                                alt: attachment.file_name.clone(),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        
                        if *selection_mode.read() == SelectionMode::Toolbar {
                            SelectionToolbar {
                                position_top: selection_data.read().1,
                                position_left: selection_data.read().2,
                                on_copy: move |_| {
                                    let text = selection_data.read().0.clone();
                                    spawn(async move {
                                        let mut eval = document::eval(&format!("navigator.clipboard.writeText(`{}`);", text));
                                        let _: Result<serde_json::Value, _> = eval.recv().await;
                                    });
                                    selection_mode.set(SelectionMode::None);
                                },
                                on_comment: move |_| {
                                    selection_mode.set(SelectionMode::CommentInput);
                                    on_selection.call((selection_data.read().0.clone(), selection_data.read().1, selection_data.read().2));
                                }
                            }
                        }

                        if *selection_mode.read() == SelectionMode::CommentInput {
                            crate::components::inline_comment_popover::InlineCommentPopover {
                                position_top: selection_data.read().1,
                                position_left: selection_data.read().2,
                                on_save: move |comment_text: String| {
                                    let (text, _, _) = selection_data.read().clone();
                                    let new_comment = Comment {
                                        id: Uuid::new_v4().to_string(),
                                        text_selection: text,
                                        start_offset: 0, // Not used in this version
                                        end_offset: 0,   // Not used in this version
                                        comment: comment_text,
                                    };
                                    
                                    // Update session state
                                    let mut state = session_state.write();
                                    if let Some(msg) = state.get_message_mut(&message.id) {
                                        msg.comments.push(new_comment);
                                    }
                                    if let Err(e) = state.save() {
                                        tracing::error!("Failed to save session after adding comment: {}", e);
                                    }
                                    
                                    selection_mode.set(SelectionMode::None);
                                },
                                on_cancel: move |_| {
                                    selection_mode.set(SelectionMode::None);
                                }
                            }
                        }

                        if !is_user && !is_thinking {
                            div {
                                class: "flex items-center justify-end px-2 py-1 space-x-2 opacity-0 group-hover:opacity-100 transition-opacity",
                                button {
                                    class: "p-1 text-gray-400 hover:text-white rounded transition-colors",
                                    onclick: move |_| {
                                        let text = content();
                                        spawn(async move {
                                            let mut eval = document::eval(&format!("navigator.clipboard.writeText(`{}`);", text));
                                            let _: Result<serde_json::Value, _> = eval.recv().await;
                                            copied.set(true);
                                            sleep(std::time::Duration::from_secs(2)).await;
                                            copied.set(false);
                                        });
                                    },
                                    title: "Copy message",
                                    if *copied.read() {
                                        Icon { width: 14, height: 14, icon: fi_icons::FiCheck }
                                    } else {
                                        Icon { width: 14, height: 14, icon: fi_icons::FiCopy }
                                    }
                                }
                            }
                        }
                        
                        if !is_thinking && thought_signature.is_some() {
                            div {
                                class: "mt-3 pt-3 border-t border-gray-600",
                                button {
                                    class: "flex items-center text-xs text-gray-400 hover:text-gray-300 focus:outline-none",
                                    onclick: move |_| {
                                        let current = *show_thinking.read();
                                        show_thinking.set(!current);
                                    },
                                    if *show_thinking.read() {
                                        Icon { 
                                            width: 12, 
                                            height: 12, 
                                            icon: fi_icons::FiChevronDown,
                                            class: "mr-1"
                                        }
                                    } else {
                                        Icon { 
                                            width: 12, 
                                            height: 12, 
                                            icon: fi_icons::FiChevronRight,
                                            class: "mr-1"
                                        }
                                    }
                                    "Thinking Process"
                                }
                                if *show_thinking.read() {
                                    div {
                                        class: "mt-2 p-3 bg-dark-bg rounded-lg text-xs text-gray-300 font-mono whitespace-pre-wrap",
                                        "{thought_signature.as_ref().unwrap()}"
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
        _ => rsx! {}
    }
}

#[component]
pub fn LinkWithControls(href: String, text: String) -> Element {
    let mut draft = use_context::<Signal<String>>();
    let mut copied = use_signal(|| false);
    let mut is_hovered = use_signal(|| false);
    let href_clone_for_copy = href.clone();
    let href_clone_for_summarize = href.clone();

    rsx! {
        span {
            class: "relative inline-block",
            onmouseenter: move |_| is_hovered.set(true),
            onmouseleave: move |_| is_hovered.set(false),
            a {
                href: "{href}",
                target: "_blank",
                rel: "noopener noreferrer",
                class: "text-primary-400 hover:text-primary-300",
                "{text}"
            }
            span {
                class: format!("inline-flex items-center absolute left-full ml-1 z-10 {} transition-opacity duration-200 bg-gray-900 bg-opacity-75 border border-gray-700 rounded-full shadow-lg p-0.5 space-x-0.5", if *is_hovered.read() { "opacity-100" } else { "opacity-0" }),
                
                button {
                    class: "p-1.5 rounded text-gray-400 hover:bg-dark-card hover:text-white transition-colors",
                    onclick: move |_| {
                        let href_clone = href_clone_for_copy.clone();
                        spawn(async move {
                            if copy_to_clipboard(&href_clone).is_ok() {
                                copied.set(true);
                                sleep(Duration::from_secs(2)).await;
                                copied.set(false);
                            }
                        });
                    },
                    if *copied.read() {
                        Icon { width: 16, height: 16, icon: fi_icons::FiCheck }
                    } else {
                        Icon { width: 16, height: 16, icon: fi_icons::FiClipboard }
                    }
                }
                button {
                    class: "p-1.5 rounded text-gray-400 hover:bg-dark-card hover:text-white transition-colors",
                    onclick: move |_| {
                        let summary_prompt = format!("Please fetch {} and summarize.", href_clone_for_summarize);
                        draft.set(summary_prompt);
                        let _ = document::eval(r#"
                            const el = document.getElementById('chat-textarea');
                            if (el) {
                                el.focus();
                                el.style.height = 'auto';
                                el.style.height = (el.scrollHeight) + 'px';
                            }
                        "#);
                    },
                    Icon { width: 16, height: 16, icon: fi_icons::FiFileText }
                }
            }
        }
    }
}

#[component]
fn ThinkingIndicator(thinking_mode_enabled: bool) -> Element {
    rsx! {
        if thinking_mode_enabled {
            div {
                class: "flex items-center space-x-2",
                Icon {
                    width: 16,
                    height: 16,
                    icon: fi_icons::FiCpu,
                    class: "text-white animate-pulse"
                }
                span {
                    class: "text-sm text-white",
                    "Thinking..."
                }
            }
        } else {
            div {
                class: "flex items-center space-x-1",
                span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-fast" },
                span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-medium" },
                span { class: "w-2.5 h-2.5 bg-white rounded-full animate-pulse-slow" },
            }
        }
    }
}

#[component]
pub fn WelcomeMessage() -> Element {
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
