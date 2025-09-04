use crate::session::{ConversationSummary, Session};
use crate::components::llm;
use crate::settings::Settings;
use crate::components::stream_manager::StreamManagerContext;
use crate::components::shared::{MessageContent};

/// Processes conversation history to extract and update short-term context.
pub struct ConversationProcessor {
    stream_manager: StreamManagerContext,
}

impl ConversationProcessor {
    /// Creates a new `ConversationProcessor`.
    pub fn new(stream_manager: StreamManagerContext) -> Self {
        Self { stream_manager }
    }

    /// Takes the last few messages, generates a context summary using a fast LLM,
    /// and updates the session's active context.
    pub async fn process_and_respond(&self, session: &mut Session, settings: &Settings) -> String {
        // For now, we will just generate the summary and not call tools.
        // The logic for tool calling will be added here later.
        if let Some(summary) = self.generate_summary(session, settings).await {
            session.active_context.conversation_summary = summary;
        }

        // This part will be replaced with logic that decides whether to call a tool
        // or to send the user's message to the LLM.
        let last_message = session.messages.last().unwrap();
        if let MessageContent::Text(text) = &last_message.content {
            text.clone()
        } else {
            "".to_string()
        }
    }

    /// Takes the last few messages, generates a context summary using a fast LLM,
    /// and updates the session's active context.
    async fn generate_summary(&self, session: &Session, settings: &Settings) -> Option<ConversationSummary> {
        // 1. Get the previous summary from the active context by serializing the struct
        let previous_summary = serde_json::to_string(&session.active_context.conversation_summary)
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to serialize previous summary: {}. Using default.", e);
                "{}".to_string()
            });

        // 2. Get the last 5 messages and format them
        let recent_history: String = session
            .messages
            .iter()
           .rev()
           .take(5)
           .rev()
           .map(|m| {
               let content_str = match &m.content {
                   MessageContent::Text(text) => text.clone(),
                   MessageContent::ToolCall(tc) => format!("[Tool Call: {}]", tc.tool_name),
               };
               format!("{}: {}", m.author, content_str)
           })
            .collect::<Vec<String>>()
            .join("\n");

        if recent_history.is_empty() {
            return None;
        }

        // 3. Get API key, prioritizing settings, then environment variable
        let api_key = settings.api_key.clone().unwrap_or_else(|| {
            std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set in settings or environment")
        });

        // 4. Call the LLM to refine the summary
        match llm::summarize_conversation(
            api_key,
            settings.summary_model.clone(),
            previous_summary,
            recent_history,
        )
        .await
        {
            Ok(summary_json) => {
                match serde_json::from_value::<ConversationSummary>(summary_json) {
                    Ok(summary) => {
                        tracing::info!("Successfully deserialized new conversation summary.");
                        Some(summary)
                    }
                    Err(e) => {
                        tracing::error!("Failed to deserialize conversation summary: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to summarize conversation: {}", e);
                None
            }
        }
    }
}