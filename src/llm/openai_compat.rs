use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use serde_json::{json, Value};

use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use super::types::{LlmPrompt, ChatRole, ContentBlock};
use super::convert::{LlmFormatConverter, StreamEvent};
use super::LlmConnector;
use super::config::OpenAiCompatConfig;
use crate::mcp::manager::McpContext;

#[derive(Serialize, Deserialize, Debug)]
struct OpenAiMessage {
    role: String,
    content: String,
}

pub struct OpenAiCompatConnector {
    config: OpenAiCompatConfig,
}

impl OpenAiCompatConnector {
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl LlmConnector for OpenAiCompatConnector {
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
    ) {
        let base = self.config.endpoint.trim_end_matches('/');
        let endpoint = if base.ends_with("/v1") {
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        };
        let native_request = self.to_native_request(&prompt_data, true);
        
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build reqwest client");

        let mut request_builder = client.post(&endpoint)
            .json(&native_request)
            .header("Content-Type", "application/json");

        if let Some(api_key) = &self.config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = match request_builder.send().await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(StreamMessage::Error { message: format!("Network error: {}", e) });
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let _ = tx.send(StreamMessage::Error { message: format!("OpenAI API Error [{}]: {}", status, body) });
            return;
        }

        let mut stream = response.bytes_stream();

        // State for tracking <think>/<thinking> tag boundaries across chunks
        let mut inside_think = false;
        let mut think_buffer = String::new();

        // State for detecting inline <tool_call> tags (Hermes/Qwen format)
        // When vLLM doesn't produce structured tool_calls, Qwen wraps them as:
        //   <tool_call>{"name": "fn_name", "arguments": {"key": "value"}}</tool_call>
        let mut inside_tool_call = false;
        let mut tool_call_buffer = String::new();

        // State for accumulating tool calls across streaming chunks.
        // OpenAI sends tool calls incrementally: `name` and `id` in the first chunk,
        // then `arguments` as partial JSON strings across subsequent chunks.
        // We accumulate here and flush when Done or stream ends.
        #[allow(dead_code)]
        struct PendingToolCall {
            id: String,
            name: String,
            server_name: String,
            arguments: String,
        }
        let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();

        // Thought summary accumulated during thinking phase (to attach to tool calls)
        let mut accumulated_thought_summary: Option<String> = None;

