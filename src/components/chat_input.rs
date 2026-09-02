// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use base64::{engine::general_purpose, Engine as _};
use dioxus::{html::HasFileData, prelude::*};
use dioxus_free_icons::{icons::fi_icons, Icon};
use image::{imageops, DynamicImage, ImageFormat};
use rfd::FileDialog;
use std::io::Cursor;
use std::time::SystemTime;

#[cfg(debug_assertions)]
use crate::context::prompt_builder::PromptBuilder;
use crate::llm::GeminiModel;
use crate::settings::{get_default_model_icon, get_slot_icon, LlmProvider, Settings, SettingsManager, UiState};
use hobbes_core::models::Attachment;

use crate::components::chat_queue::{self, QueuedMessage, CHAT_QUEUE};
use crate::components::focus_context::FocusContext;
use crate::components::shared::{ChatBarIconButton, DraftContext, SessionIdContext};
use crate::components::skill_autocomplete::SkillAutocomplete;
use crate::components::stream_manager::StreamManagerContext;
use crate::hotkey::matches_hotkey;
use crate::processing::summarization_scheduler::SchedulerSignal;
use crate::session_events::{log_event, log_events, SessionEvent};
use crate::skills::{Skill, SkillRegistry};

#[derive(Clone, Debug, PartialEq)]
pub enum ChatCommand {
    ToggleProfile,
    OpenAttachments,
    ToggleSettings,
    ToggleHistory,
    ToggleMcp,
    /// Show/hide the full-width Planner view in place of the chat column.
    TogglePlanner,
    /// Show/hide the full-width Fleet view (its own tab, planner idiom).
    ToggleFleet,
    NewChat,
    NewChatWithMemory,
    ScrollToBottom,
    FocusChat,
    DeleteSession(String),
    CancelGeneration,
    CopyToDraft(String),
    RestoreToDraft(String, Vec<hobbes_core::models::Attachment>),
    /// "Activate" a planner todo. Handled entirely in main.rs: it opens a
    /// fresh chat tab and parks the text in PendingChatSeedContext in the same
    /// effect; ChatInput's pending-seed consumer then fills the draft and
    /// lands the caret after the render that unhid the chat.
    StartTodoInChat(String),
    TriggerAiAnalysis,
    SwitchToSettingsTab(crate::settings::SettingsTab, Option<String>),
    SwitchTab(usize),
    SwitchToSession(String),
    /// Switch the current session's profile to the profile at this index.
    SwitchProfile(usize),
    /// Switch the current session's model to the model at this index in the available models list.
    SwitchModel(usize),
    /// Switch the current session's LLM connector to the instance with this
    /// stable ID in `Settings::llm_connectors`.
    SwitchConnector(String),
    /// Toggle the model selector dropdown in the chat bar.
    #[allow(dead_code)] // Constructed and consumed locally via signal pattern
    ToggleModelSelector,
    /// Toggle the provider selector dropdown in the chat bar.
    ToggleProviderSelector,
    CloseTab,
}

/// Distinguishes which autocomplete context is active in the chat input.
#[derive(Clone, Copy, PartialEq, Debug)]
enum AutocompleteMode {
    /// Standard skill activation: `/skillname`
    Skill,
    /// Unload a loaded skill: `/unload skillname`
    Unload,
    /// Fleet session reference: `@sessionname` (expanded on send).
    Fleet,
}

/// One-line preview of a queued message for its chip. Trims, caps length with an
/// ellipsis, and labels attachment-only queue entries so the chip isn't blank.
fn queue_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(attachment only)".to_string();
    }
    let mut preview: String = trimmed.chars().take(80).collect();
    if trimmed.chars().count() > 80 {
        preview.push('…');
    }
    preview
}

/// Provider-aware model display name. For Gemini, uses the curated display name
/// from GeminiModel. For other providers, the raw slug is already human-readable
/// (e.g. `gpt-4o-mini`, `Qwen/Qwen3-235B`).
fn display_name_for_provider(provider: &LlmProvider, model_slug: &str) -> String {
    match provider {
        LlmProvider::Gemini => GeminiModel::from_slug(model_slug).display_name(),
        LlmProvider::Claude => {
            crate::llm::claude_models::ClaudeModel::from_slug(model_slug).display_name()
        }
        _ => model_slug.to_string(),
    }
}

