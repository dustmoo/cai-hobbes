// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use super::continuation_controller::ContinuationController;
use crate::components::shared::{StreamMessage, ToolCallStatus};

use crate::services::tool_call_summarizer::ToolCallSummarizer;
use crate::session::SessionState;
use crate::settings::Settings;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;

use crate::processing::summarization_scheduler::SchedulerSignal;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<StreamMessage>>>,
    active_stream_handles: Signal<HashMap<Uuid, Task>>,
    llm_connector: Signal<std::sync::Arc<dyn crate::llm::LlmConnector>>,
    session_state: Signal<SessionState>,
    mcp_manager: Signal<crate::mcp::manager::McpManager>,
    tool_call_summarizer: Signal<ToolCallSummarizer>,
    settings: Signal<Settings>,
    continuation_controller: Signal<ContinuationController>,
    scheduler: Coroutine<SchedulerSignal>,
    permission_manager: Signal<crate::context::permissions::PermissionManager>,
    skill_registry: Signal<crate::skills::SkillRegistry>,
    mcp_context: Signal<crate::mcp::manager::McpContext>,
    /// The global planner — needed by the built-in planner tools, which run on
    /// the streaming path here (P-015: dispatch stays in builtin_tools).
    planner_state: Signal<crate::todo::PlannerState>,
    pub stream_activity: Signal<u64>,
    pub streaming_sessions: Signal<HashSet<String>>,
    stream_session_map: Signal<HashMap<Uuid, String>>,
    pub content_generated: Signal<HashSet<Uuid>>,
    save_error_signal: Signal<Option<String>>,
    usage_log: Signal<crate::usage_log::UsageLog>,
    /// Guard: prevents overlapping proactive summarization tasks.
    /// Keyed by session_id to allow concurrent summarization across tabs.
    summarizing_sessions: Signal<HashSet<String>>,
}

impl StreamManagerContext {
    pub fn is_streaming(self, message_id: &Uuid) -> bool {
        self.stream_receivers.read().contains_key(message_id)
    }

    pub fn is_generating(self, message_id: &Uuid) -> bool {
        self.active_stream_handles.read().contains_key(message_id)
    }

    pub fn has_generated_content(self, message_id: &Uuid) -> bool {
        self.content_generated.read().contains(message_id)
    }

    pub fn is_session_streaming(self, session_id: &str) -> bool {
        if self.streaming_sessions.read().contains(session_id) {
            return true;
        }
        // Fallback: check map of active stream handles to catch continuations
        self.stream_session_map.read().values().any(|v| v == session_id)
    }

    /// Find the active message_id for a given session (for cancel).
    /// Reverse-lookups stream_session_map: message_id → session_id.
    pub fn active_message_for_session(self, session_id: &str) -> Option<Uuid> {
        self.stream_session_map
            .read()
            .iter()
            .find(|(_, sid)| sid.as_str() == session_id)
            .map(|(mid, _)| *mid)
    }

    /// The connector instance a session resolves to (session pin → legacy kind
    /// match → global active connector). Cloned so no locks are held.
    fn effective_connector(&self, session_id: &str) -> Option<crate::settings::ProviderInstance> {
        let settings = self.settings.read();
        let state = self.session_state.read();
        match state.sessions.get(session_id) {
            Some(s) => settings.connector_for_session(s).cloned(),
            None => settings.active_connector().cloned(),
        }
    }

    /// The session's resolved OpenAI-compat config when watch-word recovery
    /// is enabled on it. Watch words are a per-instance OpenAI-compat
    /// feature: gate on the session's resolved connector config, not the
    /// global one.
    fn watch_word_config(&self, session_id: &str) -> Option<crate::settings::OpenAiCompatConfig> {
        self.effective_connector(session_id).and_then(|inst| {
            match inst.config {
                crate::settings::ProviderInstanceConfig::OpenAiCompat(c)
                    if c.watch_words_enabled =>
                {
                    Some(c)
                }
                _ => None,
            }
        })
    }

    /// The chat model a session resolves to (session override → provider default).
    fn effective_model(&self, session_id: &str) -> String {
        let settings = self.settings.read();
        match self.session_state.read().sessions.get(session_id) {
            Some(s) => settings.chat_model_for_session(s),
            None => settings.active_chat_model(),
        }
    }

    /// Resolve the connector for a session. Sessions without overrides (or whose
    /// overrides match the global selection) use the shared global connector;
    /// otherwise a connector is built for the session's provider + model.
    fn connector_for_session(&self, session_id: &str) -> Arc<dyn crate::llm::LlmConnector> {
        let (instance, model, matches_global) = {
            let settings = self.settings.read();
            let state = self.session_state.read();
            let instance = match state.sessions.get(session_id) {
                Some(s) => settings.connector_for_session(s).cloned(),
                None => settings.active_connector().cloned(),
            };
            let model = match state.sessions.get(session_id) {
                Some(s) => settings.chat_model_for_session(s),
                None => settings.active_chat_model(),
            };
            // Reuse the shared global connector only when the session resolves
            // to the same connector INSTANCE (by id) and the same model.
            let matches_global = match (&instance, settings.active_connector()) {
                (Some(inst), Some(active)) => {
                    inst.id == active.id && model == active.config.chat_model()
                }
                _ => true, // no connectors configured — only the global one exists
            };
            (instance, model, matches_global)
        };

        match (matches_global, instance) {
            (false, Some(inst)) => {
                tracing::info!(
                    session_id,
                    connector = %inst.name,
                    provider = inst.provider().display_name(),
                    model,
                    "Session overrides global LLM selection — building session connector"
                );
                crate::llm::build_connector_for_instance(&inst, Some(&model))
            }
            _ => self.llm_connector.read().clone(),
        }
    }

    pub fn start_stream(
        mut self,
        message_id: Uuid,
        session_id: String,
        prompt_data: crate::llm::LlmPrompt,
        on_complete: impl FnOnce() + 'static,
        mcp_context: Option<crate::mcp::manager::McpContext>,
        profile_id: Option<String>,
    ) {
        self.streaming_sessions.write().insert(session_id.clone());
        self.stream_session_map.write().insert(message_id, session_id.clone());
        tracing::info!(message_id = %message_id, session_id = %session_id, "'start_stream' entered.");
        // Create a channel for the MessageBubble to receive chunks.
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<StreamMessage>();

        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, stream_rx);
        *self.stream_activity.write() += 1;

