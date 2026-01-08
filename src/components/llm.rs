use serde::{Deserialize, Serialize};
use reqwest::Client;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use async_trait::async_trait;

use crate::components::shared::{StreamMessage, ToolCall};
use crate::context::prompt_builder::LlmPrompt;
use crate::mcp::manager::McpContext;
use crate::settings::GeminiConfig;
const BASE_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

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
pub(crate) struct GeminiRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<ToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
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

#[derive(Serialize, Deserialize)]
#[derive(Debug, Clone)]
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
struct GeminiErrorResponse {
    error: GeminiError,
}

#[derive(Deserialize, Debug)]
struct GeminiError {
    message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SystemInstruction {
    pub parts: Vec<Part>,
}

#[derive(Deserialize, Debug)]
struct GeminiResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: ContentResponse,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ContentResponse {
    #[serde(default)]
    parts: Vec<PartResponse>,
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
struct PartResponse {
    #[serde(default)]
    text: String,
    function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
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

impl GeminiConnector {
    pub fn new(config: GeminiConfig) -> Self {
        Self { 
            config,
            base_url: BASE_API_URL.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Helper to build the correct API endpoint for a given model.
    /// If model name already includes "models/" prefix (from API), use it directly.
    /// Otherwise, prepend "models/" for backward compatibility.
    fn build_model_endpoint(&self, model: &str, action: &str, api_key: &str) -> String {
        let model_path = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{}", model)
        };
        format!("{}/{}:{}?key={}", self.base_url, model_path, action, api_key)
    }

    /// Select the most useful tools from a large toolkit using LLM
    pub async fn select_tools_for_toolkit(
        &self,
        request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, Box<dyn std::error::Error + Send + Sync>> {
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
                    )) as Box<dyn std::error::Error + Send + Sync>);
                }
            }
        };
        
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");
        
        let prompt = build_selection_prompt(request);
        
