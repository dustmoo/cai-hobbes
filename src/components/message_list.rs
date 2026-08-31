use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

use crate::components::tool_call_display::{PermissionPrompt, ToolCallDisplay};

use super::chat::{MessageBubble, WelcomeMessage};
use super::shared::{MessageContent, SessionIdContext};

const INITIAL_MESSAGES_TO_SHOW: usize = 20;

#[component]
pub fn MessageList(
    stream_update_trigger: Signal<i32>,
    show_scroll_button: Signal<bool>,
    on_delete: EventHandler<Uuid>,
    on_comment: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    let settings = use_context::<Signal<crate::settings::Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut visible_message_count = use_signal(|| INITIAL_MESSAGES_TO_SHOW);
    let _ = stream_update_trigger.read();

    let SessionIdContext(current_session_id) = use_context::<SessionIdContext>();
    let session_id = current_session_id;

    // PROFILE COLOR: Derive from session-specific composio_profile, falling back to global default
    let profile_color = {
        let state = session_state.read();
        let settings_read = settings.read();
        let session_profile = state
            .sessions
            .get(&*session_id.read())
            .and_then(|s| s.composio_profile.as_ref());
        crate::components::shared::resolve_profile_color(session_profile, &settings_read)
            .unwrap_or_default()
    };

    use_effect(move || {
        if let Some(cmd) = chat_command.read().clone() {
            if matches!(
                cmd,
                crate::components::chat_input::ChatCommand::ScrollToBottom
            ) {
                spawn(async move {
                    let _ = document::eval(
                        r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' }); }
                    "#,
                    )
                    .await;
                });
            }
        }
    });

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
                    let session_id = session_id.read().clone();

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
                            let total_messages = session_state.read().sessions.get(&session_id).map_or(0, |s| s.messages.len());
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
                    if let Some(session) = state.sessions.get(&*session_id.read()) {
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
                                    {
                                        let session_id = session_id.read().clone();
                                        rsx! {
                                            match &message.content {
                                                MessageContent::Text { content: text, thought_signature: _, thought_summary: _ } => {
                                                    let stream_manager = consume_context::<crate::components::stream_manager::StreamManagerContext>();
                                                    let is_generating = stream_manager.is_generating(&message.id);
                                                    // Don't render empty bubbles — thinking data (thought_signature/thought_summary)
                                                    // is absorbed into the adjacent ToolCall card, so an empty text message
                                                    // with only thinking metadata should be hidden.
                                                    let should_render = !text.trim().is_empty() || is_generating || !message.attachments.is_empty();

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
                                                    let bubble_classes = "bg-input text-fg self-start mr-auto";
                                                    let container_classes = "flex justify-start";
                                                    let author_classes = format!(
                                                        "text-xs text-fg-muted mt-1 px-2 {}",
                                                        "text-left"
                                                    );
                                                    rsx! {
                                                        div {
                                                            key: "{message.id}",
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: "relative group rounded-2xl {bubble_classes}",
                                                                    ToolCallDisplay {
                                                                        tool_call: tool_call.clone(),
                                                                        usage: message.usage.clone(),
                                                                        token_display_mode: ui_state.read().token_display_mode.clone()
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
                                                MessageContent::PermissionRequest(tool_call) => {
                                                    let container_classes = "flex justify-start";
                                                    let author_classes = "text-xs text-fg-muted mt-1 px-2 text-left";
                                                    rsx! {
                                                        div {
                                                            key: "{message.id}",
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
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
                                                MessageContent::SkillCall(skill_call) => {
                                                    let container_classes = "flex justify-end text-fg";
                                                    let author_classes = "text-xs text-fg-muted mt-1 px-2 text-right";
                                                    rsx! {
                                                        div {
                                                            key: "{message.id}",
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: format!("relative group rounded-2xl {} shadow-md overflow-hidden", skill_call.profile_color.clone().unwrap_or_else(|| profile_color.clone())),
                                                                    div {
                                                                        class: "px-4 pt-3 pb-1 text-[10px] font-bold opacity-60 uppercase tracking-[0.2em]",
                                                                        "> SKILL EXECUTION"
                                                                    }
                                                                    crate::components::skill_call_display::SkillCallDisplay {
                                                                        skill_call: skill_call.clone(),
                                                                        on_use_result: move |output: String| {
                                                                            chat_command.set(Some(crate::components::chat_input::ChatCommand::CopyToDraft(output)));
                                                                        },
                                                                        on_analyze: move |_| {
                                                                            // Trigger AI continuation via Command Bridge
                                                                            chat_command.set(Some(crate::components::chat_input::ChatCommand::TriggerAiAnalysis));
                                                                        },
                                                                    }
                                                                }
                                                                div {
                                                                    class: "{author_classes}",
                                                                    "User"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                MessageContent::SkillPermissionRequest(skill_call) => {
                                                    let container_classes = "flex justify-end text-fg";
                                                    let author_classes = "text-xs text-fg-muted mt-1 px-2 text-right";
                                                    let execution_id = skill_call.execution_id.clone();
                                                    let message_id = message.id;
                                                    rsx! {
                                                        div {
                                                            key: "{message.id}",
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: format!("relative group rounded-2xl {} shadow-md overflow-hidden", skill_call.profile_color.clone().unwrap_or_else(|| profile_color.clone())),
                                                                    div {
                                                                        class: "px-4 pt-3 pb-1 text-[10px] font-bold opacity-60 uppercase tracking-[0.2em]",
                                                                        "> SKILL COMMAND"
                                                                    }
                                                                    crate::components::skill_call_display::SkillCallDisplay {
                                                                        skill_call: skill_call.clone(),
                                                                        show_permission_prompt: true,
                                                                        on_approve: {
                                                                            let session_id = session_id.clone();
                                                                            move |_exec_id: String| {
                                                                                let msg_id = message_id;
                                                                                let session_id = session_id.clone();
                                                                                spawn(async move {
                                                                                    // Get session's profile for profile-scoped MCP context
                                                                                    let (skill_call_clone, profile_id) = {
                                                                                        let state = session_state.read();
                                                                                        if let Some(session) = state.sessions.get(&session_id) {
                                                                                            let sc = session.messages.iter()
                                                                                                .find(|m| m.id == msg_id)
                                                                                                .and_then(|m| match &m.content {
                                                                                                    MessageContent::SkillPermissionRequest(sc) => Some(sc.clone()),
                                                                                                    _ => None,
                                                                                                });
                                                                                            // Get session's composio_profile (which is the stable ID)
                                                                                            let profile = session.composio_profile.clone();
                                                                                            (sc, profile)
                                                                                        } else { (None, None) }
                                                                                    };

                                                                                    // Get FRESH, profile-scoped MCP context using mcp_manager
                                                                                    // This follows the same lifecycle as chat.rs:539-557
                                                                                    if let Some(ref id) = profile_id {
                                                                                        let _ = mcp_manager.read().ensure_native_client_for_profile(id, &settings.read()).await;
                                                                                    }
                                                                                    let mut fresh_mcp_context = mcp_manager.read().get_mcp_context(profile_id).await;
                                                                                    // Enrich with toolkit slugs from Settings so on-demand toolkits
                                                                                    // don't produce false "Missing capability" warnings
                                                                                    fresh_mcp_context.enrich_from_settings(&settings.read());
                                                                                    let mcp_context = if fresh_mcp_context.servers.is_empty() {
                                                                                        None
                                                                                    } else {
                                                                                        Some(fresh_mcp_context)
                                                                                    };

                                                                                    if let Some(mut sc) = skill_call_clone {
                                                                                        tracing::info!("Executing skill: {}", sc.skill_name);
                                                                                        match crate::skills::execute_skill(&mut sc, mcp_context.as_ref()).await {
                                                                                            Ok(result) => {
                                                                                                tracing::info!("Skill executed: {:?}", result.status);
                                                                                                // Update message with completed SkillCall
                                                                                                let mut state = session_state.write();
                                                                                                let mut skill_events: Vec<crate::session_events::SessionEvent> = Vec::new();
                                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                                        msg.content = MessageContent::SkillCall(sc.clone());
                                                                                                        skill_events.push(crate::session_events::SessionEvent::ToolResult { message: msg.clone() });
                                                                                                    }
                                                                                                    // Persist skill into session.loaded_skills
                                                                                                    if sc.status == crate::components::shared::SkillCallStatus::Completed {
                                                                                                        session.loaded_skills.insert(
                                                                                                            sc.skill_name.clone(),
                                                                                                            sc.response.clone(),
                                                                                                        );
                                                                                                        skill_events.push(crate::session_events::SessionEvent::SkillLoaded {
                                                                                                            name: sc.skill_name.clone(),
                                                                                                            payload: sc.response.clone(),
                                                                                                        });
                                                                                                        tracing::info!("Persisted skill '{}' into session.loaded_skills (permission path)", sc.skill_name);
                                                                                                    }
                                                                                                }
                                                                                                drop(state);
                                                                                                crate::session_events::log_events(&session_id, skill_events);
                                                                                                // Trigger continuation so LLM responds to skill output
                                                                                                chat_command.set(Some(crate::components::chat_input::ChatCommand::TriggerAiAnalysis));
                                                                                            }
                                                                                            Err(e) => {
                                                                                                tracing::error!("Skill execution failed: {}", e);
                                                                                                sc.status = crate::components::shared::SkillCallStatus::Error;
                                                                                                sc.response = e.to_string();
                                                                                                let mut state = session_state.write();
                                                                                                let mut error_snapshot: Option<crate::components::chat::Message> = None;
                                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                                        msg.content = MessageContent::SkillCall(sc);
                                                                                                        error_snapshot = Some(msg.clone());
                                                                                                    }
                                                                                                }
                                                                                                drop(state);
                                                                                                if let Some(message) = error_snapshot {
                                                                                                    crate::session_events::log_event(&session_id, crate::session_events::SessionEvent::ToolResult { message });
                                                                                                }
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                });
                                                                            }
                                                                        },
                                                                        on_deny: {
                                                                            let session_id = session_id.clone();
                                                                            move |_exec_id: String| {
                                                                                let msg_id = message_id;
                                                                                let session_id = session_id.clone();
                                                                                // Mark skill as denied
                                                                                let mut state = session_state.write();
                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                        if let MessageContent::SkillPermissionRequest(sc) = &msg.content {
                                                                                            let mut denied_sc = sc.clone();
                                                                                            denied_sc.status = crate::components::shared::SkillCallStatus::Error;
                                                                                            denied_sc.response = "Permission denied by user.".to_string();
                                                                                            msg.content = MessageContent::SkillCall(denied_sc);
                                                                                        }
                                                                                    }
                                                                                }
                                                                                tracing::info!("Skill permission denied for execution_id: {}", execution_id);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                div {
                                                                    class: "{author_classes}",
                                                                    "User"
                                                                }
                                                            }
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
                if *show_scroll_button.read() {
                    button {
                        class: "absolute bottom-4 right-4 z-10 p-2 bg-btn-primary text-fg rounded-full shadow-lg hover:bg-btn-primary-hover focus:outline-none focus:ring-2 focus:ring-btn-primary transition-opacity duration-300 ease-in-out",
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
