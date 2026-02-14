// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use super::chat_input::ChatInput;
use super::confirm_delete_modal::ConfirmDeleteModal;
use super::continuation_controller::ContinuationController;
use super::forget_memory_modal::ForgetMemoryModal;
use super::message_list::MessageList;
use super::new_chat_memory_modal::NewChatMemoryModal;
use super::quick_fix::QuickFix;
use super::shared::{DraftContext, MessageContent, SessionIdContext, StreamMessage};
use crate::components::markdown_renderer::{MarkdownRenderer, ThinkingMarkdownRenderer};
use crate::components::stream_manager::StreamManagerContext;
use crate::context::permissions::PermissionManager;
use crate::mcp::manager::{COMPOSIO_NATIVE_PREFIX, is_composio_native};
use crate::context::prompt_builder::PromptBuilder;
use crate::session::ActiveContext;
use crate::settings::Settings;
use dioxus::html::geometry::euclid::Rect;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use feature_clipboard::copy_to_clipboard;
use hobbes_core::models::Attachment;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::time::Duration;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;
use tokio::sync::mpsc;
use tokio::time::sleep;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SelectionData {
    text: String,
    #[serde(default)]
    markdown: String,
    #[serde(default)]
    top: f64,
    #[serde(default)]
    left: f64,
    #[serde(default)]
    hide: bool,
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
    /// Token usage data and cost for this message
    #[serde(default)]
    pub usage: Option<crate::components::shared::UsageData>,
}

