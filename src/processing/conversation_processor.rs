use crate::session::Session;
use crate::components::llm;

/// Processes conversation history to extract and update short-term context.
pub struct ConversationProcessor {}

impl ConversationProcessor {
    /// Creates a new `ConversationProcessor`.
    pub fn new() -> Self {
        Self {}
    }

    /// Takes the last few messages, generates a context summary using a fast LLM,
    /// and updates the session's active context.
    pub async fn generate_summary(&self, session: &Session) -> Option<serde_json::Value> {
        // 1. Get the last 5 messages and format them
        let history: String = session
            .messages
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|m| format!("{}: {}", m.author, m.content))
            .collect::<Vec<String>>()
            .join("\n");

        if history.is_empty() {
            return None;
        }

        // 2. Call the LLM to summarize and extract entities
        match llm::summarize_conversation(history).await {
            Ok(summary_json) => {
                if summary_json.is_null() {
                    tracing::warn!("LLM summarization returned null.");
                    return None;
                }
                
                tracing::info!("Generated conversation context: {}", summary_json);
                Some(summary_json)
            }
            Err(e) => {
                tracing::error!("Failed to summarize conversation: {}", e);
                None
            }
        }
    }
}