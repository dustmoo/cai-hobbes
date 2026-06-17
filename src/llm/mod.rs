pub mod claude;
pub mod claude_models;
pub mod config;
pub mod convert;
pub mod gemini;
pub mod gemini_cache;
pub mod openai_compat;
pub mod openai_pricing;
pub mod openai_responses;
pub mod types;

pub use config::{ClaudeConfig, GeminiConfig, OpenAiCompatConfig};
// convert module used internally via use super::convert
pub use crate::components::shared::UsageData;
pub use claude::ClaudeConnector;
pub use gemini::*;
#[allow(unused_imports)]
pub use types::{ChatMessage, ChatRole, ContentBlock, LlmPrompt, ToolDefinition};

use crate::components::shared::StreamMessage;
use crate::mcp::manager::McpContext;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Build a connector for a specific provider + chat model from the current
/// settings. Used for per-session provider/model overrides, where the global
/// connector (built from `Settings::active_llm`) does not match the session.
/// Gemini connectors share a process-wide cache store, so per-request
/// construction does not orphan server-side cachedContents entries.
pub fn build_connector_for(
    settings: &crate::settings::Settings,
    provider: crate::settings::LlmProvider,
    model: &str,
) -> std::sync::Arc<dyn LlmConnector> {
    use crate::settings::LlmProvider;
    match provider {
        LlmProvider::Gemini => {
            let mut config = settings.gemini_config.clone();
            config.chat_model = model.to_string();
            std::sync::Arc::new(GeminiConnector::new_shared(config))
        }
        LlmProvider::OpenAiCompat => {
            let mut config = settings.openai_compat_config.clone();
            config.model = model.to_string();
            openai_responses::build_openai_connector(config)
        }
        LlmProvider::Claude => {
            let mut config = settings.claude_config.clone();
            config.model = model.to_string();
            std::sync::Arc::new(ClaudeConnector::new(config))
        }
    }
}

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