        // Helper closure: flush all pending tool calls as StreamMessage::ToolCall
        let flush_tool_calls = |pending: &mut Vec<PendingToolCall>, tx: &mpsc::UnboundedSender<StreamMessage>, thought_summary: &mut Option<String>| {
            for tc in pending.drain(..) {
                let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                let tool_call = ToolCall::new(
                    tc.server_name,
                    tc.name,
                    args,
                    None, // thought_signature - not available from OpenAI compat
                    thought_summary.take(), // attach thinking to the first tool call
                );
                tracing::info!("OpenAI Compat: Sending tool call '{}' on server '{}'", tool_call.tool_name, tool_call.server_name);
                let _ = tx.send(StreamMessage::ToolCall(tool_call));
            }
        };

        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let chunk = String::from_utf8_lossy(&bytes);
                    let events = self.parse_stream_chunk(&chunk);
                    for event in events {
                        match event {
                            StreamEvent::Thinking { text, .. } => {
                                // Native reasoning_content from DeepSeek API
                                // Accumulate for potential attachment to tool calls
                                if let Some(ref mut summary) = accumulated_thought_summary {
                                    summary.push_str(&text);
                                } else {
                                    accumulated_thought_summary = Some(text.clone());
                                }
                                let _ = tx.send(StreamMessage::Text {
                                    content: String::new(),
                                    thought_signature: None,
                                    thought_summary: Some(text),
                                });
                            }
                            StreamEvent::Text { content } => {
                                // First, check for inline <tool_call> tags (Hermes/Qwen format)
                                // These appear when vLLM doesn't produce structured tool_calls
                                let mut text_to_process = content;
                                let mut remaining_text = String::new();

                                // Handle tool_call tag boundaries in the text stream
                                loop {
                                    if inside_tool_call {
                                        // We're accumulating a tool call — look for </tool_call>
                                        if let Some(end_pos) = text_to_process.find("</tool_call>") {
                                            // Complete the tool call buffer
                                            tool_call_buffer.push_str(&text_to_process[..end_pos]);
                                            let after = text_to_process[end_pos + 12..].to_string(); // skip "</tool_call>"
                                            
                                            // Parse the buffered JSON and emit as tool call
                                            Self::parse_inline_tool_call(
                                                &tool_call_buffer,
                                                &tx,
                                                &mut accumulated_thought_summary,
                                                &|sanitized| self.resolve_tool_call(sanitized),
                                            );
                                            
                                            tool_call_buffer.clear();
                                            inside_tool_call = false;
                                            
                                            // Continue processing remaining text after </tool_call>
                                            text_to_process = after;
                                            continue;
                                        } else {
                                            // No closing tag yet — buffer everything
                                            tool_call_buffer.push_str(&text_to_process);
                                            break;
                                        }
                                    } else {
                                        // Not inside a tool call — look for <tool_call>
                                        if let Some(start_pos) = text_to_process.find("<tool_call>") {
                                            // Emit text before the tag
                                            remaining_text.push_str(&text_to_process[..start_pos]);
                                            inside_tool_call = true;
                                            text_to_process = text_to_process[start_pos + 11..].to_string(); // skip "<tool_call>"
                                            continue;
                                        } else {
                                            // No tool_call tag — pass through
                                            remaining_text.push_str(&text_to_process);
                                            break;
                                        }
                                    }
                                }

                                // Process remaining (non-tool-call) text through think tag detection
                                if !remaining_text.is_empty() {
                                    let processed = self.split_think_tags(&remaining_text, &mut inside_think, &mut think_buffer);
                                    for (is_thinking, text) in processed {
                                        if is_thinking {
                                            // Accumulate thinking for potential tool call attachment
                                            if let Some(ref mut summary) = accumulated_thought_summary {
                                                summary.push_str(&text);
                                            } else {
                                                accumulated_thought_summary = Some(text.clone());
                                            }
                                            let _ = tx.send(StreamMessage::Text {
                                                content: String::new(),
                                                thought_signature: None,
                                                thought_summary: Some(text),
                                            });
                                        } else if !text.is_empty() {
                                            let _ = tx.send(StreamMessage::Text {
                                                content: text,
                                                thought_signature: None,
                                                thought_summary: None,
                                            });
                                        }
                                    }
                                }
                            }
                            StreamEvent::ToolCall { id, name, server_name, arguments, .. } => {
                                // OpenAI streams tool calls incrementally:
                                //   Chunk 1: id + name (arguments empty)
                                //   Chunk 2+: arguments fragments
                                // We match by index position in our pending_tool_calls vec.
                                // If `name` is non-empty, it's a new tool call declaration.
                                // If `name` is empty, it's an argument fragment for the last pending call.
                                if !name.is_empty() {
                                    // New tool call — resolve server/tool name
                                    let server = server_name.unwrap_or_else(|| "unknown".to_string());
                                    pending_tool_calls.push(PendingToolCall {
                                        id,
                                        name,
                                        server_name: server,
                                        arguments: arguments.to_string(),
                                    });
                                } else if let Some(last) = pending_tool_calls.last_mut() {
                                    // Argument fragment — append to last pending tool call
                                    let frag = arguments.as_str().unwrap_or("");
                                    last.arguments.push_str(frag);
                                }
                            }
                            StreamEvent::Usage(usage) => {
                                let _ = tx.send(StreamMessage::Usage(usage));
                            }
                            StreamEvent::Done => {
                                // Flush any pending tool calls before returning
                                flush_tool_calls(&mut pending_tool_calls, &tx, &mut accumulated_thought_summary);
                                return;
                            }
                            StreamEvent::Error { message } => {
                                let _ = tx.send(StreamMessage::Error { message });
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error { message: format!("Stream error: {}", e) });
                    return;
                }
            }
        }

        // Stream ended without explicit [DONE] — flush any pending tool calls
        flush_tool_calls(&mut pending_tool_calls, &tx, &mut accumulated_thought_summary);
    }

    async fn summarize_conversation(
        &self,
        _previous_summary: String,
        _recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        // Simple implementation for now
        let summary_model = self.config.summary_model.as_ref().unwrap_or(&self.config.model);
        tracing::info!(model = %summary_model, "LLM: Summarizing (OpenAI Compat)");

        // We can reuse the same logic as Gemini if we wanted, but let's keep it simple
        Ok(json!({
            "summary": "Summarization placeholder for OpenAI-compatible providers.",
            "entities": {},
            "sentiment": "neutral"
        }))
    }
}

