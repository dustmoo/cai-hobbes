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

/// True when a Gemini HTTP error body describes an invalid/mismatched
/// `thoughtSignature` on a function-call (or thought) part — the shape
/// returned when history carries a signature minted by a different provider
/// after a mid-chat model switch.
///
/// Observed wording (Gemini `generateContent`/`streamGenerateContent` 400s,
/// `status: "INVALID_ARGUMENT"`):
///   - "Invalid thought signature provided. ..."
///   - "Corrupted thought signature provided. ..."
///   - "Unable to submit request because it has a thought_signature field
///     that is invalid ..."
///   - "The thought signature ... is invalid/does not match/expired"
///
/// Matched conservatively: the status must be 400, and the body must mention
/// "signature" together with either "thought" or "INVALID_ARGUMENT"
/// (case-insensitive) so unrelated 400s (bad API key, malformed schema,
/// safety blocks, etc.) never trigger a retry.
pub fn is_gemini_thought_signature_error(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("signature") && (lower.contains("thought") || lower.contains("invalid_argument"))
}

/// True when an Anthropic HTTP error body describes an invalid/mismatched
/// `signature` on a `thinking` content block — the shape returned when
/// history carries a signature minted by a different provider after a
/// mid-chat model switch.
///
/// Observed wording (Anthropic Messages API 400s, `error.type ==
/// "invalid_request_error"`):
///   - "messages.N.content.M.signature: Invalid signature for thinking
///     block."
///   - "... thinking block signature ... invalid/does not match/unable to
///     verify"
///
/// Matched conservatively: the status must be 400, the body must be an
/// `invalid_request_error`, and it must mention "signature" together with
/// "thinking" (case-insensitive) so unrelated 400s (bad model, missing
/// max_tokens, etc.) never trigger a retry.
pub fn is_claude_thought_signature_error(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("invalid_request_error")
        && lower.contains("signature")
        && lower.contains("thinking")
}

/// Clear provider-minted thought/thinking signatures from a message history.
/// Used as a belt-and-braces guard when history carries a signature minted by
/// a *different* provider than the one about to be called — that signature
/// will always be rejected. Both connectors' `to_native_request` already
/// treat a cleared signature safely: Gemini omits `thoughtSignature` on the
/// function-call part; Claude drops the `thinking` block entirely rather than
/// sending one without a signature (which would itself 400). So clearing here
/// is sufficient to make the retried request valid.
pub fn strip_thought_signatures(messages: &mut [ChatMessage]) {
    for message in messages.iter_mut() {
        for block in message.content.iter_mut() {
            match block {
                ContentBlock::Thinking { signature, .. } => *signature = None,
                ContentBlock::ToolCall { signature, .. } => *signature = None,
                _ => {}
            }
        }
    }
}

/// Pull the JSON object out of a model's summary reply, tolerating the ways
/// smaller (often local) models wrap it: surrounding prose, a code fence, or
/// a stray extra brace (`{{"summary": … }` was seen from a local model). Tries
/// the whole text, then parses from each `{` in turn and keeps the first
/// complete object, ignoring whatever trails it.
pub fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(text.trim()) {
        return Some(v);
    }
    text.match_indices('{').take(16).find_map(|(i, _)| {
        serde_json::Deserializer::from_str(&text[i..])
            .into_iter::<serde_json::Value>()
            .next()
            .and_then(Result::ok)
            .filter(serde_json::Value::is_object)
    })
}

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

    #[test]
    fn extract_json_object_tolerates_wrapping() {
        let want = serde_json::json!({"summary": "did x", "tab_emoji": "🧾"});
        for text in [
            r#"{"summary":"did x","tab_emoji":"🧾"}"#,
            // Stray leading brace, as a local model returned it.
            r#"{{"summary":"did x","tab_emoji":"🧾"}"#,
            // Stray trailing brace.
            r#"{"summary":"did x","tab_emoji":"🧾"}}"#,
            "Here you go:\n```json\n{\"summary\":\"did x\",\"tab_emoji\":\"🧾\"}\n```",
        ] {
            assert_eq!(extract_json_object(text), Some(want.clone()), "{text}");
        }
        assert_eq!(extract_json_object("no json here"), None);
        assert_eq!(extract_json_object("[1, 2]"), None);
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

    // ── Foreign thought-signature guard: shared matcher/stripper ─────────
    // Per-connector wording/serialization tests live in gemini.rs and
    // claude.rs; these cover the pure, provider-agnostic pieces.

    #[test]
    fn gemini_matcher_requires_400_and_signature_plus_thought_or_invalid_argument() {
        assert!(is_gemini_thought_signature_error(
            400,
            r#"{"error":{"message":"Invalid thought signature provided.","status":"INVALID_ARGUMENT"}}"#
        ));
        // Wrong status code entirely.
        assert!(!is_gemini_thought_signature_error(
            500,
            r#"{"error":{"message":"Invalid thought signature provided.","status":"INVALID_ARGUMENT"}}"#
        ));
        // 400 but no signature wording at all.
        assert!(!is_gemini_thought_signature_error(
            400,
            r#"{"error":{"message":"The model is overloaded.","status":"INVALID_ARGUMENT"}}"#
        ));
    }

    #[test]
    fn claude_matcher_requires_400_invalid_request_error_signature_and_thinking() {
        assert!(is_claude_thought_signature_error(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"thinking block signature mismatch"}}"#
        ));
        assert!(!is_claude_thought_signature_error(
            401,
            r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
        ));
        // invalid_request_error mentioning signature but not thinking (e.g. an
        // unrelated field) must not match.
        assert!(!is_claude_thought_signature_error(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"webhook signature header missing"}}"#
        ));
    }

    #[test]
    fn strip_thought_signatures_is_a_no_op_on_text_and_tool_result_blocks() {
        let mut messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    name: "search".into(),
                    content: serde_json::json!({"ok": true}),
                },
                ContentBlock::Image {
                    mime_type: "image/png".into(),
                    data: "base64data".into(),
                },
            ],
        }];
        let before = messages.clone();
        strip_thought_signatures(&mut messages);
        assert_eq!(messages, before, "non-signature-bearing blocks must be untouched");
    }
}
