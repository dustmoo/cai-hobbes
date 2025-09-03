use crate::session::{ConversationSummary, Session};
use crate::components::llm;
use crate::settings::Settings;

/// Processes conversation history to extract and update short-term context.
pub struct ConversationProcessor {}

impl ConversationProcessor {
    /// Creates a new `ConversationProcessor`.
    pub fn new() -> Self {
        Self {}
    }

    /// Takes the last few messages, generates a context summary using a fast LLM,
    /// and updates the session's active context.
    pub async fn generate_summary(&self, session: &Session, settings: &Settings) -> Option<ConversationSummary> {
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
            .map(|m| format!("{}: {}", m.author, match &m.content {
                crate::components::chat::MessageContent::Text { content } => content.clone(),
                crate::components::chat::MessageContent::ToolCall { call } => serde_json::to_string(&call).unwrap_or_default(),
            }))
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