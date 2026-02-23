use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::components::shared::{StreamMessage, ToolCall};
use crate::context::prompt_builder::LlmPrompt;
use crate::mcp::manager::McpContext;
use crate::settings::GeminiConfig;

use crate::session::Tool;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GeminiRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<SystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    pub function_calling_config: FunctionCallingConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCallingConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_function_names: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCallPart {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponsePart {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum Part {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
    },
    FunctionCall {
        function_call: FunctionCallPart,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        function_response: FunctionResponsePart,
    },
    InlineData {
        inline_data: InlineDataPart,
    },
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InlineDataPart {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Deserialize, Debug)]
pub struct GeminiErrorResponse {
    pub error: GeminiError,
}

#[derive(Deserialize, Debug)]
pub struct GeminiError {
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SystemInstruction {
    pub parts: Vec<Part>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GeminiResponse {
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

/// Usage metadata from Gemini API response
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    pub prompt_token_count: i32,
    #[serde(default)]
    pub candidates_token_count: Option<i32>,
    pub total_token_count: i32,
    #[serde(default)]
    pub thoughts_token_count: Option<i32>,
    #[serde(default)]
    pub cached_content_token_count: Option<i32>,
}

/// Supported Gemini Models for Pricing
#[derive(Debug, PartialEq, Clone)]
pub enum GeminiModel {
    Gemini3_1ProPreview,
    Gemini3_0ProPreview,
    Gemini3_0FlashPreview,
    Gemini2_5Pro,
    Gemini2_5Flash,
    Gemini2_5FlashLite,
    Gemini2_5ComputerUsePreview,
    Gemini2_0Flash,
    Gemini2_0FlashLite,
    Gemini2_0FlashThinking,

    Gemma3,
    NanoBanana,
    NanoBananaPro,
    Unknown(String),
}

impl GeminiModel {
    pub fn from_slug(slug: &str) -> Self {
        // Strip optional "models/" prefix if present, though usually handled by caller
        let s = slug.strip_prefix("models/").unwrap_or(slug);

        match s {
            // Gemini 3 Series - STRICT PREFIX MATCHING FIRST
            _ if s.starts_with("gemini-3.1-pro") => GeminiModel::Gemini3_1ProPreview,
            _ if s.starts_with("gemini-3.0-pro") || s.starts_with("gemini-3-pro") => {
                GeminiModel::Gemini3_0ProPreview
            }
            _ if s.starts_with("gemini-3.0-flash") || s.starts_with("gemini-3-flash") => {
                GeminiModel::Gemini3_0FlashPreview
            }
            "deep-research-pro-preview-dec-12-2025" => GeminiModel::Gemini3_0ProPreview,

            // Gemini 2.5 Series
            "gemini-2.5-pro" | "gemini-2.5-pro-preview-tts" => GeminiModel::Gemini2_5Pro,
            "gemini-2.5-flash"
            | "gemini-2.5-flash-preview-sep-2025"
            | "gemini-2.5-flash-preview-tts" => GeminiModel::Gemini2_5Flash,
            "gemini-2.5-flash-lite" | "gemini-2.5-flash-lite-preview-sep-2025" => {
                GeminiModel::Gemini2_5FlashLite
            }
            "gemini-2.5-computer-use-preview-10-2025" => GeminiModel::Gemini2_5ComputerUsePreview,

            // Gemini 2.0 Series
            "gemini-2.0-flash"
            | "gemini-2.0-flash-001"
            | "gemini-2.0-flash-experimental"
            | "gemini-2.0-flash-image-generation-experimental"
            | "gemini-experimental-1206" => GeminiModel::Gemini2_0Flash,
            "gemini-2.0-flash-lite"
            | "gemini-2.0-flash-lite-001"
            | "gemini-2.0-flash-lite-preview"
            | "gemini-2.0-flash-lite-preview-02-05" => GeminiModel::Gemini2_0FlashLite,
            "gemini-2.0-flash-thinking-exp-01-21" | "gemini-2.0-flash-thinking-exp" => {
                GeminiModel::Gemini2_0FlashThinking
            }

            // Gemini 1.5 Series (DEPRECATED: Map to 2.x equivalents)
            "gemini-pro-latest"
            | "gemini-1.5-pro"
            | "gemini-1.5-pro-latest"
            | "gemini-robotics-er-1.5-preview" => GeminiModel::Gemini2_5Pro,
            "gemini-flash-latest" | "gemini-1.5-flash" | "gemini-1.5-flash-latest" => {
                GeminiModel::Gemini2_0Flash
            }
            "gemini-flash-lite-latest" | "gemini-1.5-flash-8b" => GeminiModel::Gemini2_0FlashLite,

            // Nano & Gemma
            "nano-banana" => GeminiModel::NanoBanana,
            "nano-banana-pro" => GeminiModel::NanoBananaPro,

            // Strict match for Gemma3 variants
            _ if s.starts_with("gemma-3") || s.starts_with("gemma-3n") => GeminiModel::Gemma3,

            // Legacy Prefix matching (Priority is strictly below Gemini 3 specific checks)
            _ if s.starts_with("gemini-2.5-pro") => GeminiModel::Gemini2_5Pro,
            _ if s.starts_with("gemini-2.5-flash") => GeminiModel::Gemini2_5Flash,

            // Fallback Heuristics for Unknown/New Models
            _ if s.contains("thinking") => GeminiModel::Gemini2_0FlashThinking,
            _ if s.contains("nano") => GeminiModel::NanoBanana,
            _ if s.contains("gemma") => GeminiModel::Gemma3,
            _ if s.contains("flash-lite") => GeminiModel::Gemini2_0FlashLite, // assume cheap
            _ if s.contains("flash") => GeminiModel::Gemini2_0Flash,          // assume mid-tier
            // Jan 2026: 1.5 deprecated, use 2.5 Pro (has thinking budget support)
            _ if s.contains("pro") => GeminiModel::Gemini2_5Pro,

            _ => {
                tracing::warn!("Unknown Gemini model slug encountered: '{}'. Defaulting to Gemini 2.0 Flash pricing.", s);
                GeminiModel::Unknown(s.to_string())
            }
        }
    }

    pub fn get_rates(&self, prompt_tokens: i32) -> (f64, f64) {
        match self {
            GeminiModel::Gemini3_1ProPreview | GeminiModel::Gemini3_0ProPreview => {
                if prompt_tokens > 200_000 {
                    (4.00, 18.00)
                } else {
                    (2.00, 12.00)
                }
            }
            GeminiModel::Gemini3_0FlashPreview => (0.50, 3.00),

            GeminiModel::Gemini2_5Pro => (1.25, 10.00),
            GeminiModel::Gemini2_5Flash => (0.15, 0.60), // Updated to correct rates: $0.15 Input, $0.60 Output
            GeminiModel::Gemini2_5FlashLite => (0.10, 0.40),
            GeminiModel::Gemini2_5ComputerUsePreview => {
                if prompt_tokens > 200_000 {
                    (2.50, 15.00)
                } else {
                    (1.25, 10.00)
                }
            }

            GeminiModel::Gemini2_0Flash | GeminiModel::Gemini2_0FlashThinking => (0.10, 0.40),
            GeminiModel::Gemini2_0FlashLite => (0.075, 0.30),

            GeminiModel::Gemma3 | GeminiModel::NanoBanana | GeminiModel::NanoBananaPro => {
                (0.00, 0.00)
            }

            GeminiModel::Unknown(_) => {
                // Safety Default: Gemini 2.0 Flash (1.5 deprecated Jan 2026)
                (0.10, 0.40)
            }
        }
    }
}

/// Calculate cost in USD based on model and token usage.
/// Pricing per million tokens for Gemini models (as of Jan 2026).
/// Handles dynamic rates for Thinking Mode and Context Caching.
pub fn calculate_cost(model: &str, usage: &UsageMetadata) -> f64 {
    let gemini_model = GeminiModel::from_slug(model);
    let (input_rate, mut output_rate) = gemini_model.get_rates(usage.prompt_token_count);

    // Safety check for cached tokens
    let cached_tokens = usage.cached_content_token_count.unwrap_or(0);
    // prompt_token_count includes cached tokens, so we determine standard input tokens by subtracting
    let standard_input_tokens = (usage.prompt_token_count - cached_tokens).max(0);

    // Cached content is typically ~25% of the standard input rate
    let cached_rate = input_rate * 0.25;

    let input_cost = (standard_input_tokens as f64 / 1_000_000.0) * input_rate;
    let cached_cost = (cached_tokens as f64 / 1_000_000.0) * cached_rate;

    // Check for Thinking Mode Surcharges (Gemini 2.5 Flash)
    if let GeminiModel::Gemini2_5Flash = gemini_model {
        if usage.thoughts_token_count.unwrap_or(0) > 0 {
            output_rate = 3.50; // Thinking Mode Output Rate
        }
    }

    let completion_tokens = usage.candidates_token_count.unwrap_or(0);
    let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_rate;

    input_cost + cached_cost + output_cost
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: ContentResponse,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ContentResponse {
    #[serde(default)]
    pub parts: Vec<PartResponse>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    pub name: String,
    pub args: serde_json::Value,
    pub thought_signature: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PartResponse {
    #[serde(default)]
    pub text: String,
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default)]
    pub thought: Option<bool>,
}

impl From<PartResponse> for Part {
    fn from(resp: PartResponse) -> Self {
        if let Some(fc) = resp.function_call {
            Part::FunctionCall {
                function_call: FunctionCallPart {
                    name: fc.name,
                    args: fc.args,
                },
                thought_signature: fc.thought_signature.or(resp.thought_signature),
            }
        } else {
            Part::Text {
                text: resp.text,
                thought: resp.thought,
            }
        }
    }
}

#[async_trait]
pub trait LlmConnector: Send + Sync {
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        mcp_context: Option<McpContext>,
    );

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;
}

pub struct GeminiConnector {
    config: GeminiConfig,
    base_url: String,
}

/// Thinking configuration variants per Google AI docs
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThinkingConfigStyle {
    /// Gemini 3.x Pro - uses thinkingLevel: low|high
    LevelPro,
    /// Gemini 3.x Flash - uses thinkingLevel: minimal|low|medium|high
    LevelFlash,
    /// Gemini 2.x - uses thinkingBudget (token count)
    Budget,
    /// Not supported (1.x and older)
    None,
}

impl GeminiModel {
    /// Returns true if this model supports thinking/reasoning features
    /// Source: <https://ai.google.dev/gemini-api/docs/thinking>
    #[allow(dead_code)] // Used in tests and available for future UI integration
    pub fn supports_thinking(&self) -> bool {
        !matches!(self.thinking_config_style(), ThinkingConfigStyle::None)
    }

    /// Returns the thinking config style for this model
    pub fn thinking_config_style(&self) -> ThinkingConfigStyle {
        match self {
            GeminiModel::Gemini3_1ProPreview | GeminiModel::Gemini3_0ProPreview => ThinkingConfigStyle::LevelPro,
            GeminiModel::Gemini3_0FlashPreview | GeminiModel::Gemini2_0FlashThinking => {
                ThinkingConfigStyle::LevelFlash
            }
            GeminiModel::Gemini2_5Pro
            | GeminiModel::Gemini2_5Flash
            | GeminiModel::Gemini2_5FlashLite
            | GeminiModel::Gemini2_5ComputerUsePreview => ThinkingConfigStyle::Budget,
            _ => ThinkingConfigStyle::None,
        }
    }

    /// Returns the official human-readable name for this model
    pub fn display_name(&self) -> String {
        match self {
            GeminiModel::Gemini3_1ProPreview => "Gemini 3.1 Pro".to_string(),
            GeminiModel::Gemini3_0ProPreview => "Gemini 3 Pro".to_string(),
            GeminiModel::Gemini3_0FlashPreview => "Gemini 3 Flash".to_string(),
            GeminiModel::Gemini2_5Pro => "Gemini 2.5 Pro".to_string(),
            GeminiModel::Gemini2_5Flash => "Gemini 2.5 Flash".to_string(),
            GeminiModel::Gemini2_5FlashLite => "Gemini 2.5 Flash Lite".to_string(),
            GeminiModel::Gemini2_0Flash => "Gemini 2.0 Flash".to_string(),
            GeminiModel::Gemini2_0FlashLite => "Gemini 2.0 Flash Lite".to_string(),
            GeminiModel::Gemini2_0FlashThinking => "Gemini 2.0 Flash Thinking".to_string(),
            GeminiModel::Gemini2_5ComputerUsePreview => "Gemini 2.5 Computer Use".to_string(),
            GeminiModel::NanoBanana => "Nano Banana (Image · Planned)".to_string(),
            GeminiModel::NanoBananaPro => "Nano Banana Pro (Image · Planned)".to_string(),
            GeminiModel::Gemma3 => "Gemma 3".to_string(),
            GeminiModel::Unknown(slug) => {
                // Fallback for unknown slugs: 
                // 1. Strip models/ prefix
                // 2. Replace - and _ with space
                // 3. Title case words
                let clean_slug = slug.strip_prefix("models/").unwrap_or(slug);
                clean_slug
                    .replace("-", " ")
                    .replace("_", " ")
                    .split_whitespace()
                    .map(|word| {
                        let mut c = word.chars();
                        match c.next() {
                            None => String::new(),
                            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        }
                    })
                    .collect::<Vec<String>>()
                    .join(" ")
            }
        }
    }

    /// Valid thinking levels for Flash 3 (Pro only supports low/high)
    pub fn valid_thinking_levels(&self) -> &'static [&'static str] {
        match self.thinking_config_style() {
            ThinkingConfigStyle::LevelPro => &["low", "high"],
            ThinkingConfigStyle::LevelFlash => &["minimal", "low", "medium", "high"],
            _ => &[],
        }
    }

    /// Returns the canonical API slug for this model.
    /// This is the authoritative source for model identifiers sent to the API.
    pub fn canonical_slug(&self) -> &str {
        match self {
            GeminiModel::Gemini3_1ProPreview => "gemini-3.1-pro-preview",
            GeminiModel::Gemini3_0ProPreview => "gemini-3-pro-preview",
            GeminiModel::Gemini3_0FlashPreview => "gemini-3-flash-preview",
            GeminiModel::Gemini2_5Pro => "gemini-2.5-pro",
            GeminiModel::Gemini2_5Flash => "gemini-2.5-flash",
            GeminiModel::Gemini2_5FlashLite => "gemini-2.5-flash-lite",
            GeminiModel::Gemini2_5ComputerUsePreview => "gemini-2.5-computer-use-preview-10-2025",
            GeminiModel::Gemini2_0Flash => "gemini-2.0-flash",
            GeminiModel::Gemini2_0FlashLite => "gemini-2.0-flash-lite",
            GeminiModel::Gemini2_0FlashThinking => "gemini-2.0-flash-thinking-exp",
            GeminiModel::Gemma3 => "gemma-3",
            GeminiModel::NanoBanana => "nano-banana",
            GeminiModel::NanoBananaPro => "nano-banana-pro",
            GeminiModel::Unknown(slug) => slug,
        }
    }

    /// Returns the API version this model requires.
    /// Centralizes version routing so model-specific overrides are trivial.
    pub fn api_version(&self) -> &'static str {
        // Currently all models use v1beta.
        // When Google migrates models to v1 or v1alpha, update here.
        "v1beta"
    }
}