impl LlmFormatConverter for OpenAiCompatConnector {
    fn to_native_request(&self, prompt: &LlmPrompt, streaming: bool) -> serde_json::Value {
        let mut messages = Vec::new();

        if let Some(system) = &prompt.system {
            messages.push(OpenAiMessage {
                role: "system".to_string(),
                content: system.clone(),
            });
        }

        for msg in &prompt.messages {
            let mut text_parts = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::Thinking { text, .. } => {
                        text_parts.push(format!("<thinking>\n{}\n</thinking>", text));
                    }
                    _ => {}
                }
            }
            messages.push(OpenAiMessage {
                role: match msg.role {
                    ChatRole::User => "user".to_string(),
                    ChatRole::Assistant => "assistant".to_string(),
                    ChatRole::System => "system".to_string(),
                    ChatRole::Tool => "tool".to_string(),
                },
                content: text_parts.join("\n"),
            });
        }

        let tools = if !self.config.tools_enabled || prompt.tools.is_empty() {
             None
        } else {
             // OpenAI tool format
             let openai_tools: Vec<serde_json::Value> = prompt.tools.iter().map(|t| {
                 let prefixed_name = format!("{}__{}", t.server_name, t.name);
                 json!({
                     "type": "function",
                     "function": {
                         "name": self.sanitize_tool_name(&prefixed_name),
                         "description": t.description,
                         "parameters": t.parameters
                     }
                 })
             }).collect();
             Some(openai_tools)
        };

        json!({
            "model": self.config.model,
            "messages": messages,
            "tools": tools,
            "stream": streaming,
        })
    }

    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for line in chunk.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            
            if let Some(json_str) = line.strip_prefix("data: ") {
                let json_str = json_str.trim();
                if json_str == "[DONE]" {
                    events.push(StreamEvent::Done);
                    continue;
                }
                
                if let Ok(val) = serde_json::from_str::<Value>(json_str) {
                    if let Some(choices) = val["choices"].as_array() {
                        if let Some(choice) = choices.first() {
                            // Text delta
                            if let Some(content) = choice["delta"]["content"].as_str() {
                                events.push(StreamEvent::Text { content: content.to_string() });
                            }
                            
                            // Tool calls delta — OpenAI streams these incrementally:
                            //   First chunk:  {"index": 0, "id": "call_xxx", "function": {"name": "fn_name", "arguments": ""}}
                            //   Later chunks: {"index": 0, "function": {"arguments": "{\"key\": "}}
                            //   More chunks:  {"index": 0, "function": {"arguments": "\"value\"}"}}
                            // We emit each delta as a StreamEvent::ToolCall.
                            // The accumulation happens in generate_content_stream.
                            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                                for tc in tool_calls {
                                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                                    let name_raw = tc.get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default();
                                    let args_fragment = tc.get("function")
                                        .and_then(|f| f.get("arguments"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    if !name_raw.is_empty() {
                                        // First chunk for this tool call — resolve server/tool name
                                        let (resolved_server, resolved_tool) = self.resolve_tool_call(name_raw);
                                        events.push(StreamEvent::ToolCall {
                                            id,
                                            name: resolved_tool,
                                            server_name: Some(resolved_server),
                                            arguments: json!(args_fragment),
                                            signature: None,
                                        });
                                    } else {
                                        // Continuation chunk — just argument fragments
                                        events.push(StreamEvent::ToolCall {
                                            id,
                                            name: String::new(), // empty = continuation
                                            server_name: None,
                                            arguments: json!(args_fragment),
                                            signature: None,
                                        });
                                    }
                                }
                            }

                            // Native reasoning_content (DeepSeek API, some OpenAI-compat providers)
                            if let Some(reasoning) = choice["delta"]["reasoning_content"].as_str() {
                                if !reasoning.is_empty() {
                                    events.push(StreamEvent::Thinking {
                                        text: reasoning.to_string(),
                                        signature: None,
                                    });
                                }
                            }
                        }
                    }
                    
                    // Usage
                    if let Some(usage) = val.get("usage") {
                        // Optional usage parsing
                        if let (Some(p), Some(c), Some(t)) = (usage["prompt_tokens"].as_u64(), usage["completion_tokens"].as_u64(), usage["total_tokens"].as_u64()) {
                             events.push(StreamEvent::Usage(UsageData {
                                 prompt_tokens: p as i32,
                                 completion_tokens: c as i32,
                                 total_tokens: t as i32,
                                 cached_content_tokens: None,
                                 thoughts_tokens: None,
                                 cost: Some(0.0), // Cost calculation for OpenAI compat is complex due to varied models
                             }));
                        }
                    }
                }
            }
        }
        events
    }

    fn convert_mcp_tool(&self, tool: &rmcp::model::Tool, server_name: &str) -> Result<crate::llm::ToolDefinition, String> {
        Ok(crate::llm::ToolDefinition::from_mcp(tool, server_name))
    }

    fn sanitize_tool_name(&self, name: &str) -> String {
        // OpenAI is permissive, but we'll stick to alpha_numeric and underscores for safety
        name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
    }

    fn original_tool_name(&self, sanitized: &str) -> Option<String> {
        Some(sanitized.to_string())
    }

    fn max_tools(&self) -> usize {
        128
    }
}

impl OpenAiCompatConnector {
    fn resolve_tool_call(&self, sanitized_name: &str) -> (String, String) {
        if let Some(pos) = sanitized_name.find("__") {
            let server = sanitized_name[..pos].to_string();
            let tool = sanitized_name[pos + 2..].to_string();
            (server, tool)
        } else {
            ("unknown".to_string(), sanitized_name.to_string())
        }
    }

