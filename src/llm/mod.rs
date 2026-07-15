pub mod claude;
pub mod claude_models;
pub mod config;
pub mod context_cache;
pub mod convert;
pub mod gemini;
pub mod gemini_cache;
pub mod openai_compat;
pub mod openai_models;
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

/// Build a connector for a specific connector instance, optionally overriding
/// the chat model (per-session model pins). Gemini connectors share a
/// process-wide cache store, so per-request construction does not orphan
/// server-side cachedContents entries; cache entries are fingerprinted by API
/// key so instances with different keys never cross-reuse them.
pub fn build_connector_for_instance(
    instance: &crate::settings::ProviderInstance,
    model_override: Option<&str>,
) -> std::sync::Arc<dyn LlmConnector> {
    use crate::llm::config::ProviderInstanceConfig;
    match &instance.config {
        ProviderInstanceConfig::Gemini(config) => {
            let mut config = config.clone();
            if let Some(model) = model_override {
                config.chat_model = model.to_string();
            }
            std::sync::Arc::new(GeminiConnector::new_shared(config))
        }
        ProviderInstanceConfig::OpenAiCompat(config) => {
            let mut config = config.clone();
            if let Some(model) = model_override {
                config.model = model.to_string();
            }
            openai_responses::build_openai_connector(config)
        }
        ProviderInstanceConfig::Claude(config) => {
            let mut config = config.clone();
            if let Some(model) = model_override {
                config.model = model.to_string();
            }
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

    /// Produce a knowledge-preserving summary of a single (large) tool result so
    /// it can replace the raw payload once it becomes historical, keeping the
    /// salient facts in context while reclaiming tokens.
    ///
    /// The default implementation reuses each provider's `summarize_conversation`
    /// HTTP path, framing the tool output with explicit instructions to preserve
    /// concrete facts (IDs, names, numbers, dates, statuses) and returning the
    /// `summary` field. Providers may override for a better-tuned prompt.
    async fn summarize_tool_result(
        &self,
        tool_name: &str,
        response: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let framing = format!(
            "You are compressing a large result from the tool '{tool_name}' so a \
             downstream agent can rely on it later without re-fetching.\n\n\
             Produce a DENSE, factual summary that PRESERVES every concrete detail \
             the agent would need: ALL identifiers/IDs, names, email addresses, \
             numbers, dates, statuses, and counts. Never invent or omit an ID. \
             Prefer a compact list or table over prose. Do not editorialize.\n\n\
             Tool result to summarize:\n---\n{response}\n---"
        );
        let value = self.summarize_conversation(String::new(), framing).await?;
        // The conversation summarizer returns a JSON object with a `summary`
        // field; fall back to the whole payload as text if the shape differs.
        let summary = value
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| value.to_string());
        Ok(summary)
    }

    /// Optional: Perform tool selection for a specific toolkit.
    /// Default implementation returns "not supported".
    async fn select_tools_for_toolkit(
        &self,
        _request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, String> {
        Err("Tool selection not supported by this provider".to_string())
    }
}