impl GeminiConnector {
    pub fn new(config: GeminiConfig) -> Self {
        Self {
            config,
            base_url: "https://generativelanguage.googleapis.com".to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Helper to build the correct API endpoint for a given model.
    /// Derives the API version dynamically from the model via struct-based authority.
    /// If model name already includes "models/" prefix (from API), use it directly.
    /// Otherwise, prepend "models/" for backward compatibility.
    fn build_model_endpoint(&self, model: &str, action: &str, api_key: &str) -> String {
        let gemini_model = GeminiModel::from_slug(model);
        let api_version = gemini_model.api_version();
        let model_path = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{}", model)
        };
        format!(
            "{}/{}/{}:{}?key={}",
            self.base_url, api_version, model_path, action, api_key
        )
    }

    /// Select the most useful tools from a large toolkit using LLM
    pub async fn select_tools_for_toolkit(
        &self,
        request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<
        crate::mcp::tool_selection::ToolSelectionResponse,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        use crate::mcp::tool_selection::{build_selection_prompt, parse_selection_response};

        tracing::info!(
            model = %self.config.summary_model,
            toolkit = %request.toolkit_name,
            tool_count = %request.available_tools.len(),
            max_tools = %request.max_tools,
            "LLM: Selecting tools for toolkit"
        );

        let api_key = match self.config.api_key.clone() {
            Some(key) => key,
            None => match std::env::var("GEMINI_API_KEY") {
                Ok(key) => key,
                Err(_) => {
                    tracing::warn!("Skipping tool selection: GEMINI_API_KEY not set");
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "GEMINI_API_KEY not configured",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>);
                }
            },
        };

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");

        let prompt = build_selection_prompt(request);

        let request_body = GeminiRequest {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: prompt,
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
            tool_config: None,
            generation_config: None,
        };

        let url =
            self.build_model_endpoint(&self.config.summary_model, "generateContent", &api_key);

        let response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            tracing::error!("Gemini API Error [{}]: {}", status, body_text);
            return Err(Box::new(std::io::Error::other(format!(
                "API request failed with status {}: {}",
                status, body_text
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: GeminiResponse = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if let Some(candidate) = response_json.candidates.first() {
            if let Some(part) = candidate.content.parts.first() {
                tracing::debug!("Raw LLM tool selection response: {}", part.text);

                match parse_selection_response(&part.text) {
                    Ok(selection) => {
                        tracing::info!(
                            "Tool selection complete: {} tools selected - {}",
                            selection.selected_tools.len(),
                            selection.reasoning
                        );
                        return Ok(selection);
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse tool selection response: {}", e);
                        return Err(
                            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                                as Box<dyn std::error::Error + Send + Sync>,
                        );
                    }
                }
            }
        }

        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No response from LLM for tool selection",
        )) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Generate content (non-streaming)
    pub async fn generate_content(
        &self,
        request: GeminiRequest,
    ) -> Result<GeminiResponse, Box<dyn std::error::Error + Send + Sync>> {
        let api_key = match self.config.api_key.clone() {
            Some(key) => key,
            None => match std::env::var("GEMINI_API_KEY") {
                Ok(key) => key,
                Err(_) => {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "GEMINI_API_KEY not configured",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>);
                }
            },
        };

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();

        let mut model = self.config.chat_model.clone();
        if model.starts_with("models/") {
            model = model.strip_prefix("models/").unwrap().to_string();
        }

        // Use the helper for standardizing
        let url = self.build_model_endpoint(&model, "generateContent", &api_key);

        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();

            // Try to parse structured error
            if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
                return Err(Box::new(std::io::Error::other(format!(
                    "Gemini API Error [{}]: {}",
                    status, error_response.error.message
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            } else {
                return Err(Box::new(std::io::Error::other(format!(
                    "Gemini API Error [{}]: {}",
                    status, body_text
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
        }

        let response_json: GeminiResponse = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        Ok(response_json)
    }
}

#[async_trait]
impl LlmConnector for GeminiConnector {
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        mcp_context: Option<McpContext>,
    ) {
        let api_key = match self.config.api_key.clone() {
            Some(key) => key,
            None => match std::env::var("GEMINI_API_KEY") {
                Ok(key) => key,
                Err(_) => {
                    tracing::error!("GEMINI_API_KEY not set in settings or environment");
                    let _ = tx.send(StreamMessage::Error {
                        message: "⚠️ **API Key Not Configured**\n\nPlease set your Gemini API key in Settings → AI Model to use Hobbes.".to_string(),
                    });
                    return;
                }
            },
        };
        let mut model = self.config.chat_model.clone();
        if model.starts_with("models/") {
            model = model.strip_prefix("models/").unwrap().to_string();
        }

        tracing::info!(model = %model, "LLM: Generating content stream");

        const MAX_RETRIES: u32 = 2;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("Failed to build reqwest client");

        // Build thinking config based on model and settings
        let generation_config = if self.config.thinking_enabled {
            let gemini_model = GeminiModel::from_slug(&model);
            match gemini_model.thinking_config_style() {
                ThinkingConfigStyle::LevelPro | ThinkingConfigStyle::LevelFlash => {
                    // Validate level is supported for this model
                    let level = if gemini_model
                        .valid_thinking_levels()
                        .contains(&self.config.thinking_level.as_str())
                    {
                        self.config.thinking_level.clone()
                    } else {
                        "high".to_string() // Default fallback
                    };
                    Some(ThinkingConfig {
                        thinking_level: Some(level),
                        thinking_budget: None,
                        include_thoughts: Some(true),
                    })
                }
                ThinkingConfigStyle::Budget => Some(ThinkingConfig {
                    thinking_level: None,
                    thinking_budget: self.config.thinking_budget,
                    include_thoughts: Some(true),
                }),
                ThinkingConfigStyle::None => None,
            }
            .map(|tc| GenerationConfig {
                thinking_config: Some(tc),
            })
        } else {
            None
        };

        let mut request_body = GeminiRequest {
            contents: prompt_data.contents,
            tools: prompt_data.tools.clone(),
            system_instruction: prompt_data.system_instruction,
            tool_config: if prompt_data.tools.is_some() {
                Some(ToolConfig {
                    function_calling_config: FunctionCallingConfig {
                        mode: "AUTO".to_string(),
                        allowed_function_names: None,
                    },
                })
            } else {
                None
            },
            generation_config,
        };

        // --- Synchronous Logging Block ---
        {
            // Create a sanitized version of the request for logging
            let mut sanitized_request = request_body.clone();
            for content in &mut sanitized_request.contents {
                for part in &mut content.parts {
                    if let Part::InlineData { inline_data } = part {
                        let original_len = inline_data.data.len();
                        if original_len > 100 {
                            let truncated_data =
                                inline_data.data.chars().take(50).collect::<String>();
                            inline_data.data =
                                format!("[{} bytes, truncated]...{}", original_len, truncated_data);
                        }
                    }
                }
            }
            tracing::debug!("Sending Gemini request: {:?}", sanitized_request);

            // Write the full request (with tools) to debug file for diagnosis
            if tracing::enabled!(tracing::Level::DEBUG) {
                if let Ok(request_json) = serde_json::to_string_pretty(&request_body) {
                    let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
                    if std::fs::create_dir_all(&debug_dir).is_ok() {
                        let file_path = debug_dir.join("gemini_request.json");
                        if let Err(e) = std::fs::write(&file_path, &request_json) {
                            tracing::warn!("Failed to write Gemini debug file: {}", e);
                        } else {
                            tracing::debug!("Wrote Gemini request to {:?}", file_path);
                        }
                    }
                }
            }
            tracing::info!("Using chat model: {}", model);
        }
        // --- End Synchronous Logging Block ---

        // --- End Synchronous Logging Block ---

        let url =
            self.build_model_endpoint(&model, "streamGenerateContent", &api_key) + "&alt=sse";

        for attempt in 0..MAX_RETRIES {
            let response = match client.post(&url).json(&request_body).send().await {
                Ok(r) => r,
                Err(e) => {
                    let error_msg = if e.is_timeout() {
                        tracing::error!(
                            "Gemini API Request TIMED OUT on attempt {}. Duration: 600s",
                            attempt + 1
                        );
                        format!(
                            "Request timed out after 600 seconds (attempt {}/{})",
                            attempt + 1,
                            MAX_RETRIES
                        )
                    } else {
                        tracing::error!("Error sending request on attempt {}: {}", attempt + 1, e);
                        format!(
                            "Network error: {} (attempt {}/{})",
                            e,
                            attempt + 1,
                            MAX_RETRIES
                        )
                    };

                    if attempt + 1 == MAX_RETRIES {
                        let _ = tx.send(StreamMessage::Error {
                            message: format!(
                                "Failed to connect to Gemini API after {} attempts. {}",
                                MAX_RETRIES, error_msg
                            ),
                        });
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Failed to read error body".to_string());
                let error_message = if let Ok(error_response) =
                    serde_json::from_str::<GeminiErrorResponse>(&body_text)
                {
                    tracing::error!(
                        "Gemini API Error [{}]: {}",
                        status,
                        error_response.error.message
                    );
                    format!(
                        "Gemini API Error [{}]: {}",
                        status, error_response.error.message
                    )
                } else {
                    tracing::error!("Gemini API Error [{}]: {}", status, body_text);
                    format!("Gemini API Error [{}]: {}", status, body_text)
                };

                let _ = tx.send(StreamMessage::Error {
                    message: error_message,
                });
                return;
            }

            let mut stream = response.bytes_stream();
            let mut has_sent_data = false;
            let mut tool_not_found_count: u32 = 0;
            let mut finish_reason: Option<String> = None;
            let mut buffer = Vec::<u8>::new();
            let mut malformed_call_detected = false;
            let mut unexpected_tool_call_detected = false;
            let mut current_attempt_parts = Vec::<Part>::new();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => {
                        buffer.extend_from_slice(&bytes);
                        while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
                            let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
                            let line = String::from_utf8_lossy(&line_bytes).trim().to_string();

                            if let Some(json_str) = line.strip_prefix("data: ") {
                                if json_str.is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<GeminiResponse>(json_str) {
                                    Ok(parsed) => {
                                        if let Some(candidate) = parsed.candidates.first() {
                                            if let Some(reason) = &candidate.finish_reason {
                                                finish_reason = Some(reason.clone());
                                                if reason == "MALFORMED_FUNCTION_CALL" {
                                                    tracing::warn!("Malformed function call detected on attempt {}. Retrying...", attempt + 1);
                                                    malformed_call_detected = true;
                                                    break; // Break from inner while to retry
                                                }
                                                if reason == "UNEXPECTED_TOOL_CALL" {
                                                    tracing::warn!("Unexpected tool call detected on attempt {}. Retrying with correction...", attempt + 1);
                                                    unexpected_tool_call_detected = true;
                                                    break; // Break from inner while to retry
                                                }
                                                if reason != "STOP" {
                                                    tracing::warn!(
                                                        "Gemini stream finished with reason: {}",
                                                        reason
                                                    );
                                                }
                                            }
                                            // Process ALL parts - Gemini with thinking returns multiple parts
                                            // (thought parts AND content parts) in a single response
                                            for part in &candidate.content.parts {
                                                current_attempt_parts
                                                    .push(Part::from(part.clone()));
                                                // Check if this is a thought summary part first
                                                if part.thought.unwrap_or(false)
                                                    && !part.text.is_empty()
                                                {
                                                    // This is a thought summary - send it as thought_summary
                                                    if tx
                                                        .send(StreamMessage::Text {
                                                            content: String::new(),
                                                            thought_signature: None,
                                                            thought_summary: Some(
                                                                part.text.clone(),
                                                            ),
                                                        })
                                                        .is_err()
                                                    {
                                                        return;
                                                    }
                                                    has_sent_data = true;
                                                } else if let Some(function_call) =
                                                    &part.function_call
                                                {
                                                    // Log raw JSON if it contains a function call
                                                    tracing::debug!(
                                                        "Raw JSON with function call: {}",
                                                        json_str
                                                    );

                                                    // Log the thought_signature field for debugging
                                                    if let Some(ref thought_sig) =
                                                        part.thought_signature
                                                    {
                                                        tracing::info!("Received function call '{}' with thought_signature: '{}'",
                                                        function_call.name,
                                                        if thought_sig.len() > 50 { &thought_sig[..50] } else { thought_sig }
                                                    );
                                                    } else {
                                                        tracing::warn!("Received function call '{}' WITHOUT thought_signature field", function_call.name);
                                                    }

                                                    let mut found_tool = false;

                                                    // Note: composio_meta routing removed - Tool Router handles on-demand tools
                                                    if let Some(context) = &mcp_context {
                                                        'server_loop: for server in &context.servers
                                                        {
                                                            for tool in &server.tools {
                                                                let sanitized_tool_name = crate::gemini::convert::get_prefixed_tool_name(&server.name, &tool.name);
                                                                if sanitized_tool_name
                                                                    == function_call.name
                                                                {
                                                                    let tool_call = ToolCall::new(
                                                                        server.name.clone(),
                                                                        tool.name.to_string(), // Use original tool name for execution
                                                                        function_call.args.clone(),
                                                                        part.thought_signature
                                                                            .clone()
                                                                            .or(function_call
                                                                                .thought_signature
                                                                                .clone()),
                                                                        None, // thought_summary will be populated by stream_manager
                                                                    );
                                                                    if tx
                                                                        .send(
                                                                            StreamMessage::ToolCall(
                                                                                tool_call,
                                                                            ),
                                                                        )
                                                                        .is_err()
                                                                    {
                                                                        return;
                                                                    }
                                                                    has_sent_data = true;
                                                                    found_tool = true;
                                                                    break 'server_loop;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if !found_tool {
                                                        tool_not_found_count += 1;
                                                        tracing::error!("LLM requested tool '{}' which was not found in the provided context (count: {}).", function_call.name, tool_not_found_count);
                                                        // After repeated failures, emit the persistent error message that triggers QuickFix buttons
                                                            let tool_error_msg = if tool_not_found_count >= 2 {
                                                            "[Hobbes encountered a persistent error ('TOOL_NOT_FOUND') after multiple retries. The model may be hallucinating a tool that does not exist.]".to_string()
                                                        } else {
                                                            format!(
                                                            "⚠️ **Tool Not Available: `{}`**\n\n\
                                                            Hobbes tried to use a tool that isn't currently loaded. This can happen if:\n\n\
                                                            • The MCP server providing this tool is not running\n\
                                                            • The tool requires authentication that hasn't been set up\n\
                                                            • The tool list needs to be refreshed\n\n\
                                                            Please check your MCP Integration settings.",
                                                            function_call.name)
                                                        };
                                                        if tx
                                                            .send(StreamMessage::Text {
                                                                content: tool_error_msg,
                                                                thought_signature: None,
                                                                thought_summary: None,
                                                            })
                                                            .is_err()
                                                        {
                                                            return;
                                                        }
                                                        has_sent_data = true;
                                                        // Circuit breaker: stop the stream after persistent tool-not-found
                                                        if tool_not_found_count >= 2 {
                                                            return;
                                                        }
                                                    }
                                                } else if !part.text.is_empty() {
                                                    // Check if the text is structured JSON that needs unwrapping
                                                    let (content, thought_summary) =
                                                        if part.text.trim().starts_with('{')
                                                            && part.text.trim().ends_with('}')
                                                        {
                                                            unparse_json_response(&part.text)
                                                        } else {
                                                            (part.text.clone(), None)
                                                        };

                                                    if !content.is_empty()
                                                        || thought_summary.is_some()
                                                    {
                                                        if tx
                                                            .send(StreamMessage::Text {
                                                                content,
                                                                thought_signature: None,
                                                                thought_summary,
                                                            })
                                                            .is_err()
                                                        {
                                                            return;
                                                        }
                                                        has_sent_data = true;
                                                    }
                                                }
                                            }
                                        }

                                        // Send usage data if present
                                        if let Some(usage) = &parsed.usage_metadata {
                                            let cost = calculate_cost(&model, usage);
                                            let usage_data = crate::components::shared::UsageData {
                                                prompt_tokens: usage.prompt_token_count,
                                                completion_tokens: usage
                                                    .candidates_token_count
                                                    .unwrap_or(0),
                                                total_tokens: usage.total_token_count,
                                                thoughts_tokens: usage.thoughts_token_count,
                                                cached_content_tokens: usage
                                                    .cached_content_token_count,
                                                cost: Some(cost),
                                            };
                                            if tx.send(StreamMessage::Usage(usage_data)).is_err() {
                                                tracing::warn!(
                                                    "Failed to send usage data to stream"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to parse JSON chunk from stream: {}. Chunk: '{}'", e, json_str);
                                        // Check if this is a malformed call finish reason
                                        if json_str.contains("MALFORMED_FUNCTION_CALL") {
                                            tracing::warn!("Malformed function call detected via string search on attempt {}. Retrying...", attempt + 1);
                                            malformed_call_detected = true;
                                            break; // Break from inner while to retry
                                        }
                                        // Check if this is an unexpected tool call finish reason
                                        if json_str.contains("UNEXPECTED_TOOL_CALL") {
                                            tracing::warn!("Unexpected tool call detected via string search on attempt {}. Retrying with correction...", attempt + 1);
                                            unexpected_tool_call_detected = true;
                                            break; // Break from inner while to retry
                                        }
                                        let error_message = "[Hobbes encountered a stream error. Please check the logs for details.]";
                                        if tx
                                            .send(StreamMessage::Text {
                                                content: error_message.to_string(),
                                                thought_signature: None,
                                                thought_summary: None,
                                            })
                                            .is_err()
                                        {
                                            tracing::error!(
                                                "Failed to send stream error message to UI."
                                            );
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        if malformed_call_detected || unexpected_tool_call_detected {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error in stream: {}", e);
                        break;
                    }
                }
            }

            if malformed_call_detected || unexpected_tool_call_detected {
                if attempt + 1 < MAX_RETRIES {
                    tracing::warn!(
                        "Retry triggered for stream error (attempt {}/{}). Sleeping 1s...",
                        attempt + 1,
                        MAX_RETRIES
                    );

                    // Ground the model by adding its failed attempt and a correction to the context history
                    // This prevents "model myopia" where the model repeats the same hallucination.
                    if !current_attempt_parts.is_empty() {
                        request_body.contents.push(Content {
                            role: "model".to_string(),
                            parts: current_attempt_parts,
                        });

                        let correction_text = if unexpected_tool_call_detected {
                            // Generate a list of available tools to help the model correct itself
                            let available_tools_str = if let Some(context) = &mcp_context {
                                let mut tools = Vec::new();
                                for server in &context.servers {
                                    for tool in &server.tools {
                                        let sanitized_name =
                                            crate::gemini::convert::get_prefixed_tool_name(
                                                &server.name, &tool.name,
                                            );
                                        tools.push(format!("- {}", sanitized_name));
                                    }
                                }
                                if tools.is_empty() {
                                    "No tools are currently available.".to_string()
                                } else {
                                    format!("Available tools:\n{}", tools.join("\n"))
                                }
                            } else {
                                "No tools context available.".to_string()
                            };

                            format!("[System Note]: The previous generation failed because you attempted to call a tool that is not in the `tools` list. \n\n{}\n\nPlease verify the `tools` list and try again. Do not hallucinate function names. If you cannot perform the action with available tools, explain why.", available_tools_str)
                        } else {
                            "[System Note]: The previous generation failed because the function call was malformed. Please ensure your tool call matches the defined schema exactly.".to_string()
                        };

                        request_body.contents.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part::Text {
                                text: correction_text,
                                thought: None,
                            }],
                        });
                    }

                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue; // Go to the next iteration of the for loop
                } else {
                    tracing::error!(
                        "Stream error persisted after {} retries. Aborting.",
                        MAX_RETRIES
                    );
                    // Send an explicit failure message to the UI
                    if tx.send(StreamMessage::Text {
                    content: format!("[Hobbes encountered a persistent error ('{}') after multiple retries. The model may be hallucinating a tool that does not exist.]",
                        if unexpected_tool_call_detected { "UNEXPECTED_TOOL_CALL" } else { "MALFORMED_FUNCTION_CALL" }),
                    thought_signature: None,
                    thought_summary: None,
                }).is_err() {
                    tracing::error!("Failed to send final error message to UI.");
                }
                    // We return here to stop the stream.
                    // The outer 'if !has_sent_data' check might fire too if we didn't send anything earlier,
                    // but we just sent a message, so we should be good?
                    // Wait, 'has_sent_data' is local to the attempt loop? No, it's defined inside 'attempt' loop.
                    // So if we 'return', the task ends.
                    return;
                }
            }

            if !has_sent_data {
                tracing::error!(
                    "Gemini finished without sending data. Finish Reason: {:?}. Request was: {}",
                    finish_reason,
                    serde_json::to_string_pretty(&request_body).unwrap_or_default()
                );
                let default_message = match finish_reason.as_deref() {
                Some("SAFETY") => "[Hobbes did not provide a response due to the safety filter.]".to_string(),
                Some("UNEXPECTED_TOOL_CALL") => {
                    "⚠️ **Tool Connection Issue**\n\n\
                    Hobbes tried to use a tool that isn't currently available. Please check:\n\n\
                    1. **Is the MCP server running?** Open Settings → MCP Integration and verify the server status.\n\
                    2. **Is the tool connected?** For Composio tools, ensure the profile is active and connected.\n\
                    3. **Try refreshing** the tool list by toggling the MCP server off and on.\n\n\
                    If the issue persists, the tool may need to be re-authorized or the server restarted.".to_string()
                },
                Some("MALFORMED_FUNCTION_CALL") => {
                    "⚠️ **Tool Call Error**\n\n\
                    Hobbes encountered an issue formatting a tool request. This is usually temporary.\n\
                    Please try your request again.".to_string()
                },
                Some(reason) => format!(
                    "⚠️ **Response Issue**\n\n\
                    Hobbes could not complete the response.\n\
                    **Reason:** {}\n\n\
                    If this persists, try simplifying your request or checking your tool connections.",
                    reason
                ),
                None => "[Hobbes did not provide a response due to an internal error.]".to_string(),
            };
                if tx
                    .send(StreamMessage::Text {
                        content: default_message,
                        thought_signature: None,
                        thought_summary: None,
                    })
                    .is_err()
                {
                    tracing::error!("Failed to send default message to UI.");
                }
            }
            // If we've successfully processed the stream without a malformed call, break the retry loop.
            break;
        }
    }

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!(model = %self.config.summary_model, "LLM: Summarizing conversation");

        let api_key = match self.config.api_key.clone() {
            Some(key) => key,
            None => {
                match std::env::var("GEMINI_API_KEY") {
                    Ok(key) => key,
                    Err(_) => {
                        tracing::warn!("Skipping summarization: GEMINI_API_KEY not set in settings or environment");
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "GEMINI_API_KEY not configured",
                        ))
                            as Box<dyn std::error::Error + Send + Sync>);
                    }
                }
            }
        };
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("Failed to build reqwest client");

        let full_prompt = format!(
            r#"
You are an AI assistant that refines a conversation summary.
You will be given a previous summary (which may be empty) and the most recent messages in a conversation.
Your primary task is to integrate the new information from the recent messages into the previous summary, updating and extending it.
Preserve existing information while incorporating new facts, entities, or user preferences.

A crucial part of your task is to analyze the **sentiment and mood** of the user in the "Recent Messages".

Format your response as a single, clean JSON object with three keys: "summary", "entities", and "sentiment".
- "summary": A concise, updated summary of the entire conversation so far.
- "entities": An object containing all key-value pairs of extracted information. If the user mentions their name, be sure to extract it and include it as `{{\"user_name\": \"...\"}}` in this object.
- "sentiment": A brief string describing the user's current sentiment or mood (e.g., "curious and collaborative", "frustrated but focused", "pleased with the progress", "neutral"). This should reflect the feeling of the recent messages.

Previous Summary:
---
{}
---

Recent Messages:
---
{}
"#,
            previous_summary, recent_messages
        );

        let request_body = GeminiRequest {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: full_prompt,
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
            tool_config: None,
            generation_config: None,
        };

        tracing::info!("Using summary model: {}", self.config.summary_model);
        let url =
            self.build_model_endpoint(&self.config.summary_model, "generateContent", &api_key);

        let response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
                tracing::error!(
                    "Gemini API Error [{}]: {}",
                    status,
                    error_response.error.message
                );
            } else {
                tracing::error!("Gemini API Error [{}]: {}", status, body_text);
            }
            // Return a structured error instead of panicking or returning a generic reqwest::Error
            return Err(Box::new(std::io::Error::other(format!(
                "API request failed with status {}: {}",
                status, body_text
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: GeminiResponse = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if let Some(candidate) = response_json.candidates.first() {
            if let Some(part) = candidate.content.parts.first() {
                // The model's response is expected to be a JSON string.
                tracing::debug!("Raw LLM summary response: {}", part.text);

                // Attempt to parse the text directly as JSON.
                if let Ok(json_value) = serde_json::from_str(&part.text) {
                    return Ok(json_value);
                }

                // If direct parsing fails, try to extract it from a markdown code block.
                if let Some(start) = part.text.find('{') {
                    if let Some(end) = part.text.rfind('}') {
                        let potential_json = &part.text[start..=end];
                        if let Ok(json_value) = serde_json::from_str(potential_json) {
                            tracing::warn!("Successfully parsed JSON from markdown code block.");
                            return Ok(json_value);
                        }
                    }
                }

                // If all parsing fails, return the raw text as the summary.
                tracing::warn!(
                    "Failed to parse LLM response as JSON. Returning raw text as summary."
                );
                let fallback_json = serde_json::json!({
                    "summary": part.text,
                    "entities": {}
                });
                return Ok(fallback_json);
            }
        }

        Ok(serde_json::Value::Null)
    }
}

fn unparse_json_response(text: &str) -> (String, Option<String>) {
    // Attempt to parse the text directly as JSON.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(obj) = json.as_object() {
            // Check for common fields in the model's structured output
            let reply_text = obj
                .get("reply_text")
                .or_else(|| obj.get("content"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let thought = obj
                .get("thought")
                .or_else(|| obj.get("thought_summary"))
                .or_else(|| obj.get("action_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // If we found either a reply or a thought, consider it a successful unwrap
            if reply_text.is_some() || thought.is_some() {
                return (reply_text.unwrap_or_default(), thought);
            }
        }
    }

    // If not valid JSON or doesn't have the expected fields, return as-is
    (text.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_unparse_json_response() {
        // Test with reply_text and thought
        let text = r#"{"reply_text": "Hello world", "thought": "The user said hello"}"#;
        let (content, thought) = unparse_json_response(text);
        assert_eq!(content, "Hello world");
        assert_eq!(thought, Some("The user said hello".to_string()));

        // Test with content and action_name
        let text = r#"{"content": "Checking status", "action_name": "Checking system health"}"#;
        let (content, thought) = unparse_json_response(text);
        assert_eq!(content, "Checking status");
        assert_eq!(thought, Some("Checking system health".to_string()));

        // Test with invalid JSON
        let text = "Just some plain text";
        let (content, thought) = unparse_json_response(text);
        assert_eq!(content, "Just some plain text");
        assert!(thought.is_none());

        // Test with JSON but missing expected fields
        let text = r#"{"other_field": "some value"}"#;
        let (content, thought) = unparse_json_response(text);
        assert_eq!(content, text);
        assert!(thought.is_none());
    }

    #[tokio::test]
    async fn test_thinking_summary_parsing() {
        // Start a mock server
        let mock_server = MockServer::start().await;

        // Define the mock response body (SSE format)
        let thought_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "text": "I am thinking about the user's request.",
                        "thought": true
                    }]
                }
            }]
        });

        let content_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "text": "Here is the answer."
                    }]
                }
            }]
        });

        let response_body = format!(
            "data: {}\n\ndata: {}\n\n",
            thought_json,
            content_json
        );

        // Configure the mock server
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
            .mount(&mock_server)
            .await;

        // Configure GeminiConnector
        let config = GeminiConfig {
            api_key: Some("test-key".to_string()),
            chat_model: "gemini-2.5-pro".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            thinking_enabled: true,
            thinking_level: "high".to_string(),
            thinking_budget: Some(1024),
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        // Create prompt data
        let prompt_data = LlmPrompt {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "Hello".to_string(),
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
        };

        // Create channel
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Run generate_content_stream
        connector
            .generate_content_stream(prompt_data, tx, None)
            .await;

        // Verify results
        let mut thought_received = false;
        let mut content_received = false;

        while let Some(msg) = rx.recv().await {
            match msg {
                StreamMessage::Text {
                    content,
                    thought_summary,
                    ..
                } => {
                    if let Some(summary) = thought_summary {
                        assert_eq!(summary, "I am thinking about the user's request.");
                        thought_received = true;
                    }
                    if !content.is_empty() {
                        assert_eq!(content, "Here is the answer.");
                        content_received = true;
                    }
                }
                StreamMessage::Error { message } => {
                    panic!("Received error: {}", message);
                }
                _ => {}
            }
        }

        assert!(thought_received, "Did not receive thought summary");
        assert!(content_received, "Did not receive content");
    }

    #[tokio::test]
    async fn test_unexpected_tool_call_error_message() {
        let mock_server = MockServer::start().await;

        // Simulate a response with UNEXPECTED_TOOL_CALL finish reason and no content
        let response_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": []
                },
                "finishReason": "UNEXPECTED_TOOL_CALL"
            }]
        });

        let response_body = format!("data: {}\n\n", response_json);

        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
            .mount(&mock_server)
            .await;

        let config = GeminiConfig {
            api_key: Some("test-key".to_string()),
            chat_model: "gemini-2.5-pro".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            thinking_enabled: false,
            thinking_level: "high".to_string(),
            thinking_budget: None,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "Use a tool".to_string(),
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, None)
            .await;

        let mut received_error_guidance = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("[Hobbes encountered a persistent error")
                    && content.contains("UNEXPECTED_TOOL_CALL")
                {
                    received_error_guidance = true;
                }
            }
        }

        assert!(
            received_error_guidance,
            "Should receive persistent error message for UNEXPECTED_TOOL_CALL"
        );
    }

    #[tokio::test]
    async fn test_safety_filter_error_message() {
        let mock_server = MockServer::start().await;

        let response_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": []
                },
                "finishReason": "SAFETY"
            }]
        });

        let response_body = format!("data: {}\n\n", response_json);

        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
            .mount(&mock_server)
            .await;

        let config = GeminiConfig {
            api_key: Some("test-key".to_string()),
            chat_model: "gemini-2.5-pro".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            thinking_enabled: false,
            thinking_level: "high".to_string(),
            thinking_budget: None,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "Test".to_string(),
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, None)
            .await;

        let mut received_safety_message = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("safety filter") {
                    received_safety_message = true;
                }
            }
        }

        assert!(
            received_safety_message,
            "Should receive safety filter message"
        );
    }

    #[tokio::test]
    async fn test_tool_not_found_sends_user_message() {
        let mock_server = MockServer::start().await;

        // Response with a function call for a tool that won't be in the context
        let response_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "unknown_server_nonexistent_tool",
                            "args": {}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        });

        let response_body = format!("data: {}\n\n", response_json);

        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
            .mount(&mock_server)
            .await;

        let config = GeminiConfig {
            api_key: Some("test-key".to_string()),
            chat_model: "gemini-2.5-pro".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            thinking_enabled: false,
            thinking_level: "high".to_string(),
            thinking_budget: None,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "Use a tool".to_string(),
                    thought: None,
                }],
            }],
            tools: None,
            system_instruction: None,
        };

        // Pass an empty MCP context so the tool won't be found
        let mcp_context = Some(crate::mcp::manager::McpContext { servers: vec![], connected_toolkit_slugs: vec![] });

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, mcp_context)
            .await;

        let mut received_tool_error = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("Tool Not Available")
                    && content.contains("unknown_server_nonexistent_tool")
                {
                    received_tool_error = true;
                }
            }
        }

        assert!(
            received_tool_error,
            "Should receive tool not available error with tool name"
        );
    }

    #[test]
    fn test_calculate_cost() {
        // Helper to make usage metadata
        let make_usage = |prompt: i32, candidates: i32| UsageMetadata {
            prompt_token_count: prompt,
            candidates_token_count: Some(candidates),
            total_token_count: prompt + candidates,
            thoughts_token_count: None,
            cached_content_token_count: None,
        };

        // Flash 2.5: $0.15/1M input, $0.60/1M output
        let usage = make_usage(1_000_000, 0);
        let cost = calculate_cost("gemini-2.5-flash", &usage);
        assert!(
            (cost - 0.15).abs() < 1e-6,
            "Gemini 2.5 Flash Input: Expected $0.15, got {}",
            cost
        );

        let usage = make_usage(0, 1_000_000);
        let cost = calculate_cost("gemini-2.5-flash", &usage);
        assert!(
            (cost - 0.60).abs() < 1e-6,
            "Gemini 2.5 Flash Output: Expected $0.60, got {}",
            cost
        );

        // Pro 2.5 <= 200k: $1.25/1M input, $10.00/1M output
        let usage = make_usage(1_000_000, 0);
        let cost = calculate_cost("gemini-2.5-pro", &usage);
        assert!(
            (cost - 1.25).abs() < 1e-6,
            "Gemini 2.5 Pro Input: Expected $1.25, got {}",
            cost
        );

        // Fallback (unknown): Defaults to Gemini 2.0 Flash (Safety mechanism)
        // 1M tokens * $0.10 rates
        let usage = make_usage(1_000_000, 0);
        let cost = calculate_cost("unknown-model", &usage);
        assert!(
            (cost - 0.10).abs() < 1e-6,
            "Unknown model: Expected default $0.10 (Flash), got {}",
            cost
        );
    }

    #[test]
    fn test_parse_usage_metadata() {
        let json = r#"{
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150,
                "thoughtsTokenCount": 20
            }
        }"#;

        let response: GeminiResponse =
            serde_json::from_str(json).expect("Failed to parse GeminiResponse with usageMetadata");
        assert!(response.usage_metadata.is_some());
        let usage = response.usage_metadata.unwrap();
        assert_eq!(usage.prompt_token_count, 100);
        assert_eq!(usage.candidates_token_count, Some(50));
        assert_eq!(usage.total_token_count, 150);
        assert_eq!(usage.thoughts_token_count, Some(20));
    }

    #[test]
    fn test_supports_thinking() {
        assert!(GeminiModel::Gemini3_1ProPreview.supports_thinking());
        assert!(GeminiModel::Gemini3_0ProPreview.supports_thinking());
        assert!(GeminiModel::Gemini3_0FlashPreview.supports_thinking());
        assert!(GeminiModel::Gemini2_5Pro.supports_thinking());
        assert!(GeminiModel::Gemini2_5Flash.supports_thinking());
        assert!(GeminiModel::Gemini2_0FlashThinking.supports_thinking());
        assert!(!GeminiModel::Gemini2_0Flash.supports_thinking());
        assert!(!GeminiModel::Gemma3.supports_thinking());
    }

    #[test]
    fn test_thinking_config_style() {
        use ThinkingConfigStyle::*;
        assert_eq!(
            GeminiModel::Gemini3_1ProPreview.thinking_config_style(),
            LevelPro
        );
        assert_eq!(
            GeminiModel::Gemini3_0ProPreview.thinking_config_style(),
            LevelPro
        );
        assert_eq!(
            GeminiModel::Gemini3_0FlashPreview.thinking_config_style(),
            LevelFlash
        );
        // Experimental Flash Thinking 2.0 uses LevelFlash (minimal/low/medium/high)
        assert_eq!(
            GeminiModel::Gemini2_0FlashThinking.thinking_config_style(),
            LevelFlash
        );
        assert_eq!(GeminiModel::Gemini2_5Flash.thinking_config_style(), Budget);
        assert_eq!(GeminiModel::Gemini2_0Flash.thinking_config_style(), None);
        assert_eq!(GeminiModel::Gemma3.thinking_config_style(), None);
    }

    #[test]
    fn test_valid_thinking_levels() {
        assert_eq!(
            GeminiModel::Gemini3_1ProPreview.valid_thinking_levels(),
            &["low", "high"]
        );
        assert_eq!(
            GeminiModel::Gemini3_0ProPreview.valid_thinking_levels(),
            &["low", "high"]
        );
        assert_eq!(
            GeminiModel::Gemini3_0FlashPreview.valid_thinking_levels(),
            &["minimal", "low", "medium", "high"]
        );
        assert!(GeminiModel::Gemini2_0Flash
            .valid_thinking_levels()
            .is_empty());
        assert!(GeminiModel::Gemma3.valid_thinking_levels().is_empty());
    }

    #[test]
    fn test_from_slug_versioned_models() {
        // Gemini 3 versioned slugs should map correctly
        // Test Official API style (gemini-3.1-pro and gemini-3-pro)
        assert_eq!(
            GeminiModel::from_slug("gemini-3.1-pro-preview"),
            GeminiModel::Gemini3_1ProPreview
        );
        assert_eq!(
            GeminiModel::from_slug("gemini-3-pro-preview"),
            GeminiModel::Gemini3_0ProPreview
        );
        assert_eq!(
            GeminiModel::from_slug("gemini-3-flash-preview"),
            GeminiModel::Gemini3_0FlashPreview
        );

        let model = GeminiModel::from_slug("gemini-3.0-pro-preview-02-05");
        assert_eq!(model, GeminiModel::Gemini3_0ProPreview);
        assert!(model.supports_thinking());
        assert_eq!(model.thinking_config_style(), ThinkingConfigStyle::LevelPro);

        // With models/ prefix
        let model2 = GeminiModel::from_slug("models/gemini-3.0-pro-preview-02-05");
        assert_eq!(model2, GeminiModel::Gemini3_0ProPreview);

        // Flash versioned
        let flash = GeminiModel::from_slug("gemini-3.0-flash-preview-01-21");
        assert_eq!(flash, GeminiModel::Gemini3_0FlashPreview);
        assert_eq!(
            flash.thinking_config_style(),
            ThinkingConfigStyle::LevelFlash
        );

        // Gemini 2.0 Flash Thinking
        let ft = GeminiModel::from_slug("gemini-2.0-flash-thinking-exp-01-21");
        assert_eq!(ft, GeminiModel::Gemini2_0FlashThinking);
        assert!(ft.supports_thinking());
        assert_eq!(ft.thinking_config_style(), ThinkingConfigStyle::LevelFlash);

        // Pro heuristic now maps to 2.5 Pro (has thinking support)
        let pro_unknown = GeminiModel::from_slug("some-new-pro-model");
        assert_eq!(pro_unknown, GeminiModel::Gemini2_5Pro);
        assert!(pro_unknown.supports_thinking());
    }

    #[test]
    fn test_canonical_slug_round_trip() {
        // Ensure canonical slugs resolve back to the correct model
        let models = [
            GeminiModel::Gemini3_1ProPreview,
            GeminiModel::Gemini3_0ProPreview,
            GeminiModel::Gemini3_0FlashPreview,
            GeminiModel::Gemini2_5Pro,
            GeminiModel::Gemini2_5Flash,
            GeminiModel::Gemini2_5FlashLite,
            GeminiModel::Gemini2_0Flash,
            GeminiModel::Gemini2_0FlashLite,
            GeminiModel::Gemini2_0FlashThinking,
        ];
        for model in &models {
            let slug = model.canonical_slug();
            let resolved = GeminiModel::from_slug(slug);
            assert_eq!(
                &resolved, model,
                "canonical_slug '{}' did not round-trip for {:?}",
                slug, model
            );
        }
    }

    #[test]
    fn test_build_model_endpoint_format() {
        use crate::settings::GeminiConfig;
        let config = GeminiConfig {
            api_key: Some("test-key".to_string()),
            chat_model: "gemini-3-flash-preview".to_string(),
            summary_model: "gemini-2.5-flash".to_string(),
            thinking_enabled: false,
            thinking_level: "high".to_string(),
            thinking_budget: None,
        };
        let connector = GeminiConnector::new(config);
        let url =
            connector.build_model_endpoint("gemini-3-pro-preview", "generateContent", "test-key");
        assert!(
            url.contains("/v1beta/models/gemini-3-pro-preview:generateContent"),
            "URL should contain correct model path: {}",
            url
        );
        assert!(
            url.starts_with("https://generativelanguage.googleapis.com/"),
            "URL should use correct base: {}",
            url
        );
    }

    #[test]
    fn test_api_version() {
        // All known models currently use v1beta
        assert_eq!(GeminiModel::Gemini3_0ProPreview.api_version(), "v1beta");
        assert_eq!(GeminiModel::Gemini2_5Flash.api_version(), "v1beta");
        assert_eq!(GeminiModel::Gemini2_0Flash.api_version(), "v1beta");
    }
}