        let request_body = GeminiRequest {
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text { text: prompt, thought: None }],
            }],
            tools: None,
            system_instruction: None,
            tool_config: None,
            generation_config: None,
        };
        
        let url = self.build_model_endpoint(&self.config.summary_model, "generateContent", &api_key);
        
        let response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        
        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
            tracing::error!("Gemini API Error [{}]: {}", status, body_text);
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("API request failed with status {}: {}", status, body_text),
            )) as Box<dyn std::error::Error + Send + Sync>);
        }
        
        let response_json: GeminiResponse = response.json().await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        
        if let Some(candidate) = response_json.candidates.get(0) {
            if let Some(part) = candidate.content.parts.get(0) {
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
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            e,
                        )) as Box<dyn std::error::Error + Send + Sync>);
                    }
                }
            }
        }
        
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No response from LLM for tool selection",
        )) as Box<dyn std::error::Error + Send + Sync>)
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
            }
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
        let thinking_config = if model.starts_with("gemini-3") {
            // Gemini 3 Pro uses thinkingLevel
            Some(ThinkingConfig {
                thinking_level: Some(self.config.thinking_level.clone()),
                thinking_budget: None,
                include_thoughts: Some(true),
            })
        } else if model.starts_with("gemini-2.5") || model.starts_with("gemini-2.0") {
            // Gemini 2.5 and 2.0 series use thinkingBudget
            Some(ThinkingConfig {
                thinking_level: None,
                thinking_budget: self.config.thinking_budget,
                include_thoughts: Some(true),
            })
        } else {
            // Unknown model, skip thinking config
            None
        };

        thinking_config.map(|tc| GenerationConfig {
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
                        let truncated_data = inline_data.data.chars().take(50).collect::<String>();
                        inline_data.data = format!("[{} bytes, truncated]...{}", original_len, truncated_data);
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

    let url = if self.config.chat_model.starts_with("models/") || !self.base_url.contains("generativelanguage.googleapis.com") {
         // Use the helper for standardizing
         self.build_model_endpoint(&self.config.chat_model, "streamGenerateContent", &api_key) + "&alt=sse"
    } else {
        // Fallback or explicit full path logic if ever needed, but standardizing is safer
         self.build_model_endpoint(&self.config.chat_model, "streamGenerateContent", &api_key) + "&alt=sse"
    };

    for attempt in 0..MAX_RETRIES {
        let response = match client.post(&url).json(&request_body).send().await {
            Ok(r) => r,
            Err(e) => {
                let error_msg = if e.is_timeout() {
                    tracing::error!("Gemini API Request TIMED OUT on attempt {}. Duration: 600s", attempt + 1);
                    format!("Request timed out after 600 seconds (attempt {}/{})", attempt + 1, MAX_RETRIES)
                } else {
                    tracing::error!("Error sending request on attempt {}: {}", attempt + 1, e);
                    format!("Network error: {} (attempt {}/{})", e, attempt + 1, MAX_RETRIES)
                };
                
                if attempt + 1 == MAX_RETRIES {
                    let _ = tx.send(StreamMessage::Error {
                        message: format!("Failed to connect to Gemini API after {} attempts. {}", MAX_RETRIES, error_msg),
                    });
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
            let error_message = if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
                tracing::error!("Gemini API Error [{}]: {}", status, error_response.error.message);
                format!("Gemini API Error [{}]: {}", status, error_response.error.message)
            } else {
                tracing::error!("Gemini API Error [{}]: {}", status, body_text);
                format!("Gemini API Error [{}]: {}", status, body_text)
            };
            
            let _ = tx.send(StreamMessage::Error { message: error_message });
            return;
        }

        let mut stream = response.bytes_stream();
        let mut has_sent_data = false;
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

                        if line.starts_with("data: ") {
                            let json_str = &line["data: ".len()..];
                            if json_str.is_empty() { continue; }
                            match serde_json::from_str::<GeminiResponse>(json_str) {
                                Ok(parsed) => {
                                    if let Some(candidate) = parsed.candidates.get(0) {
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
                                                tracing::warn!("Gemini stream finished with reason: {}", reason);
                                            }
                                        }
                                        // Process ALL parts - Gemini with thinking returns multiple parts
                                        // (thought parts AND content parts) in a single response
                                        for part in &candidate.content.parts {
                                            current_attempt_parts.push(Part::from(part.clone()));
                                            // Check if this is a thought summary part first
                                            if part.thought.unwrap_or(false) && !part.text.is_empty() {
                                                // This is a thought summary - send it as thought_summary
                                                if tx.send(StreamMessage::Text {
                                                    content: String::new(),
                                                    thought_signature: None,
                                                    thought_summary: Some(part.text.clone()),
                                                }).is_err() {
                                                    return;
                                                }
                                                has_sent_data = true;
                                            } else if let Some(function_call) = &part.function_call {
                                                // Log raw JSON if it contains a function call
                                                tracing::debug!("Raw JSON with function call: {}", json_str);
                                                
                                                // Log the thought_signature field for debugging
                                                if let Some(ref thought_sig) = part.thought_signature {
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
                                                    'server_loop: for server in &context.servers {
                                                        for tool in &server.tools {
                                                            let sanitized_tool_name = crate::gemini::convert::sanitize_function_name(&format!("{}_{}", server.name, tool.name));
                                                            if sanitized_tool_name == function_call.name {
                                                                let tool_call = ToolCall::new(
                                                                    server.name.clone(),
                                                                    tool.name.to_string(), // Use original tool name for execution
                                                                    function_call.args.clone(),
                                                                    part.thought_signature.clone().or(function_call.thought_signature.clone()),
                                                                    None, // thought_summary will be populated by stream_manager
                                                                );
                                                                if tx.send(StreamMessage::ToolCall(tool_call)).is_err() {
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
                                                    tracing::error!("LLM requested tool '{}' which was not found in the provided context.", function_call.name);
                                                    // Send a user-friendly message about the missing tool
                                                    let tool_error_msg = format!(
                                                        "⚠️ **Tool Not Available: `{}`**\n\n\
                                                        Hobbes tried to use a tool that isn't currently loaded. This can happen if:\n\n\
                                                        • The MCP server providing this tool is not running\n\
                                                        • The tool requires authentication that hasn't been set up\n\
                                                        • The tool list needs to be refreshed\n\n\
                                                        Please check your MCP Integration settings.",
                                                        function_call.name
                                                    );
                                                    if tx.send(StreamMessage::Text {
                                                        content: tool_error_msg,
                                                        thought_signature: None,
                                                        thought_summary: None,
                                                    }).is_err() {
                                                        return;
                                                    }
                                                    has_sent_data = true;
                                                }
                                            } else if !part.text.is_empty() {
                                                // Check if the text is structured JSON that needs unwrapping
                                                let (content, thought_summary) = if part.text.trim().starts_with('{') && part.text.trim().ends_with('}') {
                                                    unparse_json_response(&part.text)
                                                } else {
                                                    (part.text.clone(), None)
                                                };

                                                if !content.is_empty() || thought_summary.is_some() {
                                                    if tx.send(StreamMessage::Text {
                                                        content,
                                                        thought_signature: None,
                                                        thought_summary: thought_summary,
                                                    }).is_err() {
                                                        return;
                                                    }
                                                    has_sent_data = true;
                                                }
                                            }
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
                                    if tx.send(StreamMessage::Text {
                                        content: error_message.to_string(),
                                        thought_signature: None,
                                        thought_summary: None,
                                    }).is_err() {
                                        tracing::error!("Failed to send stream error message to UI.");
                                    }
                                    return;
                                }
                            }
                        }
                    }
                    if malformed_call_detected || unexpected_tool_call_detected { break; }
                }
                Err(e) => {
                    tracing::error!("Error in stream: {}", e);
                    break;
                }
            }
        }


        if malformed_call_detected || unexpected_tool_call_detected {
            if attempt + 1 < MAX_RETRIES {
                tracing::warn!("Retry triggered for stream error (attempt {}/{}). Sleeping 1s...", attempt + 1, MAX_RETRIES);
                
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
                                     let sanitized_name = crate::gemini::convert::sanitize_function_name(&format!("{}_{}", server.name, tool.name));
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
                tracing::error!("Stream error persisted after {} retries. Aborting.", MAX_RETRIES);
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
            tracing::error!("Gemini finished without sending data. Finish Reason: {:?}. Request was: {}", finish_reason, serde_json::to_string_pretty(&request_body).unwrap_or_default());
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
            if tx.send(StreamMessage::Text {
                content: default_message,
                thought_signature: None,
                thought_summary: None,
            }).is_err() {
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
            None => match std::env::var("GEMINI_API_KEY") {
                Ok(key) => key,
                Err(_) => {
                    tracing::warn!("Skipping summarization: GEMINI_API_KEY not set in settings or environment");
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "GEMINI_API_KEY not configured",
                    )) as Box<dyn std::error::Error + Send + Sync>);
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
        previous_summary,
        recent_messages
    );

    let request_body = GeminiRequest {
        contents: vec![Content {
            role: "user".to_string(),
            parts: vec![Part::Text { text: full_prompt, thought: None }],
        }],
        tools: None,
        system_instruction: None,
        tool_config: None,
        generation_config: None,
    };

    tracing::info!("Using summary model: {}", self.config.summary_model);
    let url = self.build_model_endpoint(&self.config.summary_model, "generateContent", &api_key);

    let response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
            tracing::error!("Gemini API Error [{}]: {}", status, error_response.error.message);
        } else {
            tracing::error!("Gemini API Error [{}]: {}", status, body_text);
        }
        // Return a structured error instead of panicking or returning a generic reqwest::Error
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("API request failed with status {}: {}", status, body_text),
        )) as Box<dyn std::error::Error + Send + Sync>);
    }

    let response_json: GeminiResponse = response.json().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    if let Some(candidate) = response_json.candidates.get(0) {
        if let Some(part) = candidate.content.parts.get(0) {
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
            tracing::warn!("Failed to parse LLM response as JSON. Returning raw text as summary.");
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
            let reply_text = obj.get("reply_text")
                .or_else(|| obj.get("content"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            
            let thought = obj.get("thought")
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
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path};
    use tokio::sync::mpsc;

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
            thought_json.to_string(),
            content_json.to_string()
        );

        // Configure the mock server
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
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
                parts: vec![Part::Text { text: "Hello".to_string(), thought: None }],
            }],
            tools: None,
            system_instruction: None,
        };

        // Create channel
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Run generate_content_stream
        connector.generate_content_stream(prompt_data, tx, None).await;

        // Verify results
        let mut thought_received = false;
        let mut content_received = false;

        while let Some(msg) = rx.recv().await {
            match msg {
                StreamMessage::Text { content, thought_summary, .. } => {
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

        let response_body = format!("data: {}\n\n", response_json.to_string());

        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
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
                parts: vec![Part::Text { text: "Use a tool".to_string(), thought: None }],
            }],
            tools: None,
            system_instruction: None,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector.generate_content_stream(prompt_data, tx, None).await;

        let mut received_error_guidance = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("[Hobbes encountered a persistent error") && content.contains("UNEXPECTED_TOOL_CALL") {
                    received_error_guidance = true;
                }
            }
        }

        assert!(received_error_guidance, "Should receive persistent error message for UNEXPECTED_TOOL_CALL");
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

        let response_body = format!("data: {}\n\n", response_json.to_string());

        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
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
                parts: vec![Part::Text { text: "Test".to_string(), thought: None }],
            }],
            tools: None,
            system_instruction: None,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        connector.generate_content_stream(prompt_data, tx, None).await;

        let mut received_safety_message = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("safety filter") {
                    received_safety_message = true;
                }
            }
        }

        assert!(received_safety_message, "Should receive safety filter message");
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

        let response_body = format!("data: {}\n\n", response_json.to_string());

        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
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
                parts: vec![Part::Text { text: "Use a tool".to_string(), thought: None }],
            }],
            tools: None,
            system_instruction: None,
        };

        // Pass an empty MCP context so the tool won't be found
        let mcp_context = Some(crate::mcp::manager::McpContext { servers: vec![] });
        
        let (tx, mut rx) = mpsc::unbounded_channel();
        connector.generate_content_stream(prompt_data, tx, mcp_context).await;

        let mut received_tool_error = false;
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Text { content, .. } = msg {
                if content.contains("Tool Not Available") && content.contains("unknown_server_nonexistent_tool") {
                    received_tool_error = true;
                }
            }
        }

        assert!(received_tool_error, "Should receive tool not available error with tool name");
    }
}