#[component]
pub fn ChatInput(
    is_sending: Signal<bool>,
    has_new_comments: Signal<bool>,
    has_pending_approvals: Signal<bool>,
    on_send: EventHandler<(String, Vec<Attachment>)>,
    on_cancel: EventHandler<()>,
    on_interaction: EventHandler<()>,
    on_new_chat_with_memory: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let SessionIdContext(current_session_id) = use_context::<SessionIdContext>();
    let settings = use_context::<Signal<Settings>>();
    let _settings_manager = use_context::<Signal<SettingsManager>>();
    let _mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let _planner_state = use_context::<Signal<crate::todo::PlannerState>>();
    let ui_state = use_context::<Signal<UiState>>();
    let DraftContext(mut draft) = use_context::<DraftContext>();
    let mut is_dragging = use_signal(|| false);
    let mut attachments = use_signal(Vec::<Attachment>::new);
    let mut is_processing_attachments = use_signal(|| false);
    let mut show_profile_selector = use_signal(|| false);
    let mut show_model_selector = use_signal(|| false);
    let mut show_provider_selector = use_signal(|| false);
    let mut show_new_chat_menu = use_signal(|| false);
    let scheduler = use_context::<Coroutine<SchedulerSignal>>();
    let focus_context = use_context::<Signal<FocusContext>>();
    let mut textarea_mounted =
        use_signal(|| Option::<std::rc::Rc<dioxus::html::MountedData>>::None);

    // Streaming state + skill permissions, hoisted to render scope so the
    // shared `submit` callback and the queue-drain effect can use them.
    let stream_manager = consume_context::<StreamManagerContext>();
    let permission_manager =
        use_context::<Signal<crate::context::permissions::PermissionManager>>();

    // Skill Autocomplete State
    let skill_registry = use_context::<Signal<SkillRegistry>>();
    let mut show_skill_autocomplete = use_signal(|| false);
    let mut skill_autocomplete_index = use_signal(|| 0usize);
    let mut filtered_skills = use_signal(Vec::<Skill>::new);
    let mut autocomplete_mode = use_signal(|| AutocompleteMode::Skill);
    // Byte range of the in-progress /token in the draft (cursor-aware
    // autocomplete); selection splices into this range instead of rewriting
    // the whole draft.
    let mut autocomplete_token: Signal<Option<(usize, usize)>> = use_signal(|| None);

    // Active Focus Management: Reclaim focus when context reverts to ChatInput
    use_effect(move || {
        let current_context = *focus_context.read();
        let mounted_opt = textarea_mounted.read().clone();

        if current_context == FocusContext::ChatInput {
            if let Some(mounted) = mounted_opt {
                spawn(async move {
                    tracing::debug!("ChatInput reclaiming focus via FocusContext");
                    let _ = mounted.set_focus(true).await;
                });
            }
        }
    });

    // Deterministic seed hand-off for StartTodoInChat: main.rs opens the tab
    // and parks the text here in the SAME effect, so this consumer's first run
    // for the new value happens after the render that unhid the chat — the
    // uncontrolled textarea is visible and set_chat_draft's focus sticks.
    let crate::components::shared::PendingChatSeedContext(mut pending_seed) =
        use_context::<crate::components::shared::PendingChatSeedContext>();
    use_effect(move || {
        let Some(text) = pending_seed.read().clone() else {
            return;
        };
        // Consume before applying: the write re-runs this effect, which then
        // sees None and stops — one application per activation.
        pending_seed.set(None);
        let caret = text.encode_utf16().count();
        crate::components::shared::set_chat_draft(draft, text, Some(caret), true);
    });

    // Listen for global chat commands (from menu hotkeys)
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    use_effect(move || {
        if let Some(cmd) = chat_command.read().clone() {
            tracing::debug!("ChatInput received ChatCommand: {:?}", cmd);
            match cmd {
                ChatCommand::ToggleProfile => {
                    show_profile_selector.set(!show_profile_selector());
                }
                ChatCommand::ToggleModelSelector => {
                    show_model_selector.set(!show_model_selector());
                }
                ChatCommand::ToggleProviderSelector => {
                    show_provider_selector.set(!show_provider_selector());
                }
                ChatCommand::ToggleSettings
                | ChatCommand::ToggleHistory
                | ChatCommand::ToggleMcp
                | ChatCommand::TogglePlanner
                | ChatCommand::ToggleFleet
                | ChatCommand::StartTodoInChat(_)
                | ChatCommand::SwitchToSettingsTab(_, _)
                | ChatCommand::SwitchTab(_)
                | ChatCommand::SwitchToSession(_)
                | ChatCommand::NewChat
                | ChatCommand::DeleteSession(_)
                | ChatCommand::SwitchProfile(_)
                | ChatCommand::SwitchModel(_)
                | ChatCommand::SwitchConnector(_)
                | ChatCommand::CloseTab => {
                    // Handled globally in main.rs
                }
                ChatCommand::NewChatWithMemory => {
                    tracing::info!("ChatCommand::NewChatWithMemory triggered");
                    on_new_chat_with_memory.call(());
                }
                ChatCommand::OpenAttachments => {
                    // Trigger attachment dialog
                    let mut attachments = attachments;
                    let mut is_processing_attachments = is_processing_attachments;
                    spawn(async move {
                        is_processing_attachments.set(true);
                        if let Some(files) = FileDialog::new()
                            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                            .pick_files()
                        {
                            for file_path in files {
                                if let Ok(file_data) = tokio::fs::read(&file_path).await {
                                    let file_name = file_path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string();
                                    let extension = file_path
                                        .extension()
                                        .and_then(std::ffi::OsStr::to_str)
                                        .unwrap_or("");
                                    if let Some(mime_type) = get_mime_type(extension) {
                                        if let Some(attachment) = process_image_data(
                                            file_name,
                                            mime_type.to_string(),
                                            file_data,
                                        )
                                        .await
                                        {
                                            attachments.write().push(attachment);
                                        }
                                    } else {
                                        tracing::warn!(
                                            "Unsupported file type selected: {:?}",
                                            file_path
                                        );
                                    }
                                }
                            }
                        }
                        is_processing_attachments.set(false);
                    });
                }
                ChatCommand::ScrollToBottom => {
                    // Handled by MessageList
                }
                ChatCommand::FocusChat => {
                    let _ = document::eval(
                        r#"
                        const el = document.getElementById('chat-textarea');
                        if (el) { el.focus(); }
                    "#,
                    );
                }
                ChatCommand::CancelGeneration => {
                    if *is_sending.read() {
                        tracing::info!("ChatCommand::CancelGeneration triggered");
                        on_cancel.call(());
                    }
                }
                ChatCommand::CopyToDraft(text) => {
                    tracing::info!("ChatCommand::CopyToDraft triggered: {} chars", text.len());
                    crate::components::shared::set_chat_draft(draft, text, None, false);
                }
                ChatCommand::RestoreToDraft(text, restore_attachments) => {
                    tracing::info!("ChatCommand::RestoreToDraft triggered: {} chars, {} attachments", text.len(), restore_attachments.len());
                    crate::components::shared::set_chat_draft(draft, text, None, true);
                    attachments.set(restore_attachments);
                }
                ChatCommand::TriggerAiAnalysis => {
                    tracing::info!("ChatCommand::TriggerAiAnalysis triggered");
                    // Signal the centralized execution loop in chat.rs (use_effect
                    // monitoring has_pending_approvals). That loop handles both tool
                    // approvals AND skill continuations — when no tools are pending,
                    // it calls continuation_controller.trigger_continuation() which
                    // is the single correct path to resume the AI turn.
                    // DO NOT also call on_send / has_new_comments here — that would
                    // create a second parallel LLM stream (double-turn regression).
                    has_pending_approvals.set(true);
                }
            }

            // Command clearing is handled centrally in main.rs to avoid double-clear race
        }
    });

    // Core submission path, shared by live sends and queued-message drains.
    // Takes explicit `(text, attachments)` so a drained queue entry runs through
    // the exact same skill-detection + send logic as a freshly typed message.
    // Deliberately does NOT touch the draft — when draining a queued message the
    // draft may hold unrelated live text; clearing it is the caller's job.
    let submit = use_callback(move |(user_message, atts): (String, Vec<Attachment>)| {
        // Skill Command Detection
        {
            // /unload command — remove a loaded skill from session context.
            // Stays prefix-only: it is a control command, not a skill.
            let trimmed = user_message.trim();
            if trimmed.starts_with("/unload ") {
                let skill_name = trimmed.trim_start_matches("/unload ").trim();
                if !skill_name.is_empty() {
                    let mut unload_log: Option<(String, crate::components::chat::Message)> = None;
                    {
                        let mut state = session_state.write();
                        if let Some(session) = state.get_active_session_mut() {
                            if session.loaded_skills.remove(skill_name).is_some() {
                                // Push a confirmation message
                                let confirmation = crate::components::chat::Message {
                                    id: uuid::Uuid::new_v4(),
                                    author: "User".to_string(),
                                    content: crate::components::shared::MessageContent::Text {
                                        content: format!("Unloaded skill '{}' from session context.", skill_name),
                                        thought_signature: None,
                                        thought_summary: None,
                                    },
                                    attachments: Vec::new(),
                                    comments: Vec::new(),
                                    created_at: chrono::Utc::now(),
                                    usage: None,
                                };
                                unload_log = Some((session.id.clone(), confirmation.clone()));
                                session.messages.push(confirmation);
                                tracing::info!("Unloaded skill '{}' from session.loaded_skills", skill_name);
                            } else {
                                tracing::warn!("Skill '{}' not found in loaded_skills", skill_name);
                            }
                        }
                    }
                    if let Some((session_id, confirmation)) = unload_log {
                        log_events(
                            &session_id,
                            vec![
                                SessionEvent::SkillUnloaded { name: skill_name.to_string() },
                                SessionEvent::UserMessage { message: confirmation },
                            ],
                        );
                    }
                }
                return;
            }

            // Detect a /skill token at the start of any line so users can
            // write explanatory context and invoke a skill in the same
            // message; mid-sentence mentions of a skill stay plain text. The
            // surrounding text reaches the skill turn via normal history.
            let invocation = {
                let registry = skill_registry.read();
                crate::skills::invocation::detect_skill_invocation(&user_message, |name| {
                    registry.get_skill(name).is_some()
                })
            };
            {
                let skill_opt = invocation.as_ref().and_then(|inv| {
                    let registry = skill_registry.read();
                    registry.get_skill(&inv.skill_name)
                });

                if let (Some(skill), Some(inv)) = (skill_opt, invocation) {
                    let arguments = inv.arguments;
                    let permission_status = permission_manager
                        .read()
                        .check_skill_permission(&skill.metadata.name);

                    let skill_call = crate::components::shared::SkillCall {
                        execution_id: uuid::Uuid::new_v4().to_string(),
                        skill_name: skill.metadata.name.clone(),
                        arguments,
                        status: if permission_status
                            == crate::context::permissions::PermissionStatus::Allowed
                        {
                            crate::components::shared::SkillCallStatus::Running
                        } else {
                            crate::components::shared::SkillCallStatus::Pending
                        },
                        response: String::new(),
                        instructions: skill.instructions.clone(),
                        path: skill.path.clone(),
                        has_scripts: !skill.scripts.is_empty(),
                        raw_output: None,
                        profile_color: {
                            let settings_read = settings.read();
                            let profile_name =
                                session_state.read().get_active_session().and_then(|s| {
                                    settings_read.resolve_session_profile_display_name(
                                        s.composio_profile.as_deref(),
                                    )
                                });
                            crate::components::shared::resolve_profile_color(
                                profile_name.as_ref(),
                                &settings_read,
                            )
                        },
                    };

                    // First, push a normal user text bubble showing the command (history parity)
                    let user_bubble = crate::components::chat::Message {
                        id: uuid::Uuid::new_v4(),
                        author: "User".to_string(),
                        content: crate::components::shared::MessageContent::Text {
                            content: user_message.clone(),
                            thought_signature: None,
                            thought_summary: None,
                        },
                        attachments: atts.clone(),
                        comments: Vec::new(),
                        created_at: chrono::Utc::now(),
                        usage: None,
                    };

                    if permission_status == crate::context::permissions::PermissionStatus::Allowed {
                        // AUTO-EXECUTE PATH
                        if cfg!(debug_assertions) {
                            tracing::info!("[Auto-executing: /{}]", skill.metadata.name);
                        }

                        let skill_message = crate::components::chat::Message {
                            id: uuid::Uuid::new_v4(),
                            author: "User".to_string(),
                            content: crate::components::shared::MessageContent::SkillCall(
                                skill_call.clone(),
                            ),
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        };

                        let msg_id = skill_message.id;
                        let mut state = session_state.write();
                        let mut skill_session_id: Option<String> = None;
                        if let Some(session) = state.get_active_session_mut() {
                            skill_session_id = Some(session.id.clone());
                            session.messages.push(user_bubble.clone());
                            session.messages.push(skill_message.clone());
                        }
                        drop(state); // Drop lock before async
                        if let Some(sid) = &skill_session_id {
                            log_events(
                                sid,
                                vec![
                                    SessionEvent::UserMessage { message: user_bubble },
                                    SessionEvent::ToolCall { message: skill_message },
                                ],
                            );
                        }

                        // Spawn Execution
                        let mut sc_clone = skill_call.clone();
                        let mut session_state = session_state; // move clone into closure
                        let mut mcp_context = _mcp_context.read().clone();
                        // Enrich with toolkit slugs from Settings so on-demand toolkits
                        // don't produce false "Missing capability" warnings
                        mcp_context.enrich_from_settings(&settings.read());

                        spawn(async move {
                            match crate::skills::execute_skill(&mut sc_clone, Some(&mcp_context))
                                .await
                            {
                                Ok(result) => {
                                    // Update message with completed SkillCall
                                    let mut state = session_state.write();
                                    let mut skill_events: Vec<SessionEvent> = Vec::new();
                                    let mut event_session_id: Option<String> = None;
                                    if let Some(session) = state.get_active_session_mut() {
                                        event_session_id = Some(session.id.clone());
                                        if let Some(msg) =
                                            session.messages.iter_mut().find(|m| m.id == msg_id)
                                        {
                                            sc_clone.status = result.status;
                                            sc_clone.response = result.output;
                                            msg.content = crate::components::shared::MessageContent::SkillCall(sc_clone.clone());
                                            skill_events.push(SessionEvent::ToolResult { message: msg.clone() });
                                        }
                                        // Persist skill into session.loaded_skills so it
                                        // remains in system context across all future turns
                                        if sc_clone.status == crate::components::shared::SkillCallStatus::Completed {
                                            session.loaded_skills.insert(
                                                sc_clone.skill_name.clone(),
                                                sc_clone.response.clone(),
                                            );
                                            skill_events.push(SessionEvent::SkillLoaded {
                                                name: sc_clone.skill_name.clone(),
                                                payload: sc_clone.response.clone(),
                                            });
                                            tracing::info!("Persisted skill '{}' into session.loaded_skills", sc_clone.skill_name);
                                        }
                                    }
                                    drop(state); // Release lock before triggering
                                    if let Some(sid) = &event_session_id {
                                        log_events(sid, skill_events);
                                    }
                                                 // Auto-trigger LLM to respond with the injected skill context
                                    chat_command.set(Some(ChatCommand::TriggerAiAnalysis));
                                }
                                Err(e) => {
                                    tracing::error!("Skill execution failed: {}", e);
                                    let mut state = session_state.write();
                                    let mut error_log: Option<(String, crate::components::chat::Message)> = None;
                                    if let Some(session) = state.get_active_session_mut() {
                                        let sid = session.id.clone();
                                        if let Some(msg) =
                                            session.messages.iter_mut().find(|m| m.id == msg_id)
                                        {
                                            sc_clone.status =
                                                crate::components::shared::SkillCallStatus::Error;
                                            sc_clone.response = format!("Error: {}", e);
                                            msg.content = crate::components::shared::MessageContent::SkillCall(sc_clone);
                                            error_log = Some((sid, msg.clone()));
                                        }
                                    }
                                    drop(state);
                                    if let Some((sid, message)) = error_log {
                                        log_event(&sid, SessionEvent::ToolResult { message });
                                    }
                                }
                            }
                        });
                    } else {
                        // PROMPT PATH (Existing Logic)
                        let skill_message = crate::components::chat::Message {
                            id: uuid::Uuid::new_v4(),
                            author: "User".to_string(),
                            content:
                                crate::components::shared::MessageContent::SkillPermissionRequest(
                                    skill_call,
                                ),
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        };

                        let mut state = session_state.write();
                        let mut prompt_session_id: Option<String> = None;
                        if let Some(session) = state.get_active_session_mut() {
                            prompt_session_id = Some(session.id.clone());
                            session.messages.push(user_bubble.clone());
                            session.messages.push(skill_message.clone());
                        }
                        drop(state);
                        if let Some(sid) = &prompt_session_id {
                            log_events(
                                sid,
                                vec![
                                    SessionEvent::UserMessage { message: user_bubble },
                                    SessionEvent::UserMessage { message: skill_message },
                                ],
                            );
                        }
                    }

                    return;
                }
            }
        }

        // Fleet @-mentions: append the mentioned sessions' current status as
        // a frozen-at-send context block. Gated by the bridge's armed flag
        // (fleet on + Pro) — the same condition under which names are shown
        // in the autocomplete.
        let user_message = if crate::fleet::bridge::enabled() {
            let live = crate::fleet::shared().snapshot();
            crate::fleet::mention::expand(
                &user_message,
                &live,
                chrono::Utc::now(),
                chrono::Local::now().date_naive(),
            )
            .unwrap_or(user_message)
        } else {
            user_message
        };

        on_send.call((user_message, atts));
    });

    // Reset the auto-grown textarea back to its base height after a send/queue.
    let reset_textarea_height = move || {
        let _ = document::eval(
            r#"
            const el = document.getElementById('chat-textarea');
            if (el) { el.style.height = 'auto'; }
        "#,
        );
    };

    let mut send_message = move || {
        if *is_processing_attachments.read() {
            tracing::warn!("'send_message' blocked: still processing attachments.");
            return;
        }
        let user_message = draft.read().clone();
        let atts = attachments.read().clone();
        if user_message.is_empty()
            && atts.is_empty()
            && !*has_new_comments.read()
            && !*has_pending_approvals.read()
        {
            return;
        }

        // If this session's turn is still in flight, queue the message instead of
        // sending. The turn drains it on completion (see the drain effect below).
        let session_id = current_session_id.read().clone();
        if stream_manager.is_session_streaming(&session_id) {
            if !user_message.is_empty() || !atts.is_empty() {
                chat_queue::queue_push(
                    &mut CHAT_QUEUE.write(),
                    &session_id,
                    QueuedMessage::new(user_message, atts),
                );
                crate::components::shared::set_chat_draft(draft, String::new(), None, false);
                attachments.set(Vec::new());
                reset_textarea_height();
            }
            // Comment/approval-only submits aren't queueable; keep their prior
            // "blocked while streaming" behavior (just fall through to return).
            return;
        }

        submit.call((user_message, atts));
        crate::components::shared::set_chat_draft(draft, String::new(), None, false);
        attachments.set(Vec::new());
        reset_textarea_height();
    };

    // Drain one queued message whenever the active session is idle — either it
    // just finished a turn, or the user switched back to a session that has a
    // backlog. `dispatching` guards the async gap between dispatching a message
    // and its stream actually starting, so a burst of other-tab stream activity
    // can't pop a second message and race two turns on one session.
    let mut dispatching = use_signal(|| false);
    use_effect(move || {
        // Re-run on any stream state change, active-session switch, or explicit
        // drain request (e.g. a fired timer enqueuing for the active session).
        let _ = stream_manager.stream_activity.read();
        let _ = chat_queue::QUEUE_DRAIN_TICK.read();
        let session_id = current_session_id.read().clone();

        if stream_manager.is_session_streaming(&session_id) {
            // The message we dispatched has started streaming — release the guard.
            if *dispatching.peek() {
                dispatching.set(false);
            }
            return;
        }
        if *dispatching.peek() {
            return;
        }

        if let Some(qm) = chat_queue::queue_pop_next(&mut CHAT_QUEUE.write(), &session_id) {
            dispatching.set(true);
            submit.call((qm.text, qm.attachments));
            // Failsafe: a queued no-op (e.g. a command that starts no stream)
            // must not wedge the queue. Real turns clear the guard sooner via the
            // is_streaming branch above; this only fires if nothing ever streams.
            spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                dispatching.set(false);
            });
        }
    });

    rsx! {
        if !attachments.read().is_empty() {
            div {
                class: "flex items-center space-x-2 p-2 bg-section rounded-t-lg",
                for (index, attachment) in attachments.read().iter().enumerate() {
                    div {
                        class: "flex items-center space-x-2 bg-card p-1 rounded",
                        span {
                            class: "text-sm text-fg-muted",
                            "{attachment.file_name}"
                        }
                        button {
                            class: "p-1 rounded-full text-fg-muted hover:bg-input hover:text-fg",
                            onclick: move |_| {
                                attachments.write().remove(index);
                            },
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiX
                            }
                        }
                    }
                }
            }
        }
        // Live "timer running" indicator for the active session.
        PendingTimersBar {}
        // Queued messages waiting for the in-flight turn to finish.
        {
            let session_id = current_session_id.read().clone();
            let chips: Vec<(uuid::Uuid, String)> = CHAT_QUEUE
                .read()
                .get(&session_id)
                .map(|q| q.iter().map(|m| (m.id, queue_preview(&m.text))).collect())
                .unwrap_or_default();
            if chips.is_empty() {
                rsx! {}
            } else {
                rsx! {
                    div {
                        class: "flex flex-col gap-1 px-3 pt-2 pb-1 bg-section border-t border-subtle",
                        div {
                            class: "flex items-center justify-between text-xs text-fg-muted",
                            span { "Queued ({chips.len()})" }
                            button {
                                class: "px-2 py-0.5 rounded hover:bg-card hover:text-fg transition-colors",
                                onclick: move |_| {
                                    let sid = current_session_id.read().clone();
                                    chat_queue::queue_clear(&mut CHAT_QUEUE.write(), &sid);
                                },
                                "Clear all"
                            }
                        }
                        for (id, preview) in chips {
                            div {
                                key: "{id}",
                                class: "flex items-center justify-between gap-2 bg-card rounded px-2 py-1",
                                span {
                                    class: "text-sm text-fg truncate",
                                    "{preview}"
                                }
                                button {
                                    class: "p-1 rounded-full text-fg-muted hover:bg-input hover:text-fg shrink-0",
                                    title: "Remove from queue",
                                    onclick: move |_| {
                                        let sid = current_session_id.read().clone();
                                        chat_queue::queue_remove(&mut CHAT_QUEUE.write(), &sid, id);
                                    },
                                    Icon { width: 14, height: 14, icon: fi_icons::FiX }
                                }
                            }
                        }
                    }
                }
            }
        }
        div {
            class: if *is_dragging.read() {
                "bg-app p-4 border-t-2 border-dashed border-primary-500"
            } else {
                "bg-app p-4 border-t border-subtle"
            },
            onmousedown: |e| e.stop_propagation(),
            ondragover: move |event| {
                event.prevent_default();
                is_dragging.set(true);
            },
            ondragleave: move |_| {
                is_dragging.set(false);
            },
            ondrop: move |event| {
                event.prevent_default();
                is_dragging.set(false);
                if let Some(file_engine) = event.files() {
                    is_processing_attachments.set(true);
                    spawn(async move {
                        let files = file_engine.files();
                        for file_name in &files {
                            let extension = std::path::Path::new(file_name)
                                .extension()
                                .and_then(std::ffi::OsStr::to_str)
                                .unwrap_or("");
                            if let Some(mime_type) = get_mime_type(extension) {
                                if let Some(file_data) = file_engine.read_file(file_name).await {
                                    if let Some(attachment) = process_image_data(
                                        file_name.clone(),
                                        mime_type.to_string(),
                                        file_data,
                                    )
                                    .await
                                    {
                                        attachments.write().push(attachment);
                                    }
                                }
                            } else {
                                tracing::warn!("Unsupported file type dropped: {}", file_name);
                            }
                        }
                        is_processing_attachments.set(false);
                    });
                }
            },
            div {
                class: "flex items-center space-x-3",
                ChatBarIconButton {
                    icon: fi_icons::FiSettings,
                    onclick: move |_| chat_command.set(Some(ChatCommand::ToggleSettings)),
                    title: "Settings"
                }
                ChatBarIconButton {
                    icon: fi_icons::FiClock,
                    onclick: move |_| chat_command.set(Some(ChatCommand::ToggleHistory)),
                    visible: ui_state.read().show_history_icon,
                    title: "History"
                }
                ChatBarIconButton {
                    icon: fi_icons::FiPackage,
                    onclick: move |_| chat_command.set(Some(ChatCommand::ToggleMcp)),
                    visible: ui_state.read().show_mcp_icon,
                    title: "MCP Tools"
                }
                ChatBarIconButton {
                    icon: fi_icons::FiCheckSquare,
                    onclick: move |_| chat_command.set(Some(ChatCommand::TogglePlanner)),
                    visible: ui_state.read().show_planner_icon,
                    title: "Planner"
                }
                ChatBarIconButton {
                    icon: crate::components::pixel_icons::HobbesInvader,
                    onclick: move |_| chat_command.set(Some(ChatCommand::ToggleFleet)),
                    visible: ui_state.read().show_fleet_icon,
                    title: "Fleet"
                }
                SessionCostIcon {}

                // LLM Connector Selector (session-scoped, mirrors the profile picker)
                { if ui_state.read().show_provider_selector {
                    {
                        // The session's resolved connector (pin → legacy kind → global active)
                        let active_instance = {
                            let settings_read = settings.read();
                            session_state.read().get_active_session()
                                .and_then(|s| settings_read.connector_for_session(s).cloned())
                                .or_else(|| settings_read.active_connector().cloned())
                        };
                        let active_id = active_instance.as_ref().map(|c| c.id.clone());
                        // Every connector, in canonical order — the index drives the
                        // ⇧⌥⌘{n} hotkey hint. The session's current connector is shown
                        // highlighted rather than hidden.
                        let all_connectors: Vec<(usize, crate::settings::ProviderInstance)> = settings
                            .read()
                            .llm_connectors
                            .iter()
                            .cloned()
                            .enumerate()
                            .collect();

                        if let (Some(active), true) = (active_instance, all_connectors.len() > 1) {
                            let active_provider = active.provider();
                            rsx! {
                                div {
                                    class: "relative",
                                    button {
                                        class: format!("w-8 h-8 rounded-full {} border border-subtle flex items-center justify-center text-xs font-bold text-fg hover:brightness-110 hover:border-primary-500 transition-all focus:outline-none focus:ring-2 focus:ring-primary-600 shadow-md", active_provider.color_class()),
                                        title: "Connector: {active.name} ({active_provider.display_name()})",
                                        onclick: move |_| show_provider_selector.set(!show_provider_selector()),
                                        "{active.initial()}"
                                    }

                                    if show_provider_selector() {
                                        div {
                                            class: "absolute bottom-10 left-0 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1",
                                            for (index, instance) in all_connectors.into_iter() {
                                                {
                                                    let configured = settings.read().is_connector_configured(&instance);
                                                    let is_current = Some(&instance.id) == active_id.as_ref();
                                                    let provider = instance.provider();
                                                    let connector_id = instance.id.clone();
                                                    let name = instance.name.clone();
                                                    let initial = instance.initial();
                                                    let row_class: &'static str = if is_current {
                                                        "w-full text-left px-4 py-2 text-sm text-fg bg-primary-900/40 transition-colors flex items-center justify-between"
                                                    } else if configured {
                                                        "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between"
                                                    } else {
                                                        "w-full text-left px-4 py-2 text-sm text-fg-muted/40 hover:bg-primary-900/50 hover:text-fg-muted transition-colors flex items-center justify-between"
                                                    };

                                                    rsx! {
                                                        button {
                                                            class: "{row_class}",
                                                            onclick: move |_| {
                                                                if is_current {
                                                                    // Already the session's connector — just close
                                                                } else if configured {
                                                                    chat_command.set(Some(ChatCommand::SwitchConnector(connector_id.clone())));
                                                                } else {
                                                                    chat_command.set(Some(ChatCommand::SwitchToSettingsTab(crate::settings::SettingsTab::General, None)));
                                                                }
                                                                show_provider_selector.set(false);
                                                            },
                                                            div {
                                                                class: "flex items-center space-x-2",
                                                                span {
                                                                    class: format!("w-6 h-6 rounded-full {} border border-faint flex items-center justify-center text-[10px] text-fg font-bold shadow-sm shrink-0", provider.color_class()),
                                                                    "{initial}"
                                                                }
                                                                span { class: "truncate", "{name}" }
                                                                if is_current {
                                                                    span { class: "text-primary-400 text-xs shrink-0", "✓" }
                                                                }
                                                            }
                                                            if index < 9 {
                                                                span {
                                                                    class: "text-xs text-fg-muted font-mono",
                                                                    "⇧⌥⌘{index + 1}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Click outside handler to close dropdown
                                    if show_provider_selector() {
                                        div {
                                            class: "fixed inset-0 z-40",
                                            onclick: move |_| show_provider_selector.set(false)
                                        }
                                    }
                                }
                            }
                        } else {
                            rsx!({})
                        }
                    }
                } else { rsx!({}) } }

                // Model Selector Dropdown (session-scoped: slots/model follow the
                // session's effective connector, falling back to the global active one)
                { if ui_state.read().show_model_selector {
                    let (session_provider, current_model, slots) = {
                        let settings_read = settings.read();
                        let state = session_state.read();
                        let session = state.get_active_session();
                        let instance = session
                            .and_then(|s| settings_read.connector_for_session(s))
                            .or_else(|| settings_read.active_connector());
                        let provider = instance
                            .map(|i| i.provider())
                            .unwrap_or(settings_read.active_llm);
                        let current_model = match session {
                            Some(s) => settings_read.chat_model_for_session(s),
                            None => settings_read.active_chat_model(),
                        };
                        let slots = instance
                            .map(|i| i.config.model_slots().clone())
                            .unwrap_or_default();
                        (provider, current_model, slots)
                    };
                    let user_icons = settings.read().model_icons.clone();
                    // Icon priority: user-set icon > slot position icon > default
                    let current_slot_idx = slots.iter().position(|s| s == &current_model);
                    let model_icon = user_icons.get(&current_model).cloned()
                        .or_else(|| current_slot_idx.map(get_slot_icon))
                        .unwrap_or_else(|| get_default_model_icon(&current_model));

                    rsx! {
                        div {
                            class: "relative",
                            button {
                                class: "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm hover:border-primary-500 transition-all cursor-pointer focus:outline-none focus:ring-2 focus:ring-primary-600",
                                title: "{display_name_for_provider(&session_provider, &current_model)}",
                                onclick: move |_| show_model_selector.set(!show_model_selector()),
                                "{model_icon}"
                            }

                            if show_model_selector() && !slots.is_empty() {
                                div {
                                    class: "absolute bottom-10 left-0 w-64 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1 max-h-80 overflow-y-auto",
                                    for (index, model_slug) in slots.iter().enumerate() {
                                        if !model_slug.is_empty() {
                                        {
                                            let is_active = *model_slug == current_model;
                                            // Icon priority: user-set > slot position > default
                                            let icon = user_icons.get(model_slug).cloned()
                                                .unwrap_or_else(|| get_slot_icon(index));
                                            // Display name: strip common prefixes for readability
                                            let display_name = display_name_for_provider(&session_provider, model_slug);

                                            rsx! {
                                                button {
                                                    class: if is_active {
                                                        "w-full text-left px-4 py-2 text-sm text-fg bg-primary-900/50 flex items-center justify-between"
                                                    } else {
                                                        "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between"
                                                    },
                                                    onclick: {
                                                        move |_| {
                                                            chat_command.set(Some(ChatCommand::SwitchModel(index)));
                                                            show_model_selector.set(false);
                                                        }
                                                    },
                                                    div {
                                                        class: "flex items-center space-x-2 min-w-0",
                                                        span {
                                                            class: "w-6 h-6 rounded-full bg-section border border-faint flex items-center justify-center text-xs shrink-0",
                                                            "{icon}"
                                                        }
                                                        span { class: "truncate", "{display_name}" }
                                                    }
                                                    // Hotkey hint (1-indexed, Control+N)
                                                    if index < 9 {
                                                        span {
                                                            class: "text-xs text-fg-muted font-mono shrink-0 ml-2",
                                                            "^{index + 1}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        }
                                    }
                                }
                            }
                            // Click outside handler to close dropdown
                            if show_model_selector() {
                                div {
                                    class: "fixed inset-0 z-40",
                                    onclick: move |_| show_model_selector.set(false)
                                }
                            }
                        }
                    }
                } else { rsx!({}) } }

                // Composio Profile Selector
                { if ui_state.read().show_profile_selector {
                    {
                        let settings_read = settings.read();
                        let profiles = &settings_read.composio_profiles;

                        if profiles.len() > 1 {
                            // session.composio_profile stores ID (matching settings.active_composio_profile)
                            let active_profile = session_state.read().get_active_session()
                                .and_then(|s| s.composio_profile.as_ref())
                                .and_then(|id| profiles.iter().find(|p| &p.id == id))  // Match by ID
                                .or_else(|| settings_read.get_active_profile())
                                .cloned()
                                .unwrap_or_else(|| profiles[0].clone());

                            let active_initial = active_profile.name.chars().next().unwrap_or('?').to_uppercase();

                            rsx! {
                                div {
                                    class: "relative",
                                    button {
                                        class: format!("w-8 h-8 rounded-full {} border border-subtle flex items-center justify-center text-xs font-bold text-fg hover:brightness-110 hover:border-primary-500 transition-all focus:outline-none focus:ring-2 focus:ring-primary-600 shadow-md", active_profile.color),
                                        onclick: move |_| show_profile_selector.set(!show_profile_selector()),
                                        "{active_initial}"
                                    }

                                    if show_profile_selector() {
                                        div {
                                            class: "absolute bottom-10 left-0 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1",
                                            for (index, profile) in profiles.iter().enumerate() {
                                                {
                                                    let profile_name = profile.name.clone();
                                                    let profile_color = profile.color.clone();
                                                    let profile_initial = profile.name.chars().next().unwrap_or('?').to_uppercase().to_string();

                                                    if profile.name != active_profile.name {
                                                        rsx! {
                                                            button {
                                                                class: "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between",
                                                                onclick: move |_| {
                                                                    // Route through the global command so the session
                                                                    // pin is PERSISTED (save_async) and the profile
                                                                    // change is journaled (ComposioProfileSet in the
                                                                    // SwitchProfile handler) — writing the signals
                                                                    // directly here would lose both.
                                                                    chat_command.set(Some(ChatCommand::SwitchProfile(index)));

                                                                    // Trigger summary refresh
                                                                    scheduler.send(SchedulerSignal::ForceRefresh);
                                                                    show_profile_selector.set(false);
                                                                },
                                                                div {
                                                                    class: "flex items-center space-x-2",
                                                                    span {
                                                                        class: format!("w-6 h-6 rounded-full {} border border-faint flex items-center justify-center text-[10px] text-fg font-bold shadow-sm", profile_color),
                                                                        "{profile_initial}"
                                                                    }
                                                                    span { class: "truncate", "{profile_name}" }
                                                                }
                                                                // Hotkey hint (1-indexed). Profile switching is
                                                                // the RESERVED Cmd+Option+N combo (hotkey.rs) —
                                                                // plain Cmd+N is tab switching, so a bare ⌘ hint
                                                                // advertised a binding this popup doesn't own.
                                                                if index < 9 {
                                                                    span {
                                                                        class: "text-xs text-fg-muted font-mono",
                                                                        "⌥⌘{index + 1}"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    } else {
                                                        rsx! {{}}
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Click outside handler to close dropdown
                                    if show_profile_selector() {
                                        div {
                                            class: "fixed inset-0 z-40",
                                            onclick: move |_| show_profile_selector.set(false)
                                        }
                                    }
                                }
                            }
                        } else {
                            rsx!({})
                        }
                    }
                } else { rsx!({}) } }
                ChatBarIconButton {
                    icon: fi_icons::FiPaperclip,
                    visible: ui_state.read().show_attachments_icon,
                    title: "Attachments",
                    onclick: move |_| {
                        let mut attachments = attachments;
                        spawn(async move {
                            is_processing_attachments.set(true);
                            if let Some(files) = FileDialog::new()
                                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                                .pick_files()
                            {
                                for file_path in files {
                                    if let Ok(file_data) = tokio::fs::read(&file_path).await {
                                        let file_name = file_path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .to_string();
                                        let extension = file_path
                                            .extension()
                                            .and_then(std::ffi::OsStr::to_str)
                                            .unwrap_or("");
                                        if let Some(mime_type) = get_mime_type(extension) {
                                            if let Some(attachment) = process_image_data(
                                                file_name,
                                                mime_type.to_string(),
                                                file_data,
                                            )
                                            .await
                                            {
                                                attachments.write().push(attachment);
                                            }
                                        } else {
                                            tracing::warn!(
                                                "Unsupported file type selected: {:?}",
                                                file_path
                                            );
                                        }
                                    }
                                }
                            }
                            is_processing_attachments.set(false);
                        });
                    }
                }
                textarea {
                    id: "chat-textarea",
                    class: "flex-1 py-2 px-4 rounded-xl bg-input border border-subtle text-fg placeholder-gray-500 focus:outline-none focus:ring-2 focus:ring-primary-500 resize-none overflow-y-auto",
                    style: "max-height: 50vh;",
                    rows: "1",
                    placeholder: "Type your message...",
                    // UNCONTROLLED on purpose: a controlled `value` binding races
                    // the IPC round-trip and snaps the caret to the end mid-typing.
                    // Programmatic writes go through shared::set_chat_draft, which
                    // updates the DOM explicitly.
                    initial_value: "{draft}",
                    onmounted: move |evt| {
                        textarea_mounted.set(Some(evt.data()));
                    },
                    oninput: move |event| {
                        scheduler.send(SchedulerSignal::Activity);

                        // Auto-resize the textarea to fit its content.
                        // This eval is necessary because we need to measure the
                        // DOM element's scrollHeight, which is not accessible
                        // through Dioxus's virtual DOM.
                        let _ = document::eval(r#"
                            const el = document.getElementById('chat-textarea');
                            if (el) {
                                el.style.height = 'auto';
                                el.style.height = (el.scrollHeight) + 'px';
                            }
                        "#);

                        draft.set(event.value());

                        // Skill Autocomplete Detection
                        let val = event.value();
                        if let Some(unload_query) = val.strip_prefix("/unload ") {
                            // Autocomplete for /unload — source from session.loaded_skills
                            autocomplete_mode.set(AutocompleteMode::Unload);
                            let state = session_state.read();
                            if let Some(session) = state.get_active_session() {
                                let matches: Vec<Skill> = session.loaded_skills.keys()
                                    .filter(|name| {
                                        unload_query.is_empty() || name.to_lowercase().contains(&unload_query.to_lowercase())
                                    })
                                    .map(|name| {
                                        // Build a lightweight Skill for the autocomplete UI
                                        Skill {
                                            metadata: crate::skills::parser::SkillMetadata {
                                                name: name.clone(),
                                                description: "Loaded skill".to_string(),
                                                disable_model_invocation: false,
                                                user_invocable: true,
                                                allowed_tools: None,
                                                argument_hint: None,
                                            },
                                            instructions: String::new(),
                                            path: std::path::PathBuf::new(),
                                            root_path: std::path::PathBuf::new(),
                                            scripts: Vec::new(),
                                            resources: Vec::new(),
                                        }
                                    })
                                    .collect();
                                filtered_skills.set(matches);
                                skill_autocomplete_index.set(0);
                                show_skill_autocomplete.set(true);
                            }
                        } else {
                            // Cursor-aware skill autocomplete: trigger on a
                            // /token at the caret, anywhere in the draft. The
                            // caret position lives only in the DOM, so fetch it
                            // via eval before running the pure token scan.
                            spawn(async move {
                                let mut eval = document::eval(r#"
                                    const el = document.getElementById('chat-textarea');
                                    dioxus.send(el ? el.selectionStart : -1);
                                "#);
                                let cursor: i64 = eval.recv().await.unwrap_or(-1);
                                let query_opt = if cursor >= 0 {
                                    crate::skills::invocation::autocomplete_query_at(
                                        &val,
                                        cursor as usize,
                                    )
                                } else {
                                    None
                                };
                                match query_opt {
                                    Some(q) => {
                                        autocomplete_mode.set(AutocompleteMode::Skill);
                                        let matches: Vec<Skill> = {
                                            let registry = skill_registry.read();
                                            registry
                                                .list_skills()
                                                .into_iter()
                                                .filter(|s| {
                                                    q.query.is_empty()
                                                        || s.metadata
                                                            .name
                                                            .to_lowercase()
                                                            .contains(&q.query.to_lowercase())
                                                })
                                                .collect()
                                        };
                                        autocomplete_token.set(Some(q.token_range));
                                        filtered_skills.set(matches);
                                        skill_autocomplete_index.set(0);
                                        show_skill_autocomplete.set(true);
                                    }
                                    None => {
                                        // No /skill token — offer fleet
                                        // @-mentions when the fleet is armed.
                                        let fleet_q = if crate::fleet::bridge::enabled() && cursor >= 0 {
                                            crate::fleet::mention::mention_query_at(&val, cursor as usize)
                                        } else {
                                            None
                                        };
                                        match fleet_q {
                                            Some(q) => {
                                                autocomplete_mode.set(AutocompleteMode::Fleet);
                                                let live = crate::fleet::shared().snapshot();
                                                let today = chrono::Local::now().date_naive();
                                                let now_utc = chrono::Utc::now();
                                                let mut rows: Vec<&crate::fleet::FleetSession> = live
                                                    .sessions
                                                    .values()
                                                    .filter(|s| {
                                                        q.query.is_empty()
                                                            || s.name
                                                                .to_lowercase()
                                                                .contains(&q.query.to_lowercase())
                                                    })
                                                    .collect();
                                                rows.sort_by(|a, b| a.name.cmp(&b.name));
                                                rows.dedup_by(|a, b| a.name == b.name);
                                                let matches: Vec<Skill> = rows
                                                    .into_iter()
                                                    .map(|s| Skill {
                                                        metadata: crate::skills::parser::SkillMetadata {
                                                            name: s.name.clone(),
                                                            description: {
                                                                let mut d = format!(
                                                                    "{} today",
                                                                    crate::todo::model::format_minutes(
                                                                        s.minutes_on(today, now_utc),
                                                                    )
                                                                );
                                                                if let Some(b) = &s.brief {
                                                                    d.push_str(" — ");
                                                                    d.push_str(&crate::fleet::truncate_summary(
                                                                        &b.headline,
                                                                        80,
                                                                    ));
                                                                }
                                                                d
                                                            },
                                                            disable_model_invocation: false,
                                                            user_invocable: true,
                                                            allowed_tools: None,
                                                            argument_hint: None,
                                                        },
                                                        instructions: String::new(),
                                                        path: std::path::PathBuf::new(),
                                                        root_path: std::path::PathBuf::new(),
                                                        scripts: Vec::new(),
                                                        resources: Vec::new(),
                                                    })
                                                    .collect();
                                                autocomplete_token.set(Some(q.token_range));
                                                filtered_skills.set(matches);
                                                skill_autocomplete_index.set(0);
                                                show_skill_autocomplete.set(true);
                                            }
                                            None => {
                                                show_skill_autocomplete.set(false);
                                                filtered_skills.set(Vec::new());
                                                autocomplete_token.set(None);
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    },
                    onkeydown: move |event| {
                        tracing::debug!("ChatInput::onkeydown - Key: {:?}, Modifiers: {:?}, Context: {:?}", event.key(), event.data.modifiers(), *focus_context.read());
                        // If a modal owns focus, don't handle keyboard events
                        if *focus_context.read() != FocusContext::ChatInput {
                            tracing::debug!("ChatInput ignoring event - Focus owns: {:?}", *focus_context.read());
                            return;
                        }

                        scheduler.send(SchedulerSignal::Activity);
                        let modifiers = event.data.modifiers();
                        let hotkeys = settings.read().hotkeys.clone();

                        // 1. Check Configurable Hotkeys
                        if matches_hotkey(&event, &hotkeys.cancel_generation) {
                            if *is_sending.read() {
                                event.prevent_default();
                                on_cancel.call(());
                            }
                            return;
                        }

                        let is_force_submit = matches_hotkey(&event, &hotkeys.submit_chat);

                        // 2. Filter Global Modifiers
                        // Allow if it matches our force submit (e.g. Cmd+Enter), otherwise let browser/OS handle Cmd+X, Cmd+R, etc.
                        let has_cmd_opt_ctrl = modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::ALT);
                        if has_cmd_opt_ctrl && !is_force_submit {
                            return;
                        }

                        // 3. Tab Handling (Indentation)
                        if event.key() == Key::Tab {
                            event.prevent_default();
                            let script = if modifiers.contains(Modifiers::SHIFT) {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const value = el.value;
                                    let line_start = value.lastIndexOf('\n', start - 1) + 1;
                                    if (value.substring(line_start, line_start + 1) === '\t') {
                                        el.value = value.substring(0, line_start) + value.substring(line_start + 1);
                                        el.selectionStart = el.selectionEnd = Math.max(start - 1, line_start);
                                    }
                                }
                                "#
                            } else {
                                r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    const start = el.selectionStart;
                                    const end = el.selectionEnd;
                                    el.value = el.value.substring(0, start) + '\t' + el.value.substring(end);
                                    el.selectionStart = el.selectionEnd = start + 1;
                                }
                                "#
                            };
                            let _ = document::eval(script);
                            let _ = document::eval(r#"
                                const el = document.getElementById('chat-textarea');
                                if (el) {
                                    var event = new Event('input', { bubbles: true, cancelable: true });
                                    el.dispatchEvent(event);
                                }
                            "#);
                            return;
                        }

                        // 4. Skill Autocomplete Navigation
                        if *show_skill_autocomplete.read() {
                            let skills = filtered_skills.read();
                            let current_idx = *skill_autocomplete_index.read();

                            match event.key() {
                                Key::ArrowDown => {
                                    event.prevent_default();
                                    if current_idx < skills.len().saturating_sub(1) {
                                        skill_autocomplete_index.set(current_idx + 1);
                                    }
                                    return;
                                }
                                Key::ArrowUp => {
                                    event.prevent_default();
                                    if current_idx > 0 {
                                        skill_autocomplete_index.set(current_idx - 1);
                                    }
                                    return;
                                }
                                Key::Escape => {
                                    event.prevent_default();
                                    show_skill_autocomplete.set(false);
                                    return;
                                }
                                Key::Enter | Key::Tab => {
                                    if !skills.is_empty() {
                                        event.prevent_default();
                                        let selected = skills.get(current_idx).cloned();
                                        drop(skills); // Release borrow before mutating
                                        if let Some(skill) = selected {
                                            let current_draft = draft.read().clone();

                                            match *autocomplete_mode.read() {
                                                AutocompleteMode::Unload => {
                                                    // For unload, always fill the full command
                                                    let skill_command = format!("/unload {}", skill.metadata.name);
                                                    tracing::info!("Populated draft with skill command: {}", skill_command);
                                                    crate::components::shared::set_chat_draft(draft, skill_command, None, true);
                                                }
                                                mode @ (AutocompleteMode::Skill | AutocompleteMode::Fleet) => {
                                                    // Splice the completed /name (or @name) into the
                                                    // in-progress token's range, preserving text.
                                                    let token_range: Option<(usize, usize)> =
                                                        *autocomplete_token.read();
                                                    // The stored range comes from an async cursor eval and can be
                                                    // stale against the current draft — a non-boundary index would
                                                    // panic on slicing, so validate char boundaries too.
                                                    let (start, end) = token_range
                                                        .filter(|(s, e)| {
                                                            *s <= *e
                                                                && *e <= current_draft.len()
                                                                && current_draft.is_char_boundary(*s)
                                                                && current_draft.is_char_boundary(*e)
                                                        })
                                                        .unwrap_or((0, current_draft.len().min(
                                                            current_draft.find(' ').unwrap_or(current_draft.len()),
                                                        )));
                                                    let completed = if mode == AutocompleteMode::Fleet {
                                                        format!("@{} ", skill.metadata.name)
                                                    } else {
                                                        format!("/{} ", skill.metadata.name)
                                                    };
                                                    let mut new_draft = String::with_capacity(
                                                        current_draft.len() + completed.len(),
                                                    );
                                                    new_draft.push_str(&current_draft[..start]);
                                                    new_draft.push_str(&completed);
                                                    let cursor_byte = new_draft.len();
                                                    new_draft.push_str(current_draft[end..].trim_start());
                                                    // Restore the caret just after the inserted command
                                                    let cursor_utf16 =
                                                        crate::skills::invocation::byte_to_utf16_offset(
                                                            &new_draft,
                                                            cursor_byte,
                                                        );
                                                    tracing::info!("Spliced skill command into draft: {}", new_draft);
                                                    crate::components::shared::set_chat_draft(
                                                        draft,
                                                        new_draft,
                                                        Some(cursor_utf16),
                                                        true,
                                                    );
                                                }
                                            }
                                            autocomplete_token.set(None);
                                            show_skill_autocomplete.set(false);
                                            on_interaction.call(());
                                        }
                                        return;
                                    }
                                }
                                _ => {}
                            }
                        }

                        // 5. Submit Handling
                        let is_standard_submit = event.key() == Key::Enter && !modifiers.contains(Modifiers::SHIFT);

                        // Submit if standard Enter (no shift) OR Force Submit matches
                        if is_standard_submit || is_force_submit {
                            event.prevent_default();
                            on_interaction.call(());
                            send_message();
                        }
                    },
                    onpaste: move |_| {
                        // All clipboard operations must happen on the main thread.
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            if let Ok(image) = clipboard.get_image() {
                                // Only spawn a task if there's an image to process.
                                spawn(async move {
                                    is_processing_attachments.set(true);
                                    let image_data = arboard::ImageData {
                                        width: image.width,
                                        height: image.height,
                                        bytes: image.bytes,
                                    };
                                    if let Some(attachment) = process_clipboard_image(image_data).await {
                                        attachments.write().push(attachment);
                                    }
                                    is_processing_attachments.set(false);
                                });
                            }
                        }
                    },
                }

                // Skill Autocomplete Component
                if *show_skill_autocomplete.read() {
                    SkillAutocomplete {
                        skills: filtered_skills.read().clone(),
                        selected_index: *skill_autocomplete_index.read(),
                        on_select: move |skill: Skill| {
                            match *autocomplete_mode.read() {
                                AutocompleteMode::Unload => {
                                    crate::components::shared::set_chat_draft(
                                        draft,
                                        format!("/unload {}", skill.metadata.name),
                                        None,
                                        true,
                                    );
                                }
                                mode @ (AutocompleteMode::Skill | AutocompleteMode::Fleet) => {
                                    // Splice into the in-progress token, preserving context
                                    let current_draft = draft.read().clone();
                                    let token_range: Option<(usize, usize)> = *autocomplete_token.read();
                                    // Same as the keyboard path: the range may be stale against the
                                    // current draft, so reject non-char-boundary indices before slicing.
                                    let (start, end) = token_range
                                        .filter(|(s, e)| {
                                            *s <= *e
                                                && *e <= current_draft.len()
                                                && current_draft.is_char_boundary(*s)
                                                && current_draft.is_char_boundary(*e)
                                        })
                                        .unwrap_or((0, current_draft.len()));
                                    let completed = if mode == AutocompleteMode::Fleet {
                                        format!("@{} ", skill.metadata.name)
                                    } else {
                                        format!("/{} ", skill.metadata.name)
                                    };
                                    let mut new_draft = String::with_capacity(current_draft.len() + completed.len());
                                    new_draft.push_str(&current_draft[..start]);
                                    new_draft.push_str(&completed);
                                    let cursor_utf16 = crate::skills::invocation::byte_to_utf16_offset(&new_draft, new_draft.len());
                                    new_draft.push_str(current_draft[end..].trim_start());
                                    crate::components::shared::set_chat_draft(
                                        draft,
                                        new_draft,
                                        Some(cursor_utf16),
                                        true,
                                    );
                                }
                            }
                            autocomplete_token.set(None);
                            show_skill_autocomplete.set(false);
                            on_interaction.call(());
                            tracing::info!("Selected skill from autocomplete: {}", skill.metadata.name);
                        }
                    }
                }
                div {
                    class: "flex items-center space-x-3",
                    {
                        cfg_if::cfg_if! {
                            if #[cfg(debug_assertions)] {
                                rsx! {
                                    button {
                                        class: "p-2 rounded-full text-fg-muted hover:bg-card hover:text-fg focus:outline-none focus:ring-2 focus:ring-gray-600",
                                        onclick: move |_| {
                                            spawn(async move {
                                                let mcp_context = {
                                                    let mcp_manager_reader = _mcp_manager.read();
                                                    mcp_manager_reader.get_mcp_context(None).await
                                                };

                                                let context_string = {
                                                    let state = session_state.read();
                                                    if let Some(session) = state.get_active_session().cloned() {
                                                        let mut session_for_debug = session;

                                                        if !mcp_context.servers.is_empty() {
                                                            session_for_debug.active_context.mcp_tools = Some(mcp_context);
                                                        }

                                                        let settings_reader = settings.read();
                                                        let builder = PromptBuilder::new(&session_for_debug, &settings_reader, &state)
                                                            .with_planner_today(crate::todo::handlers::planner_today_context(
                                                                &_planner_state.read(),
                                                                &settings_reader,
                                                                chrono::Local::now().date_naive(),
                                                            ));
                                                        let result = builder.build_prompt("[DEBUG USER MESSAGE]".to_string());
                                                        let prompt_data = result.prompt;
                                                        format!("{:#?}", prompt_data)
                                                    } else {
                                                        "[No active session]".to_string()
                                                    }
                                                };
                                                let timestamp = SystemTime::now()
                                                    .duration_since(SystemTime::UNIX_EPOCH)
                                                    .unwrap()
                                                    .as_secs();
                                                let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
                                                if std::fs::create_dir_all(&debug_dir).is_ok() {
                                                    let file_path = debug_dir.join(format!("prompt_{}.log", timestamp));
                                                    if let Err(e) = std::fs::write(&file_path, &context_string) {
                                                        tracing::error!("Failed to write debug prompt to file: {}", e);
                                                    } else {
                                                        tracing::debug!("Debug prompt written to {:?}", file_path);
                                                    }
                                                }
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
                    // New Chat Menu (Popup)
                    div {
                        class: "relative",
                        button {
                            class: "p-2 rounded-full text-fg-muted hover:bg-card hover:text-fg focus:outline-none focus:ring-2 focus:ring-gray-600",
                            title: "New Chat",
                            onclick: move |_| show_new_chat_menu.set(!show_new_chat_menu()),
                            Icon {
                                width: 20,
                                height: 20,
                                icon: fi_icons::FiPlus
                            }
                        }

                        if show_new_chat_menu() {
                            div {
                                class: "absolute bottom-10 right-0 w-64 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1",
                                // New Chat (No Memory)
                                button {
                                    class: "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between",
                                    onclick: move |_| {
                                        chat_command.set(Some(ChatCommand::NewChat));
                                        show_new_chat_menu.set(false);
                                    },
                                    div {
                                        class: "flex items-center space-x-2",
                                        Icon {
                                            width: 16,
                                            height: 16,
                                            icon: fi_icons::FiPlus,
                                            class: "text-fg-muted"
                                        }
                                        span { "New Chat" }
                                    }
                                    span {
                                        class: "text-xs text-fg-muted font-mono",
                                        "{format_hotkey(&settings.read().hotkeys.toggle_new_chat)}"
                                    }
                                }
                                // New Chat with Memory
                                button {
                                    class: "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between",
                                    onclick: move |_| {
                                        on_new_chat_with_memory.call(());
                                        show_new_chat_menu.set(false);
                                    },
                                    div {
                                        class: "flex items-center space-x-2",
                                        Icon {
                                            width: 16,
                                            height: 16,
                                            icon: fi_icons::FiCpu,
                                            class: "text-primary-400"
                                        }
                                        span { "New Chat with Memory" }
                                    }
                                    span {
                                        class: "text-xs text-fg-muted font-mono",
                                        "{format_hotkey(&settings.read().hotkeys.toggle_new_chat_with_memory)}"
                                    }
                                }
                            }
                        }
                        // Click outside handler to close dropdown
                        if show_new_chat_menu() {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |_| show_new_chat_menu.set(false)
                            }
                        }
                    }
                    if !*is_sending.read() {
                        button {
                            class: "px-5 py-2 bg-btn-primary rounded-full text-fg font-semibold hover:bg-btn-primary-hover focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-opacity-50 transition-colors disabled:bg-gray-500",
                            disabled: *is_processing_attachments.read(),
                            onclick: move |_| {
                                on_interaction.call(());
                                send_message();
                            },
                            if *has_new_comments.read() || *has_pending_approvals.read() { "Submit" } else { "Send" }
                        }
                    } else {
                        button {
                            class: "px-4 py-2 bg-red-600 rounded-full text-fg font-semibold hover:bg-red-700 focus:outline-none focus:ring-2 focus:ring-red-500 focus:ring-opacity-50 transition-colors flex items-center space-x-2",
                            onclick: move |_| on_cancel.call(()),
                            Icon {
                                width: 20,
                                height: 20,
                                icon: fi_icons::FiSquare
                            },
                            span { "Stop" }
                        }
                    }
                }
            }
        }
    }
}

async fn process_image_data(
    file_name: String,
    mime_type: String,
    file_data: Vec<u8>,
) -> Option<Attachment> {
    tokio::task::spawn_blocking(move || {
        image::load_from_memory(&file_data).ok().and_then(|img| {
            let resized_img = if img.height() > 200 {
                img.resize(u32::MAX, 200, imageops::FilterType::Lanczos3)
            } else {
                img
            };
            let format = match mime_type.as_str() {
                "image/png" => ImageFormat::Png,
                "image/jpeg" => ImageFormat::Jpeg,
                "image/gif" => ImageFormat::Gif,
                "image/webp" => ImageFormat::WebP,
                _ => ImageFormat::Png,
            };
            let mut buffer = Cursor::new(Vec::new());
            resized_img.write_to(&mut buffer, format).ok().map(|_| {
                let data = general_purpose::STANDARD.encode(buffer.into_inner());
                Attachment {
                    file_name,
                    mime_type,
                    data,
                }
            })
        })
    })
    .await
    .ok()
    .flatten()
}

async fn process_clipboard_image(image_data: arboard::ImageData<'_>) -> Option<Attachment> {
    let width = image_data.width as u32;
    let height = image_data.height as u32;
    let bytes = image_data.bytes.into_owned();

    tokio::task::spawn_blocking(move || {
        image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, bytes).and_then(
            |img_buf| {
                let resized_img: DynamicImage = if img_buf.height() > 200 {
                    let dynamic_image: DynamicImage = img_buf.into();
                    dynamic_image.resize(u32::MAX, 200, imageops::FilterType::Lanczos3)
                } else {
                    img_buf.into()
                };

                let mut buffer = Cursor::new(Vec::new());
                resized_img
                    .write_to(&mut buffer, ImageFormat::Png)
                    .ok()
                    .map(|_| {
                        let data = general_purpose::STANDARD.encode(buffer.into_inner());
                        let timestamp = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let file_name = format!("pasted-image-{}.png", timestamp);
                        Attachment {
                            file_name,
                            mime_type: "image/png".to_string(),
                            data,
                        }
                    })
            },
        )
    })
    .await
    .ok()
    .flatten()
}

fn get_mime_type(extension: &str) -> Option<&'static str> {
    match extension.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Formats a hotkey string for display (e.g., "CmdOrCtrl+Shift+N" -> "⌘⇧N")
fn format_hotkey(hotkey: &str) -> String {
    hotkey
        .replace("CmdOrCtrl", "⌘")
        .replace("Ctrl", "⌃")
        .replace("Alt", "⌥")
        .replace("Shift", "⇧")
        .replace("+", "")
}

/// Minimal live indicator of pending timers for the active session, shown just
/// above the chat bar. Owns its own 1 Hz tick so the countdown updates without
/// re-rendering the (large) `ChatInput`. Renders nothing when no timer is pending.
#[component]
fn PendingTimersBar() -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let SessionIdContext(session_id) = use_context::<SessionIdContext>();

    // 1-second tick driving the live countdown.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            *tick.write() += 1;
        }
    });
    let _ = tick.read(); // subscribe so the countdown re-renders each second

    let now = chrono::Utc::now();
    let sid = session_id.read().clone();
    let remaining: Vec<i64> = session_state
        .read()
        .sessions
        .get(&sid)
        .map(|s| {
            s.scheduled_timers
                .iter()
                .filter(|t| t.status == crate::timers::TimerStatus::Pending)
                .map(|t| (t.fire_at - now).num_seconds().max(0))
                .collect()
        })
        .unwrap_or_default();

    if remaining.is_empty() {
        return rsx! {};
    }

    let count = remaining.len();
    let soonest = remaining.iter().copied().min().unwrap_or(0);
    let (mm, ss) = (soonest / 60, soonest % 60);
    let label = if count == 1 {
        "1 timer running".to_string()
    } else {
        format!("{} timers running", count)
    };

    rsx! {
        div {
            class: "flex items-center gap-1.5 px-3 pt-1 pb-3 text-xs text-fg-muted",
            span {
                class: "inline-block w-1.5 h-1.5 rounded-full bg-primary-500 animate-pulse",
            }
            Icon { width: 13, height: 13, icon: fi_icons::FiClock }
            span { "{label} · next in {mm}:{ss:02}" }
        }
    }
}

#[component]
fn SessionCostIcon() -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let ui_state = consume_context::<Signal<UiState>>();
    let stream_manager = consume_context::<StreamManagerContext>();
    // Consume SessionIdContext to ensure this component re-renders on every tab switch
    let SessionIdContext(current_target_id) = use_context::<SessionIdContext>();
    let mut show_popover = use_signal(|| false);
    let mut prev_cost = use_signal(|| 0.0_f64);
    let mut prev_session_id = use_signal(String::new);
    let mut animation_target = use_signal(|| 0.0_f64);
    let mut cost_animating = use_signal(|| false);

    // Subscribe to the stream lifecycle so the cost/token counters refresh at every
    // turn start/end/continuation. As a memoized no-props component, the background
    // usage write into `session_state` doesn't reliably re-render this icon on its own
    // (the value only surfaced after a tab switch flushed the scheduler); `stream_activity`
    // is the app's canonical "stream state changed" signal and is bumped after the final
    // usage is recorded, so reading it here guarantees a refresh without leaving the tab.
    // Read unconditionally, before any early return, so the subscription can't be dropped.
    let _ = stream_manager.stream_activity.read();

    // Check if we should show the icon
    if !ui_state.read().show_session_cost_icon {
        return rsx! {};
    }

    let current_session_id = current_target_id.read().clone();
    let state = session_state.read();
    let session = match state.sessions.get(&current_session_id) {
        Some(s) => s,
        None => return rsx! {},
    };

    let total_cost = session.total_cost();
    let total_tokens = session.total_tokens();
    let avg_tokens = session.average_tokens_per_turn();

    use_effect(move || {
        let current_id = current_target_id.read().clone();
        let s_state = session_state.read();
        let cost = match s_state.sessions.get(&current_id) {
            Some(s) => s.total_cost(),
            None => return,
        };

        if *prev_session_id.peek() != current_id {
            prev_session_id.set(current_id);
            prev_cost.set(cost);
            animation_target.set(cost);
            return;
        }

        let old_cost = *prev_cost.peek();
        let last_target = *animation_target.peek();
        if cost > old_cost
            && (cost - old_cost).abs() > 0.0001
            && (cost - last_target).abs() > 0.0001
        {
            prev_cost.set(cost);
            animation_target.set(cost);
            cost_animating.set(true);
            spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(650)).await;
                cost_animating.set(false);
            });
        } else if (cost - old_cost).abs() > 0.0001 {
            prev_cost.set(cost);
            animation_target.set(cost);
        }
    });

    let cost_class = if *cost_animating.read() {
        "text-xs font-mono font-medium cost-tick"
    } else {
        "text-xs font-mono font-medium"
    };

    rsx! {
        div {
            class: "relative",
            button {
                class: "p-2 rounded-full text-fg-muted hover:bg-card hover:text-green-400 focus:outline-none focus:ring-2 focus:ring-gray-600 flex items-center space-x-1 transition-colors",
                onclick: move |_| {
                    let current = *show_popover.read();
                    show_popover.set(!current);
                },
                title: "Session Cost",
                Icon {
                    width: 16,
                    height: 16,
                    icon: fi_icons::FiDollarSign
                }
                span {
                    id: "session-cost-display",
                    class: cost_class,
                    {format!("{:.2}", total_cost)}
                }
            }

            if *show_popover.read() {
                div {
                    class: "absolute bottom-full left-0 mb-2 w-64 bg-card border border-faint rounded-lg shadow-xl p-3 z-50",

                    div { class: "text-sm font-semibold text-gray-200 mb-2 border-b border-faint pb-1", "Session Usage" }

                    div { class: "space-y-2 text-xs",
                        div { class: "flex justify-between text-fg-muted",
                            span { "Total Cost:" }
                            span { class: "text-green-400 font-mono", {format!("${:.4}", total_cost)} }
                        }
                        div { class: "flex justify-between text-fg-muted",
                            span { "Total Tokens:" }
                            span { class: "text-fg font-mono", "{total_tokens}" }
                        }
                        div { class: "flex justify-between text-fg-muted",
                            span { "Avg Tokens/Turn:" }
                            span { class: "text-fg font-mono", {format!("{:.0}", avg_tokens)} }
                        }
                    }

                    // All-time totals (lifetime counters from deleted sessions + all stored sessions)
                    {
                        let (stored_cost, stored_tokens) = crate::session_store::sum_cost_tokens();
                        let all_time_cost = state.lifetime_cost + stored_cost;
                        let all_time_tokens = state.lifetime_tokens + stored_tokens;
                        rsx! {
                            div { class: "mt-2 pt-2 border-t border-faint space-y-2 text-xs",
                                div { class: "text-sm font-semibold text-gray-200 mb-1", "All-Time" }
                                div { class: "flex justify-between text-fg-muted",
                                    span { "Lifetime Cost:" }
                                    span { class: "text-green-400 font-mono", {format!("${:.4}", all_time_cost)} }
                                }
                                div { class: "flex justify-between text-fg-muted",
                                    span { "Lifetime Tokens:" }
                                    span { class: "text-fg font-mono",
                                        {if all_time_tokens > 1_000_000 {
                                            format!("{:.1}M", all_time_tokens as f64 / 1_000_000.0)
                                        } else if all_time_tokens > 1_000 {
                                            format!("{:.1}K", all_time_tokens as f64 / 1_000.0)
                                        } else {
                                            format!("{}", all_time_tokens)
                                        }}
                                    }
                                }
                            }
                        }
                    }
                    div { class: "mt-2 pt-2 border-t border-faint text-[10px] text-fg-muted text-center",
                        "Estimates based on Gemini pricing"
                    }
                }
                // Backdrop to close
                div {
                    class: "fixed inset-0 z-40 bg-transparent cursor-default",
                    onclick: move |_| show_popover.set(false),
                }
            }
        }
    }
}
