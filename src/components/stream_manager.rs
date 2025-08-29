use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use uuid::Uuid;
use crate::session::SessionState;
use crate::components::llm;

#[derive(Clone, Copy)]
pub struct StreamManagerContext {
    stream_receivers: Signal<HashMap<Uuid, UnboundedReceiver<String>>>,
    session_state: Signal<SessionState>,
}

impl StreamManagerContext {
    pub fn is_streaming(self, message_id: &Uuid) -> bool {
        self.stream_receivers.read().contains_key(message_id)
    }

    pub fn start_stream(
        mut self,
        message_id: Uuid,
        final_prompt: String,
    ) {
        // Create a channel for the UI to receive chunks.
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<String>();
        
        // Store the receiver for the MessageBubble to pick up.
        self.stream_receivers.write().insert(message_id, ui_rx);

        // Spawn a master task to manage the LLM call and state updates.
        spawn(async move {
            // Create the channel for the LLM to send chunks to.
            let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<String>();

            // Spawn the LLM task. It runs in the background.
            spawn(async move {
                llm::generate_content_stream(final_prompt, llm_tx).await;
            });

            // This part of the task listens for chunks from the LLM,
            // forwards them to the UI, and builds the final response.
            let mut full_response = String::new();
            while let Some(chunk) = llm_rx.recv().await {
                // Forward the chunk to the UI. If it fails, the UI component
                // has probably been dropped, so we can stop.
                if ui_tx.send(chunk.clone()).is_err() {
                    break;
                }
                full_response.push_str(&chunk);
            }

            // Stream is complete. Now, write the final content to the session state.
            // This is the single source of truth for the final state update.
            let mut state = self.session_state.write();
            if let Some(session) = state.get_active_session_mut() {
                if let Some(message) = session.messages.iter_mut().find(|m| m.id == message_id) {
                    message.content = full_response;
                }
            }
            
            // Save the session state after the update.
            if let Err(e) = state.save() {
                tracing::error!("Failed to save session state after stream: {}", e);
            }
        });
    }

    pub fn take_stream(mut self, message_id: &Uuid) -> Option<UnboundedReceiver<String>> {
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
    let context = use_hook(|| StreamManagerContext {
        stream_receivers: Signal::new(HashMap::new()),
        session_state,
    });

    // Provide the context to children.
    use_context_provider(|| context);
    rsx! { {props.children} }
}