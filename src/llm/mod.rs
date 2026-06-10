pub mod claude;
pub mod claude_models;
pub mod config;
pub mod convert;
pub mod gemini;
pub mod gemini_cache;
pub mod openai_compat;
pub mod types;

pub use config::{ClaudeConfig, GeminiConfig, OpenAiCompatConfig};
// convert module used internally via use super::convert
pub use crate::components::shared::UsageData;
pub use claude::ClaudeConnector;
pub use gemini::*;
pub use openai_compat::OpenAiCompatConnector;
#[allow(unused_imports)]
pub use types::{ChatMessage, ChatRole, ContentBlock, LlmPrompt, ToolDefinition};

use crate::components::shared::StreamMessage;
use crate::mcp::manager::McpContext;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[async_trait]
pub trait LlmConnector: Send + Sync {
    /// Canonical entry point for chat streaming.
    /// Accepts neutral LlmPrompt (refactored from Gemini types).
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        mcp_context: Option<McpContext>,
        session_id: Option<String>,
    );

    /// Optional: Invalidate any cached context for the given session.
    /// Default implementation is a no-op for non-caching providers.
    async fn invalidate_session_cache(&self, _session_id: &str) {}

    /// Summarize a conversation into a structured JSON payload.
    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

    /// Optional: Perform tool selection for a specific toolkit.
    /// Default implementation returns "not supported".
    #[allow(dead_code)]
    async fn select_tools_for_toolkit(
        &self,
        _request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, String> {
        Err("Tool selection not supported by this provider".to_string())
    }
}