        // Spawn a master task to manage the LLM call and state updates.
        let master_task_handle = spawn(async move {
            tracing::info!(message_id = %message_id, "Stream master task SPAWNED.");
            // Keep the built prompt and MCP context so we can re-send a trimmed
            // version if the provider rejects the prompt as too large
            // (adapt-to-error self-calibration, handled in the Error arm).
            let mut current_prompt = prompt_data;
            let mut context_retry_count: u32 = 0;
            const MAX_CONTEXT_RETRIES: u32 = 2;

            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<StreamMessage>();
            {
                let llm_connector = self.connector_for_session(&session_id);
                let session_id_for_cache = session_id.clone();
                let pd = current_prompt.clone();
                let mcp = mcp_context.clone();
                spawn(async move {
                    llm_connector
                        .generate_content_stream(pd, llm_tx, mcp, Some(session_id_for_cache))
                        .await;
                });
            }

            let mut is_first_message = true;
            let (tool_results_tx, mut tool_results_rx) =
                mpsc::unbounded_channel::<crate::components::shared::ToolCallRecord>();
            let mut tool_call_count = 0;
            // Track whether the placeholder message has been upgraded to a ToolCall.
            // Only the FIRST tool call in a turn should upgrade it; subsequent
            // parallel tool calls must create separate messages for transparency.
            let mut first_tool_call_placed = false;
            // Defense-in-depth: track dispatched tool calls to prevent duplicate execution
            // even if the LLM connector fails to dedup (e.g., different providers).
            let mut dispatched_tool_calls: std::collections::HashSet<(String, String, u64)> =
                std::collections::HashSet::new();
            let completed_tool_tasks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut final_text_for_this_turn = String::new();
            let mut thought_signature_for_this_turn: Option<String> = None;
            let mut thought_summary_for_this_turn: Option<String> = None;
            // Tracks whether the UI consumer (MessageBubble) has disconnected,
            // e.g. due to a tab switch unmounting the component. When true,
            // we stop sending chunks to stream_tx but continue processing
            // the LLM stream and writing to session_state.
            let mut ui_disconnected = false;
            // Tracks whether the stream ended due to a provider error (e.g., decode failure).
            // When true AND auto-recovery is enabled, we retry the continuation.
            let mut stream_error_occurred = false;

            while let Some(message) = llm_rx.recv().await {
                match message {
                    StreamMessage::Text {
                        content,
                        thought_signature,
                        thought_summary,
                    } => {
                        // Mark as having generated content on first text chunk
                        if !content.is_empty() {
                            self.content_generated.write().insert(message_id);
                        }

                        // Append the content to the buffer
                        final_text_for_this_turn.push_str(&content);
                        if thought_signature.is_some() {
                            thought_signature_for_this_turn = thought_signature.clone();
                        }
                        if let Some(summary) = &thought_summary {
                            if let Some(current) = &mut thought_summary_for_this_turn {
                                current.push_str(summary);
                            } else {
                                thought_summary_for_this_turn = Some(summary.clone());
                            }
                        }
                        // Forward to UI stream if the consumer is still alive.
                        // When the user switches tabs, MessageBubble unmounts and drops
                        // its receiver — stream_tx.send() returns Err. We must NOT break
                        // the loop here; the background LLM stream must continue running
                        // and writing to session_state so the data is preserved.
                        let has_renderable_content =
                            !content.is_empty() || thought_signature.is_some() || thought_summary.is_some();
                        if has_renderable_content && !ui_disconnected {
                            if stream_tx
                                .send(StreamMessage::Text {
                                    content: content.clone(),
                                    thought_signature: thought_signature.clone(),
                                    thought_summary: thought_summary.clone(),
                                })
                                .is_err()
                            {
                                // UI consumer dropped (tab switch). Continue processing
                                // the LLM stream in data-only mode.
                                tracing::info!(
                                    message_id = %message_id,
                                    session_id = %session_id,
                                    "UI consumer disconnected (tab switch). Continuing stream in background."
                                );
                                ui_disconnected = true;
                            }
                        }
                        self.scheduler.send(SchedulerSignal::Activity);

                        // Write accumulated text to session_state on every chunk.
                        // This ensures that when the user switches back to this tab,
                        // they see the current content — not just the first chunk snapshot.
                        if !final_text_for_this_turn.is_empty() {
                            let mut state = self.session_state.write();
                            if let Some(msg) =
                                state.get_message_mut_in_session(&session_id, &message_id)
                            {
                                if let crate::components::shared::MessageContent::Text {
                                    content: msg_content,
                                    thought_signature: msg_thought_sig,
                                    thought_summary: msg_thought_sum,
                                } = &mut msg.content
                                {
                                    *msg_content = final_text_for_this_turn.clone();
                                    *msg_thought_sig = thought_signature_for_this_turn.clone();
                                    *msg_thought_sum = thought_summary_for_this_turn.clone();
                                }
                            }
                        }

                        // Only mark as non-first when we have actual content, so that
                        // thinking-only messages don't prevent ToolCall from upgrading in-place.
                        if !content.is_empty() {
                            is_first_message = false;
                        }
                    }
                    StreamMessage::Error { message: error_msg } => {
                        // Adapt-to-error: if the provider rejected the prompt as too
                        // large and nothing has streamed yet (clean state), trim the
                        // prompt to a smaller, recalibrated window and retry in-turn.
                        // The reduced window is also recorded so subsequent turns are
                        // budgeted correctly without hitting the error again.
                        if is_first_message
                            && context_retry_count < MAX_CONTEXT_RETRIES
                            && crate::llm::context_cache::is_context_overflow(&error_msg)
                        {
                            context_retry_count += 1;
                            let (instance, model, scope, chars_per_token) = {
                                let settings = self.settings.read();
                                let state = self.session_state.read();
                                let session = state.sessions.get(&session_id);
                                let instance = session
                                    .and_then(|s| settings.connector_for_session(s))
                                    .or_else(|| settings.active_connector())
                                    .cloned();
                                let model = session
                                    .map(|s| settings.chat_model_for_session(s))
                                    .unwrap_or_else(|| settings.active_chat_model());
                                let scope = match instance.as_ref().map(|i| &i.config) {
                                    Some(crate::settings::ProviderInstanceConfig::OpenAiCompat(
                                        c,
                                    )) => c.endpoint.clone(),
                                    Some(crate::settings::ProviderInstanceConfig::Claude(_)) => {
                                        "claude".to_string()
                                    }
                                    _ => "gemini".to_string(),
                                };
                                let cpt = instance
                                    .as_ref()
                                    .map(|i| {
                                        settings
                                            .effective_context_tuning_for_connector(i)
                                            .chars_per_token
                                    })
                                    .unwrap_or_else(|| {
                                        settings
                                            .effective_context_tuning_for(settings.active_llm)
                                            .chars_per_token
                                    });
                                (instance, model, scope, cpt)
                            };

                            // The connector may already have recorded an explicit
                            // limit parsed from the raw body; the resolver reflects it.
                            // Shrink to 80% of the best estimate (8k floor) so we make
                            // progress even when no exact number was given.
                            let current = instance
                                .as_ref()
                                .and_then(|i| {
                                    self.settings
                                        .read()
                                        .resolve_context_window_for_connector(i, &model)
                                })
                                .unwrap_or(128_000);
                            let new_window = current.saturating_sub(current / 5).max(8_000);
                            crate::llm::context_cache::record_window(&scope, &model, new_window);

                            // Trim the already-built prompt to fit the new window and
                            // re-send on a fresh channel (protect the 6 most recent msgs).
                            current_prompt.enforce_context_budget(new_window, 6, chars_per_token);
                            tracing::warn!(
                                session_id = %session_id,
                                new_window,
                                attempt = context_retry_count,
                                "Context overflow — recalibrating budget and retrying in-turn"
                            );

                            let (new_tx, new_rx) = mpsc::unbounded_channel::<StreamMessage>();
                            {
                                let llm_connector = self.connector_for_session(&session_id);
                                let sid = session_id.clone();
                                let pd = current_prompt.clone();
                                let mcp = mcp_context.clone();
                                spawn(async move {
                                    llm_connector
                                        .generate_content_stream(pd, new_tx, mcp, Some(sid))
                                        .await;
                                });
                            }
                            llm_rx = new_rx;
                            continue;
                        }

                        tracing::error!("LLM stream error: {}", error_msg);

                        // Save error message to session
                        {
                            let mut state = self.session_state.write();
                            if is_first_message {
                                // Update the placeholder message with the error
                                if let Some(msg) =
                                    state.get_message_mut_in_session(&session_id, &message_id)
                                {
                                    msg.content =
                                        crate::components::shared::MessageContent::Error {
                                            message: error_msg.clone(),
                                        };
                                }
                            } else {
                                // Create a new error message
                                let new_id = Uuid::new_v4();
                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                    session.messages.push(crate::components::chat::Message {
                                        id: new_id,
                                        author: "Hobbes".to_string(),
                                        content: crate::components::shared::MessageContent::Error {
                                            message: error_msg.clone(),
                                        },
                                        attachments: Vec::new(),
                                        comments: Vec::new(),
                                        created_at: chrono::Utc::now(),
                                        usage: None,
                                    });
                                }
                            }
                        }

                        // Forward error to UI (if consumer still alive)
                        if !ui_disconnected {
                            let _ = stream_tx.send(StreamMessage::Error { message: error_msg });
                        }

                        // Error ends the stream
                        stream_error_occurred = true;
                        break;
                    }
                    StreamMessage::ToolCall(mut tool_call) => {
                        // Mark as generated since we got a tool call
                        self.content_generated.write().insert(message_id);

                        // Defense-in-depth dedup: check if this exact tool call was already dispatched
                        {
                            use std::hash::{Hash, Hasher};
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            tool_call.arguments.hash(&mut hasher);
                            let args_hash = hasher.finish();
                            let dedup_key = (
                                tool_call.server_name.clone(),
                                tool_call.tool_name.clone(),
                                args_hash,
                            );
                            if dispatched_tool_calls.contains(&dedup_key) {
                                tracing::warn!(
                                    "stream_manager: Duplicate tool call suppressed: '{}::{}' (args_hash: {})",
                                    tool_call.server_name, tool_call.tool_name, args_hash
                                );
                                continue;
                            }
                            dispatched_tool_calls.insert(dedup_key);
                        }

                        // Attach any accumulated thinking summary to this tool call
                        if tool_call.thought_summary.is_none()
                            && thought_summary_for_this_turn.is_some()
                        {
                            tool_call.thought_summary = thought_summary_for_this_turn.clone();
                        }

                        // Thought Signature Handling (per Gemini API requirements):
                        // 1. If this tool call HAS a signature, capture it for subsequent calls
                        // 2. If this tool call LACKS a signature, use the captured one from earlier in this turn
                        // Gemini only sends thought_signature with the FIRST functionCall in parallel batches.
                        if tool_call.thought_signature.is_some() {
                            // Capture signature from this tool call for potential reuse
                            if thought_signature_for_this_turn.is_none() {
                                thought_signature_for_this_turn =
                                    tool_call.thought_signature.clone();
                                tracing::info!(
                                    "Captured thought_signature from tool call '{}': '{}'",
                                    tool_call.tool_name,
                                    tool_call
                                        .thought_signature
                                        .as_ref()
                                        .map(|s| if s.len() > 30 { &s[..30] } else { s })
                                        .unwrap_or("None")
                                );
                            }
                        } else if thought_signature_for_this_turn.is_some() {
                            // Propagate captured signature to this tool call
                            tool_call.thought_signature = thought_signature_for_this_turn.clone();
                            tracing::info!("Propagated thought_signature to tool call '{}' from earlier in turn", tool_call.tool_name);
                        } else {
                            tracing::warn!("Tool call '{}' has NO thought_signature and none available to propagate!", tool_call.tool_name);
                        }

                        tool_call_count += 1;
                        let tool_call_message_id = {
                            let mut state = self.session_state.write();
                            // Message Upgrading: Only the FIRST tool call upgrades the
                            // placeholder.  Subsequent parallel tool calls always get their
                            // own message so every invocation is visible to the user.
                            if !first_tool_call_placed && (is_first_message || final_text_for_this_turn.is_empty()) {
                                first_tool_call_placed = true;
                                if let Some(msg) =
                                    state.get_message_mut_in_session(&session_id, &message_id)
                                {
                                    msg.content =
                                        crate::components::shared::MessageContent::ToolCall(
                                            tool_call.clone(),
                                        );
                                }
                                message_id
                            } else {
                                let new_id = Uuid::new_v4();
                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                    session.messages.push(crate::components::chat::Message {
                                        id: new_id,
                                        author: "Hobbes".to_string(),
                                        content:
                                            crate::components::shared::MessageContent::ToolCall(
                                                tool_call.clone(),
                                            ),
                                        attachments: Vec::new(),
                                        comments: Vec::new(),
                                        created_at: chrono::Utc::now(),
                                        usage: None,
                                    });
                                }
                                new_id
                            }
                        };
                        // Notify the scroll effect: a tool-call card just appeared.
                        *self.stream_activity.write() += 1;

                        let mcp_manager = self.mcp_manager;
                        let mut session_state = self.session_state;
                        let tool_results_tx_clone = tool_results_tx.clone();
                        let completed_tool_tasks_clone = completed_tool_tasks.clone();
                        let profile_id_inner = profile_id.clone();
                        let session_id_inner = session_id.clone();
                        let _handle = spawn(async move {
                            let args_json: serde_json::Value =
                                serde_json::from_str(&tool_call.arguments)
                                    .unwrap_or(serde_json::Value::Null);
                            let profile_id = profile_id_inner;

                            // Built-in tools run here, before MCP dispatch — they
                            // need SessionState and the skill/permission registries
                            // that McpManager doesn't own. The same dispatcher backs
                            // the approval-resume path in chat.rs; see
                            // components::builtin_tools.
                            if let Some(outcome) =
                                crate::components::builtin_tools::dispatch_builtin_tool(
                                    crate::components::builtin_tools::BuiltinToolCtx {
                                        session_state,
                                        settings: self.settings,
                                        skill_registry: self.skill_registry,
                                        permission_manager: self.permission_manager,
                                        mcp_context: self.mcp_context,
                                        planner: self.planner_state,
                                    },
                                    &tool_call,
                                    &args_json,
                                    &session_id_inner,
                                    profile_id.as_ref(),
                                )
                                .await
                            {
                                let crate::components::builtin_tools::BuiltinOutcome {
                                    status,
                                    response,
                                    persist,
                                } = outcome;

                                {
                                    let mut state = session_state.write();
                                    if let Some(msg) = state.get_message_mut_in_session(&session_id_inner, &tool_call_message_id) {
                                        if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                            tc.status = status;
                                            tc.response = response.clone();
                                        }
                                    }
                                    state.touch_session(&session_id_inner);
                                }
                                // Notify the scroll effect: the card's status/response changed size.
                                *self.stream_activity.write() += 1;
                                if persist {
                                    crate::session::SessionState::save_async(&session_state.read(), None);
                                }

                                let record = crate::components::shared::ToolCallRecord {
                                    call: tool_call.clone(),
                                    result: crate::components::shared::ToolResult {
                                        status,
                                        response,
                                    },
                                    profile_color: {
                                        let settings_read = self.settings.read();
                                        crate::components::shared::resolve_profile_color(
                                            profile_id.as_ref(),
                                            &settings_read,
                                        )
                                    },
                                };
                                let _ = tool_results_tx_clone.send(record);
                                completed_tool_tasks_clone
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                return;
                            }

                            let result_receiver = mcp_manager
                                .read()
                                .use_mcp_tool(
                                    &tool_call.server_name,
                                    &tool_call.tool_name,
                                    args_json,
                                    false,
                                    profile_id.clone(),
                                )
                                .await;

                            let (status, response_str, is_permission_request, image_path) =
                                match result_receiver {
                                    Ok(mut receiver) => {
                                        let mut aggregated_content: Vec<rmcp::model::Content> =
                                            Vec::new();
                                        let mut final_status = ToolCallStatus::Completed;
                                        let mut error_string = None;

                                        while let Some(result) = receiver.recv().await {
                                            match result {
                                                Ok(call_tool_result) => {
                                                    aggregated_content
                                                        .extend(call_tool_result.content);
                                                }
                                                Err(e) => {
                                                    final_status = ToolCallStatus::Error;
                                                    error_string = Some(e);
                                                    break;
                                                }
                                            }
                                        }

                                        if final_status == ToolCallStatus::Error {
                                            let err_msg = error_string.unwrap_or_default();
                                            // Detect "No connected account found" error from Composio
                                            if err_msg.contains("No connected account found") {
                                                tracing::info!("Composio auth required error detected. Initiating connection flow...");
                                                match mcp_manager
                                                    .read()
                                                    .initiate_composio_auth(
                                                        &tool_call.server_name,
                                                        &tool_call.tool_name,
                                                        profile_id.clone(),
                                                    )
                                                    .await
                                                {
                                                    Ok(url) => {
                                                        tracing::info!("Successfully initiated Composio auth flow. URL: {}", url);
                                                        (ToolCallStatus::AuthRequired, url, false, None)
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to initiate Composio auth flow: {}", e);
                                                        (final_status, err_msg, false, None)
                                                    }
                                                }
                                            } else {
                                                (final_status, err_msg, false, None)
                                            }
                                        } else {
                                            // Check for auth requirement
                                            let mut auth_url = None;
                                            for content in &aggregated_content {
                                                let json_content = serde_json::to_value(content)
                                                    .unwrap_or(serde_json::Value::Null);
                                                if let Some(text) = json_content
                                                    .get("text")
                                                    .and_then(|t| t.as_str())
                                                {
                                                    if text.contains("Authentication required")
                                                        && text.contains("connect your account")
                                                    {
                                                        if let Some(start) = text.find("http") {
                                                            auth_url = Some(
                                                                text[start..].trim().to_string(),
                                                            );
                                                        }
                                                    }
                                                }
                                            }

                                            if let Some(url) = auth_url {
                                                (ToolCallStatus::AuthRequired, url, false, None)
                                            } else {
                                                // ── Image-aware content extraction ───────────────────────────
                                                // Scan aggregated_content for image blobs. Save them to disk
                                                // (max 768px tall) and build a text-only response string.
                                                // The file path is stored in tc.cached_image_path so
                                                // prompt_builder can inject it as ContentBlock::Image on the
                                                // next continuation turn.
                                                let mut text_parts: Vec<String> = Vec::new();
                                                let mut captured_image_path: Option<String> = None;

                                                for content in &aggregated_content {
                                                    let json_val = serde_json::to_value(content)
                                                        .unwrap_or(serde_json::Value::Null);

                                                    // MCP image content: {"type":"image","data":"<b64>","mimeType":"image/png"}
                                                    // Also handle nested {"blob":"<b64>"} from EmbeddedResource.
                                                    let b64_candidate = json_val.get("data")
                                                        .or_else(|| json_val.get("blob"))
                                                        .and_then(|v| v.as_str());

                                                    if let Some(b64) = b64_candidate {
                                                        let mime = json_val.get("mimeType")
                                                            .or_else(|| json_val.get("mime_type"))
                                                            .and_then(|m| m.as_str())
                                                            .unwrap_or("image/png");
                                                        // Only save actual image types, not arbitrary blobs
                                                        if mime.starts_with("image/") {
                                                            if let Some(path) = save_tool_image(b64, mime).await {
                                                                let path_str = format!("file://{}", path.display());
                                                                // Keep only the most recent image per tool loop
                                                                captured_image_path = Some(path_str.clone());
                                                                let file_name = path.file_name()
                                                                    .unwrap_or_default()
                                                                    .to_string_lossy();
                                                                text_parts.push(format!(
                                                                    "[Screenshot captured — cached at {}]",
                                                                    file_name
                                                                ));
                                                                tracing::info!(
                                                                    "Tool image cached: {} ({} bytes b64)",
                                                                    path.display(), b64.len()
                                                                );
                                                            } else {
                                                                tracing::warn!("Failed to cache tool image");
                                                                text_parts.push("[Screenshot captured but failed to save]".to_string());
                                                            }
                                                            continue; // don't also serialize as text
                                                        }
                                                    }

                                                    // Text content
                                                    if let Some(text) = json_val.get("text").and_then(|t| t.as_str()) {
                                                        text_parts.push(text.to_string());
                                                    } else {
                                                        // Fallback: compact JSON for non-image, non-text blobs
                                                        let s = serde_json::to_string(&json_val).unwrap_or_default();
                                                        if !s.is_empty() && s != "null" {
                                                            text_parts.push(s);
                                                        }
                                                    }
                                                }

                                                let text_response = if text_parts.is_empty() {
                                                    "[Tool returned no text output]".to_string()
                                                } else {
                                                    text_parts.join("\n")
                                                };

                                                (final_status, text_response, false, captured_image_path)
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Check if this error is actually a serialized ToolCall indicating a permission request
                                        if let Ok(_tc) =
                                            serde_json::from_str::<
                                                crate::components::shared::ToolCall,
                                            >(&e)
                                        {
                                            (ToolCallStatus::Error, e, true, None)
                                        } else {
                                            (ToolCallStatus::Error, e, false, None)
                                        }
                                    }
                                };

                            let mut state = session_state.write();

                            if let Some(msg) = state.get_message_mut_in_session(
                                &session_id_inner,
                                &tool_call_message_id,
                            ) {
                                if is_permission_request {
                                    if let Ok(tc) =
                                        serde_json::from_str::<crate::components::shared::ToolCall>(
                                            &response_str,
                                        )
                                    {
                                        msg.content = crate::components::shared::MessageContent::PermissionRequest(tc);
                                    }
                                } else if let crate::components::shared::MessageContent::ToolCall(
                                    tc,
                                ) = &mut msg.content
                                {
                                    tc.status = status;
                                    tc.response = response_str.clone();
                                    // Store image path for prompt_builder vision injection
                                    if image_path.is_some() {
                                        tc.cached_image_path = image_path.clone();
                                    }
                                }
                            }
                            drop(state);
                            // Notify the scroll effect: the card's status/response changed size
                            // (or it became a PermissionRequest prompt).
                            *self.stream_activity.write() += 1;

                            if !is_permission_request {
                                let record = crate::components::shared::ToolCallRecord {
                                    call: tool_call.clone(),
                                    result: crate::components::shared::ToolResult {
                                        status,
                                        response: response_str,
                                    },
                                    profile_color: {
                                        let settings_read = self.settings.read();
                                        crate::components::shared::resolve_profile_color(
                                            profile_id.as_ref(),
                                            &settings_read,
                                        )
                                    },
                                };
                                let _ = tool_results_tx_clone.send(record);
                            }
                            // Signal completion regardless of permission status
                            completed_tool_tasks_clone
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        });
                        is_first_message = false;
                    }
                    StreamMessage::Usage(usage_data) => {
                        // Update message with usage data
                        let mut state = self.session_state.write();
                        if let Some(msg) =
                            state.get_message_mut_in_session(&session_id, &message_id)
                        {
                            msg.usage = Some(usage_data.clone());
                        }

                        // NOTE: accumulated_cost/accumulated_tokens are managed solely by
                        // Session::delete_message_and_after() now — they represent costs
                        // from *deleted* messages.  total_cost()/total_tokens() already
                        // includes them, so we must NOT re-derive them here (that would
                        // create a feedback loop where accumulators grow each SSE chunk).
                        if let Some(session) = state.sessions.get_mut(&session_id) {
                            session.accumulated_turns = session
                                .messages
                                .iter()
                                .filter(|m| m.usage.is_some())
                                .count()
                                as i32;
                        }

                        // Forward usage to UI
                        if stream_tx.send(StreamMessage::Usage(usage_data)).is_err() {
                            tracing::warn!("Failed to forward usage data to stream");
                        }
                    }
                }
            }

            if !final_text_for_this_turn.trim().is_empty() {
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
                    if let crate::components::shared::MessageContent::Text {
                        content,
                        thought_signature,
                        thought_summary,
                    } = &mut msg.content
                    {
                        *content = final_text_for_this_turn.clone();
                        *thought_signature = thought_signature_for_this_turn.clone();
                        *thought_summary = thought_summary_for_this_turn.clone();
                    }
                }
            } else if tool_call_count == 0 && thought_summary_for_this_turn.is_some() {
                // No meaningful text content, no tool calls, but thinking WAS produced.
                // This happens with vLLM --enable-reasoning: the model puts its entire
                // analysis in the `reasoning` field and only emits whitespace as `content`.
                // Promote the thinking content to be the visible response text so the
                // user sees the analysis directly instead of a hidden "Thinking Process."
                let promoted_text = thought_summary_for_this_turn.clone().unwrap_or_default();
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
                    if let crate::components::shared::MessageContent::Text {
                        content,
                        thought_signature,
                        thought_summary,
                    } = &mut msg.content
                    {
                        *content = promoted_text.clone();
                        *thought_signature = thought_signature_for_this_turn.clone();
                        // Clear thought_summary since we promoted it to content —
                        // no need to show it twice under "Thinking Process"
                        *thought_summary = None;
                    }
                }
                // Also forward to UI so the bubble renders
                let _ = stream_tx.send(StreamMessage::Text {
                    content: promoted_text,
                    thought_signature: thought_signature_for_this_turn.clone(),
                    // Don't send thought_summary — the content IS the thinking
                    thought_summary: None,
                });
            } else if tool_call_count == 0 {
                // Truly empty response: no text, no thinking, no tool calls.
                // Surface this to the user instead of going blank (loud failure).
                tracing::warn!("Model returned empty response — no text, thinking, or tool calls.");
                let empty_msg = "⚠️ The model returned an empty response. This can happen when the context is too large or the model doesn't understand the instructions. Try sending your message again or switching to a different model.";
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
                    if let crate::components::shared::MessageContent::Text { content, .. } =
                        &mut msg.content
                    {
                        *content = empty_msg.to_string();
                    }
                }
                let _ = stream_tx.send(StreamMessage::Text {
                    content: empty_msg.to_string(),
                    thought_signature: None,
                    thought_summary: None,
                });
            }

            // Wait for all tool execution tasks to complete before proceeding to collect results.
            // This prevents a race condition where the receiver loop closes before all tools are finished.
            while completed_tool_tasks.load(std::sync::atomic::Ordering::SeqCst) < tool_call_count {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }

            drop(tool_results_tx);
            let mut collected_records = Vec::new();
            while let Some(record) = tool_results_rx.recv().await {
                collected_records.push(record);
            }

            if tool_call_count > 0 {
                let permission_requests_detected = collected_records.len() < tool_call_count;
                if permission_requests_detected {
                    tracing::info!(
                        "Permission requests detected: {} tool calls dispatched, {} results received. Pausing for user approval.",
                        tool_call_count,
                        collected_records.len()
                    );
                    // Don't trigger continuation; user needs to approve the permission request(s).
                    // Save and clean up.
                    if !collected_records.is_empty() {
                        self.session_state
                            .write()
                            .tool_call_history
                            .extend(collected_records.clone());
                    }
                    self.active_stream_handles.write().remove(&message_id);
                    self.content_generated.write().remove(&message_id); // Cleanup
                    self.stream_session_map.write().remove(&message_id);
                    crate::session::SessionState::save_async(&self.session_state.read(), None);
                    on_complete();
                    self.streaming_sessions.write().remove(&session_id);
                    *self.stream_activity.write() += 1; // Notify TabBar: stream ended
                    self.scheduler.send(SchedulerSignal::Activity);
                    return; // Wait for user to approve
                }

                self.session_state
                    .write()
                    .tool_call_history
                    .extend(collected_records.clone());
                // Tools were called in this turn. Increment the turn counter and trigger a continuation.
                if let Some(session) = self.session_state.write().sessions.get_mut(&session_id) {
                    session.increment_turn_count();
                }

                // CRITICAL FIX: Remove the active stream handle for THIS message before triggering the continuation.
                // The continuation will spawn a NEW task with a NEW message ID.
                // If we don't remove this one, is_any_generating() will remain true forever because this task never "completes" normally.
                self.active_stream_handles.write().remove(&message_id);
                self.content_generated.write().remove(&message_id); // Cleanup
                self.stream_session_map.write().remove(&message_id);

                crate::session::SessionState::save_async(&self.session_state.read(), None);

                // Clean up the current stream state before triggering the next one
                on_complete();
                self.streaming_sessions.write().remove(&session_id);
                *self.stream_activity.write() += 1; // Notify TabBar: stream ended

                // Proactive summarization: when messages approach the history limit,
                // fire off a background summary before the next continuation turn.
                // Without this, turns chain too fast for the 5s idle timer to fire,
                // causing the model to lose context on long tool loops.
                {
                    let msg_count = self.session_state.read()
                        .sessions.get(&session_id)
                        .map(|s| s.messages.len())
                        .unwrap_or(0);
                    let (tuning, context_tokens, enable_summarization) = {
                        let settings_guard = self.settings.read();
                        let state_guard = self.session_state.read();
                        let session = state_guard.sessions.get(&session_id);
                        let instance = session
                            .and_then(|s| settings_guard.connector_for_session(s))
                            .or_else(|| settings_guard.active_connector());
                        let model = session
                            .map(|s| settings_guard.chat_model_for_session(s))
                            .unwrap_or_else(|| settings_guard.active_chat_model());
                        let (tuning, context_tokens) =
                            settings_guard.tuning_and_window(instance, &model);
                        (tuning, context_tokens, settings_guard.enable_summarization)
                    };
                    // Compute a dynamic threshold: how many messages actually fit
                    // in this model's context budget, then trigger summarization
                    // 4 messages before that limit.
                    // stream_manager doesn't have the built system/tool sizes that
                    // prompt_builder has, so we use a 50% overhead estimate
                    // (system + tools + safety typically consume 30–40%).
                    // The conservative factor means we fire slightly early — correct direction.
                    let effective_limit = if let Some(ctx) = context_tokens {
                        let total_chars = (ctx as f64 * tuning.chars_per_token) as usize;
                        let history_budget = (total_chars as f64 * 0.50) as usize;
                        let avg_msg_chars: usize = 800;
                        let dynamic_max = (history_budget / avg_msg_chars).max(4);
                        tuning.chat_history_length.min(dynamic_max)
                    } else {
                        tuning.chat_history_length
                    };
                    let threshold = effective_limit.saturating_sub(4);
                    tracing::debug!(
                        effective_limit,
                        threshold,
                        "Proactive summarizer: dynamic threshold resolved"
                    );

                    if msg_count >= threshold && msg_count > 0 && enable_summarization {
                        // Overlap guard: skip if a proactive summary is already in-flight for THIS session.
                        if self.summarizing_sessions.read().contains(&session_id) {
                            tracing::debug!(
                                session_id,
                                "Proactive summarization skipped — already in-flight for this session"
                            );
                        } else {
                            tracing::info!(
                                session_id, msg_count, effective_limit, threshold,
                                "Proactive summarization: messages approaching history limit"
                            );
                            let session_snapshot = self.session_state.read()
                                .sessions.get(&session_id).cloned();
                            let settings_snapshot = self.settings.read().clone();
                            if let Some(active_session) = session_snapshot {
                                let connector = self.connector_for_session(&session_id);
                                let prev_summary = serde_json::to_string(
                                    &active_session.active_context.conversation_summary
                                ).unwrap_or_else(|_| "{}".to_string());

                                // Format last 5 messages for the summarizer
                                let messages_text: Vec<String> = active_session.messages.iter()
                                    .rev().take(5).rev()
                                    .map(|m| format!("{}: {}", m.author, m.content.display_summary()))
                                    .collect();

                                let active_profile_name = settings_snapshot
                                    .active_composio_profile.as_deref()
                                    .and_then(|id| settings_snapshot.profile_name_for_id(id))
                                    .unwrap_or("None")
                                    .to_string();
                                let system_note = format!(
                                    "[System: Active Profile '{}']", active_profile_name
                                );
                                let recent = std::iter::once(system_note)
                                    .chain(messages_text)
                                    .collect::<Vec<_>>()
                                    .join("\n");

                                if !recent.is_empty() {
                                    // Mark in-flight before spawning
                                    self.summarizing_sessions.write().insert(session_id.clone());
                                    let mut summarizing_sessions = self.summarizing_sessions;
                                    let session_id_clone = session_id.clone();
                                    
                                    // Timeout safety net: blindly clear the flag after 30 seconds
                                    let sid = session_id.clone();
                                    dioxus::prelude::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                                        if summarizing_sessions.peek().contains(&sid) {
                                            tracing::warn!(session_id = %sid, "Proactive summarization timed out, forcibly resetting flag.");
                                            summarizing_sessions.write().remove(&sid);
                                        }
                                    });

                                    // Fire-and-forget: don't block continuation on LLM call.
                                    let mut session_state = self.session_state;
                                    dioxus::prelude::spawn(async move {
                                        match connector.summarize_conversation(prev_summary, recent).await {
                                            Ok(summary_json) => {
                                                if let Ok(summary) = serde_json::from_value::<crate::session::ConversationSummary>(summary_json) {
                                                    tracing::info!(session_id = %session_id_clone, "Proactive summary updated (background)");
                                                    let mut state = session_state.write();
                                                    if let Some(session) = state.sessions.get_mut(&session_id_clone) {
                                                        session.active_context.conversation_summary = summary;
                                                    }
                                                }
                                            }
                                            Err(e) => tracing::warn!(session_id = %session_id_clone, "Proactive summarization failed: {}", e),
                                        }
                                        summarizing_sessions.write().remove(&session_id_clone);
                                    });
                                }
                            }
                        }
                    }
                }

                self.scheduler.send(SchedulerSignal::Activity);

                // Check turn limit BEFORE triggering continuation.
                // Without this gate the AI continuation loop runs indefinitely.
                let turn_count = self.session_state.read().sessions.get(&session_id)
                    .map(|s| s.current_ai_turn_count).unwrap_or(0);
                if self.permission_manager.read().is_turn_limit_reached_for(turn_count) {
                    let max_turns = self.settings.read().permission_settings.max_ai_turns;
                    tracing::info!(session_id, "Turn limit reached ({} turns). Halting continuation.", max_turns);
                    let warning_msg = format!(
                        "Pardon, I have reached the 'Max Turn Limit' currently set to {} in settings and need permission to continue.",
                        max_turns
                    );
                    let mut state = self.session_state.write();
                    if let Some(session) = state.sessions.get_mut(&session_id) {
                        session.messages.push(crate::components::chat::Message {
                            id: Uuid::new_v4(),
                            author: "Hobbes".to_string(),
                            content: crate::components::shared::MessageContent::Text {
                                content: warning_msg,
                                thought_signature: None,
                                thought_summary: None,
                            },
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        });
                    }
                    return; // Stop — user must send a message to reset and continue.
                }

                self.continuation_controller.write().trigger_continuation(session_id.clone());
                return; // End this stream task. The continuation will start a new one.
            }

            // If we reach here, it means tool_call_count was 0.
            // Before finalizing the turn, check for watch word matches
            // that indicate the model stalled mid-response (OpenAI-compat only).
            if self.check_watch_word_recovery(&session_id, &final_text_for_this_turn, &message_id).await {
                return; // Watch word recovery triggered continuation — end this stream task.
            }

            // Stream error auto-recovery: if the stream died with a provider error
            // (e.g., "error decoding response body") and auto-recovery is enabled,
            // retry the continuation instead of leaving the user stranded.
            if stream_error_occurred {
                if let Some(recovery_config) = self.watch_word_config(&session_id) {
                    let max_recoveries = recovery_config.max_watch_word_recoveries;

                    let recovery_count = self.session_state.read().sessions.get(&session_id)
                        .map(|s| s.watch_word_recovery_count).unwrap_or(0);

                    if recovery_count < max_recoveries {
                        tracing::info!(
                            session_id,
                            recovery = recovery_count + 1,
                            max = max_recoveries,
                            "Stream error detected. Triggering auto-recovery retry."
                        );

                        let recovery_text =
                            "[System: Auto-recovery triggered — the previous response was interrupted by a stream error]\n\
                             The connection to the model was interrupted. Please continue where you left off and complete the task.".to_string();

                        {
                            let mut state = self.session_state.write();
                            if let Some(session) = state.sessions.get_mut(&session_id) {
                                session.watch_word_recovery_count += 1;
                                session.messages.push(crate::components::chat::Message {
                                    id: Uuid::new_v4(),
                                    author: "User".to_string(),
                                    content: crate::components::shared::MessageContent::Text {
                                        content: recovery_text,
                                        thought_signature: None,
                                        thought_summary: None,
                                    },
                                    attachments: Vec::new(),
                                    comments: Vec::new(),
                                    created_at: chrono::Utc::now(),
                                    usage: None,
                                });
                                session.increment_turn_count();
                            }
                        }

                        // Check turn limit before triggering continuation
                        let turn_count = self.session_state.read().sessions.get(&session_id)
                            .map(|s| s.current_ai_turn_count).unwrap_or(0);
                        if self.permission_manager.read().is_turn_limit_reached_for(turn_count) {
                            tracing::info!(session_id, "Stream error recovery halted: turn limit reached.");
                        } else {
                            // Clean up current stream state before triggering retry.
                            self.active_stream_handles.write().remove(&message_id);
                            self.content_generated.write().remove(&message_id);
                            self.stream_session_map.write().remove(&message_id);
                            crate::session::SessionState::save_async(&self.session_state.read(), None);
                            on_complete();
                            self.streaming_sessions.write().remove(&session_id);
                            *self.stream_activity.write() += 1;

                            self.continuation_controller.write().trigger_continuation(session_id.clone());
                            return; // End this stream task. Continuation will retry.
                        }
                    } else {
                        tracing::info!(
                            session_id,
                            max = max_recoveries,
                            "Stream error detected but recovery limit reached. Halting."
                        );
                    }
                }
            }

            // The turn is truly over — no tool calls, no watch word recovery, no error recovery.
            tracing::info!(message_id = %message_id, "LLM stream COMPLETE.");

            // Record usage to the standalone usage log (audit ledger).
            // This captures the final usage data for the turn, avoiding the
            // double-counting that would occur if we recorded on every SSE chunk.
            {
                let state = self.session_state.read();
                if let Some(msg) = state.sessions.get(&session_id)
                    .and_then(|s| s.messages.iter().find(|m| m.id == message_id))
                {
                    if let Some(usage) = &msg.usage {
                        let model = self.effective_model(&session_id);
                        self.usage_log.write().record(crate::usage_log::UsageLogEntry {
                            timestamp: chrono::Utc::now(),
                            session_id: session_id.clone(),
                            model,
                            prompt_tokens: usage.prompt_tokens,
                            completion_tokens: usage.completion_tokens,
                            total_tokens: usage.total_tokens,
                            thoughts_tokens: usage.thoughts_tokens,
                            cached_content_tokens: usage.cached_content_tokens,
                            cost: usage.cost,
                        });
                    }
                }
            }

            {
                let mut state = self.session_state.write();
                state.touch_session(&session_id);
            }
            // Save after releasing the write guard — save_async borrows via read()
            crate::session::SessionState::save_async(
                &self.session_state.read(),
                Some(self.save_error_signal),
            );

            let settings = self.settings.read().clone();
            let summarizer = self.tool_call_summarizer.read();
            summarizer
                .summarize_and_cleanup(&mut self.session_state.write(), &settings, &session_id)
                .await;
            drop(summarizer);

            // Proactively summarize this turn's large tool results in the
            // background so they can be substituted (not paginated) once they
            // become historical. Fire-and-forget; never blocks turn completion.
            self.spawn_tool_result_summaries(&session_id, &settings);

            on_complete();
            self.streaming_sessions.write().remove(&session_id);
            *self.stream_activity.write() += 1; // Notify TabBar: stream ended
            self.scheduler.send(SchedulerSignal::Activity);
            tracing::info!(message_id = %message_id, "Completion signal SENT.");

            // Remove the handle from the map upon completion
            self.active_stream_handles.write().remove(&message_id);
            self.content_generated.write().remove(&message_id); // Cleanup
            self.stream_session_map.write().remove(&message_id);
            tracing::info!(message_id = %message_id, "Active stream handle removed.");
        });

        // Store the handle so we can abort it if needed
        self.active_stream_handles
            .write()
            .insert(message_id, master_task_handle);
    }

    /// Spawn a fire-and-forget task that generates knowledge-preserving summaries
    /// for the just-completed turn's large tool results. Once those results become
    /// historical, Pass 2 substitutes the summary instead of paginating — keeping
    /// the facts in context. The size threshold is the provider's historical
    /// per-result budget, so on a large window (where results stay full anyway)
    /// few or no summaries are generated and no tokens are wasted.
    fn spawn_tool_result_summaries(
        &self,
        session_id: &str,
        settings: &crate::settings::Settings,
    ) {
        let pending = {
            let state = self.session_state.read();
            let Some(session) = state.sessions.get(session_id) else {
                return;
            };
            let model = settings.chat_model_for_session(session);
            let (tuning, context_tokens) =
                settings.tuning_and_window(settings.connector_for_session(session), &model);
            // A result smaller than its eventual historical budget will always
            // fit and never needs a summary; only summarize ones that would be
            // compressed later.
            let threshold = match context_tokens {
                Some(tokens) => {
                    let total = tokens as f64
                        * tuning.chars_per_token
                        * (1.0 - tuning.context_safety_margin);
                    (total * (1.0 - tuning.active_result_budget_ratio)) as usize
                }
                None => tuning.max_tool_output_length,
            };
            crate::services::tool_result_summarizer::collect_pending(session, threshold)
        };

        if pending.is_empty() {
            return;
        }

        let connector = self.connector_for_session(session_id);
        let mut session_state = self.session_state;
        let save_error_signal = self.save_error_signal;
        let session_id = session_id.to_string();
        dioxus::prelude::spawn(async move {
            let mut updated = false;
            for item in pending {
                match connector
                    .summarize_tool_result(&item.tool_name, &item.response)
                    .await
                {
                    Ok(summary) => {
                        let mut state = session_state.write();
                        if let Some(session) = state.sessions.get_mut(&session_id) {
                            if crate::services::tool_result_summarizer::apply_summary(
                                session,
                                item.message_id,
                                summary,
                            ) {
                                updated = true;
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        tool = %item.tool_name,
                        "Tool result summarization failed: {}", e
                    ),
                }
            }
            if updated {
                tracing::info!(
                    session_id = %session_id,
                    "Tool result summaries updated (background)"
                );
                crate::session::SessionState::save_async(
                    &session_state.read(),
                    Some(save_error_signal),
                );
            }
        });
    }

    /// Check for watch word matches in the model's final text output and trigger
    /// auto-recovery if appropriate. Returns `true` when recovery was triggered
    /// and the caller should `return` (ending the current stream task).
    ///
    /// Watch words are patterns (e.g. "Let me check that...") that indicate a local
    /// LLM stalled mid-response. This is gated by:
    /// - OpenAI-compat provider only
    /// - `watch_words_enabled` setting
    /// - Non-empty response text
    /// - Response length below `watch_word_max_response_chars` (long responses are legit)
    /// - Per-session recovery count below `max_watch_word_recoveries`
    async fn check_watch_word_recovery(
        &mut self,
        session_id: &str,
        final_text: &str,
        message_id: &Uuid,
    ) -> bool {
        let watch_config = match self.watch_word_config(session_id) {
            Some(c) if !final_text.is_empty() => c,
            _ => return false,
        };

        let max_recoveries = watch_config.max_watch_word_recoveries;
        let max_response_chars = watch_config.watch_word_max_response_chars;
        let watch_words = watch_config.watch_words;

        // Skip watch word detection on long responses — they're almost
        // certainly complete even if they contain a watch word pattern.
        let response_len = final_text.trim().len();
        if response_len > max_response_chars {
            tracing::debug!(
                session_id,
                response_len,
                max_response_chars,
                "Skipping watch word check: response exceeds max_response_chars threshold."
            );
            return false;
        }

        let recovery_count = self.session_state.read().sessions.get(session_id)
            .map(|s| s.watch_word_recovery_count).unwrap_or(0);

        if recovery_count >= max_recoveries {
            if watch_words.iter().any(|ww| ww.matches(final_text)) {
                tracing::info!(
                    session_id,
                    max = max_recoveries,
                    "Watch word detected but recovery limit reached. Halting."
                );
            }
            return false;
        }

        let matched = match watch_words.iter().find(|ww| ww.matches(final_text)) {
            Some(m) => m,
            None => return false,
        };

        let pattern = matched.pattern.clone();
        let instruction = matched.effective_instruction();
        tracing::info!(
            session_id,
            pattern = %pattern,
            recovery = recovery_count + 1,
            max = max_recoveries,
            "Watch word detected in AI output. Triggering auto-recovery."
        );

        // Build recovery message with context from the stalled output.
        // Truncate to avoid bloating the prompt (max 300 chars of stalled text).
        let stalled_preview = if final_text.len() > 300 {
            let boundary = crate::str_utils::floor_char_boundary(final_text, 300);
            format!("{}...", &final_text[..boundary])
        } else {
            final_text.to_string()
        };
        let recovery_text = format!(
            "[System: Auto-recovery triggered — watch word \"{}\" detected in your output]\n\
             Your previous response ended abruptly after:\n\
             \"{}\"\n\n\
             {}",
            pattern,
            stalled_preview.trim(),
            instruction,
        );

        // Inject as a User message so the model sees clear direction on its next turn.
        {
            let mut state = self.session_state.write();
            if let Some(session) = state.sessions.get_mut(session_id) {
                session.watch_word_recovery_count += 1;
                session.messages.push(crate::components::chat::Message {
                    id: Uuid::new_v4(),
                    author: "User".to_string(),
                    content: crate::components::shared::MessageContent::Text {
                        content: recovery_text,
                        thought_signature: None,
                        thought_summary: None,
                    },
                    attachments: Vec::new(),
                    comments: Vec::new(),
                    created_at: chrono::Utc::now(),
                    usage: None,
                });
                session.increment_turn_count();
            }
        }

        // Check turn limit before triggering continuation
        let turn_count = self.session_state.read().sessions.get(session_id)
            .map(|s| s.current_ai_turn_count).unwrap_or(0);
        if self.permission_manager.read().is_turn_limit_reached_for(turn_count) {
            tracing::info!(session_id, "Watch word recovery halted: turn limit reached.");
            return false;
        }

        // Clean up current stream state before triggering the next one.
        self.active_stream_handles.write().remove(message_id);
        self.content_generated.write().remove(message_id);
        self.stream_session_map.write().remove(message_id);
        crate::session::SessionState::save_async(&self.session_state.read(), None);
        self.streaming_sessions.write().remove(session_id);
        *self.stream_activity.write() += 1;

        self.continuation_controller.write().trigger_continuation(session_id.to_string());
        true
    }

    pub fn cancel_stream(mut self, message_id: &Uuid, session_id: &str) {
        tracing::info!(message_id = %message_id, session_id = %session_id, "Attempting to cancel stream.");

        // 1. Remove and abort the task handle
        if let Some(handle) = self.active_stream_handles.write().remove(message_id) {
            handle.cancel();
            tracing::info!(message_id = %message_id, "Aborted stream task handle.");
        }

        // Cleanup generated state
        self.content_generated.write().remove(message_id);

        // 2. Preserve the AI's partial response — do NOT delete the message.
        //    The model may have already produced useful text/thinking content
        //    that the user wants to keep. Only remove orphaned Running tool
        //    call messages (they have no results and break the next prompt).
        {
            let mut state = self.session_state.write();

            // Check if the AI message has any content worth preserving.
            let has_content = state.sessions.get(session_id)
                .and_then(|s| s.messages.iter().find(|m| m.id == *message_id))
                .map(|msg| {
                    if let crate::components::shared::MessageContent::Text { content, .. } = &msg.content {
                        !content.trim().is_empty()
                    } else {
                        false
                    }
                })
                .unwrap_or(false);

            if !has_content {
                // No content was generated — remove the empty placeholder.
                state.remove_message_in_session(session_id, message_id);
            }

            // Trim trailing orphaned Running tool calls
            if let Some(session) = state.sessions.get_mut(session_id) {
                let mut removed_count = 0;
                while let Some(last) = session.messages.last() {
                    if let crate::components::shared::MessageContent::ToolCall(tc) = &last.content {
                        if tc.status == crate::components::shared::ToolCallStatus::Running {
                            let orphan_id = last.id;
                            session.messages.pop();
                            removed_count += 1;
                            tracing::info!(
                                message_id = %orphan_id,
                                "Removed orphaned Running tool call after cancel."
                            );
                            continue;
                        }
                    }
                    break;
                }
                if removed_count > 0 {
                    tracing::info!(
                        "Removed {} orphaned Running tool call(s) after stream cancellation.",
                        removed_count
                    );
                }
            }

            // Clear stale tool_call_history from the cancelled turn so the
            // next prompt doesn't include CRITICAL RECOVERY INSTRUCTION or
            // stale tool context.
            state.tool_call_history.clear();
        }

        // 3. Remove the stream receiver
        if self.stream_receivers.write().remove(message_id).is_some() {
            tracing::info!(message_id = %message_id, "Removed stream receiver.");
        } else {
            tracing::warn!(message_id = %message_id, "No stream receiver found to remove.");
        }

        self.streaming_sessions.write().remove(session_id);
        *self.stream_activity.write() += 1; // Notify TabBar: stream cancelled
        self.stream_session_map.write().remove(message_id);
        tracing::info!(message_id = %message_id, "Stream cancellation process complete.");
    }

    pub fn take_stream(mut self, message_id: &Uuid) -> Option<UnboundedReceiver<StreamMessage>> {
        let result = self.stream_receivers.write().remove(message_id);
        if result.is_some() {
            *self.stream_activity.write() += 1;
        }
        result
    }
}

#[derive(Props, PartialEq, Clone)]
pub struct StreamManagerProps {
    children: Element,
}

#[component]
pub fn StreamManager(props: StreamManagerProps) -> Element {
    let session_state = consume_context::<Signal<SessionState>>();
    let mcp_manager = consume_context::<Signal<crate::mcp::manager::McpManager>>();
    let settings = consume_context::<Signal<Settings>>();
    let continuation_controller =
        use_context_provider(|| Signal::new(ContinuationController::new()));
    let scheduler = use_context::<Coroutine<SchedulerSignal>>();
    let permission_manager =
        consume_context::<Signal<crate::context::permissions::PermissionManager>>();
    let usage_log = consume_context::<Signal<crate::usage_log::UsageLog>>();
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        active_stream_handles: Signal::new(HashMap::new()),
        llm_connector: consume_context::<Signal<Arc<dyn crate::llm::LlmConnector>>>(),
        session_state,
        mcp_manager,
        tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
        settings,
        continuation_controller,
        scheduler,
        permission_manager,
        skill_registry: consume_context::<Signal<crate::skills::SkillRegistry>>(),
        mcp_context: consume_context::<Signal<crate::mcp::manager::McpContext>>(),
        planner_state: consume_context::<Signal<crate::todo::PlannerState>>(),
        stream_activity: Signal::new(0),
        streaming_sessions: Signal::new(HashSet::new()),
        stream_session_map: Signal::new(HashMap::new()),
        content_generated: Signal::new(HashSet::new()),
        save_error_signal: consume_context::<crate::components::shared::SaveErrorContext>().0,
        usage_log,
        summarizing_sessions: Signal::new(HashSet::new()),
    });

    // Provide the context to children.
    use_context_provider(|| context);
    rsx! { {props.children} }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::permissions::PermissionManager;
    use crate::mcp::manager::McpManager;
    use crate::secret_manager::SecretManager;
    use crate::settings::Settings;
    use dioxus_signals::Signal;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_stream_registration_and_deregistration() {
        let mut dom = VirtualDom::new(|| {
            let session_state = use_context_provider(|| Signal::new(SessionState::new()));
            let settings = use_context_provider(|| Signal::new(Settings::default()));
            let permission_manager =
                use_context_provider(|| Signal::new(PermissionManager::new(settings)));
            let secret_manager = use_context_provider(|| Signal::new(SecretManager::new()));
            let mcp_manager = use_context_provider(|| {
                Signal::new(McpManager::new(
                    PathBuf::new(),
                    permission_manager,
                    secret_manager,
                    settings,
                ))
            });
            let continuation_controller =
                use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| {
                Signal::new(Arc::new(crate::llm::GeminiConnector::new(
                    settings.read().gemini_config.clone(),
                )) as Arc<dyn crate::llm::LlmConnector>)
            });
            let scheduler = use_coroutine(|_| async {});
            let mut stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                active_stream_handles: Signal::new(HashMap::new()),
                llm_connector,
                session_state,
                mcp_manager,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
                continuation_controller,
                scheduler,
                permission_manager,
                skill_registry: Signal::new(crate::skills::SkillRegistry::new()),
                mcp_context: Signal::new(crate::mcp::manager::McpContext {
                    servers: Vec::new(),
                    connected_toolkit_slugs: Vec::new(),
                }),
                planner_state: Signal::new(crate::todo::PlannerState::default()),
                stream_activity: Signal::new(0),
                streaming_sessions: Signal::new(HashSet::new()),
                stream_session_map: Signal::new(HashMap::new()),
                content_generated: Signal::new(HashSet::new()),
                save_error_signal: Signal::new(None),
                usage_log: Signal::new(crate::usage_log::UsageLog::default()),
                summarizing_sessions: Signal::new(HashSet::new()),
            });

            let message_id = Uuid::new_v4();

            // Initially, no stream should be registered
            assert!(!stream_manager.is_streaming(&message_id));

            // Register a stream
            let (_tx, rx) = mpsc::unbounded_channel();
            stream_manager
                .stream_receivers
                .write()
                .insert(message_id, rx);

            // Now a stream should be registered
            assert!(stream_manager.is_streaming(&message_id));

            // Take the stream
            let taken_rx = stream_manager.take_stream(&message_id);
            assert!(taken_rx.is_some());

            // After taking, the stream should no longer be registered
            assert!(!stream_manager.is_streaming(&message_id));

            rsx! { div {} }
        });

        dom.rebuild_in_place();
        dom.wait_for_suspense().await;
    }

    #[tokio::test]
    async fn test_take_nonexistent_stream() {
        let mut dom = VirtualDom::new(|| {
            let session_state = use_context_provider(|| Signal::new(SessionState::new()));
            let settings = use_context_provider(|| Signal::new(Settings::default()));
            let permission_manager =
                use_context_provider(|| Signal::new(PermissionManager::new(settings)));
            let secret_manager = use_context_provider(|| Signal::new(SecretManager::new()));
            let mcp_manager = use_context_provider(|| {
                Signal::new(McpManager::new(
                    PathBuf::new(),
                    permission_manager,
                    secret_manager,
                    settings,
                ))
            });
            let continuation_controller =
                use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| {
                Signal::new(Arc::new(crate::llm::GeminiConnector::new(
                    settings.read().gemini_config.clone(),
                )) as Arc<dyn crate::llm::LlmConnector>)
            });
            let scheduler = use_coroutine(|_| async {});
            let stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                active_stream_handles: Signal::new(HashMap::new()),
                llm_connector,
                session_state,
                mcp_manager,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
                continuation_controller,
                scheduler,
                permission_manager,
                skill_registry: Signal::new(crate::skills::SkillRegistry::new()),
                mcp_context: Signal::new(crate::mcp::manager::McpContext {
                    servers: Vec::new(),
                    connected_toolkit_slugs: Vec::new(),
                }),
                planner_state: Signal::new(crate::todo::PlannerState::default()),
                stream_activity: Signal::new(0),
                streaming_sessions: Signal::new(HashSet::new()),
                stream_session_map: Signal::new(HashMap::new()),
                content_generated: Signal::new(HashSet::new()),
                save_error_signal: Signal::new(None),
                usage_log: Signal::new(crate::usage_log::UsageLog::default()),
                summarizing_sessions: Signal::new(HashSet::new()),
            });

            let message_id = Uuid::new_v4();

            // Taking a stream that doesn't exist should return None
            let taken_rx = stream_manager.take_stream(&message_id);
            assert!(taken_rx.is_none());

            rsx! { div {} }
        });

        dom.rebuild_in_place();
        dom.wait_for_suspense().await;
    }
}

