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

    /// Generate a fleet re-entry brief from a framed transcript digest
    /// (`fleet::briefs::brief_framing`). Rides each provider's
    /// `summarize_conversation` path — and therefore its cheap
    /// `summary_model` — with the previous brief in the summary slot for
    /// incremental updates. The fixed summarize schema is mapped onto a
    /// `SessionBrief` by `fleet::briefs::brief_from_summary_value`.
    async fn generate_fleet_brief(
        &self,
        previous_brief_json: String,
        framed_digest: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        self.summarize_conversation(previous_brief_json, framed_digest)
            .await
    }

    /// One-shot day-rollup narrative (`fleet::briefs::rollup_framing`).
    /// Returns the `summary` field, falling back to the stringified payload
    /// when the shape drifts.
    async fn generate_fleet_rollup(
        &self,
        framed_day: String,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let value = self.summarize_conversation(String::new(), framed_day).await?;
        Ok(value
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| value.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Trait-object stub: canned `summarize_conversation`, no network — for
    /// exercising the defaulted fleet-brief/rollup methods.
    struct StubConnector(serde_json::Value);

    #[async_trait]
    impl LlmConnector for StubConnector {
        async fn generate_content_stream(
            &self,
            _prompt_data: LlmPrompt,
            _tx: mpsc::UnboundedSender<StreamMessage>,
            _mcp_context: Option<McpContext>,
            _session_id: Option<String>,
        ) {
        }

        async fn summarize_conversation(
            &self,
            _previous_summary: String,
            _recent_messages: String,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn fleet_rollup_extracts_summary_with_fallback() {
        let with_summary =
            StubConnector(serde_json::json!({"summary": "Shipped briefs.", "sentiment": "good"}));
        assert_eq!(
            with_summary
                .generate_fleet_rollup("framed".into())
                .await
                .unwrap(),
            "Shipped briefs."
        );

        // Empty/absent summary → stringified payload, never an empty answer.
        let odd = StubConnector(serde_json::json!({"summary": "", "other": 1}));
        let out = odd.generate_fleet_rollup("framed".into()).await.unwrap();
        assert!(out.contains("other"));
    }

    #[tokio::test]
    async fn fleet_brief_rides_summarize_conversation() {
        let stub = StubConnector(serde_json::json!({"summary": "did x"}));
        let v = stub
            .generate_fleet_brief(String::new(), "framed".into())
            .await
            .unwrap();
        assert_eq!(v["summary"], "did x");
    }
}
