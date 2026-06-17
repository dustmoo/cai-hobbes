use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use super::types::LlmPrompt;
use super::GeminiConfig;
use super::LlmConnector;
use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use crate::mcp::manager::McpContext;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
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
    #[serde(rename = "cachedContent", skip_serializing_if = "Option::is_none")]
    pub cached_content: Option<String>,
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
    Gemini3_5Pro,
    Gemini3_5Flash,
    Gemini3_1ProPreview,
    Gemini3_1FlashPreview,
    Gemini3_1FlashLitePreview,
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
            // Gemini 3.5 Series - HIGHEST PRIORITY
            _ if s.starts_with("gemini-3.5-pro") => GeminiModel::Gemini3_5Pro,
            _ if s.starts_with("gemini-3.5-flash") => GeminiModel::Gemini3_5Flash,

            // Gemini 3.x Series - STRICT PREFIX MATCHING
            _ if s.starts_with("gemini-3.1-pro") => GeminiModel::Gemini3_1ProPreview,
            _ if s.starts_with("gemini-3.1-flash-lite") => GeminiModel::Gemini3_1FlashLitePreview,
            _ if s.starts_with("gemini-3.1-flash") => GeminiModel::Gemini3_1FlashPreview,
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
            _ if s.contains("flash-lite") => GeminiModel::Gemini3_1FlashLitePreview, // 3.1 is current gen
            _ if s.contains("flash") => GeminiModel::Gemini3_5Flash,          // assume latest gen
            // May 2026: default pro fallback to 3.5 Pro (upcoming June 2026)
            _ if s.contains("pro") => GeminiModel::Gemini3_5Pro,

            _ => {
                tracing::warn!("Unknown Gemini model slug encountered: '{}'. Defaulting to Gemini 2.0 Flash pricing.", s);
                GeminiModel::Unknown(s.to_string())
            }
        }
    }

    /// Per-1M-token (input, output) USD rates. Manually maintained and subject
    /// to drift — re-verify against ai.google.dev/gemini-api/docs/pricing
    /// (see SYSTEM_PATTERNS P-013). Last verified: 2026-06-17.
    pub fn get_rates(&self, prompt_tokens: i32) -> (f64, f64) {
        match self {
            // Gemini 3.5 Series
            GeminiModel::Gemini3_5Flash => (1.50, 9.00),
            // 3.5 Pro: not yet released (June 2026); using 3.1 Pro rates as placeholder
            GeminiModel::Gemini3_5Pro => {
                if prompt_tokens > 200_000 {
                    (4.00, 18.00)
                } else {
                    (2.00, 12.00)
                }
            }

            // Gemini 3.x Series
            GeminiModel::Gemini3_1ProPreview | GeminiModel::Gemini3_0ProPreview => {
                if prompt_tokens > 200_000 {
                    (4.00, 18.00)
                } else {
                    (2.00, 12.00)
                }
            }
            GeminiModel::Gemini3_0FlashPreview | GeminiModel::Gemini3_1FlashPreview => (0.50, 3.00),
            GeminiModel::Gemini3_1FlashLitePreview => (0.25, 1.50),

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
    #[allow(dead_code)]
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

use crate::llm::convert::LlmFormatConverter;
use crate::llm::convert::StreamEvent;

impl LlmFormatConverter for GeminiConnector {
    fn to_native_request(&self, prompt: &LlmPrompt, _streaming: bool) -> serde_json::Value {
        let mut contents: Vec<Content> = Vec::new();

        for msg in &prompt.messages {
            let mut parts = Vec::new();
            for block in &msg.content {
                match block {
                    crate::llm::ContentBlock::Text { text } => {
                        parts.push(Part::Text {
                            text: text.clone(),
                            thought: None,
                        });
                    }
                    crate::llm::ContentBlock::Thinking { text, signature: _ } => {
                        parts.push(Part::Text {
                            text: text.clone(),
                            thought: Some(true),
                        });
                        // Gemini doesn't take the signature back in a simple text part usually,
                        // but if we were continuing a thought it might be needed elsewhere.
                    }
                    crate::llm::ContentBlock::ToolCall {
                        id: _,
                        name,
                        arguments,
                        signature,
                    } => {
                        // Name is already sanitized by prompt_builder's get_prefixed_tool_name.
                        // Do NOT re-sanitize — sanitize_tool_name would convert hyphens to
                        // underscores, breaking consistency with tool declarations.
                        parts.push(Part::FunctionCall {
                            function_call: FunctionCallPart {
                                name: name.clone(),
                                args: arguments.clone(),
                            },
                            thought_signature: signature.clone(),
                        });
                    }
                    crate::llm::ContentBlock::ToolResult {
                        call_id: _,
                        name,
                        content,
                    } => {
                        // Gemini requires `response` to be a Protobuf Struct (JSON object).
                        // Wrap non-object values (arrays, strings, etc.) in {"result": ...}.
                        let response_value = if content.is_object() {
                            content.clone()
                        } else {
                            serde_json::json!({ "result": content })
                        };
                        // Name is already sanitized by prompt_builder's get_prefixed_tool_name.
                        parts.push(Part::FunctionResponse {
                            function_response: FunctionResponsePart {
                                name: name.clone(),
                                response: response_value,
                            },
                        });
                    }
                    crate::llm::ContentBlock::Image { mime_type, data } => {
                        parts.push(Part::InlineData {
                            inline_data: InlineDataPart {
                                mime_type: mime_type.clone(),
                                data: data.clone(),
                            },
                        });
                    }
                }
            }

            let role_str = match msg.role {
                crate::llm::ChatRole::User => "user".to_string(),
                crate::llm::ChatRole::Assistant => "model".to_string(),
                crate::llm::ChatRole::Tool => "user".to_string(), // Gemini uses 'user' role for tool results
                crate::llm::ChatRole::System => "user".to_string(), // Should be handled by system_instruction
            };

            // Merge consecutive same-role Content objects (Gemini API requirement).
            // The API strictly requires alternating user/model roles; consecutive
            // same-role entries cause 400 errors. This restores the v0.9.48
            // `add_to_prompt` closure behavior that was lost in the abstraction refactor.
            if let Some(last) = contents.last_mut() {
                if last.role == role_str {
                    last.parts.extend(parts);
                    continue;
                }
            }
            contents.push(Content {
                role: role_str,
                parts,
            });
        }

        let system_instruction = prompt.system.as_ref().map(|s| SystemInstruction {
            parts: vec![Part::Text {
                text: s.clone(),
                thought: None,
            }],
        });

        let tools = if prompt.tools.is_empty() {
            None
        } else {
            // Route through Gemini schema sanitizer for type coercion, array-items fix, enum stripping
            let mut function_declarations = Vec::new();
            for t in &prompt.tools {
                let sanitized_name =
                    crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);

                // Use the existing Gemini schema sanitizer for robust conversion
                match crate::gemini::convert::convert_schema(&t.parameters) {
                    Ok(gemini_schema) => {
                        match serde_json::to_value(
                            &crate::gemini::types::GeminiFunctionDeclaration {
                                name: sanitized_name,
                                description: if t.description.is_empty() {
                                    None
                                } else {
                                    Some(t.description.clone())
                                },
                                parameters: Some(gemini_schema),
                            },
                        ) {
                            Ok(tool_value) => function_declarations.push(tool_value),
                            Err(e) => tracing::warn!(
                                "Tool '{}' serialization failed: {}. Skipping.",
                                t.name,
                                e
                            ),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Tool '{}:{}' incompatible with Gemini: {}. Skipping.",
                            t.server_name,
                            t.name,
                            e
                        );
                    }
                }
            }

            // Enforce Gemini's 128-tool limit to prevent 400 errors
            const GEMINI_TOOL_LIMIT: usize = 128;
            if function_declarations.len() > GEMINI_TOOL_LIMIT {
                let original_count = function_declarations.len();
                function_declarations.truncate(GEMINI_TOOL_LIMIT);
                tracing::warn!(
                    "Tool count ({}) exceeds Gemini limit ({}). Truncated {} tools.",
                    original_count,
                    GEMINI_TOOL_LIMIT,
                    original_count - GEMINI_TOOL_LIMIT
                );
            }

            if function_declarations.is_empty() {
                None
            } else {
                Some(vec![Tool {
                    function_declarations,
                }])
            }
        };

        // Determine tool config
        let tool_config = if tools.is_some() {
            Some(ToolConfig {
                function_calling_config: FunctionCallingConfig {
                    mode: "AUTO".to_string(),
                    allowed_function_names: None,
                },
            })
        } else {
            None
        };

        // Build thinking config based on model and settings
        let mut model_slug = self.config.chat_model.clone();
        if model_slug.starts_with("models/") {
            model_slug = model_slug.strip_prefix("models/").unwrap().to_string();
        }
        let gemini_model = GeminiModel::from_slug(&model_slug);

        let generation_config = if self.config.thinking_enabled {
            match gemini_model.thinking_config_style() {
                ThinkingConfigStyle::LevelPro | ThinkingConfigStyle::LevelFlash => {
                    let level = if gemini_model
                        .valid_thinking_levels()
                        .contains(&self.config.thinking_level.as_str())
                    {
                        self.config.thinking_level.clone()
                    } else {
                        "high".to_string()
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
                response_mime_type: None,
                response_schema: None,
            })
        } else {
            None
        };

        let request = GeminiRequest {
            contents,
            tools,
            system_instruction,
            tool_config,
            generation_config,
            cached_content: None,
        };

        serde_json::to_value(request).unwrap_or(serde_json::Value::Null)
    }

    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Gemini SSE format is typically data: {...}
        let json_str = if let Some(stripped) = chunk.strip_prefix("data: ") {
            stripped
        } else {
            chunk
        };

        if json_str.trim() == "[DONE]" {
            events.push(StreamEvent::Done);
            return events;
        }

        match serde_json::from_str::<GeminiResponse>(json_str) {
            Ok(parsed) => {
                if let Some(candidate) = parsed.candidates.first() {
                    for part in &candidate.content.parts {
                        if let Some(fc) = &part.function_call {
                            // Pass the raw sanitized name through — resolution happens
                            // in generate_content_stream via lookup, not by splitting.
                            // GAP 4: Match old merge order — prefer part-level over fc-level
                            events.push(StreamEvent::ToolCall {
                                id: "gemini".to_string(),
                                name: fc.name.clone(),
                                server_name: None,
                                arguments: fc.args.clone(),
                                signature: part
                                    .thought_signature
                                    .clone()
                                    .or(fc.thought_signature.clone()),
                            });
                        } else if part.thought.unwrap_or(false) && !part.text.is_empty() {
                            events.push(StreamEvent::Thinking {
                                text: part.text.clone(),
                                signature: part.thought_signature.clone(),
                            });
                        } else if !part.text.is_empty() {
                            // GAP 2: Apply unparse_json_response for wrapped JSON text
                            let (content, thought_summary) = if part.text.trim().starts_with('{')
                                && part.text.trim().ends_with('}')
                            {
                                unparse_json_response(&part.text)
                            } else {
                                (part.text.clone(), None)
                            };
                            if !content.is_empty() {
                                events.push(StreamEvent::Text { content });
                            }
                            if let Some(summary) = thought_summary {
                                events.push(StreamEvent::Thinking {
                                    text: summary,
                                    signature: None,
                                });
                            }
                        }
                    }
                }
                if let Some(usage) = parsed.usage_metadata {
                    let model = self.config.chat_model.clone();
                    let cost = calculate_cost(&model, &usage);
                    events.push(StreamEvent::Usage(UsageData {
                        prompt_tokens: usage.prompt_token_count,
                        completion_tokens: usage.candidates_token_count.unwrap_or(0),
                        total_tokens: usage.total_token_count,
                        cached_content_tokens: usage.cached_content_token_count,
                        thoughts_tokens: usage.thoughts_token_count,
                        cost: Some(cost),
                    }));
                }
            }
            Err(e) => {
                // If it's not a full GeminiResponse, it might be an error or a malformed chunk
                if json_str.contains("error") {
                    if let Ok(err) = serde_json::from_str::<GeminiErrorResponse>(json_str) {
                        events.push(StreamEvent::Error {
                            message: err.error.message,
                        });
                    }
                } else {
                    // Possible partial JSON or other SSE noise
                    tracing::trace!("Failed to parse Gemini chunk: {}. Chunk: {}", e, json_str);
                }
            }
        }

        events
    }

    fn convert_mcp_tool(
        &self,
        tool: &rmcp::model::Tool,
        server_name: &str,
    ) -> Result<crate::llm::ToolDefinition, String> {
        Ok(crate::llm::ToolDefinition::from_mcp(tool, server_name))
    }

    fn sanitize_tool_name(&self, name: &str) -> String {
        // Gemini restricted characters: a-z, A-Z, 0-9, _, and max 63 chars
        let mut sanitized: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        if sanitized.len() > 63 {
            sanitized.truncate(63);
        }
        sanitized
    }

    fn original_tool_name(&self, sanitized: &str) -> Option<String> {
        // The resolved_tool_call logic handles this more robustly
        Some(sanitized.to_string())
    }

    fn max_tools(&self) -> usize {
        128
    }
}

impl GeminiConnector {
    /// Resolve a sanitized tool name back into (server_name, tool_name).
    ///
    /// When `mcp_context` is available, uses prefix-matching against known servers
    /// for lossless resolution (handles server names containing underscores like
    /// `composio-native`). Falls back to first-underscore split when no context.
    fn resolve_tool_call(
        &self,
        sanitized_name: &str,
        mcp_context: &Option<crate::mcp::manager::McpContext>,
    ) -> (String, String) {
        // Prefer lossless prefix-matching against known servers
        if let Some(ctx) = mcp_context {
            for server in &ctx.servers {
                let server_prefix = format!(
                    "{}_",
                    crate::gemini::convert::sanitize_function_name(
                        crate::gemini::convert::normalize_server_name(&server.name)
                    )
                );
                if sanitized_name.starts_with(&server_prefix) {
                    return (
                        server.name.clone(),
                        sanitized_name[server_prefix.len()..].to_string(),
                    );
                }
            }
        }
        // Fallback: first underscore split (fragile for multi-underscore server names)
        if let Some(pos) = sanitized_name.find('_') {
            let server = sanitized_name[..pos].to_string();
            let tool = sanitized_name[pos + 1..].to_string();
            (server, tool)
        } else {
            ("unknown".to_string(), sanitized_name.to_string())
        }
    }
}

// Trait moved to src/llm/mod.rs

pub struct GeminiConnector {
    config: GeminiConfig,
    base_url: String,
    cache_store: Arc<Mutex<crate::llm::gemini_cache::GeminiCacheStore>>,
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
            GeminiModel::Gemini3_5Pro
            | GeminiModel::Gemini3_1ProPreview
            | GeminiModel::Gemini3_0ProPreview => ThinkingConfigStyle::LevelPro,
            GeminiModel::Gemini3_5Flash
            | GeminiModel::Gemini3_1FlashPreview
            | GeminiModel::Gemini3_1FlashLitePreview
            | GeminiModel::Gemini3_0FlashPreview
            | GeminiModel::Gemini2_0FlashThinking => ThinkingConfigStyle::LevelFlash,
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
            GeminiModel::Gemini3_5Pro => "Gemini 3.5 Pro".to_string(),
            GeminiModel::Gemini3_5Flash => "Gemini 3.5 Flash".to_string(),
            GeminiModel::Gemini3_1ProPreview => "Gemini 3.1 Pro".to_string(),
            GeminiModel::Gemini3_1FlashPreview => "Gemini 3.1 Flash".to_string(),
            GeminiModel::Gemini3_1FlashLitePreview => "Gemini 3.1 Flash Lite".to_string(),
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
            ThinkingConfigStyle::LevelPro => &["low", "medium", "high"],
            ThinkingConfigStyle::LevelFlash => &["minimal", "low", "medium", "high"],
            _ => &[],
        }
    }

    /// Returns the canonical API slug for this model.
    /// This is the authoritative source for model identifiers sent to the API.
    pub fn canonical_slug(&self) -> &str {
        match self {
            GeminiModel::Gemini3_5Pro => "gemini-3.5-pro",
            GeminiModel::Gemini3_5Flash => "gemini-3.5-flash",
            GeminiModel::Gemini3_1ProPreview => "gemini-3.1-pro-preview",
            GeminiModel::Gemini3_1FlashPreview => "gemini-3.1-flash-preview",
            GeminiModel::Gemini3_1FlashLitePreview => "gemini-3.1-flash-lite-preview",
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

    /// Returns the context window size in tokens for this model.
    /// Source: https://ai.google.dev/gemini-api/docs/models
    pub fn context_window_tokens(&self) -> usize {
        match self {
            // Gemini 3.5: 1M context
            GeminiModel::Gemini3_5Pro | GeminiModel::Gemini3_5Flash => 1_000_000,
            // Gemini 3.x: 1M context
            GeminiModel::Gemini3_1ProPreview
            | GeminiModel::Gemini3_1FlashPreview
            | GeminiModel::Gemini3_1FlashLitePreview
            | GeminiModel::Gemini3_0ProPreview
            | GeminiModel::Gemini3_0FlashPreview => 1_000_000,
            // Gemini 2.5: 1M context
            GeminiModel::Gemini2_5Pro
            | GeminiModel::Gemini2_5Flash
            | GeminiModel::Gemini2_5FlashLite
            | GeminiModel::Gemini2_5ComputerUsePreview => 1_000_000,
            // Gemini 2.0: 1M context
            GeminiModel::Gemini2_0Flash
            | GeminiModel::Gemini2_0FlashThinking
            | GeminiModel::Gemini2_0FlashLite => 1_000_000,
            // Gemma/Nano: smaller windows
            GeminiModel::Gemma3 => 128_000,
            GeminiModel::NanoBanana | GeminiModel::NanoBananaPro => 32_000,
            // Unknown: conservative default
            GeminiModel::Unknown(_) => 1_000_000,
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

/// Process-wide Gemini cache store. Shared across connector instances so that
/// server-side cachedContents entries survive connector rebuilds (settings
/// changes, per-session provider/model overrides) instead of being orphaned
/// until their TTL expires.
fn shared_cache_store() -> Arc<Mutex<crate::llm::gemini_cache::GeminiCacheStore>> {
    static STORE: std::sync::OnceLock<Arc<Mutex<crate::llm::gemini_cache::GeminiCacheStore>>> =
        std::sync::OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(Mutex::new(crate::llm::gemini_cache::GeminiCacheStore::new())))
        .clone()
}

impl GeminiConnector {
    pub fn new(config: GeminiConfig) -> Self {
        Self {
            config,
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            cache_store: Arc::new(Mutex::new(crate::llm::gemini_cache::GeminiCacheStore::new())),
        }
    }

    /// Production constructor: uses the process-wide shared cache store.
    /// Tests use `new()` to keep cache state isolated per instance.
    pub fn new_shared(config: GeminiConfig) -> Self {
        Self {
            config,
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            cache_store: shared_cache_store(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Resolve the API key from config or the `GEMINI_API_KEY` environment variable.
    /// Returns `Err` if neither source provides a key.
    fn resolve_api_key(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(key) = self.config.api_key.clone() {
            return Ok(key);
        }
        match std::env::var("GEMINI_API_KEY") {
            Ok(key) => Ok(key),
            Err(_) => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "GEMINI_API_KEY not configured",
            )) as Box<dyn std::error::Error + Send + Sync>),
        }
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

        let api_key = self.resolve_api_key().map_err(|e| {
            tracing::warn!("Skipping tool selection: {}", e);
            e
        })?;

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
            cached_content: None,
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
                tracing::trace!("Raw LLM tool selection response: {}", part.text);

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
        let api_key = self.resolve_api_key()?;

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
        session_id: Option<String>,
    ) {
        let api_key = match self.resolve_api_key() {
            Ok(key) => key,
            Err(_) => {
                tracing::error!("GEMINI_API_KEY not set in settings or environment");
                let _ = tx.send(StreamMessage::Error {
                    message: "⚠️ **API Key Not Configured**\n\nPlease set your Gemini API key in Settings → AI Model to use Hobbes.".to_string(),
                });
                return;
            }
        };

        let mut model = self.config.chat_model.clone();
        if model.starts_with("models/") {
            model = model.strip_prefix("models/").unwrap().to_string();
        }

        tracing::info!(model = %model, "LLM: Generating content stream (Refactored)");

        const MAX_RETRIES: u32 = 2;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("Failed to build reqwest client");

        // Convert neutral prompt to native request once as a base
        let native_request_val = self.to_native_request(&prompt_data, true);
        let mut request_body: GeminiRequest = match serde_json::from_value(native_request_val) {
            Ok(req) => req,
            Err(e) => {
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Failed to build Gemini request: {}", e),
                });
                return;
            }
        };

        // Explicit Context Caching check/create
        let mut cache_entry_opt = None;
        if self.config.context_caching_enabled {
            if let Some(ref sess_id) = session_id {
                if request_body.contents.len() > 1 {
                    let prefix_contents = &request_body.contents[0 .. request_body.contents.len() - 1];
                    let prefix_hash = crate::llm::gemini_cache::compute_prefix_hash(
                        request_body.system_instruction.as_ref(),
                        request_body.tools.as_deref(),
                        prefix_contents,
                    );

                    let mut store = self.cache_store.lock().await;
                    store.cleanup_expired();
                    if let Some(entry) = store.get_valid_cache(sess_id, &model, prefix_hash) {
                        cache_entry_opt = Some(entry.clone());
                    } else {
                        let chars_per_token = self.config.context_tuning.chars_per_token
                            .unwrap_or(crate::context::token_estimator::DEFAULT_CHARS_PER_TOKEN);
                        
                        let system_tokens = prompt_data.system.as_ref().map_or(0, |s| crate::context::token_estimator::estimate_tokens_with_ratio(s, chars_per_token));
                        let tool_tokens: usize = prompt_data.tools.iter().map(|t| crate::context::token_estimator::estimate_tool_definition_tokens(t, chars_per_token)).sum();
                        let prefix_messages_tokens: usize = prompt_data.messages.iter().take(prompt_data.messages.len().saturating_sub(1))
                            .map(|m| crate::context::token_estimator::estimate_message_tokens_with_ratio(m, chars_per_token)).sum();

                        let estimated_prefix_tokens = system_tokens + tool_tokens + prefix_messages_tokens;

                        if estimated_prefix_tokens >= 4096 {
                            let system_instr = request_body.system_instruction.clone();
                            let t_list = request_body.tools.clone();
                            let contents_prefix = prefix_contents.to_vec();

                            drop(store); // release lock during network request

                            match crate::llm::gemini_cache::api_create_cache(
                                &client,
                                &self.base_url,
                                &api_key,
                                &model,
                                sess_id,
                                system_instr,
                                t_list,
                                contents_prefix,
                                self.config.cache_ttl_seconds,
                                prefix_hash,
                            ).await {
                                Ok(new_entry) => {
                                    let mut store_lock = self.cache_store.lock().await;
                                    store_lock.insert_entry(sess_id.clone(), new_entry.clone());
                                    cache_entry_opt = Some(new_entry);
                                }
                                Err(e) => {
                                    tracing::warn!("Gemini Context Caching: failed to create cache: {}. Proceeding without cache.", e);
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(entry) = cache_entry_opt {
            request_body.cached_content = Some(entry.cache_id.clone());
            request_body.system_instruction = None;
            request_body.tools = None;
            request_body.tool_config = None;
            if request_body.contents.len() >= entry.cached_message_count {
                request_body.contents.drain(0..entry.cached_message_count);
            }
            tracing::info!("Using Gemini explicit context cache: id={}, cached messages count={}", entry.cache_id, entry.cached_message_count);
        }

        // --- Logging Block ---
        {
            if tracing::enabled!(tracing::Level::DEBUG) {
                if let Ok(request_json) = serde_json::to_string_pretty(&request_body) {
                    let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
                    if std::fs::create_dir_all(&debug_dir).is_ok() {
                        let _ =
                            std::fs::write(debug_dir.join("gemini_request.json"), &request_json);
                    }
                }
            }
        }

        let url = self.build_model_endpoint(&model, "streamGenerateContent", &api_key) + "&alt=sse";

        for attempt in 0..MAX_RETRIES {
            let response = match client.post(&url).json(&request_body).send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt + 1 == MAX_RETRIES {
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Network error after {} attempts: {}", MAX_RETRIES, e),
                        });
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body_text = response.text().await.unwrap_or_default();

                // Retry on server errors (5xx) if attempts remain — preview models often 500 transiently
                if status.is_server_error() && attempt + 1 < MAX_RETRIES {
                    tracing::warn!(
                        "Gemini API server error [{}] on attempt {}, retrying in {}s: {}",
                        status,
                        attempt + 1,
                        2u64.pow(attempt),
                        &body_text[..body_text.len().min(200)]
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
                    continue;
                }

                let msg = if let Ok(err) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
                    err.error.message
                } else {
                    body_text
                };
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Gemini API Error [{}]: {}", status, msg),
                });
                return;
            }

            let mut stream = response.bytes_stream();
            let mut buffer = Vec::<u8>::new();
            let mut current_attempt_parts = Vec::<Part>::new();
            let mut malformed_call_detected = false;
            let mut unexpected_tool_call_detected = false;
            let mut tool_not_found_count: u32 = 0;
            let mut finish_reason: Option<String> = None;
            // Dedup guard: Gemini SSE sends cumulative candidate snapshots, so the same
            // functionCall can appear in multiple chunks. Track (name, args_hash) to
            // prevent duplicate execution.
            let mut emitted_tool_calls: std::collections::HashSet<(String, u64)> =
                std::collections::HashSet::new();

            // Buffer for usage data: Gemini emits usage_metadata on EVERY SSE chunk,
            // not just the final one. With thinking models, early chunks report thinking
            // tokens inside candidates_token_count; the final chunk correctly separates
            // them into thoughts_token_count, reducing the total. We hold the last-seen
            // usage and emit it once after the stream ends to avoid the "33 → 27" jitter.
            let mut pending_usage: Option<crate::components::shared::UsageData> = None;

            // State for tracking leaked <|channel>thought / <|channel>response tokens.
            // These can leak from Gemma models even through the Gemini API.
            let mut inside_channel_think = false;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => {
                        buffer.extend_from_slice(&bytes);
                        while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
                            let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
                            let line = String::from_utf8_lossy(&line_bytes);

                            // Check for Gemini-specific finish reasons that require retry
                            if line.contains("MALFORMED_FUNCTION_CALL") {
                                malformed_call_detected = true;
                                finish_reason = Some("MALFORMED_FUNCTION_CALL".to_string());
                                break;
                            }
                            if line.contains("UNEXPECTED_TOOL_CALL") {
                                unexpected_tool_call_detected = true;
                                finish_reason = Some("UNEXPECTED_TOOL_CALL".to_string());
                                break;
                            }
                            // Track SAFETY finish reason
                            if line.contains("\"SAFETY\"") && line.contains("finishReason") {
                                finish_reason = Some("SAFETY".to_string());
                            }

                            let events = self.parse_stream_chunk(&line);
                            for event in events {
                                match event {
                                    StreamEvent::Text { content } => {
                                        if !content.is_empty() {
                                            // Strip leaked <|channel>thought tokens before display
                                            let channel_segments = super::convert::strip_channel_tokens(
                                                &content,
                                                &mut inside_channel_think,
                                            );
                                            for (is_thinking, seg_text) in channel_segments {
                                                if is_thinking {
                                                    current_attempt_parts.push(Part::Text {
                                                        text: seg_text.clone(),
                                                        thought: Some(true),
                                                    });
                                                    let _ = tx.send(StreamMessage::Text {
                                                        content: String::new(),
                                                        thought_signature: None,
                                                        thought_summary: Some(seg_text),
                                                    });
                                                } else if !seg_text.is_empty() {
                                                    current_attempt_parts.push(Part::Text {
                                                        text: seg_text.clone(),
                                                        thought: None,
                                                    });
                                                    let _ = tx.send(StreamMessage::Text {
                                                        content: seg_text,
                                                        thought_signature: None,
                                                        thought_summary: None,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    StreamEvent::Thinking { text, signature } => {
                                        current_attempt_parts.push(Part::Text {
                                            text: text.clone(),
                                            thought: Some(true),
                                        });
                                        let _ = tx.send(StreamMessage::Text {
                                            content: String::new(),
                                            thought_signature: signature,
                                            thought_summary: Some(text),
                                        });
                                    }
                                    StreamEvent::ToolCall {
                                        id: _,
                                        name: fc_name,
                                        server_name: _,
                                        arguments,
                                        signature,
                                    } => {
                                        // GAP 1: Diagnostic logging for thought_signature
                                        if let Some(ref sig) = signature {
                                            tracing::info!("Received function call '{}' with thought_signature: '{}'",
                                                fc_name,
                                                if sig.len() > 50 { &sig[..50] } else { sig }
                                            );
                                        } else {
                                            tracing::warn!("Received function call '{}' WITHOUT thought_signature field", fc_name);
                                        }

                                        // GAP 3: ALWAYS accumulate the function call into current_attempt_parts,
                                        // even if tool is not found. This is critical for grounding retries —
                                        // the model must see its own failed attempt for self-correction.
                                        current_attempt_parts.push(Part::FunctionCall {
                                            function_call: FunctionCallPart {
                                                name: fc_name.clone(),
                                                args: arguments.clone(),
                                            },
                                            thought_signature: signature.clone(),
                                        });

                                        // Original pattern: iterate all servers/tools, compare
                                        // get_prefixed_tool_name against the raw function call name.
                                        // On match, use the ORIGINAL server.name and tool.name.
                                        let mut found_tool = false;
                                        let mut matched_server = String::new();
                                        let mut matched_tool = String::new();

                                        if let Some(ref ctx) = mcp_context {
                                            'server_loop: for server in &ctx.servers {
                                                for tool in &server.tools {
                                                    let sanitized_tool_name = crate::gemini::convert::get_prefixed_tool_name(&server.name, &tool.name);
                                                    if sanitized_tool_name == fc_name {
                                                        matched_server = server.name.clone();
                                                        matched_tool = tool.name.to_string();
                                                        found_tool = true;
                                                        break 'server_loop;
                                                    }
                                                }
                                            }

                                            // Fallback for on-demand tools (e.g. from COMPOSIO_GET_APP_TOOLS).
                                            // These live in the MCP manager's dynamic cache, not the static snapshot.
                                            // Reverse-map: try each server's normalized prefix against fc_name.
                                            if !found_tool {
                                                for server in &ctx.servers {
                                                    // Build the prefix the same way get_prefixed_tool_name does:
                                                    // normalize_server_name → sanitize_function_name → append "_"
                                                    let server_prefix = format!("{}_",
                                                        crate::gemini::convert::sanitize_function_name(
                                                            crate::gemini::convert::normalize_server_name(&server.name)
                                                        )
                                                    );
                                                    if fc_name.starts_with(&server_prefix) {
                                                        matched_server = server.name.clone();
                                                        matched_tool = fc_name
                                                            [server_prefix.len()..]
                                                            .to_string();
                                                        found_tool = true;
                                                        tracing::info!(
                                                            "On-demand tool '{}' resolved via prefix match: server='{}', tool='{}'",
                                                            fc_name, matched_server, matched_tool
                                                        );
                                                        break;
                                                    }
                                                }
                                            }
                                        } else {
                                            // No context — use resolve_tool_call with None
                                            found_tool = true;
                                            let (s, t) = self.resolve_tool_call(&fc_name, &None);
                                            matched_server = s;
                                            matched_tool = t;
                                        }

                                        if found_tool {
                                            // Dedup: hash the arguments to create a fingerprint
                                            use std::hash::{Hash, Hasher};
                                            let mut hasher =
                                                std::collections::hash_map::DefaultHasher::new();
                                            arguments.to_string().hash(&mut hasher);
                                            let args_hash = hasher.finish();
                                            let dedup_key = (fc_name.clone(), args_hash);

                                            if emitted_tool_calls.contains(&dedup_key) {
                                                tracing::warn!(
                                                    "Duplicate tool call suppressed: '{}' (args_hash: {})",
                                                    fc_name, args_hash
                                                );
                                            } else {
                                                emitted_tool_calls.insert(dedup_key);
                                                let tool_call = ToolCall::new(
                                                    matched_server,
                                                    matched_tool,
                                                    arguments.clone(),
                                                    signature.clone(),
                                                    None,
                                                );
                                                let _ = tx.send(StreamMessage::ToolCall(tool_call));
                                            }
                                        } else {
                                            // Tool not found in mcp_context - send user-friendly error
                                            tool_not_found_count += 1;
                                            tracing::error!("LLM requested tool '{}' which was not found in the provided context (count: {}).", fc_name, tool_not_found_count);

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
                                                    fc_name)
                                            };
                                            let _ = tx.send(StreamMessage::Text {
                                                content: tool_error_msg,
                                                thought_signature: None,
                                                thought_summary: None,
                                            });
                                        }
                                    }
                                    StreamEvent::Usage(usage) => {
                                        // Buffer — emit only the final authoritative values
                                        // after the stream completes (see pending_usage flush below).
                                        pending_usage = Some(crate::components::shared::UsageData {
                                            prompt_tokens: usage.prompt_tokens,
                                            completion_tokens: usage.completion_tokens,
                                            total_tokens: usage.total_tokens,
                                            cost: usage.cost,
                                            cached_content_tokens: usage.cached_content_tokens,
                                            thoughts_tokens: usage.thoughts_tokens,
                                        });
                                    }
                                    StreamEvent::Error { message } => {
                                        let _ = tx.send(StreamMessage::Error { message });
                                        break;
                                    }
                                    StreamEvent::Done => break,
                                }
                            }
                        }
                        if malformed_call_detected || unexpected_tool_call_detected {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Stream error: {}", e),
                        });
                        break;
                    }
                }
            }

            // Emit the final canonical usage now that the stream has ended.
            // Flushing here rather than per-chunk avoids the "high → low" display
            // jitter caused by Gemini re-bucketing thinking tokens in the final chunk.
            if let Some(usage) = pending_usage.take() {
                let _ = tx.send(StreamMessage::Usage(usage));
            }

            // Specific Gemini retry/grounding logic
            if (malformed_call_detected || unexpected_tool_call_detected)
                && attempt + 1 < MAX_RETRIES
            {
                tracing::warn!("Retry triggered for Gemini (attempt {}).", attempt + 1);
                if !current_attempt_parts.is_empty() {
                    request_body.contents.push(Content {
                        role: "model".to_string(),
                        parts: current_attempt_parts,
                    });

                    let correction = if unexpected_tool_call_detected {
                        // Enumerate available tools from the request to ground the model's retry
                        let available_tools_str = if let Some(tools) = &request_body.tools {
                            let mut names = Vec::new();
                            for tool in tools {
                                for decl in &tool.function_declarations {
                                    if let Some(name) = decl.get("name").and_then(|n| n.as_str()) {
                                        names.push(format!("- {}", name));
                                    }
                                }
                            }
                            if names.is_empty() {
                                "No tools are currently available.".to_string()
                            } else {
                                format!("Available tools:\n{}", names.join("\n"))
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
                            text: correction,
                            thought: None,
                        }],
                    });
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }

            // Retries exhausted for malformed/unexpected tool calls — send persistent error to user
            if malformed_call_detected || unexpected_tool_call_detected {
                tracing::error!(
                    "Stream error persisted after {} retries. Aborting.",
                    MAX_RETRIES
                );
                let _ = tx.send(StreamMessage::Text {
                    content: format!(
                        "[Hobbes encountered a persistent error ('{}') after multiple retries. The model may be hallucinating a tool that does not exist.]",
                        if unexpected_tool_call_detected { "UNEXPECTED_TOOL_CALL" } else { "MALFORMED_FUNCTION_CALL" }
                    ),
                    thought_signature: None,
                    thought_summary: None,
                });
                return;
            }

            // Handle safety filter stop reason
            if let Some(reason) = &finish_reason {
                if reason == "SAFETY" {
                    tracing::error!("Gemini generation stopped by safety filter.");
                    let _ = tx.send(StreamMessage::Text {
                        content: "[The response was blocked by the safety filter. Please try rephrasing your request.]".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                    });
                    return;
                }
            }

            break;
        }
    }

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!(model = %self.config.summary_model, "LLM: Summarizing conversation");

        let api_key = self.resolve_api_key().map_err(|e| {
            tracing::warn!("Skipping summarization: {}", e);
            e
        })?;
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

Analyze the sentiment and mood of the user in the "Recent Messages".

Populate the JSON response with:
- "summary": A concise, updated summary of the entire conversation so far.
- "sentiment": A brief string describing the user's current mood (e.g., "curious and collaborative", "frustrated but focused").
- "current_task": A one-sentence description of the specific task or goal the user is CURRENTLY working on. This is the active goal anchor — critical for small-context models. Example: "Implementing a dynamic history cap in prompt_builder.rs". Leave empty if no clear active task.
- "entities": An object with:
  - "user_name": The user's name if mentioned.
  - "project_name": The active project or codebase name.
  - "key_topics": Main topics discussed (array of short strings).
  - "key_decisions": Important decisions made (array of short strings).
  - "active_profile": The active Composio profile name if mentioned.
  - "blockers": Current blockers or open issues (array of short strings).

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

        // Structured output schema — enforced at the API level.
        // The model can ONLY return fields defined here.
        let summary_schema = serde_json::json!({
            "type": "OBJECT",
            "properties": {
                "summary": { "type": "STRING", "description": "Concise updated summary of the conversation" },
                "sentiment": { "type": "STRING", "description": "User's current mood" },
                "current_task": { "type": "STRING", "description": "One-sentence description of the specific task the user is currently working on. Leave empty if unclear." },
                "entities": {
                    "type": "OBJECT",
                    "properties": {
                        "user_name": { "type": "STRING", "description": "User's name if mentioned" },
                        "project_name": { "type": "STRING", "description": "Active project or codebase name" },
                        "key_topics": { "type": "ARRAY", "items": { "type": "STRING" }, "description": "Main topics discussed" },
                        "key_decisions": { "type": "ARRAY", "items": { "type": "STRING" }, "description": "Important decisions made" },
                        "active_profile": { "type": "STRING", "description": "Active Composio profile name" },
                        "blockers": { "type": "ARRAY", "items": { "type": "STRING" }, "description": "Current blockers or open issues" }
                    }
                }
            },
            "required": ["summary", "sentiment", "entities"]
        });

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
            generation_config: Some(GenerationConfig {
                thinking_config: None,
                response_mime_type: Some("application/json".to_string()),
                response_schema: Some(summary_schema),
            }),
            cached_content: None,
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
                tracing::trace!("Raw LLM summary response: {}", part.text);

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

    async fn invalidate_session_cache(&self, session_id: &str) {
        let mut store = self.cache_store.lock().await;
        if let Some(entry) = store.invalidate(session_id) {
            if let Ok(api_key) = self.resolve_api_key() {
                let client = Client::new();
                let base_url = self.base_url.clone();
                let cache_id = entry.cache_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::llm::gemini_cache::api_delete_cache(&client, &base_url, &api_key, &cache_id).await {
                        tracing::warn!("Failed to delete Gemini cache {} upon invalidation: {}", cache_id, e);
                    }
                });
            }
        }
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

        let response_body = format!("data: {}\n\ndata: {}\n\n", thought_json, content_json);

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
            model_slots: vec![],
            context_tuning: Default::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: 300,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        // Create prompt data
        let prompt_data = LlmPrompt {
            system: None,
            messages: vec![crate::llm::ChatMessage {
                role: crate::llm::ChatRole::User,
                content: vec![crate::llm::ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: vec![],
        };

        // Create channel
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Run generate_content_stream
        connector
            .generate_content_stream(prompt_data, tx, None, None)
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
            model_slots: vec![],
            context_tuning: Default::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: 300,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            system: None,
            messages: vec![crate::llm::ChatMessage {
                role: crate::llm::ChatRole::User,
                content: vec![crate::llm::ContentBlock::Text {
                    text: "Use a tool".to_string(),
                }],
            }],
            tools: vec![],
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, None, None)
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
            model_slots: vec![],
            context_tuning: Default::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: 300,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            system: None,
            messages: vec![crate::llm::ChatMessage {
                role: crate::llm::ChatRole::User,
                content: vec![crate::llm::ContentBlock::Text {
                    text: "Test".to_string(),
                }],
            }],
            tools: vec![],
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, None, None)
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
            model_slots: vec![],
            context_tuning: Default::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: 300,
        };

        let connector = GeminiConnector::new(config).with_base_url(mock_server.uri());

        let prompt_data = LlmPrompt {
            system: None,
            messages: vec![crate::llm::ChatMessage {
                role: crate::llm::ChatRole::User,
                content: vec![crate::llm::ContentBlock::Text {
                    text: "Use a tool".to_string(),
                }],
            }],
            tools: vec![],
        };

        // Pass an empty MCP context so the tool won't be found
        let mcp_context = Some(crate::mcp::manager::McpContext {
            servers: vec![],
            connected_toolkit_slugs: vec![],
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector
            .generate_content_stream(prompt_data, tx, mcp_context, None)
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
        assert!(GeminiModel::Gemini3_5Pro.supports_thinking());
        assert!(GeminiModel::Gemini3_5Flash.supports_thinking());
        assert!(GeminiModel::Gemini3_1ProPreview.supports_thinking());
        assert!(GeminiModel::Gemini3_1FlashPreview.supports_thinking());
        assert!(GeminiModel::Gemini3_1FlashLitePreview.supports_thinking());
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
            GeminiModel::Gemini3_1FlashLitePreview.thinking_config_style(),
            LevelFlash
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
            &["low", "medium", "high"]
        );
        assert_eq!(
            GeminiModel::Gemini3_0ProPreview.valid_thinking_levels(),
            &["low", "medium", "high"]
        );
        assert_eq!(
            GeminiModel::Gemini3_0FlashPreview.valid_thinking_levels(),
            &["minimal", "low", "medium", "high"]
        );
        assert_eq!(
            GeminiModel::Gemini3_1FlashLitePreview.valid_thinking_levels(),
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
            GeminiModel::from_slug("gemini-3.1-flash-lite-preview"),
            GeminiModel::Gemini3_1FlashLitePreview
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

        // Pro heuristic now maps to 3.5 Pro
        let pro_unknown = GeminiModel::from_slug("some-new-pro-model");
        assert_eq!(pro_unknown, GeminiModel::Gemini3_5Pro);
        assert!(pro_unknown.supports_thinking());

        // Gemini 3.5 Flash slugs
        assert_eq!(
            GeminiModel::from_slug("gemini-3.5-flash"),
            GeminiModel::Gemini3_5Flash
        );
        assert_eq!(
            GeminiModel::from_slug("gemini-3.5-pro"),
            GeminiModel::Gemini3_5Pro
        );
        // With preview suffix (future-proofing)
        assert_eq!(
            GeminiModel::from_slug("gemini-3.5-flash-preview"),
            GeminiModel::Gemini3_5Flash
        );
        assert_eq!(
            GeminiModel::from_slug("gemini-3.5-pro-preview"),
            GeminiModel::Gemini3_5Pro
        );
    }

    #[test]
    fn test_canonical_slug_round_trip() {
        // Ensure canonical slugs resolve back to the correct model
        let models = [
            GeminiModel::Gemini3_5Pro,
            GeminiModel::Gemini3_5Flash,
            GeminiModel::Gemini3_1ProPreview,
            GeminiModel::Gemini3_1FlashPreview,
            GeminiModel::Gemini3_1FlashLitePreview,
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
            model_slots: vec![],
            context_tuning: Default::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: 300,
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
