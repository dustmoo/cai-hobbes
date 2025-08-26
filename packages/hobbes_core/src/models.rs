//! Data models for the Hobbes application.

/// Represents a single, independent chat conversation.
#[derive(Debug, Clone)]
pub struct ChatSession {
    /// A unique identifier for the chat session.
    pub id: String,
    /// An optional, user-editable title for the conversation.
    pub title: Option<String>,
    /// The timestamp when the chat session was created.
    pub created_at: String,
    /// The timestamp of the last message in the session.
    pub last_updated_at: String,
}

/// Represents a single message within a `ChatSession`.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// A unique identifier for the message.
    pub id: String,
    /// The ID of the `ChatSession` this message belongs to.
    pub session_id: String,
    /// The ID of the parent message, forming a tree structure for conversations.
    /// `None` for the first message in a branch.
    pub parent_message_id: Option<String>,
    /// The textual content of the message.
    pub content: String,
    /// The timestamp when the message was created.
    pub timestamp: String,
    /// The source of the message, indicating who or what created it.
    pub source: MessageSource,
}

/// An enum that holds the source-specific data for a `ChatMessage`.
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// A message from the human user.
    User,
    /// A message from the Hobbes application itself (e.g., status updates, errors).
    Agent,
    /// A message from a large language model.
    Llm {
        /// Required metadata associated with the LLM's response.
        metadata: LlmMetadata,
    },
}

/// A required struct containing metadata for messages from an LLM.
#[derive(Debug, Clone)]
pub struct LlmMetadata {
    /// The name or identifier of the LLM that generated the response (e.g., "gemini-2.5-pro").
    pub model_name: String,
    /// The number of tokens used to generate the response.
    pub token_count: u32,
    /// The reason the model stopped generating text (e.g., "stop_sequence", "max_tokens").
    pub stop_reason: String,
}