// The main ChatWindow component
#[component]
pub fn ChatWindow(
    on_content_resize: EventHandler<Rect<f64, f64>>,
    on_interaction: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let mut settings = use_context::<Signal<Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let permission_manager = use_context::<Signal<PermissionManager>>();
    let mut chat_command = use_context::<Signal<Option<super::chat_input::ChatCommand>>>();
    
    // Consume current_session_id from global context
    let SessionIdContext(current_session_id_signal) = use_context::<SessionIdContext>();
    let current_target_id = current_session_id_signal;
    
    // Now provide draft via a unique context type to avoid collisions
    let draft = use_signal(|| "".to_string());
    use_context_provider(|| DraftContext(draft));
    
    let mut container_element = use_signal(|| None as Option<Rc<MountedData>>);
    let stream_manager = consume_context::<StreamManagerContext>();
    let active_message_id = use_signal(|| None::<Uuid>);
    let active_session_for_stream = use_signal(|| None::<String>);
    let mut continuation_controller = consume_context::<Signal<ContinuationController>>();
    let mut is_initial_load = use_signal(|| true);
    let mut last_session_id = use_signal(|| session_state.read().active_session_id.clone());
    let mut stream_update_trigger = use_signal(|| 0);
    let mut show_scroll_button = use_signal(|| false);

    // Shared helper: fetch fresh MCP context for a session and update its active_context.
    // Returns the fetched McpContext for downstream use (e.g. passing to send_prompt_to_llm).
    let fetch_fresh_mcp_context = move |target_id: String, settings_snapshot: Settings| async move {
        // Pass the profile ID directly (composio_profile is the stable ID)
        let profile_id = session_state.read()
            .sessions.get(&target_id)
            .and_then(|s| s.composio_profile.clone());

        // ensure_native_client_for_profile can accept name or ID — pass the ID
        if let Some(ref id) = profile_id {
            let _ = mcp_manager.read()
                .ensure_native_client_for_profile(id, &settings_snapshot).await;
        }

        let fresh = mcp_manager.read().get_mcp_context(profile_id).await;
        {
            let mut state = session_state.write();
            if let Some(session) = state.sessions.get_mut(&target_id) {
                if !fresh.servers.is_empty() {
                    session.active_context.mcp_tools = Some(fresh.clone());
                } else {
                    session.active_context.mcp_tools = None;
                }
            }
        }
        fresh
    };

    let session = use_memo(move || {
        // Explicitly read both signals to establish reactive subscriptions
        let target_id = current_target_id.read();
        let state = session_state.read();
        state.sessions.get(&*target_id).cloned()
    });


    // Delete modal state
    let mut show_delete_confirm_modal = use_signal(|| false);
    let mut pending_delete_message_id = use_signal(|| None::<String>);
    let mut delete_message_count = use_signal(|| 0);
    let mut has_new_comments = use_signal(|| false);
    let mut has_pending_approvals = use_signal(|| false);
    use_context_provider(|| has_pending_approvals);

    // New Chat with Memory Modal State
    let mut show_new_chat_memory_modal = use_signal(|| false);
    let mut modal_initial_context = use_signal(ActiveContext::default);

    // Forget Memory Modal State
    let mut show_forget_memory_modal = use_signal(|| false);
    let mut forget_modal_context = use_signal(ActiveContext::default);
    let mut modal_optimization_summary = use_signal(|| Option::<String>::None);

    let on_interaction = move || {
        show_scroll_button.set(false);
    };

    // Sync mcp_context signal changes to the active session's mcp_tools
    // This ensures the UI updates immediately when tools are loaded/unloaded
    // We filter tools by the session's active profile to ensure isolation.
    use_effect(move || {
        let mut current_context = _mcp_context.read().clone();
        let mut state = session_state.write();
        if let Some(session) = state.sessions.get_mut(&*current_target_id.read()) {
            let profile_id = session.composio_profile.clone();
            
            // Filter servers to only include non-native or profile-matching native tools
            // Use the server_name convention: config.name contains the display key,
            // but we filter by matching the profile_id against the known session profile.
            current_context.servers.retain(|s| {
                if is_composio_native(&s.name) {
                    if let Some(ref target_id) = profile_id {
                        // Include singleton (legacy) or if name contains the target profile ID/name
                        let suffix = s.name.strip_prefix("composio-native:").unwrap_or_default();
                        suffix.is_empty() || suffix == target_id
                    } else {
                        // If no profile, only allow the legacy/default composio-native
                        s.name == COMPOSIO_NATIVE_PREFIX
                    }
                } else {
                    true
                }
            });


                if !current_context.servers.is_empty() {
                session.active_context.mcp_tools = Some(current_context);
            } else {
                session.active_context.mcp_tools = None;
            }
        }
    });
    use_effect(move || {
        // By reading the session state here, the effect becomes dependent on it.
        // Any change to messages will cause this to re-run.
        let _ = stream_update_trigger.read();
        let current_session_id = current_target_id.read().clone();

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
                    let _ = document::eval(
                        r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTop = el.scrollHeight; }
                    "#,
                    )
                    .await;
                    if *is_initial_load.read() {
                        is_initial_load.set(false);
                    }
                }

                // After scrolling, check if the scroll button should be visible.
                let show_button = if let Ok(result) = document::eval(
                    r#"
                    const el = document.getElementById('message-list');
                    if (el) {
                        // Show button if not at the bottom (with a small threshold)
                        return el.scrollHeight - el.scrollTop - el.clientHeight > 10;
                    }
                    return false; // Don't show if element doesn't exist
                "#,
                )
                .await
                {
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

    use_effect(move || {
        if *has_pending_approvals.read() {
            spawn(async move {
                let mut session_state = session_state;
                let mcp_manager = mcp_manager;
                let active_session_id = current_target_id.read().clone();
                let mut tools_to_run = Vec::new();
                let mut stream_manager_is_sending = stream_manager.is_sending;

                // Indicate activity immediately
                stream_manager_is_sending.set(true);
                // Clear the signal so we don't re-trigger loop
                has_pending_approvals.set(false);

                // 1. Identify tools that need to be run and capture profile context
                let active_session_id_inner = active_session_id.clone();
                let composio_profile = {
                    let state = session_state.read();
                    state.sessions.get(&active_session_id_inner)
                        .and_then(|s| s.composio_profile.clone())
                };

                {
                    let state = session_state.read();
                    if let Some(session) = state.sessions.get(&active_session_id) {
                        for msg in &session.messages {
                            if let crate::components::shared::MessageContent::ToolCall(tc) =
                                &msg.content
                            {
                                if tc.status == crate::components::shared::ToolCallStatus::Running {
                                    tools_to_run.push((msg.id, tc.clone()));
                                }
                            }
                        }
                    }
                }

                if tools_to_run.is_empty() {
                    // This could be a Skill continuation (no tools to run)
                    continuation_controller.read().trigger_continuation();
                    return;
                }

                // 2. Execute tools
                for (msg_id, tool_call) in tools_to_run {
                    let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    // Bypass permission check since user explicitly approved this instance
                    let manager = mcp_manager.read().clone();
                    let result_receiver = manager
                        .use_mcp_tool(
                            &tool_call.server_name,
                            &tool_call.tool_name,
                            args_json,
                            true,
                            composio_profile.clone(),
                        )
                        .await;

                    let (status, response_str, _) = match result_receiver {
                        Ok(receiver) => {
                            crate::mcp::manager::McpManager::process_tool_output(receiver).await
                        }
                        Err(e) => (crate::components::shared::ToolCallStatus::Error, e, false),
                    };

                    // Update session state with result
                    {
                        let mut state = session_state.write();

                        // Update message status
                        if let Some(msg) = state.get_message_mut(&msg_id) {
                            if let crate::components::shared::MessageContent::ToolCall(tc) =
                                &mut msg.content
                            {
                                tc.status = status;
                                tc.response = response_str.clone();
                            }
                        }

                        // Add to history for context
                        state
                            .tool_call_history
                            .push(crate::components::shared::ToolCallRecord {
                                call: tool_call.clone(),
                                result: crate::components::shared::ToolResult {
                                    status,
                                    response: response_str,
                                },
                                profile_color: {
                                    let settings_read = settings.read();
                                    crate::components::shared::resolve_profile_color(
                                        composio_profile.as_ref(),
                                        &settings_read,
                                    )
                                },
                            });
                    }
                }

                // 3. Trigger continuation to send results back to LLM
                continuation_controller.read().trigger_continuation();
            });
        }
    });

    // Reusable closure for sending a message
    let send_prompt_to_llm = {
        move |prompt_data: crate::context::prompt_builder::LlmPrompt,
              mcp_context: Option<crate::mcp::manager::McpContext>,
              hobbes_message_id: Uuid,
              session_id: String| {
            spawn(async move {
                let mut active_message_id = active_message_id;
                let mut active_session_for_stream = active_session_for_stream;

                active_message_id.set(Some(hobbes_message_id));
                active_session_for_stream.set(Some(session_id.clone()));
                tracing::debug!("Lock ACQUIRED.");

                let (tx, mut rx) = mpsc::unbounded_channel::<()>();

                let on_complete = {
                    let mut active_message_id = active_message_id;
                    let mut active_session_for_stream = active_session_for_stream;
                    move || {
                        active_message_id.set(None);
                        active_session_for_stream.set(None);
                        let _ = tx.send(());
                    }
                };

                stream_manager.start_stream(
                    hobbes_message_id,
                    session_id.clone(),
                    prompt_data,
                    on_complete,
                    mcp_context,
                    {
                        let state = session_state.read();
                        state.sessions.get(&session_id)
                            .and_then(|s| s.composio_profile.clone())
                    },
                );

                rx.recv().await;
                tracing::debug!(message_id = %hobbes_message_id, "Stream completion signal RECEIVED.");

                tracing::debug!("Lock RELEASED.");
            });
        }
    };

    let send_message = std::rc::Rc::new(move |(user_message, attachments): (String, Vec<Attachment>)| {
        spawn({
            let mut session_state = session_state;
            let settings = settings;
            let send_prompt_to_llm = send_prompt_to_llm;
            let mut permission_manager = permission_manager;
            let mut has_new_comments = has_new_comments;
            let _target_session_id = current_target_id;

            async move {
            let settings_read = settings.read().clone();
            let target_id = current_target_id.read().clone();

            // Reset the AI turn count every time the user sends a message.
            permission_manager.write().reset_turn_count();

            // Clear the tool call history to ensure a fresh start for the new turn.
            session_state.write().tool_call_history.clear();

            // Check if the last message was the turn limit warning.
            let last_message_was_warning = session_state.read().sessions.get(&target_id)
                .and_then(|s| s.messages.last())
                .is_some_and(|m| {
                    if let MessageContent::Text { content: text, .. } = &m.content {
                        text.starts_with("Pardon, I have reached the 'Max Turn Limit' currently set to X in settings")
                    } else {
                        false
                    }
                });

            if last_message_was_warning {
                permission_manager.write().reset_turn_count();
            }

            if user_message.trim().is_empty() && attachments.is_empty() {
                if *has_new_comments.read() {
                    has_new_comments.set(false);
                    let hobbes_message_id = Uuid::new_v4();
                    {
                        let mut state = session_state.write();
                        if let Some(session) = state.sessions.get_mut(&target_id) {
                            session.messages.push(Message {
                                id: hobbes_message_id,
                                author: "Hobbes".to_string(),
                                content: MessageContent::Text {
                                    content: "".to_string(),
                                    thought_signature: None,
                                    thought_summary: None,
                                },
                                attachments: Vec::new(),
                                comments: Vec::new(),
                                created_at: chrono::Utc::now(),
                                usage: None,
                            });
                        }
                    }

                    let fresh_mcp_context = fetch_fresh_mcp_context(target_id.clone(), settings_read.clone()).await;

                    let prompt_data = {
                        let state = session_state.read();
                        if let Some(session) = state.sessions.get(&target_id) {
                            let builder = PromptBuilder::new(session, &settings_read, &state);
                            builder.build_prompt("".to_string(), None)
                        } else {
                            return;
                        }
                    };

                    let mcp_context = if fresh_mcp_context.servers.is_empty() {
                        None
                    } else {
                        Some(fresh_mcp_context)
                    };
                    send_prompt_to_llm(prompt_data, mcp_context, hobbes_message_id, target_id.clone());
                }
                return;
            }

            has_new_comments.set(false);

            if permission_manager.read().is_turn_limit_reached() {
                let mut state = session_state.write();
                if let Some(session) = state.sessions.get_mut(&target_id) {
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "User".to_string(),
                        content: MessageContent::Text {
                            content: user_message.clone(),
                            thought_signature: None,
                            thought_summary: None,
                        },
                        attachments,
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                        usage: None,
                    });
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "Hobbes".to_string(),
                        content: MessageContent::Text { content: format!("Pardon, I have reached the 'Max Turn Limit' currently set to {} in settings and need permission to continue.", settings_read.permission_settings.max_ai_turns), thought_signature: None, thought_summary: None },
                        attachments: Vec::new(),
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                        usage: None,
                    });
                }
                return;
            }

            let hobbes_message_id = Uuid::new_v4();
            {
                let mut state = session_state.write();
                if let Some(session) = state.sessions.get_mut(&target_id) {
                    session.messages.push(Message {
                        id: Uuid::new_v4(),
                        author: "User".to_string(),
                        content: MessageContent::Text {
                            content: user_message.clone(),
                            thought_signature: None,
                            thought_summary: None,
                        },
                        attachments,
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                        usage: None,
                    });
                    session.messages.push(Message {
                        id: hobbes_message_id,
                        author: "Hobbes".to_string(),
                        content: MessageContent::Text {
                            content: "".to_string(),
                            thought_signature: None,
                            thought_summary: None,
                        },
                        attachments: Vec::new(),
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                        usage: None,
                    });
                }
            }

            let _fresh_mcp = fetch_fresh_mcp_context(target_id.clone(), settings_read.clone()).await;

            // Preserve conversation summary across the send
            {
                let conversation_summary = session_state
                    .read()
                    .sessions.get(&target_id)
                    .map(|s| s.active_context.conversation_summary.clone())
                    .unwrap_or_default();
                let mut state = session_state.write();
                if let Some(session) = state.sessions.get_mut(&target_id) {
                    session.active_context.conversation_summary = conversation_summary;
                }
            }

            let prompt_data = {
                let user_prompt = user_message.clone();
                let state = session_state.read();
                if let Some(session) = state.sessions.get(&target_id) {
                    let builder = PromptBuilder::new(session, &settings_read, &state);
                    builder.build_prompt(user_prompt, None)
                } else {
                    tracing::error!("No active session found when building prompt");
                    return;
                }
            };

            spawn(async move {
                let state_snapshot = session_state.read().clone();
                let _ = tokio::task::spawn_blocking(move || state_snapshot.save()).await;
            });

            let mcp_context = session_state
                .read()
                .sessions.get(&target_id)
                .and_then(|s| s.active_context.mcp_tools.clone());
            send_prompt_to_llm(prompt_data, mcp_context, hobbes_message_id, target_id.clone());
        }});
    });

    let cancel_message = move || {
        if let Some(id) = *active_message_id.read() {
            let session_id = active_session_for_stream.read().clone().unwrap_or_else(|| {
                current_target_id.read().clone()
            });
            stream_manager.cancel_stream(&id, &session_id);
        }
    };

    let continue_prompt_flow = {
        Rc::new(move || {
            tracing::debug!("continue_prompt_flow callback INVOKED.");
            spawn(async move {
                tracing::debug!("continue_prompt_flow task SPAWNED.");
                let mut session_state = session_state;
                let settings = settings.read().clone();

                let hobbes_message_id = Uuid::new_v4();
                let target_id = current_target_id.read().clone();
                {
                    let mut state = session_state.write();
                    if let Some(session) = state.sessions.get_mut(&target_id) {
                        session.messages.push(Message {
                            id: hobbes_message_id,
                            author: "Hobbes".to_string(),
                            content: MessageContent::Text {
                                content: "".to_string(),
                                thought_signature: None,
                                thought_summary: None,
                            },
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        });
                    }
                }

                let fresh_mcp_context = fetch_fresh_mcp_context(target_id.clone(), settings.clone()).await;

            let prompt_data = {
                let state = session_state.read();
                if let Some(session) = state.sessions.get(&target_id) {
                    let builder = PromptBuilder::new(session, &settings, &state);
                    builder.build_prompt("".to_string(), None)
                    } else {
                        tracing::error!("Active session lost during continuation flow - aborting prompt build");
                        return;
                    }
                };

                spawn(async move {
                    let state_snapshot = session_state.read().clone();
                    let _ = tokio::task::spawn_blocking(move || state_snapshot.save()).await;
                });

                let mcp_context = if fresh_mcp_context.servers.is_empty() {
                    None
                } else {
                    Some(fresh_mcp_context)
                };
                tracing::debug!("Sending continuation prompt to LLM.");
                send_prompt_to_llm(prompt_data, mcp_context, hobbes_message_id, target_id.clone());
            });
        })
    };

    use_effect(move || {
        continuation_controller
            .write()
            .register_callback(continue_prompt_flow.clone());
    });

    let root_classes = "relative flex flex-col bg-app text-fg rounded-lg shadow-2xl h-full w-full flex-1 min-h-0";

    let delete_message = move |message_id: Uuid| {
        let confirm = settings.read().confirm_on_message_delete;
        if confirm {
            if let Some(index) = session_state
                .read()
                .sessions.get(&*current_target_id.read())
                .and_then(|s| s.messages.iter().position(|m| m.id == message_id))
            {
                let session_len = session_state
                    .read()
                    .sessions.get(&*current_target_id.read())
                    .map(|s| s.messages.len())
                    .unwrap_or(0);
                let count = session_len - index;
                delete_message_count.set(count);
                pending_delete_message_id.set(Some(message_id.to_string()));
                show_delete_confirm_modal.set(true);
            }
        } else {
            // Delete immediately
            let message_id_str = message_id.to_string();
            let active_session_id = current_target_id.read().clone();
            if let Some(session) = session_state.write().sessions.get_mut(&active_session_id) {
                session.delete_message_and_after(&message_id_str);
                // Trigger update
                stream_update_trigger.set(stream_update_trigger() + 1);
            }
        }
    };

    // Optimization Routing
    #[derive(Clone, Copy, PartialEq)]
    enum OptimizationTarget {
        Session,
        NewChatModal,
    }
    let mut optimization_target = use_signal(|| OptimizationTarget::Session);

    // Forget Modal Logic
    let on_forget_apply = move |(new_context, summary): (ActiveContext, String)| {
        let routing_target = *optimization_target.read();
        match routing_target {
            OptimizationTarget::Session => {
                {
                    let mut state = session_state.write();
                    if let Some(session) = state.sessions.get_mut(&*current_target_id.read()) {
                        session.active_context = new_context;
                        session.memory_optimization_summary = Some(summary.clone());

                        // Insert Internal Turn Message
                        session.messages.push(Message {
                            id: Uuid::new_v4(),
                            author: "Hobbes".to_string(),
                            content: MessageContent::Text {
                                content: format!("✨ **Memory Optimized**\n\n{}", summary),
                                thought_signature: None,
                                thought_summary: None,
                            },
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        });
                    }
                }
                spawn(async move {
                    let state_snapshot = session_state.read().clone();
                    let _ = tokio::task::spawn_blocking(move || state_snapshot.save()).await;
                });
                // Force refresh messagelist
                stream_update_trigger.set(stream_update_trigger() + 1);
            }
            OptimizationTarget::NewChatModal => {
                // Update the New Chat Modal's initial context signal
                // This triggers the use_effect in NewChatMemoryModal to update the JSON editor
                
                // CRITICAL: The new_context coming back might be stripped of tools (because we stripped them before sending to optimization).
                // We must preserve the tools from the *existing* modal_initial_context.
                let mut preserved_context = new_context;
                let current_modal_context = modal_initial_context.read();
                
                if preserved_context.mcp_tools.is_none() {
                    preserved_context.mcp_tools = current_modal_context.mcp_tools.clone();
                }
                if preserved_context.tools.is_none() {
                    preserved_context.tools = current_modal_context.tools.clone();
                }

                drop(current_modal_context); // Release verification read lock before write
                modal_initial_context.set(preserved_context);
                modal_optimization_summary.set(Some(summary));
            }
        }
        show_forget_memory_modal.set(false);
    };

    let session_guard = session.read();
    if session_guard.is_some() {
        rsx! {

        div {
            class: "{root_classes}",
            onmounted: move |cx| container_element.set(Some(cx.data())),
            MessageList {
                stream_update_trigger: stream_update_trigger,
                show_scroll_button: show_scroll_button,
                on_delete: delete_message,
                on_comment: move |_| has_new_comments.set(true),
            },
            ChatInput {
                is_sending: Signal::new(*stream_manager.is_sending.read() || stream_manager.is_any_generating()),
                has_new_comments: has_new_comments,
                has_pending_approvals: has_pending_approvals,
                on_send: {
                    let send_message = send_message.clone();
                    move |(msg, attachments)| send_message((msg, attachments))
                },
                on_cancel: move |_| cancel_message(),
                on_interaction: on_interaction,
                on_new_chat_with_memory: move |_| {
                    if let Some(session) = session_state.read().sessions.get(&*current_target_id.read()) {
                        modal_initial_context.set(session.active_context.clone());
                        show_new_chat_memory_modal.set(true);
                    }
                },
            }

            NewChatMemoryModal {
                is_visible: show_new_chat_memory_modal,
                initial_context: modal_initial_context.read().clone(),
                optimization_summary: modal_optimization_summary,
                on_start_chat: move |new_context: ActiveContext| {
                     // Create new session with memory
                     let new_session_id = session_state.write().create_session(settings.peek().get_active_profile().map(|p| p.id.clone()));
                     // Update context of the new session
                     if let Some(session) = session_state.write().sessions.get_mut(&new_session_id) {
                         session.active_context = new_context;
                     }
                     // Open the new session in a tab via the command bus
                     chat_command.set(Some(super::chat_input::ChatCommand::SwitchToSession(new_session_id)));
                     show_new_chat_memory_modal.set(false);
                     modal_optimization_summary.set(None); // Clear summary on close
                },
                on_optimize_memory: move |current_context: ActiveContext| {
                    optimization_target.set(OptimizationTarget::NewChatModal);
                    forget_modal_context.set(current_context);
                    show_forget_memory_modal.set(true);
                },
                on_cancel: move |_| {
                    show_new_chat_memory_modal.set(false);
                    modal_optimization_summary.set(None); // Clear summary on close
                },
            }

            ForgetMemoryModal {
                is_visible: show_forget_memory_modal,
                current_context: forget_modal_context.read().clone(),
                on_apply: on_forget_apply,
                on_cancel: move |_| show_forget_memory_modal.set(false),
            }

            ConfirmDeleteModal {
                is_visible: show_delete_confirm_modal,
                title: "Delete Messages",
                message: format!("Are you sure you want to delete this message and the {} messages that follow? This cannot be undone.", delete_message_count().saturating_sub(1)),
                confirm_button_text: "Delete",
                show_dont_ask_again: true,
                on_confirm: move |dont_ask_again: bool| {
                    if dont_ask_again {
                        settings.write().confirm_on_message_delete = false;
                    }
                    if let Some(id) = pending_delete_message_id.read().as_ref() {
                        let active_session_id = current_target_id.read().clone();
                        if let Some(session) = session_state.write().sessions.get_mut(&active_session_id) {
                            session.delete_message_and_after(id);
                            // Trigger update
                            stream_update_trigger.set(stream_update_trigger() + 1);
                        }
                    }
                    show_delete_confirm_modal.set(false);
                    pending_delete_message_id.set(None);
                },
                on_cancel: move |_| {
                    show_delete_confirm_modal.set(false);
                    pending_delete_message_id.set(None);
                }
            }
        }
    }
    } else {
        rsx! {
            div { class: "flex-1 flex items-center justify-center text-fg-muted", "Synchronizing..." }
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
        let syntax = SYNTAX_SET
            .find_syntax_by_token(&lang_for_memo)
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        let mut h = HighlightLines::new(syntax, &THEME);
        let mut html = String::new();
        for line in code.lines() {
            let regions = h.highlight_line(line, &SYNTAX_SET).unwrap();
            let html_line =
                styled_line_to_highlighted_html(&regions, IncludeBackground::No).unwrap();
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
            class: "code-block-wrapper relative bg-section rounded-lg my-2 overflow-hidden min-w-0",
            button {
                class: "absolute top-2 right-2 p-1.5 rounded text-fg-muted hover:bg-card hover:text-fg transition-colors z-10",
                onclick: move |evt| {
                    evt.stop_propagation();
                    copy_onclick(evt);
                },
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
                class: "w-full p-4 text-sm whitespace-pre-wrap break-all overflow-x-auto min-w-0",
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
    CommentEdit,
}

#[derive(Clone, Default)]
struct SelectionState {
    text: String,
    markdown: String,
    top: f64,
    left: f64,
}

#[component]
pub fn MessageBubble(
    message: Message,
    on_content_update: EventHandler<()>,
    on_selection: EventHandler<(String, f64, f64)>,
    on_delete: EventHandler<()>,
    on_comment: EventHandler<()>,
) -> Element {
    let is_user = message.author == "User";

    // Get necessary contexts
    let _settings = consume_context::<Signal<Settings>>();
    let stream_manager = consume_context::<StreamManagerContext>();
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let DraftContext(mut chat_input_draft) = consume_context::<DraftContext>();
    let save_error = consume_context::<crate::components::shared::SaveErrorContext>().0;

    let _is_thinking = false;
    let mut thought_signature: Option<String> = None;
    let mut thought_summary: Option<String> = None;

    if let MessageContent::Text {
        thought_signature: ts,
        thought_summary: tsum,
        ..
    } = &message.content
    {
        if stream_manager.is_generating(&message.id) {
            // is_thinking = true;
        }
        thought_signature = ts.clone();
        thought_summary = tsum.clone();
    }

    match &message.content {
        MessageContent::Text {
            content: text_content,
            ..
        } => {
            let mut content = use_signal(|| text_content.clone());
            let mut local_thought_summary = use_signal(|| thought_summary.clone());
            let mut copied = use_signal(|| false);

            // Token usage display settings - consume BEFORE signal initialization
            let ui_state = consume_context::<Signal<crate::settings::UiState>>();

            // Initialize toggle states from UiState defaults (not hardcoded)
            let mut show_thinking = use_signal(|| ui_state.read().default_tool_thought_open);
            let mut show_usage = use_signal(|| false); // No default setting yet

            let display_mode = ui_state.read().token_display_mode.clone();
            let usage_data = message.usage.clone();

            // Inline comment state
            let mut selection_mode = use_signal(|| SelectionMode::None);
            let mut selection_data = use_signal(SelectionState::default);
            let mut editing_comment_id = use_signal(|| None::<String>);
            let mut is_mouse_over_toolbar = use_signal(|| false);

            // State tracking for "Thinking" vs "Generating"
            let is_streaming = stream_manager.is_generating(&message.id);
            let has_content = stream_manager.has_generated_content(&message.id);


            // Setup eval for text selection
            let message_id_str = message.id.to_string();

            use_effect(move || {
                let message_id_clone = message_id_str.clone();
                spawn(async move {
                    let mut eval = document::eval(&format!(
                        r#"
                        // Markdown reconstruction: walk DOM nodes and rebuild markdown from HTML tags
                        function reconstructMarkdown(range) {{
                            const fragment = range.cloneContents();
                            return walkNode(fragment);
                        }}
                        
                        function walkNode(node) {{
                            // Text nodes: return content directly
                            if (node.nodeType === Node.TEXT_NODE) {{
                                return node.textContent || '';
                            }}
                            
                            // Element nodes: wrap children based on tag
                            const tag = node.tagName?.toLowerCase();
                            const children = Array.from(node.childNodes).map(walkNode).join('');
                            
                            switch (tag) {{
                                case 'strong':
                                case 'b':
                                    return `**${{children}}**`;
                                case 'em':
                                case 'i':
                                    return `*${{children}}*`;
                                case 'code':
                                    return `\`${{children}}\``;
                                case 'a':
                                    return `[${{children}}](${{node.href || ''}})`;
                                case 'h1': return `# ${{children}}\n`;
                                case 'h2': return `## ${{children}}\n`;
                                case 'h3': return `### ${{children}}\n`;
                                case 'h4': return `#### ${{children}}\n`;
                                case 'h5': return `##### ${{children}}\n`;
                                case 'h6': return `###### ${{children}}\n`;
                                case 'li': return `- ${{children}}\n`;
                                case 'p': return `${{children}}\n\n`;
                                case 'br': return '\n';
                                default:
                                    return children;
                            }}
                        }}
                        
                        const bubble = document.getElementById('message-bubble-{}');
                        if (bubble) {{
                            bubble.addEventListener('mouseup', (e) => {{
                                const selection = window.getSelection();
                                if (!selection.isCollapsed && bubble.contains(selection.anchorNode)) {{
                                    const range = selection.getRangeAt(0);
                                    const rect = range.getBoundingClientRect();
                                    const text = selection.toString();
                                    const markdown = reconstructMarkdown(range);
                                    
                                    // Smart positioning: PREFER BELOW, then above
                                    const popoverHeight = 160; // Approx height including padding
                                    const viewportHeight = window.innerHeight;
                                    const spaceBelow = viewportHeight - rect.bottom;
                                    const wouldOverflowBottom = spaceBelow < popoverHeight + 20;
                                    
                                    let top;
                                    if (wouldOverflowBottom) {{
                                        // Position above
                                        top = rect.top + window.scrollY - popoverHeight - 8;
                                    }} else {{
                                        // Position below
                                        top = rect.bottom + window.scrollY + 8;
                                    }}

                                    dioxus.send({{ 
                                        text: text,
                                        markdown: markdown,
                                        top: top, 
                                        left: rect.left + window.scrollX, 
                                        hide: false 
                                    }});
                                }}
                            }});
                        }}

                        // Global listener to hide toolbar when selection is cleared or user clicks out
                        document.addEventListener('selectionchange', () => {{
                            const selection = window.getSelection();
                            if (selection.isCollapsed) {{
                                dioxus.send({{ text: "", markdown: "", top: 0, left: 0, hide: true }});
                            }}
                        }});

                        document.addEventListener('mousedown', (e) => {{
                            const selection = window.getSelection();
                            const toolbar = document.getElementById('selection-toolbar');
                            if (bubble && !bubble.contains(e.target) && (!toolbar || !toolbar.contains(e.target))) {{
                                dioxus.send({{ text: "", markdown: "", top: 0, left: 0, hide: true }});
                            }}
                        }});
                    "#,
                        message_id_clone
                    ));

                    while let Ok(msg) = eval.recv().await {
                        if let Ok(data) = serde_json::from_value::<SelectionData>(msg) {
                            if data.hide {
                                if !*is_mouse_over_toolbar.read() {
                                    selection_mode.set(SelectionMode::None);
                                }
                            } else if !data.text.trim().is_empty() {
                                selection_data.set(SelectionState { text: data.text.clone(), markdown: data.markdown.clone(), top: data.top, left: data.left });
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
                                if let StreamMessage::Text {
                                    content: chunk,
                                    thought_summary: summary_chunk,
                                    ..
                                } = stream_msg
                                {
                                    tracing::debug!("CHUNK RECEIVED: '{}'", &chunk);
                                    if !chunk.is_empty() {
                                        content.write().push_str(&chunk);
                                    }
                                    if let Some(summary) = summary_chunk {
                                        let mut current = local_thought_summary.write();
                                        if let Some(curr_str) = &mut *current {
                                            curr_str.push_str(&summary);
                                        } else {
                                            *current = Some(summary);
                                        }
                                    }
                                    on_content_update.call(());
                                }
                            }
                        }
                    });
                }
            });

            let is_thinking = is_streaming && !has_content;

            let bubble_classes = if is_thinking {
                 "bg-transparent border border-dashed border-faint animate-pulse self-start mr-auto"
            } else if is_user {
                "bg-bubble-user text-white self-end ml-auto"
            } else {
                "bg-card text-fg self-start mr-auto"
            };
            let container_classes = if is_user {
                "flex justify-end"
            } else {
                "flex justify-start"
            };
            let author_classes = format!(
                "text-xs text-fg-muted mt-1 px-2 {}",
                if is_user { "text-right" } else { "text-left" }
            );

            let _button_position_classes = if is_user {
                "absolute bottom-[-10px] left-[-10px]"
            } else {
                "absolute bottom-[-10px] right-[-10px]"
            };

            let controls_position_class = if is_user {
                "bottom-[-25px] left-[-25px]"
            } else {
                "bottom-[-25px] right-[-25px]"
            };

            rsx! {
            div {
                class: "{container_classes} w-full",
                div {
                    class: "flex flex-col max-w-full min-w-0 group",
                    div {
                        id: "message-bubble-{message.id}",
                        class: "relative rounded-2xl {bubble_classes} max-w-full",
                        div {
                            class: "px-4 py-3 text-sm leading-relaxed break-words",
                            if is_thinking {
                                div {
                                    class: "flex flex-col space-y-2",
                                    button {
                                        class: "flex items-center space-x-2 text-fg-muted text-sm py-1 hover:text-fg transition-colors focus:outline-none cursor-pointer",
                                        onclick: move |_| show_thinking.toggle(),
                                        if *show_thinking.read() {
                                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronDown }
                                        } else {
                                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronRight }
                                        }
                                        div { class: "flex items-center space-x-1 ml-1",
                                             div { class: "w-1.5 h-1.5 bg-current rounded-full animate-bounce [animation-delay:-0.3s]" }
                                             div { class: "w-1.5 h-1.5 bg-current rounded-full animate-bounce [animation-delay:-0.15s]" }
                                             div { class: "w-1.5 h-1.5 bg-current rounded-full animate-bounce" }
                                        }
                                        span { class: "ml-2 font-medium", "Thinking..." }
                                    }
                                    if *show_thinking.read() {
                                        div {
                                            class: "pl-6 text-sm text-fg-muted",
                                            if let Some(summary) = local_thought_summary.read().as_ref() {
                                                 ThinkingMarkdownRenderer { content: summary.clone(), compact: false }
                                            }
                                        }
                                    }
                                }
                            } else {
                                MarkdownRenderer {
                                    content: content(),
                                    comments: message.comments.clone(),
                                    pending_highlight: if *selection_mode.read() != SelectionMode::None && *selection_mode.read() != SelectionMode::CommentEdit {
                                        Some(selection_data.read().text.clone())
                                    } else {
                                        None
                                    },
                                    on_comment_edit: {
                                        let message_comments = message.comments.clone();
                                        move |comment_id: String| {
                                            // Find the comment to get its current text
                                            if let Some(comment) = message_comments.iter().find(|c| c.id == comment_id) {
                                                editing_comment_id.set(Some(comment_id));
                                                selection_data.set(SelectionState { text: comment.text_selection.clone(), markdown: comment.text_selection.clone(), top: 100.0, left: 100.0 });
                                                selection_mode.set(SelectionMode::CommentEdit);
                                            }
                                        }
                                    },
                                    on_comment_delete: {
                                        // This loop and `session_id` are not defined in this scope.
                                        // Assuming this is a placeholder for a larger refactor,
                                        // and the intent is to replace the closure's content.
                                        // The original `message_id` is captured by the outer scope.
                                        let message_id = message.id; // Re-capture message_id from the outer scope
                                        move |comment_id: String| {
                                            // Delete the comment from session state
                                            let mut state = session_state.write();
                                            if let Some(msg) = state.get_message_mut(&message_id) {
                                                msg.comments.retain(|c| c.id != comment_id);
                                            }
                                            crate::session::SessionState::save_async(state.clone(), Some(save_error));
                                        }
                                    }
                                }

                                if content().starts_with("[Hobbes encountered a persistent error") {
                                    QuickFix {
                                        suggestions: vec![
                                            "You are using bad syntax, the user has loaded the tools please try again.".to_string(),
                                            "Please check your tool syntax & attributes and try again..".to_string(),
                                        ],
                                        on_select: move |suggestion: String| {
                                            chat_input_draft.set(suggestion);
                                            spawn(async move {
                                                let _ = document::eval(r#"
                                                    const el = document.getElementById('chat-textarea');
                                                    if (el) {
                                                        el.focus();
                                                        // Don't dispatch input event as it might race with the value update
                                                        // Just handle the resize explicitly
                                                        requestAnimationFrame(() => {
                                                            el.style.height = 'auto';
                                                            el.style.height = (el.scrollHeight) + 'px';
                                                        });
                                                    }
                                                "#);
                                            });
                                        }
                                    }
                                }
                                if !message.attachments.is_empty() {
                                    div {
                                        class: "flex flex-col space-y-2 mt-2",
                                        for attachment in &message.attachments {
                                            {
                                                // Security: Sanitize mime_type to prevent XSS via attribute injection
                                                let safe_mime = if attachment.mime_type.chars().all(|c| c.is_alphanumeric() || c == '/' || c == '-' || c == '+' || c == '.') {
                                                    &attachment.mime_type
                                                } else {
                                                    "application/octet-stream"
                                                };

                                                rsx! {
                                                    img {
                                                        src: format!("data:{};base64,{}", safe_mime, attachment.data),
                                                        class: "w-20 h-20 object-cover rounded-lg hover:opacity-80 transition-opacity cursor-pointer border border-faint",
                                                        alt: attachment.file_name.clone(),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if *selection_mode.read() == SelectionMode::Toolbar {
                            SelectionToolbar {
                                position_top: selection_data.read().top,
                                position_left: selection_data.read().left,
                                on_mouseenter: move |_| is_mouse_over_toolbar.set(true),
                                on_mouseleave: move |_| is_mouse_over_toolbar.set(false),
                                on_copy: move |_| {
                                    // Use reconstructed markdown for copy to preserve formatting
                                    let markdown = selection_data.read().markdown.clone();
                                    spawn(async move {
                                        // Security: Use serde_json::to_string to safely escape the string for JS
                                        let json_text = serde_json::to_string(&markdown).unwrap_or_else(|_| "null".to_string());
                                        let mut eval = document::eval(&format!("navigator.clipboard.writeText({});", json_text));
                                        let _: Result<serde_json::Value, _> = eval.recv().await;
                                    });
                                    selection_mode.set(SelectionMode::None);
                                },
                                on_comment: move |_| {
                                    selection_mode.set(SelectionMode::CommentInput);
                                    on_selection.call((selection_data.read().text.clone(), selection_data.read().top, selection_data.read().left));
                                }
                            }
                        }

                        if *selection_mode.read() == SelectionMode::CommentInput {
                            crate::components::inline_comment_popover::InlineCommentPopover {
                                position_top: selection_data.read().top,
                                position_left: selection_data.read().left,
                                on_save: move |comment_text: String| {
                                    let text = selection_data.read().text.clone();
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
                                    crate::session::SessionState::save_async(state.clone(), Some(save_error));

                                    on_comment.call(());

                                    selection_mode.set(SelectionMode::None);
                                },
                                on_cancel: move |_| {
                                    selection_mode.set(SelectionMode::None);
                                }
                            }
                        }

                        if *selection_mode.read() == SelectionMode::CommentEdit {
                            // Get the comment being edited
                            {{
                                let current_comment_text = editing_comment_id.read().as_ref().and_then(|comment_id| {
                                    message.comments.iter()
                                        .find(|c| &c.id == comment_id)
                                        .map(|c| c.comment.clone())
                                });

                                rsx! {
                                    crate::components::inline_comment_popover::InlineCommentPopover {
                                        position_top: 150.0,
                                        position_left: 150.0,
                                        initial_value: current_comment_text,
                                        on_save: move |new_comment_text: String| {
                                            if let Some(comment_id) = editing_comment_id.read().clone() {
                                                // Update the comment in session state
                                                let mut state = session_state.write();
                                                if let Some(msg) = state.get_message_mut(&message.id) {
                                                    if let Some(comment) = msg.comments.iter_mut().find(|c| c.id == comment_id) {
                                                        comment.comment = new_comment_text;
                                                    }
                                                }
                                                crate::session::SessionState::save_async(state.clone(), Some(save_error));
                                            }
                                            editing_comment_id.set(None);
                                            selection_mode.set(SelectionMode::None);
                                        },
                                        on_cancel: move |_| {
                                            editing_comment_id.set(None);
                                            selection_mode.set(SelectionMode::None);
                                        }
                                    }
                                }
                            }}
                        }

                        if !is_thinking {
                            div {
                                class: "absolute {controls_position_class} opacity-0 group-hover:opacity-100 transition-opacity flex items-center space-x-2 bg-card rounded-lg p-1 shadow-lg border border-faint z-10",
                                button {
                                    class: "p-1.5 text-fg-muted hover:text-fg rounded transition-colors",
                                    onclick: move |_| {
                                        let raw_markdown = message.content.get_text_content().unwrap_or_default();
                                        spawn(async move {
                                            if copy_to_clipboard(&raw_markdown).is_ok() {
                                                // The copy_to_clipboard function now handles the OS-level interaction.
                                            } else {
                                                tracing::error!("Failed to copy raw markdown to clipboard.");
                                            }
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

                                button {
                                    class: "p-1.5 text-fg-muted hover:text-red-400 rounded transition-colors",
                                    onclick: move |_| on_delete.call(()),
                                    title: "Delete message",
                                    Icon { width: 14, height: 14, icon: fi_icons::FiTrash2 }
                                }
                            }
                        }

                        // Two-column footer: Thinking Process (left) | Metering (right)
                        {
                            let has_thinking = !is_thinking && (local_thought_summary.read().is_some() || thought_signature.is_some());
                            let has_usage = usage_data.is_some() && display_mode != "none";

                            if has_thinking || has_usage {
                                rsx! {
                                    div {
                                        // Two-column layout with gap
                                        class: "mx-4 mb-2 flex justify-between items-start gap-4",

                                        // Left column: Thinking Process
                                        div {
                                            class: "flex flex-col",
                                            if has_thinking {
                                                button {
                                                    class: "flex items-center text-xs text-fg-muted hover:text-fg-muted focus:outline-none transition-colors",
                                                    onclick: move |_| {
                                                        let current = *show_thinking.read();
                                                        show_thinking.set(!current);
                                                    },
                                                    if *show_thinking.read() {
                                                        Icon {
                                                            width: 10,
                                                            height: 10,
                                                            icon: fi_icons::FiChevronDown,
                                                            class: "mr-1"
                                                        }
                                                    } else {
                                                        Icon {
                                                            width: 10,
                                                            height: 10,
                                                            icon: fi_icons::FiChevronRight,
                                                            class: "mr-1"
                                                        }
                                                    }
                                                    span { class: "opacity-70", "Thinking Process" }
                                                    if !*show_thinking.read() {
                                                        if let Some(summary) = local_thought_summary.read().as_ref().and_then(|s| extract_bold_blocks(s)) {
                                                            span { class: "ml-2 text-fg-muted truncate max-w-[200px]", "— {summary}" }
                                                        }
                                                    }
                                                }
                                                if *show_thinking.read() {
                                                    div {
                                                        class: "mt-2 p-3 bg-app rounded-lg text-xs text-fg-muted",
                                                        if let Some(summary) = local_thought_summary.read().as_ref() {
                                                            ThinkingMarkdownRenderer { content: summary.clone(), compact: false }
                                                        } else if let Some(sig) = &thought_signature {
                                                            div { class: "opacity-50 italic mb-1 font-mono whitespace-pre-wrap", "Encrypted Thought Signature:" }
                                                            span { class: "font-mono whitespace-pre-wrap", "{sig}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Right column: Token usage / Metering
                                        div {
                                            class: "flex flex-col items-end",
                                            if let Some(usage) = &usage_data {
                                                if display_mode != "none" {
                                                    button {
                                                        class: "flex items-center text-xs text-fg-muted hover:text-fg-muted focus:outline-none transition-colors",
                                                        onclick: move |_| {
                                                            let current = *show_usage.read();
                                                            show_usage.set(!current);
                                                        },
                                                        span {
                                                            class: "opacity-70 font-mono",
                                                            {
                                                                let tokens = usage.total_tokens;
                                                                let cost = usage.cost.unwrap_or(0.0);
                                                                match display_mode.as_str() {
                                                                    "tokens" => format!("{} tokens", tokens),
                                                                    "cost" => format!("${:.4}", cost),
                                                                    _ => format!("{} tokens (${:.4})", tokens, cost),
                                                                }
                                                            }
                                                        }
                                                        if *show_usage.read() {
                                                            Icon { width: 10, height: 10, icon: fi_icons::FiChevronDown, class: "ml-1" }
                                                        } else {
                                                            Icon { width: 10, height: 10, icon: fi_icons::FiChevronLeft, class: "ml-1" }
                                                        }
                                                    }
                                                    if *show_usage.read() {
                                                        div {
                                                            class: "mt-2 p-3 bg-app rounded-lg text-xs text-fg-muted font-mono",
                                                            div { class: "flex justify-between gap-4",
                                                                span { "Prompt:" }
                                                                span { "{usage.prompt_tokens}" }
                                                            }
                                                            div { class: "flex justify-between gap-4",
                                                                span { "Completion:" }
                                                                span { "{usage.completion_tokens}" }
                                                            }
                                                            if let Some(thoughts) = usage.thoughts_tokens {
                                                                div { class: "flex justify-between gap-4",
                                                                    span { "Thoughts:" }
                                                                    span { "{thoughts}" }
                                                                }
                                                            }
                                                            div { class: "flex justify-between gap-4 mt-1 pt-1 border-t border-faint",
                                                                span { "Cost:" }
                                                                span { {format!("${:.6}", usage.cost.unwrap_or(0.0))} }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                rsx! {}
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
        MessageContent::Error { message: error_msg } => {
            let container_classes = "flex justify-start";
            let bubble_classes = "bg-red-900 border border-red-700 text-fg self-start mr-auto";
            let author_classes = "text-xs text-fg-muted mt-1 px-2 text-left";

            rsx! {
                div {
                    class: "{container_classes}",
                    div {
                        class: "relative group",
                        div {
                            id: "message-bubble-{message.id}",
                            class: "rounded-lg p-4 max-w-3xl shadow-md {bubble_classes}",

                            div {
                                class: "flex items-start gap-3",
                                div {
                                    class: "flex-shrink-0 mt-1",
                                    Icon {
                                        width: 20,
                                        height: 20,
                                        icon: fi_icons::FiAlertCircle,
                                        class: "text-red-400"
                                    }
                                }
                                div {
                                    class: "flex-grow",
                                    div {
                                        class: "font-semibold text-sm mb-2",
                                        "Error"
                                    }
                                    div {
                                        class: "text-sm whitespace-pre-wrap",
                                        "{error_msg}"
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
        _ => rsx! {},
    }
}

#[component]
pub fn LinkWithControls(href: String, text: String) -> Element {
    let DraftContext(mut draft) = use_context::<DraftContext>();
    let mut copied = use_signal(|| false);
    let mut is_hovered = use_signal(|| false);
    let mut pop_left = use_signal(|| false);
    let href_clone_for_copy = href.clone();
    let href_clone_for_summarize = href.clone();
    let unique_id = Uuid::new_v4().to_string();
    let unique_id_clone = unique_id.clone();

    rsx! {
        span {
            id: "link-control-{unique_id}",
            class: "relative inline-block",
            onmouseenter: move |_| {
                is_hovered.set(true);
                let unique_id_js = unique_id_clone.clone();
                spawn(async move {
                    let mut eval = document::eval(&format!(r#"
                        const el = document.getElementById('link-control-{}');
                        if (el) {{
                            const rect = el.getBoundingClientRect();
                            const windowWidth = window.innerWidth;
                            // If the element is within 150px of the right edge, pop left
                            return rect.right > (windowWidth - 150);
                        }}
                        return false;
                    "#, unique_id_js));

                    if let Ok(should_pop_left) = eval.recv::<bool>().await {
                        pop_left.set(should_pop_left);
                    }
                });
            },
            onmouseleave: move |_| is_hovered.set(false),
            a {
                href: "{href}",
                target: "_blank",
                rel: "noopener noreferrer",
                class: "text-primary-400 hover:text-primary-300",
                "{text}"
            }
            span {
                class: format!("inline-flex items-center absolute {} z-10 {} transition-opacity duration-200 bg-card rounded-lg p-1 shadow-lg border border-faint space-x-2",
                    if *pop_left.read() { "right-full mr-1" } else { "left-full ml-1" },
                    if *is_hovered.read() { "opacity-100" } else { "opacity-0" }
                ),

                button {
                    class: "p-1.5 text-fg-muted hover:text-fg rounded transition-colors",
                    onclick: move |evt| {
                        evt.stop_propagation();
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
                        Icon { width: 14, height: 14, icon: fi_icons::FiCheck }
                    } else {
                        Icon { width: 14, height: 14, icon: fi_icons::FiCopy }
                    }
                }
                button {
                    class: "p-1.5 text-fg-muted hover:text-fg rounded transition-colors",
                    onclick: move |evt| {
                        evt.stop_propagation();
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
                    Icon { width: 14, height: 14, icon: fi_icons::FiFileText }
                }
            }
        }
    }
}

fn extract_bold_blocks(content: &str) -> Option<String> {
    let parts: Vec<&str> = content.split("**").collect();
    if parts.len() < 3 {
        return None;
    }
    let mut bolded = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 && !part.is_empty() {
            bolded.push(*part);
        }
    }
    if bolded.is_empty() {
        return None;
    }
    let summary = bolded.into_iter().take(3).collect::<Vec<_>>().join("... ");
    Some(summary)
}

#[component]
pub fn WelcomeMessage() -> Element {
    rsx! {
        div {
            class: "flex flex-col items-center justify-center h-full text-fg-muted",
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
