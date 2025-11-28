use crate::session::{ConversationSummary, Session};
use crate::components::llm::LlmConnector;
use crate::settings::Settings;
use crate::components::shared::{MessageContent};
use std::sync::Arc;

/// Processes conversation history to extract and update short-term context.
pub struct ConversationProcessor {
    llm_connector: Arc<dyn LlmConnector>,
}

impl ConversationProcessor {
    /// Creates a new `ConversationProcessor`.
    pub fn new(llm_connector: Arc<dyn LlmConnector>) -> Self {
        Self { llm_connector }
    }

    /// Takes the last few messages, generates a context summary using a fast LLM,
    /// and returns the summary.
    pub async fn generate_summary(&self, session: &Session, _settings: &Settings) -> Option<ConversationSummary> {
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
                   MessageContent::Text { content: text, .. } => text.clone(),
                   MessageContent::ToolCall(tc) => format!("[Tool Call: {}]", tc.tool_name),
                   MessageContent::PermissionRequest(tc) => format!("[Permission Request for Tool: {}]", tc.tool_name),
               };
               format!("{}: {}", m.author, content_str)
           })
            .collect::<Vec<String>>()
            .join("\n");

        if recent_history.is_empty() {
            return None;
        }

        // 3. Call the LLM to refine the summary
        match self.llm_connector.summarize_conversation(
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