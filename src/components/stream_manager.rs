use crate::services::tool_call_summarizer::ToolCallSummarizer;
use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;
use crate::session::SessionState;
use crate::components::llm;
use crate::components::shared::{StreamMessage, ToolCallStatus};
use crate::services::document_store::DocumentStore;
use std::sync::Arc;
use crate::settings::Settings;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<StreamMessage>>>,
    session_state: Signal<SessionState>,
    mcp_manager: Signal<crate::mcp::manager::McpManager>,
    document_store: Signal<Option<Arc<DocumentStore>>>,
    tool_call_summarizer: Signal<ToolCallSummarizer>,
    settings: Signal<Settings>,
}

impl StreamManagerContext {
    pub fn is_streaming(self, message_id: &Uuid) -> bool {
        self.stream_receivers.read().contains_key(message_id)
    }

    pub fn start_stream(
        mut self,
        model: String,
        message_id: Uuid,
        prompt_data: crate::context::prompt_builder::LlmPrompt,
        on_complete: impl FnOnce() + Send + 'static,
        mcp_context: Option<crate::mcp::manager::McpContext>,
    ) {
        tracing::info!(message_id = %message_id, "'start_stream' entered.");
        // Create a channel for the MessageBubble to receive chunks.
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<StreamMessage>();
        
        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, stream_rx);

