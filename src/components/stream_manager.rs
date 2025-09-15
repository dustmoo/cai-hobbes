use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;
use crate::session::SessionState;
use crate::components::llm;
use crate::components::shared::{StreamMessage, ToolCallStatus};

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<StreamMessage>>>,
    session_state: Signal<SessionState>,
    mcp_manager: Signal<crate::mcp::manager::McpManager>,
}

impl StreamManagerContext {
    pub fn is_streaming(self, message_id: &Uuid) -> bool {
        self.stream_receivers.read().contains_key(message_id)
    }

    pub fn start_stream(
        mut self,
        api_key: String,
        model: String,
        message_id: Uuid,
        prompt_data: crate::context::prompt_builder::LlmPrompt,
        on_complete: impl FnOnce() + Send + 'static,
        mcp_context: Option<crate::mcp::manager::McpContext>,
    ) {
        tracing::info!(message_id = %message_id, "'start_stream' entered.");
        // Create a channel for the UI to receive chunks.
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<StreamMessage>();
        
        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, ui_rx);

        // Spawn a master task to manage the LLM call and state updates.
        spawn(async move {
            tracing::info!(message_id = %message_id, "Stream master task SPAWNED.");
            // Create the channel for the LLM to send chunks to.
            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<StreamMessage>();

            // Spawn the LLM task. It runs in the background.
            spawn(async move {
                llm::generate_content_stream(api_key, model, prompt_data, llm_tx, mcp_context).await;
            });

            let mut is_first_message = true;
            while let Some(message) = llm_rx.recv().await {
                match message {
                    StreamMessage::Text(chunk) => {
                        let mut state = self.session_state.write();
                        if let Some(msg) = state.get_message_mut(&message_id) {
                             if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                                t.push_str(&chunk);
                            }
                        }
                        if ui_tx.send(StreamMessage::Text(chunk)).is_err() {
                            break; // UI component likely dropped
                        }
                        is_first_message = false;
                    }
                    StreamMessage::ToolCall(tool_call) => {
                        let mut state = self.session_state.write();
                        let tool_call_message_id;

                        if is_first_message {
                            // This is the first message. Let's replace the content of the original message.
                            tool_call_message_id = message_id;
                            if let Some(msg) = state.get_message_mut(&message_id) {
                                msg.content = crate::components::shared::MessageContent::ToolCall(tool_call.clone());
                            }
                        } else {
                            // This is not the first message, so create a new message for the tool call.
                            tool_call_message_id = Uuid::new_v4();
                            if let Some(session) = state.get_active_session_mut() {
                                session.messages.push(crate::components::chat::Message {
                                    id: tool_call_message_id,
                                    author: "Hobbes".to_string(),
                                    content: crate::components::shared::MessageContent::ToolCall(tool_call.clone()),
                                });
                            }
                        }
                        drop(state);

                        let mut mcp_manager = self.mcp_manager;
                        let mut session_state = self.session_state;

                        spawn(async move {
                            let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null);
                            let result = mcp_manager.write().use_mcp_tool(&tool_call.server_name, &tool_call.tool_name, args_json, false).await;

                            let mut state = session_state.write();
                            if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                    match result {
                                        Ok(response) => {
                                            tc.status = ToolCallStatus::Completed;
                                            tc.response = serde_json::to_string_pretty(&response).unwrap_or_default();
                                        },
                                        Err(e) => {
                                            // Attempt to deserialize the error into a ToolCall for permission requests
                                            if let Ok(tool_call_req) = serde_json::from_str::<crate::components::shared::ToolCall>(&e) {
                                                // This is a permission request. Find the original tool call message and update its content.
                                                if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                                   msg.content = crate::components::shared::MessageContent::PermissionRequest(tool_call_req);
                                                }
                                            } else {
                                                // This is a genuine error
                                                tc.status = ToolCallStatus::Error;
                                                tc.response = e;
                                            }
                                        }
                                    }
                                }
                            }
                        });
                        is_first_message = false;
                    }
                    StreamMessage::PermissionRequest(_) => {
                        // This is unexpected from the LLM stream and handled internally.
                        tracing::warn!("Unexpected StreamMessage::PermissionRequest received from LLM stream; ignoring.");
                    }
                }
            }

            tracing::info!(message_id = %message_id, "LLM stream COMPLETE.");
            let mut state = self.session_state.write();
            state.touch_active_session();
            if let Err(e) = state.save() {
                tracing::error!("Failed to save session state after stream: {}", e);
            } else {
                tracing::info!(message_id = %message_id, "Session state SAVED successfully.");
            }

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
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        session_state,
        mcp_manager,
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
            let mut stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                session_state,
                mcp_manager,
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
            let stream_manager = use_context_provider(|| StreamManagerContext {
                stream_receivers: Signal::new(HashMap::new()),
                session_state,
                mcp_manager,
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