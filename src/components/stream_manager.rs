use dioxus::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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

    pub fn start_and_manage_stream(
        mut self,
        message_id: Uuid,
        final_prompt: String,
        llm_tx: UnboundedSender<String>,
        ui_rx: UnboundedReceiver<String>,
    ) {
        // Insert the receiver for the UI to consume
        self.stream_receivers.write().insert(message_id, ui_rx);

        // Spawn the task that handles the entire lifecycle
        spawn(async move {
            // This task will own the final state update logic
            let mut full_response = String::new();
            let mut stream_rx = llm::generate_content_stream_channel(final_prompt, llm_tx).await;

            while let Some(chunk) = stream_rx.recv().await {
                full_response.push_str(&chunk);
            }

            // Stream is complete, now update the global state
            let mut state = self.session_state.write();
            if let Some(session) = state.get_active_session_mut() {
                if let Some(message) = session.messages.iter_mut().find(|m| m.id == message_id) {
                    message.content = full_response;
                }
            }
            
            // Save the session state after the update
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