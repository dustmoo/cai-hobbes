// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use super::continuation_controller::ContinuationController;
use crate::components::shared::{StreamMessage, ToolCallStatus};
use crate::services::tool_call_summarizer::ToolCallSummarizer;
use crate::session::SessionState;
use crate::settings::Settings;
use dioxus::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;

use crate::processing::summarization_scheduler::SchedulerSignal;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<StreamMessage>>>,
    active_stream_handles: Signal<HashMap<Uuid, Task>>,
    llm_connector: Signal<std::sync::Arc<dyn crate::components::llm::LlmConnector>>,
    session_state: Signal<SessionState>,
    mcp_manager: Signal<crate::mcp::manager::McpManager>,
    tool_call_summarizer: Signal<ToolCallSummarizer>,
    settings: Signal<Settings>,
    continuation_controller: Signal<ContinuationController>,
    scheduler: Coroutine<SchedulerSignal>,
    permission_manager: Signal<crate::context::permissions::PermissionManager>,
    pub stream_activity: Signal<u64>,
    pub is_sending: Signal<bool>,
    pub content_generated: Signal<std::collections::HashSet<Uuid>>,
    save_error_signal: Signal<Option<String>>,
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

    pub fn is_any_generating(self) -> bool {
        !self.active_stream_handles.read().is_empty()
    }

    pub fn start_stream(
        mut self,
        message_id: Uuid,
        session_id: String,
        prompt_data: crate::context::prompt_builder::LlmPrompt,
        on_complete: impl FnOnce() + 'static,
        mcp_context: Option<crate::mcp::manager::McpContext>,
        profile_id: Option<String>,
    ) {
        self.is_sending.set(true);
        tracing::info!(message_id = %message_id, session_id = %session_id, "'start_stream' entered.");
        // Create a channel for the MessageBubble to receive chunks.
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<StreamMessage>();

        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, stream_rx);
        *self.stream_activity.write() += 1;

        // Spawn a master task to manage the LLM call and state updates.
        let master_task_handle = spawn(async move {
            tracing::info!(message_id = %message_id, "Stream master task SPAWNED.");
            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<StreamMessage>();

            let llm_connector = self.llm_connector.read().clone();
            spawn(async move {
                llm_connector
                    .generate_content_stream(prompt_data, llm_tx, mcp_context)
                    .await;
            });

            let mut is_first_message = true;
            let (tool_results_tx, mut tool_results_rx) =
                mpsc::unbounded_channel::<crate::components::shared::ToolCallRecord>();
            let mut tool_call_count = 0;
            let completed_tool_tasks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut final_text_for_this_turn = String::new();
            let mut thought_signature_for_this_turn: Option<String> = None;
            let mut thought_summary_for_this_turn: Option<String> = None;

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
                        let was_empty = final_text_for_this_turn.is_empty();
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
                        if stream_tx
                            .send(StreamMessage::Text {
                                content: content.clone(),
                                thought_signature: thought_signature.clone(),
                                thought_summary: thought_summary.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        self.scheduler.send(SchedulerSignal::Activity);

                        // Critical Fix: Update session state immediately on the first chunk.
                        // This ensures that the parent `MessageList` sees that the message has content
                        // and continues to render the `MessageBubble` even if `is_generating` momentarily flips to false
                        // or if the stream ends abruptly. Solving the "disappearing bubble" regression.
                        // IMPORTANT: We must also flush thought_signature/thought_summary, otherwise when the stream
                        // ends (e.g., UNEXPECTED_TOOL_CALL), the message has no content AND no thoughts => pruned.
                        if was_empty {
                            let mut state = self.session_state.write();
                            if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
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

                        is_first_message = false;
                    }
                    StreamMessage::Error { message: error_msg } => {
                        tracing::error!("LLM stream error: {}", error_msg);

                        // Save error message to session
                        {
                            let mut state = self.session_state.write();
                            if is_first_message {
                                // Update the placeholder message with the error
                                if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
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

                        // Forward error to UI
                        if stream_tx
                            .send(StreamMessage::Error { message: error_msg })
                            .is_err()
                        {
                            break;
                        }

                        // Error ends the stream
                        break;
                    }
                    StreamMessage::ToolCall(mut tool_call) => {
                        // Mark as generated since we got a tool call
                        self.content_generated.write().insert(message_id);

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
                            // Message Upgrading: If this is the "first" content of the turn, OR if we have only
                            // received thinking data so far (final_text is empty), we upgrade the existing
                            // placeholder message to a ToolCall instead of splitting into two bubbles.
                            if is_first_message || final_text_for_this_turn.is_empty() {
                                if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
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

                            let (status, response_str, is_permission_request) =
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
                                                        (ToolCallStatus::AuthRequired, url, false)
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to initiate Composio auth flow: {}", e);
                                                        (final_status, err_msg, false)
                                                    }
                                                }
                                            } else {
                                                (final_status, err_msg, false)
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
                                                (ToolCallStatus::AuthRequired, url, false)
                                            } else {
                                                let final_json =
                                                    serde_json::to_value(aggregated_content)
                                                        .unwrap_or(serde_json::Value::Null);
                                                (
                                                    final_status,
                                                    serde_json::to_string_pretty(&final_json)
                                                        .unwrap_or_default(),
                                                    false,
                                                )
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
                                            (ToolCallStatus::Error, e, true)
                                        } else {
                                            (ToolCallStatus::Error, e, false)
                                        }
                                    }
                                };

                            let mut state = session_state.write();

                            if let Some(msg) = state.get_message_mut_in_session(&session_id_inner, &tool_call_message_id) {
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
                                }
                            }

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
                        if let Some(msg) = state.get_message_mut_in_session(&session_id, &message_id) {
                            msg.usage = Some(usage_data.clone());
                        }

                        // Recalculate session accumulated usage from authoritative message data.
                        // Gemini sends usage_metadata on multiple SSE chunks, so we must NOT
                        // blindly increment — that causes double-counting. Instead, derive
                        // accumulated values from the message-level totals (which are replaced,
                        // not accumulated, on each Usage event).
                        if let Some(session) = state.sessions.get_mut(&session_id) {
                            session.accumulated_cost = session.total_cost();
                            session.accumulated_tokens = session.total_tokens();
                            session.accumulated_turns = session.messages.iter()
                                .filter(|m| m.usage.is_some())
                                .count() as i32;
                        }

                        // Forward usage to UI
                        if stream_tx.send(StreamMessage::Usage(usage_data)).is_err() {
                            tracing::warn!("Failed to forward usage data to stream");
                        }
                    }
                }
            }

            if !final_text_for_this_turn.is_empty() {
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
                    crate::session::SessionState::save_async(&self.session_state.read(), None);
                    on_complete();
                    self.is_sending.set(false);
                    self.scheduler.send(SchedulerSignal::Activity);
                    return; // Wait for user to approve
                }

                self.session_state
                    .write()
                    .tool_call_history
                    .extend(collected_records.clone());
                // Tools were called in this turn. Increment the turn counter and trigger a continuation.
                self.permission_manager.write().increment_turn_count();

                // CRITICAL FIX: Remove the active stream handle for THIS message before triggering the continuation.
                // The continuation will spawn a NEW task with a NEW message ID.
                // If we don't remove this one, is_any_generating() will remain true forever because this task never "completes" normally.
                self.active_stream_handles.write().remove(&message_id);
                self.content_generated.write().remove(&message_id); // Cleanup

                crate::session::SessionState::save_async(&self.session_state.read(), None);

                // Clean up the current stream state before triggering the next one
                on_complete();
                self.is_sending.set(false);
                self.scheduler.send(SchedulerSignal::Activity);

                self.continuation_controller.read().trigger_continuation();
                return; // End this stream task. The continuation will start a new one.
            }

            // If we reach here, it means tool_call_count was 0. The turn is over.

            // This block now runs only when the conversation turn is truly complete.
            tracing::info!(message_id = %message_id, "LLM stream COMPLETE.");
            {
                let mut state = self.session_state.write();
                state.touch_session(&session_id);
            }
            // Save after releasing the write guard — save_async borrows via read()
            crate::session::SessionState::save_async(&self.session_state.read(), Some(self.save_error_signal));

            let settings = self.settings.read().clone();
            let summarizer = self.tool_call_summarizer.read();
            summarizer
                .summarize_and_cleanup(&mut self.session_state.write(), &settings, &session_id)
                .await;
            on_complete();
            self.is_sending.set(false);
            self.scheduler.send(SchedulerSignal::Activity);
            tracing::info!(message_id = %message_id, "Completion signal SENT.");

            // Remove the handle from the map upon completion
            self.active_stream_handles.write().remove(&message_id);
            self.content_generated.write().remove(&message_id); // Cleanup
            tracing::info!(message_id = %message_id, "Active stream handle removed.");
        });

        // Store the handle so we can abort it if needed
        self.active_stream_handles
            .write()
            .insert(message_id, master_task_handle);
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

        // 2. Remove the message from the originating session
        self.session_state.write().remove_message_in_session(session_id, message_id);

        // 3. Remove the stream receiver
        if self.stream_receivers.write().remove(message_id).is_some() {
            tracing::info!(message_id = %message_id, "Removed stream receiver.");
        } else {
            tracing::warn!(message_id = %message_id, "No stream receiver found to remove.");
        }

        self.is_sending.set(false);
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
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        active_stream_handles: Signal::new(HashMap::new()),
        llm_connector: consume_context::<Signal<Arc<dyn crate::components::llm::LlmConnector>>>(),
        session_state,
        mcp_manager,
        tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
        settings,
        continuation_controller,
        scheduler,
        permission_manager,
        stream_activity: Signal::new(0),
        is_sending: Signal::new(false),
        content_generated: Signal::new(std::collections::HashSet::new()),
        save_error_signal: consume_context::<crate::components::shared::SaveErrorContext>().0,
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
                ))
            });
            let continuation_controller =
                use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| {
                Signal::new(Arc::new(crate::components::llm::GeminiConnector::new(
                    settings.read().gemini_config.clone(),
                ))
                    as Arc<dyn crate::components::llm::LlmConnector>)
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
                stream_activity: Signal::new(0),
                is_sending: Signal::new(false),
                content_generated: Signal::new(std::collections::HashSet::new()),
                save_error_signal: Signal::new(None),
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
                ))
            });
            let continuation_controller =
                use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| {
                Signal::new(Arc::new(crate::components::llm::GeminiConnector::new(
                    settings.read().gemini_config.clone(),
                ))
                    as Arc<dyn crate::components::llm::LlmConnector>)
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
                stream_activity: Signal::new(0),
                is_sending: Signal::new(false),
                content_generated: Signal::new(std::collections::HashSet::new()),
                save_error_signal: Signal::new(None),
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
