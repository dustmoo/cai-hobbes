use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;
use crate::session::SessionState;
use crate::components::llm;
use crate::components::shared::ToolCallStatus;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<String>>>,
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
    ) {
        tracing::info!(message_id = %message_id, "'start_stream' entered.");
        // Create a channel for the UI to receive chunks.
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<String>();
        
        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, ui_rx);

        // Spawn a master task to manage the LLM call and state updates.
        spawn(async move {
            tracing::info!(message_id = %message_id, "Stream master task SPAWNED.");
            // Create the channel for the LLM to send chunks to.
            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<String>();

            // Spawn the LLM task. It runs in the background.
            spawn(async move {
                llm::generate_content_stream(api_key, model, prompt_data, llm_tx).await;
            });

            let mut buffer = String::new();
            let mut current_message_id = message_id;
            let mut parsing_state = ParsingState::Text;

            while let Some(chunk) = llm_rx.recv().await {
                // Forward the chunk to the UI. If it fails, the UI component
                // has probably been dropped, so we can stop.
                if ui_tx.send(chunk.clone()).is_err() {
                    break;
                }
                buffer.push_str(&chunk);

                loop {
                    match parsing_state {
                        ParsingState::Text => {
                            if let Some(start_index) = buffer.find("<tool_use>") {
                                let text_part = &buffer[..start_index];
                                if !text_part.is_empty() {
                                    let mut state = self.session_state.write();
                                    if let Some(msg) = state.get_message_mut(&current_message_id) {
                                        if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                                            t.push_str(text_part);
                                        }
                                    }
                                }

                                let new_message_id = Uuid::new_v4();
                                {
                                    let mut state = self.session_state.write();
                                    if let Some(session) = state.get_active_session_mut() {
                                        session.messages.push(crate::components::chat::Message {
                                            id: new_message_id,
                                            author: "Hobbes".to_string(),
                                            content: crate::components::shared::MessageContent::ToolCall(Default::default()),
                                        });
                                    }
                                }
                                current_message_id = new_message_id;
                                buffer = buffer[start_index + "<tool_use>".len()..].to_string();
                                parsing_state = ParsingState::InToolCall;
                            } else {
                                break;
                            }
                        }
                        ParsingState::InToolCall => {
                            if let Some(end_index) = buffer.find("</tool_use>") {
                                let tool_content = &buffer[..end_index];
                                // TODO: Implement proper XML parsing instead of this brittle string manipulation
                                let server_name = extract_tag_content(tool_content, "server_name");
                                let tool_name = extract_tag_content(tool_content, "tool_name");
                                let arguments = extract_tag_content(tool_content, "arguments");

                                {
                                    let mut state = self.session_state.write();
                                    if let Some(msg) = state.get_message_mut(&current_message_id) {
                                        if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                            tc.server_name = server_name.clone();
                                            tc.tool_name = tool_name.clone();
                                            tc.arguments = arguments.clone();
                                            tc.status = ToolCallStatus::Running;
                                        }
                                    }
                                }
                                
                                let mcp_manager = self.mcp_manager;
                                let mut session_state = self.session_state;
                                let tool_call_message_id = current_message_id;

                                spawn(async move {
                                    let args_json: serde_json::Value = serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);
                                    let result = mcp_manager.read().use_mcp_tool(&server_name, &tool_name, args_json).await;

                                    let mut state = session_state.write();
                                    if let Some(msg) = state.get_message_mut(&tool_call_message_id) {
                                        if let crate::components::shared::MessageContent::ToolCall(tc) = &mut msg.content {
                                            match result {
                                                Ok(response) => {
                                                    tc.status = ToolCallStatus::Completed;
                                                    tc.response = serde_json::to_string_pretty(&response).unwrap_or_default();
                                                },
                                                Err(e) => {
                                                    tc.status = ToolCallStatus::Error;
                                                    tc.response = e;
                                                }
                                            }
                                        }
                                    }
                                });

                                let new_message_id = Uuid::new_v4();
                                {
                                    let mut state = self.session_state.write();
                                    if let Some(session) = state.get_active_session_mut() {
                                        session.messages.push(crate::components::chat::Message {
                                            id: new_message_id,
                                            author: "Hobbes".to_string(),
                                            content: crate::components::shared::MessageContent::Text("".to_string()),
                                        });
                                    }
                                }
                                current_message_id = new_message_id;
                                buffer = buffer[end_index + "</tool_use>".len()..].to_string();
                                parsing_state = ParsingState::Text;
                            } else {
                                break;
                            }
                        }
                    }
                }
            }

            // After the loop, handle any remaining buffer content or malformed states.
            if parsing_state == ParsingState::InToolCall {
                // The stream ended with an unterminated tool call.
                tracing::error!("Malformed tool call: Stream ended before finding </tool_use> tag.");
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut(&current_message_id) {
                    msg.content = crate::components::shared::MessageContent::Text(
                        "Error: I received an incomplete tool call from the model. Please try again.".to_string()
                    );
                }
                // TODO: Automatically send this error back to the LLM for a retry.
            } else if !buffer.is_empty() {
                // Process any remaining text in the buffer
                let mut state = self.session_state.write();
                if let Some(msg) = state.get_message_mut(&current_message_id) {
                    if let crate::components::shared::MessageContent::Text(t) = &mut msg.content {
                        *t = buffer.clone();
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

    pub fn take_stream(mut self, message_id: &Uuid) -> Option<UnboundedReceiver<String>> {
        self.stream_receivers.write().remove(message_id)
    }

}

#[derive(Clone, Copy, PartialEq)]
enum ParsingState {
    Text,
    InToolCall,
}

fn extract_tag_content(xml: &str, tag: &str) -> String {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);
    if let Some(start) = xml.find(&start_tag) {
        if let Some(end) = xml.find(&end_tag) {
            return xml[start + start_tag.len()..end].to_string();
        }
    }
    String::new()
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