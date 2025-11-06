use crate::services::tool_call_summarizer::ToolCallSummarizer;
use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;
use crate::session::SessionState;
use crate::components::shared::{StreamMessage, ToolCallStatus};
use crate::services::document_store::DocumentStore;
use std::sync::Arc;
use crate::settings::Settings;
use super::continuation_controller::ContinuationController;

use crate::processing::summarization_scheduler::SchedulerSignal;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<StreamMessage>>>,
    active_stream_handles: Signal<HashMap<Uuid, Task>>,
    llm_connector: Signal<std::sync::Arc<dyn crate::components::llm::LlmConnector>>,
    session_state: Signal<SessionState>,
    mcp_manager: Signal<crate::mcp::manager::McpManager>,
    document_store: Signal<Option<Arc<DocumentStore>>>,
    tool_call_summarizer: Signal<ToolCallSummarizer>,
    settings: Signal<Settings>,
    continuation_controller: Signal<ContinuationController>,
    scheduler: Coroutine<SchedulerSignal>,
    pub stream_activity: Signal<u64>,
    pub is_sending: Signal<bool>,
}

impl StreamManagerContext {
    pub fn is_streaming(self, message_id: &Uuid) -> bool {
        self.stream_receivers.read().contains_key(message_id)
    }

    pub fn start_stream(
        mut self,
        message_id: Uuid,
        prompt_data: crate::context::prompt_builder::LlmPrompt,
        on_complete: impl FnOnce() + 'static,
        mcp_context: Option<crate::mcp::manager::McpContext>,
    ) {
        self.is_sending.set(true);
        tracing::info!(message_id = %message_id, "'start_stream' entered.");
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
                llm_connector.generate_content_stream(prompt_data, llm_tx, mcp_context).await;
            });

            let mut is_first_message = true;
            let (tool_results_tx, mut tool_results_rx) = mpsc::unbounded_channel::<crate::components::shared::ToolCallRecord>();
            let mut tool_call_count = 0;
            let mut final_text_for_this_turn = String::new();

            while let Some(message) = llm_rx.recv().await {
                match message {
                    StreamMessage::Text(chunk) => {
                        final_text_for_this_turn.push_str(&chunk);
                        if stream_tx.send(StreamMessage::Text(chunk)).is_err() {
                            break;
                        }
                        is_first_message = false;
                    }
                    StreamMessage::ToolCall(tool_call) => {
                        tool_call_count += 1;
                        let tool_call_message_id = {
                            let mut state = self.session_state.write();
                            if is_first_message {
                                if let Some(msg) = state.get_message_mut(&message_id) {
                                    msg.content = crate::components::shared::MessageContent::ToolCall(tool_call.clone());
                                }
                                message_id
                            } else {
                                let new_id = Uuid::new_v4();
                                if let Some(session) = state.get_active_session_mut() {
                                    session.messages.push(crate::components::chat::Message {
                                        id: new_id,
                                        author: "Hobbes".to_string(),
                                        content: crate::components::shared::MessageContent::ToolCall(tool_call.clone()),
                                        attachments: Vec::new(),
                                    });
                                }
                                new_id
                            }
                        };

                        let mcp_manager = self.mcp_manager;
                        let mut session_state = self.session_state;
                        let document_store = self.document_store;
                        let tool_results_tx_clone = tool_results_tx.clone();
                        spawn(async move {
                            let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null);
                            let result_receiver = mcp_manager.read().use_mcp_tool(&tool_call.server_name, &tool_call.tool_name, args_json, false).await;

                            let (status, response_str) = match result_receiver {
                                Ok(mut receiver) => {
                                    let mut aggregated_content: Vec<rmcp::model::Content> = Vec::new();
                                    let mut final_status = ToolCallStatus::Completed;
                                    let mut error_string = None;

                                    while let Some(result) = receiver.recv().await {
                                        match result {
                                            Ok(call_tool_result) => {
                                                aggregated_content.extend(call_tool_result.content);
                                            }
                                            Err(e) => {
                                                final_status = ToolCallStatus::Error;
                                                error_string = Some(e);
                                                break;
                                            }
                                        }
                                    }

                                    if final_status == ToolCallStatus::Error {
                                        (final_status, error_string.unwrap_or_default())
                                    } else {
                                        let final_json = serde_json::to_value(aggregated_content).unwrap_or(serde_json::Value::Null);
                                        (final_status, serde_json::to_string_pretty(&final_json).unwrap_or_default())
                                    }
                                }
                                Err(e) => (ToolCallStatus::Error, e),
                            };
                            
                            let mut state = session_state.write();

                            if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                    tc.status = status;
                                    tc.response = response_str.clone();
                                }
                            }

                            let record = crate::components::shared::ToolCallRecord {
                                call: tool_call.clone(),
                                result: crate::components::shared::ToolResult { status, response: response_str },
                            };
                            if let Some(store) = document_store.read().as_ref().cloned() {
                                let record_for_store = record.clone();
                                spawn(async move {
                                    if let Err(e) = store.upsert_tool_result(&record_for_store).await {
                                        tracing::error!("Failed to upsert tool result: {}", e);
                                    }
                                });
                            }
                            let _ = tool_results_tx_clone.send(record);
                        });
                        is_first_message = false;
                    }
                }
            }

            if !final_text_for_this_turn.is_empty() {
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut(&message_id) {
                    if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                        *t = final_text_for_this_turn.clone();
                    }
                }
            }

            drop(tool_results_tx);
            let mut collected_records = Vec::new();
            while let Some(record) = tool_results_rx.recv().await {
                collected_records.push(record);
            }

            if tool_call_count > 0 {
                assert_eq!(collected_records.len(), tool_call_count, "Mismatch between tool calls dispatched and results received.");
                self.session_state.write().tool_call_history.extend(collected_records.clone());
                // Tools were called in this turn. Trigger a continuation to start the next turn.
                self.continuation_controller.read().trigger_continuation();
                return; // End this stream task. The continuation will start a new one.
            }

            // If we reach here, it means tool_call_count was 0. The turn is over.
            if final_text_for_this_turn.trim().ends_with("<continue />") {
                if let Some(msg) = self.session_state.write().get_message_mut(&message_id) {
                    if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                        *t = t.trim().strip_suffix("<continue />").unwrap_or(t).trim().to_string();
                    }
                }
                self.continuation_controller.read().trigger_continuation();
                return;
            }
            
            // This block now runs only when the conversation turn is truly complete.
            tracing::info!(message_id = %message_id, "LLM stream COMPLETE.");
            {
                let mut state = self.session_state.write();
                state.touch_active_session();
                if let Err(e) = state.save() {
                    tracing::error!("Failed to save session state after stream: {}", e);
                } else {
                    tracing::info!(message_id = %message_id, "Session state SAVED successfully.");
                }
            }

            let settings = self.settings.read().clone();
            let summarizer = self.tool_call_summarizer.read();
            summarizer.summarize_and_cleanup(&mut self.session_state.write(), &settings).await;
            on_complete();
            self.is_sending.set(false);
            self.scheduler.send(SchedulerSignal::Activity);
            tracing::info!(message_id = %message_id, "Completion signal SENT.");

            // Remove the handle from the map upon completion
            self.active_stream_handles.write().remove(&message_id);
            tracing::info!(message_id = %message_id, "Active stream handle removed.");
        });

        // Store the handle so we can abort it if needed
        self.active_stream_handles.write().insert(message_id, master_task_handle);
    }

    pub fn cancel_stream(mut self, message_id: &Uuid) {
        tracing::info!(message_id = %message_id, "Attempting to cancel stream.");
        
        // 1. Remove and abort the task handle
        if let Some(handle) = self.active_stream_handles.write().remove(message_id) {
            handle.cancel();
            tracing::info!(message_id = %message_id, "Aborted stream task handle.");
        }

        // 2. Remove the message from the session state
        self.session_state.write().remove_message(message_id);

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
    let document_store = use_context_provider(|| Signal::new(None));
    let settings = consume_context::<Signal<Settings>>();
    let continuation_controller = use_context_provider(|| Signal::new(ContinuationController::new()));
    let scheduler = use_context::<Coroutine<SchedulerSignal>>();
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        active_stream_handles: Signal::new(HashMap::new()),
        llm_connector: consume_context::<Signal<Arc<dyn crate::components::llm::LlmConnector>>>(),
        session_state,
        mcp_manager,
        document_store,
        tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
        settings,
        continuation_controller,
        scheduler,
        stream_activity: Signal::new(0),
        is_sending: Signal::new(false),
    });

    // Provide the context to children.
    use_context_provider(|| context);
    rsx! { {props.children} }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_signals::Signal;
    use crate::mcp::manager::McpManager;
    use std::path::PathBuf;
    use crate::context::permissions::PermissionManager;
    use crate::settings::Settings;

    #[tokio::test]
    async fn test_stream_registration_and_deregistration() {
        let mut dom = VirtualDom::new(|| {
            let session_state = use_context_provider(|| Signal::new(SessionState::new()));
            let settings = use_context_provider(|| Signal::new(Settings::default()));
            let permission_manager = use_context_provider(|| Signal::new(PermissionManager::new(settings)));
            let mcp_manager = use_context_provider(|| Signal::new(McpManager::new(PathBuf::new(), permission_manager)));
            let document_store = use_context_provider(|| Signal::new(None));
            let continuation_controller = use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| Signal::new(Arc::new(crate::components::llm::GeminiConnector::new(settings.read().gemini_config.clone())) as Arc<dyn crate::components::llm::LlmConnector>));
            let scheduler = use_coroutine(|_| async {});
            let mut stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                active_stream_handles: Signal::new(HashMap::new()),
                llm_connector,
                session_state,
                mcp_manager,
                document_store,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
                continuation_controller,
                scheduler,
                stream_activity: Signal::new(0),
                is_sending: Signal::new(false),
            });

            let message_id = Uuid::new_v4();

            // Initially, no stream should be registered
            assert!(!stream_manager.is_streaming(&message_id));

            // Register a stream
            let (_tx, rx) = mpsc::unbounded_channel();
            stream_manager.stream_receivers.write().insert(message_id, rx);

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
            let permission_manager = use_context_provider(|| Signal::new(PermissionManager::new(settings)));
            let mcp_manager = use_context_provider(|| Signal::new(McpManager::new(PathBuf::new(), permission_manager)));
            let document_store = use_context_provider(|| Signal::new(None));
            let continuation_controller = use_context_provider(|| Signal::new(ContinuationController::new()));
            let llm_connector = use_context_provider(|| Signal::new(Arc::new(crate::components::llm::GeminiConnector::new(settings.read().gemini_config.clone())) as Arc<dyn crate::components::llm::LlmConnector>));
            let scheduler = use_coroutine(|_| async {});
            let stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                active_stream_handles: Signal::new(HashMap::new()),
                llm_connector,
                session_state,
                mcp_manager,
                document_store,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
                continuation_controller,
                scheduler,
                stream_activity: Signal::new(0),
                is_sending: Signal::new(false),
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