// ============================================================================
// Image caching helper
// ============================================================================

/// Decode a base64 image blob from an MCP tool result, resize it to at most
/// 768 px tall (preserving aspect ratio), re-encode as JPEG, persist to the
/// same `generated_images/` directory used by `image_client.rs`, and return
/// the saved path.
///
/// Returns `None` on any decode/resize/write failure (non-fatal; the call site
/// will substitute a placeholder text instead).
async fn save_tool_image(data_b64: &str, _mime_type: &str) -> Option<std::path::PathBuf> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .ok()?;

    // Resize on a blocking thread so we don't stall the async executor.
    let resized_bytes = tokio::task::spawn_blocking(move || {
        use image::imageops::FilterType;
        use image::ImageFormat;

        let img = image::load_from_memory(&bytes).ok()?;

        // Cap at 768 px tall; scale width proportionally.
        let resized = if img.height() > 768 {
            img.resize(u32::MAX, 768, FilterType::Lanczos3)
        } else {
            img
        };

        let mut buf = std::io::Cursor::new(Vec::new());
        resized.write_to(&mut buf, ImageFormat::Jpeg).ok()?;
        Some(buf.into_inner())
    })
    .await
    .ok()
    .flatten()?;

    // Build the output path inside the persistent app directory.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis(); // millis to avoid collisions when multiple screenshots arrive in the same second
    let mut dir = dirs::config_dir()?
        .join("com.hobbes.app")
        .join("generated_images");
    let _ = std::fs::create_dir_all(&dir);
    dir.push(format!("hobbes_screenshot_{}.jpg", timestamp));

    tokio::fs::write(&dir, resized_bytes).await.ok()?;
    Some(dir)
}