    /// Split streaming text at think tag boundaries.
    ///
    /// Supports both `<think>`/`</think>` (DeepSeek R1) and
    /// `<thinking>`/`</thinking>` (Qwen3) tag variants.
    ///
    /// Returns a list of `(is_thinking, text)` segments.
    /// Uses mutable state (`inside_think`) to handle tag splits
    /// across SSE chunk boundaries. Emits thinking content incrementally
    /// so the UI can display it as it streams.
    fn split_think_tags(
        &self,
        content: &str,
        inside_think: &mut bool,
        _think_buffer: &mut String,
    ) -> Vec<(bool, String)> {
        let mut segments: Vec<(bool, String)> = Vec::new();
        let mut remaining = content;

        while !remaining.is_empty() {
            if *inside_think {
                // We're inside a think block — look for closing tag (either variant)
                let close_result = Self::find_first_tag(remaining, &["</think>", "</thinking>"]);
                if let Some((end_pos, tag_len)) = close_result {
                    // Emit thinking content up to the closing tag
                    let thinking_text = &remaining[..end_pos];
                    if !thinking_text.is_empty() {
                        segments.push((true, thinking_text.to_string()));
                    }
                    *inside_think = false;
                    remaining = &remaining[end_pos + tag_len..];
                } else {
                    // No closing tag in this chunk — emit all as thinking immediately
                    segments.push((true, remaining.to_string()));
                    break;
                }
            } else {
                // We're outside a think block — look for opening tag (either variant)
                let open_result = Self::find_first_tag(remaining, &["<think>", "<thinking>"]);
                if let Some((start_pos, tag_len)) = open_result {
                    // Emit any text before the tag as regular content
                    let before = &remaining[..start_pos];
                    if !before.is_empty() {
                        segments.push((false, before.to_string()));
                    }
                    *inside_think = true;
                    remaining = &remaining[start_pos + tag_len..];
                } else {
                    // No think tag — emit everything as regular text
                    segments.push((false, remaining.to_string()));
                    break;
                }
            }
        }

        segments
    }

    /// Find the first occurrence of any of the given tags in the text.
    /// Returns `(position, tag_length)` of whichever tag appears first.
    fn find_first_tag(text: &str, tags: &[&str]) -> Option<(usize, usize)> {
        tags.iter()
            .filter_map(|tag| text.find(tag).map(|pos| (pos, tag.len())))
            .min_by_key(|(pos, _)| *pos)
    }

    /// Parse an inline tool call from buffered `<tool_call>` content.
    ///
    /// Hermes/Qwen format:
    ///   `<tool_call>{"name": "fn_name", "arguments": {"key": "value"}}</tool_call>`
    ///
    /// Emits a StreamMessage::ToolCall if parsing succeeds.
    fn parse_inline_tool_call(
        buffer: &str,
        tx: &mpsc::UnboundedSender<StreamMessage>,
        thought_summary: &mut Option<String>,
        resolve_fn: &dyn Fn(&str) -> (String, String),
    ) {
        let trimmed = buffer.trim();
        tracing::debug!("OpenAI Compat: Parsing inline tool_call: {}", trimmed);

        match serde_json::from_str::<Value>(trimmed) {
            Ok(val) => {
                // Hermes format: {"name": "fn_name", "arguments": {"key": "value"}}
                // Also handle: {"function": {"name": "...", "arguments": {...}}}
                let (name_raw, args) = if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
                    let arguments = val.get("arguments").cloned().unwrap_or(json!({}));
                    (name.to_string(), arguments)
                } else if let Some(func) = val.get("function") {
                    let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let arguments = func.get("arguments").cloned().unwrap_or(json!({}));
                    (name.to_string(), arguments)
                } else {
                    tracing::warn!("OpenAI Compat: Inline tool_call JSON missing 'name': {}", trimmed);
                    return;
                };

                // Resolve server__tool naming convention
                let (server_name, tool_name) = resolve_fn(&name_raw);
                
                let tool_call = ToolCall::new(
                    server_name.clone(),
                    tool_name.clone(),
                    args,
                    None,
                    thought_summary.take(),
                );
                tracing::info!("OpenAI Compat: Inline tool call '{}' on server '{}'", tool_name, server_name);
                let _ = tx.send(StreamMessage::ToolCall(tool_call));
            }
            Err(e) => {
                tracing::warn!("OpenAI Compat: Failed to parse inline tool_call JSON: {} — raw: {}", e, trimmed);
                // Fall back to emitting as text so the user at least sees it
                let _ = tx.send(StreamMessage::Text {
                    content: format!("<tool_call>{}</tool_call>", trimmed),
                    thought_signature: None,
                    thought_summary: None,
                });
            }
        }
    }
}
