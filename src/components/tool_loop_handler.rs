use dioxus::prelude::*;
use crate::components::stream_manager::StreamManagerContext;
use crate::session::SessionState;
use crate::settings::Settings;

#[derive(Props, PartialEq, Clone)]
pub struct ToolLoopHandlerProps {
    session_state: Signal<SessionState>,
    settings: Signal<Settings>,
}

#[component]
pub fn ToolLoopHandler(props: ToolLoopHandlerProps) -> Element {
    let stream_manager = consume_context::<StreamManagerContext>();

    // This effect will run whenever the messages in the active session change.
    use_effect(move || {
        let session_state = props.session_state.read();
        if let Some(session) = session_state.get_active_session() {
            if let Some(last_message) = session.messages.last() {
                if last_message.author == "Hobbes" {
                    if let crate::components::shared::MessageContent::Text(text) = &last_message.content {
                        // Simple check for now, can be made more robust.
                        if text.contains("I'll get") || text.contains("I will now") {
                            // Re-trigger the stream.
                            let (prompt_data, mcp_context) = {
                                let settings = props.settings.read();
                                let builder = crate::context::prompt_builder::PromptBuilder::new(session, &settings, &session_state);
                                let prompt = builder.build_prompt("".to_string(), None);
                                (prompt, session.active_context.mcp_tools.clone())
                            };
                            
                            let new_message_id = uuid::Uuid::new_v4();
                            stream_manager.start_stream(
                                props.settings.read().chat_model.clone(),
                                new_message_id,
                                prompt_data,
                                || {},
                                mcp_context,
                            );
                        }
                    }
                }
            }
        }
    });

    rsx! { div {} }
}