        // Spawn a master task to manage the LLM call and state updates.
        spawn(async move {
            tracing::info!(message_id = %message_id, "Stream master task SPAWNED.");
            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<StreamMessage>();

            let settings = self.settings.read().clone();
            let api_key = settings.api_key.clone().unwrap_or_else(|| {
                std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set in settings or environment")
            });
            spawn(async move {
                llm::generate_content_stream(api_key, model, prompt_data, llm_tx, mcp_context).await;
            });

            let mut is_first_message = true;
            // MPSC channel to collect results from all spawned tool-call tasks.
            let (tool_results_tx, mut tool_results_rx) = mpsc::unbounded_channel::<crate::components::shared::ToolCallRecord>();
            let mut tool_call_count = 0;

            while let Some(message) = llm_rx.recv().await {
                match message {
                    StreamMessage::Text(chunk) => {
                        let mut state = self.session_state.write();
                        if let Some(msg) = state.get_message_mut(&message_id) {
                             if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                                t.push_str(&chunk);
                            }
                        }
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
                                    });
                                }
                                new_id
                            }
                        };

                        // Each tool call runs in its own spawned task.
                        // It owns a sender to the results channel.
                        let mcp_manager = self.mcp_manager;
                        let mut session_state = self.session_state;
                        let document_store = self.document_store;
                        let tool_results_tx = tool_results_tx.clone(); // Clone sender for the task
                        spawn(async move {
                            let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null);
                            let result = mcp_manager.read().use_mcp_tool(&tool_call.server_name, &tool_call.tool_name, args_json, false).await;

                            let mut state = session_state.write();
                            let (status, response_str) = match result {
                                Ok(response) => (ToolCallStatus::Completed, serde_json::to_string_pretty(&response).unwrap_or_default()),
                                Err(e) => {
                                    if let Ok(tool_call_req) = serde_json::from_str::<crate::components::shared::ToolCall>(&e) {
                                        if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                            msg.content = crate::components::shared::MessageContent::PermissionRequest(tool_call_req);
                                        }
                                        (ToolCallStatus::Error, e)
                                    } else {
                                        (ToolCallStatus::Error, e)
                                    }
                                }
                            };

                            if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                    tc.status = status;
                                    tc.response = response_str.clone();
                                }
                            }

                            let record = crate::components::shared::ToolCallRecord {
                                call: tool_call.clone(),
                                result: crate::components::shared::ToolResult {
                                    status,
                                    response: response_str,
                                },
                            };
                            if let Some(store) = document_store.read().as_ref().cloned() {
                                let record_for_store = record.clone();
                                spawn(async move {
                                    if let Err(e) = store.upsert_tool_result(&record_for_store).await {
                                        tracing::error!("Failed to upsert tool result: {}", e);
                                    }
                                });
                            }
                            let _ = tool_results_tx.send(record);
                        });
                        is_first_message = false;
                    }
                }
            }

            // The master task will collect all results from the channel.
            // We drop the original sender here. The loop will only complete
            // once all the spawned tool-call tasks have finished and dropped their sender clones.
            // This is a robust way to await an unknown number of concurrent tasks.
            drop(tool_results_tx);
            let mut collected_records = Vec::new();
            while let Some(record) = tool_results_rx.recv().await {
                collected_records.push(record);
            }

            // Centralize all SessionState mutations to occur sequentially after results are collected.
            if tool_call_count > 0 {
                assert_eq!(collected_records.len(), tool_call_count, "Mismatch between tool calls dispatched and results received.");
                self.session_state.write().tool_call_history.extend(collected_records.clone());
            }

            // If tools were called, we now feed the results back to the LLM to get a final,
            // natural-language response. This is the core of the feedback loop.
            if !self.session_state.read().tool_call_history.is_empty() {
                let new_hobbes_message_id = Uuid::new_v4();
                let settings = self.settings.read().clone();
                
                // Create the new, empty message bubble that will display the final response.
                {
                    let mut state = self.session_state.write();
                    if let Some(session) = state.get_active_session_mut() {
                        session.messages.push(crate::components::chat::Message {
                            id: new_hobbes_message_id,
                            author: "Hobbes".to_string(),
                            content: crate::components::shared::MessageContent::Text("".to_string()),
                        });
                    }
                }

                // Build the new prompt that includes the tool call history.
                let (prompt_data, mcp_context_for_next_call) = {
                    let current_state = self.session_state.read();
                    if let Some(session) = current_state.get_active_session() {
                        let builder = crate::context::prompt_builder::PromptBuilder::new(session, &settings, &current_state);
                        let prompt = builder.build_prompt("".to_string(), None); // Empty message, context is now in history
                        (Some(prompt), session.active_context.mcp_tools.clone())
                    } else {
                        (None, None)
                    }
                };

                // Execute the second LLM call.
                if let Some(prompt) = prompt_data {
                    let (final_answer_tx, final_answer_rx) = mpsc::unbounded_channel::<StreamMessage>();
                    self.stream_receivers.write().insert(new_hobbes_message_id, final_answer_rx);

                    let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<StreamMessage>();
                    let api_key = settings.api_key.clone().unwrap_or_else(|| {
                        std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set in settings or environment")
                    });
                    let model = settings.chat_model.clone();

                    spawn(async move {
                        llm::generate_content_stream(api_key, model, prompt, llm_tx, mcp_context_for_next_call).await;
                    });

                    // Stream the final response to the new message bubble.
                    while let Some(message) = llm_rx.recv().await {
                        if let StreamMessage::Text(chunk) = message {
                            if let Some(msg) = self.session_state.write().get_message_mut(&new_hobbes_message_id) {
                                if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                                    t.push_str(&chunk);
                                }
                            }
                            if final_answer_tx.send(StreamMessage::Text(chunk)).is_err() {
                                break;
                            }
                        }
                    }
                }
            }

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
            tracing::info!(message_id = %message_id, "Completion signal SENT.");
        });
    }

    pub fn take_stream(mut self, message_id: &Uuid) -> Option<UnboundedReceiver<StreamMessage>> {
        self.stream_receivers.write().remove(message_id)
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
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        session_state,
        mcp_manager,
        document_store,
        tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
        settings,
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
            let mut stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                session_state,
                mcp_manager,
                document_store,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
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
            let stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                session_state,
                mcp_manager,
                document_store,
                tool_call_summarizer: Signal::new(ToolCallSummarizer::new()),
                settings,
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