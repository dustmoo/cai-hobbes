use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::claude_models::ClaudeModel;
use super::config::ClaudeConfig;
use super::convert::{LlmFormatConverter, StreamEvent};
use super::types::{ChatRole, ContentBlock, LlmPrompt};
use super::LlmConnector;
use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use crate::mcp::manager::McpContext;

/// Anthropic Messages API endpoint.
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
/// Required `anthropic-version` header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct ClaudeConnector {
    config: ClaudeConfig,
}

impl ClaudeConnector {
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// Resolve the API key from config or the `ANTHROPIC_API_KEY` environment
    /// variable. Returns `Err` if neither source provides a key.
    fn resolve_api_key(&self) -> Result<String, ()> {
        if let Some(key) = self.config.api_key.clone() {
            if !key.is_empty() {
                return Ok(key);
            }
        }
        std::env::var("ANTHROPIC_API_KEY").map_err(|_| ())
    }

    /// The effective `max_tokens` for a request — the user override (if set),
    /// clamped to the model's output ceiling so the API never 400s.
    fn effective_max_tokens(&self) -> u32 {
        let model = ClaudeModel::from_slug(&self.config.model);
        let ceiling = model.max_output_tokens();
        self.config
            .max_tokens
            .unwrap_or(ceiling)
            .min(ceiling)
            .max(1)
    }

    /// Fallback resolution for a prefixed tool name → (server, tool) when it
    /// isn't found in the request-scoped tool map. Splits at the first
    /// underscore, mirroring the OpenAI-compat connector.
    fn resolve_tool_call(&self, prefixed_name: &str) -> (String, String) {
        if let Some(pos) = prefixed_name.find('_') {
            (
                prefixed_name[..pos].to_string(),
                prefixed_name[pos + 1..].to_string(),
            )
        } else {
            ("unknown".to_string(), prefixed_name.to_string())
        }
    }

    /// Produce a user-friendly error message from an Anthropic API error response.
    fn format_api_error(status: u16, body: &str) -> String {
        // Anthropic error envelope: {"type":"error","error":{"type":"...","message":"..."}}
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let err_type = parsed
            .as_ref()
            .and_then(|v| v["error"]["type"].as_str())
            .unwrap_or("");
        let err_msg = parsed
            .as_ref()
            .and_then(|v| v["error"]["message"].as_str())
            .unwrap_or(body);

        match status {
            401 | 403 => "⚠️ **Authentication Failed**\n\n\
                Anthropic rejected your API key.\n\n\
                **→ Go to Settings → AI Model** and check your Claude API Key."
                .to_string(),
            404 if err_type == "not_found_error" => format!(
                "⚠️ **Model Not Found**\n\nThe model was not found or your key lacks access.\n\n{}",
                err_msg
            ),
            429 => "⚠️ **Rate Limited**\n\n\
                You've hit Anthropic's rate limit. Wait a moment and try again."
                .to_string(),
            413 => "⚠️ **Prompt Too Large**\n\n\
                The request exceeds Anthropic's size limit. Reduce conversation \
                history or loaded tools."
                .to_string(),
            529 => "⚠️ **Anthropic Overloaded**\n\n\
                The API is temporarily overloaded. Please retry shortly."
                .to_string(),
            _ => format!("⚠️ **Claude API Error [{}]**\n\n{}", status, err_msg),
        }
    }
}

#[async_trait]
impl LlmConnector for ClaudeConnector {
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
        _session_id: Option<String>,
    ) {
        let api_key = match self.resolve_api_key() {
            Ok(k) => k,
            Err(()) => {
                let _ = tx.send(StreamMessage::Error {
                    message: "⚠️ **API Key Not Configured**\n\nPlease set your Claude API key in \
                        Settings → AI Model to use Claude."
                        .to_string(),
                });
                return;
            }
        };

        let native_request = self.to_native_request(&prompt_data, true);

        // Reverse-map prefixed tool names → (server_name, tool_name), matching the
        // exact format prompt_builder uses for history. Anthropic echoes the tool
        // name we sent in the `tool_use` block, so this resolves it back.
        let mut tool_name_map: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        for tool_def in &prompt_data.tools {
            let prefixed = crate::gemini::convert::get_prefixed_tool_name(
                &tool_def.server_name,
                &tool_def.name,
            );
            tool_name_map.insert(
                prefixed,
                (tool_def.server_name.clone(), tool_def.name.clone()),
            );
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build reqwest client");

        let body_str = serde_json::to_string(&native_request)
            .expect("Failed to serialize Claude request body");

        let model_for_cost = ClaudeModel::from_slug(&self.config.model);

        // Pending tool calls: Anthropic emits a `tool_use` content_block_start
        // (carries id+name), then a run of `input_json_delta` fragments for that
        // block, then content_block_stop. We accumulate the partial JSON and
        // flush complete calls at the end (same shape as the OpenAI-compat path).
        struct PendingToolCall {
            name: String,
            server_name: String,
            arguments: String,
        }

        let flush_tool_calls = |pending: &mut Vec<PendingToolCall>,
                                tx: &mpsc::UnboundedSender<StreamMessage>| {
            for tc in pending.drain(..) {
                let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                let tool_call = ToolCall::new(tc.server_name, tc.name, args, None, None);
                tracing::debug!(
                    "Claude: Sending tool call '{}' on server '{}'",
                    tool_call.tool_name,
                    tool_call.server_name
                );
                let _ = tx.send(StreamMessage::ToolCall(tool_call));
            }
        };

        let emit_usage = |prompt: i32,
                          completion: i32,
                          cached: Option<i32>,
                          tx: &mpsc::UnboundedSender<StreamMessage>| {
            if prompt == 0 && completion == 0 {
                return;
            }
            let cost = (prompt as f64 / 1_000_000.0) * model_for_cost.input_price_per_mtok()
                + (completion as f64 / 1_000_000.0) * model_for_cost.output_price_per_mtok();
            let _ = tx.send(StreamMessage::Usage(UsageData {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
                cached_content_tokens: cached,
                thoughts_tokens: None,
                cost: Some(cost),
            }));
        };

        // Retry transient failures (network errors, 429, 529, 5xx) with backoff —
        // but only while NO content has been streamed to the UI yet, so a
        // mid-stream failure never duplicates output. Honors `retry-after`.
        const MAX_RETRIES: u32 = 2;
        for attempt in 0..=MAX_RETRIES {
            let backoff = std::time::Duration::from_millis(500u64 * (1u64 << attempt));

            let response = match client
                .post(ANTHROPIC_API_URL)
                .header("content-type", "application/json")
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .body(body_str.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        tracing::warn!(
                            "Claude: network error (attempt {}), retrying: {}",
                            attempt + 1,
                            e
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    let _ = tx.send(StreamMessage::Error {
                        message: format!("Network error: {}", e),
                    });
                    return;
                }
            };

            if !response.status().is_success() {
                let code = response.status().as_u16();
                let retryable = code == 429 || code == 529 || response.status().is_server_error();
                if retryable && attempt < MAX_RETRIES {
                    let delay = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(std::time::Duration::from_secs)
                        .unwrap_or(backoff);
                    tracing::warn!(
                        "Claude: HTTP {} (attempt {}), retrying in {:?}",
                        code,
                        attempt + 1,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(StreamMessage::Error {
                    message: Self::format_api_error(code, &body),
                });
                return;
            }

            // ── Per-attempt accumulators (reset on each retry) ──
            let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();
            // Usage is split across message_start (input) and message_delta
            // (output, cumulative); last non-zero value wins, emit once at end.
            let mut usage_prompt: i32 = 0;
            let mut usage_completion: i32 = 0;
            let mut usage_cached: Option<i32> = None;
            // Only Text/Thinking are streamed to the UI mid-flight; tool calls and
            // usage are buffered and emitted at the end. So this gates retry safely.
            let mut has_sent_data = false;

            let mut stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();

            // Gemini-style buffered-line SSE reading: accumulate raw bytes, drain
            // complete lines on '\n'. Anthropic emits one SSE `data:` JSON object
            // per line, so a complete line is always a complete event payload.
            let stream_failed = loop {
                let item = match stream.next().await {
                    Some(it) => it,
                    None => break false, // stream ended cleanly
                };
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        // Retry only if nothing was streamed yet (avoids dup output).
                        if !has_sent_data && attempt < MAX_RETRIES {
                            tracing::warn!(
                                "Claude: stream error (attempt {}, no data yet), retrying: {}",
                                attempt + 1,
                                e
                            );
                            break true;
                        }
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Stream error: {}", e),
                        });
                        return;
                    }
                };

                buffer.extend_from_slice(&bytes);
                while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
                    let line = String::from_utf8_lossy(&line_bytes);

                    for event in self.parse_stream_chunk(&line) {
                        match event {
                            StreamEvent::Text { content } => {
                                if !content.is_empty() {
                                    has_sent_data = true;
                                    let _ = tx.send(StreamMessage::Text {
                                        content,
                                        thought_signature: None,
                                        thought_summary: None,
                                    });
                                }
                            }
                            StreamEvent::Thinking { text, signature } => {
                                // Stream thinking text for display AND carry the
                                // signature (arrives via signature_delta at the end
                                // of the thinking block) so stream_manager persists
                                // it for the next-turn round-trip.
                                let summary = if text.is_empty() { None } else { Some(text) };
                                if summary.is_some() || signature.is_some() {
                                    has_sent_data = true;
                                    let _ = tx.send(StreamMessage::Text {
                                        content: String::new(),
                                        thought_signature: signature,
                                        thought_summary: summary,
                                    });
                                }
                            }
                            StreamEvent::ToolCall { name, arguments, .. } => {
                                if !name.is_empty() {
                                    // New tool_use block — resolve server/tool.
                                    let (server, tool) = tool_name_map
                                        .get(&name)
                                        .cloned()
                                        .unwrap_or_else(|| self.resolve_tool_call(&name));
                                    pending_tool_calls.push(PendingToolCall {
                                        name: tool,
                                        server_name: server,
                                        arguments: arguments.as_str().unwrap_or("").to_string(),
                                    });
                                } else if let Some(last) = pending_tool_calls.last_mut() {
                                    // input_json_delta fragment for the current block.
                                    last.arguments.push_str(arguments.as_str().unwrap_or(""));
                                }
                            }
                            StreamEvent::Usage(u) => {
                                if u.prompt_tokens > 0 {
                                    usage_prompt = u.prompt_tokens;
                                }
                                if u.completion_tokens > 0 {
                                    usage_completion = u.completion_tokens;
                                }
                                if u.cached_content_tokens.is_some() {
                                    usage_cached = u.cached_content_tokens;
                                }
                            }
                            StreamEvent::Done => {
                                flush_tool_calls(&mut pending_tool_calls, &tx);
                                emit_usage(usage_prompt, usage_completion, usage_cached, &tx);
                                return;
                            }
                            StreamEvent::Error { message } => {
                                let _ = tx.send(StreamMessage::Error { message });
                                return;
                            }
                        }
                    }
                }
            };

            if stream_failed {
                tokio::time::sleep(backoff).await;
                continue;
            }

            // Stream ended without an explicit message_stop — flush what we have.
            flush_tool_calls(&mut pending_tool_calls, &tx);
            emit_usage(usage_prompt, usage_completion, usage_cached, &tx);
            return;
        }
    }

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let api_key = self.resolve_api_key().map_err(|()| {
            Box::new(std::io::Error::other(
                "No Claude API key configured for summarization.",
            )) as Box<dyn std::error::Error + Send + Sync>
        })?;

        let summary_model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| self.config.model.clone());

        tracing::debug!(model = %summary_model, "LLM: Summarizing (Claude)");

        let system_prompt = r#"You are an AI assistant that refines a conversation summary.
You will be given a previous summary (which may be empty) and the most recent messages in a conversation.
Your primary task is to integrate the new information from the recent messages into the previous summary, updating and extending it.
Preserve existing information while incorporating new facts, entities, or user preferences.

You MUST respond with ONLY valid JSON (no prose, no markdown fences) containing exactly these fields:
- "summary": A concise, updated summary of the entire conversation so far.
- "sentiment": A brief string describing the user's current mood.
- "current_task": A one-sentence description of the specific task the user is CURRENTLY working on. Empty string if none.
- "entities": An object with:
  - "user_name": The user's name if mentioned.
  - "project_name": The active project or codebase name.
  - "key_topics": Main topics discussed (array of short strings).
  - "key_decisions": Important decisions made (array of short strings).
  - "active_profile": The active Composio profile name if mentioned.
  - "blockers": Current blockers or open issues (array of short strings)."#;

        let user_message = format!(
            "Previous Summary:\n---\n{}\n---\n\nRecent Messages:\n---\n{}\n---",
            previous_summary, recent_messages
        );

        let request_body = json!({
            "model": summary_model,
            "max_tokens": 2048,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": user_message }
            ],
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let response = client
            .post(ANTHROPIC_API_URL)
            .header("content-type", "application/json")
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(serde_json::to_string(&request_body).unwrap_or_default())
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!("Claude summarization error [{}]: {}", status, body);
            return Err(Box::new(std::io::Error::other(format!(
                "Summarization API request failed with status {}: {}",
                status, body
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: Value = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Anthropic response: { "content": [ { "type": "text", "text": "..." }, ... ] }
        let text = response_json["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or_default();

        if text.is_empty() {
            tracing::error!("Claude: Summarization response had no text content.");
            return Ok(serde_json::Value::Null);
        }

        if let Ok(json_value) = serde_json::from_str::<Value>(text) {
            return Ok(json_value);
        }
        // Fallback: extract the JSON object from any surrounding prose.
        if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
            if start <= end {
                if let Ok(json_value) = serde_json::from_str::<Value>(&text[start..=end]) {
                    tracing::warn!("Claude: Parsed summary JSON from within prose.");
                    return Ok(json_value);
                }
            }
        }
        tracing::warn!("Claude: Failed to parse summary as JSON. Using raw text.");
        Ok(json!({ "summary": text, "entities": {}, "sentiment": "neutral" }))
    }

    async fn select_tools_for_toolkit(
        &self,
        request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, String> {
        use crate::mcp::tool_selection::{build_selection_prompt, parse_selection_response};

        let api_key = self
            .resolve_api_key()
            .map_err(|()| "No Claude API key configured for tool selection.".to_string())?;

        let model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| self.config.model.clone());

        tracing::info!(
            model = %model,
            toolkit = %request.toolkit_name,
            tool_count = %request.available_tools.len(),
            "LLM: Selecting tools for toolkit (Claude)"
        );

        let request_body = json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [
                { "role": "user", "content": build_selection_prompt(request) }
            ],
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;

        let response = client
            .post(ANTHROPIC_API_URL)
            .header("content-type", "application/json")
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(serde_json::to_string(&request_body).unwrap_or_default())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Tool selection API request failed with status {}: {}",
                status, body
            ));
        }

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;
        let text = response_json["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or_default();

        if text.is_empty() {
            return Err("No response from Claude for tool selection".to_string());
        }

        parse_selection_response(text)
    }
}

impl LlmFormatConverter for ClaudeConnector {
    fn to_native_request(&self, prompt: &LlmPrompt, streaming: bool) -> serde_json::Value {
        // Extended thinking is only sent when enabled AND the model supports it.
        // Gated here so thinking blocks in history are serialized consistently.
        let thinking_enabled = self.config.extended_thinking
            && ClaudeModel::from_slug(&self.config.model).supports_thinking();

        // Build the Anthropic `messages` array. Claude only allows "user" and
        // "assistant" roles; tool results ride inside user-turn content blocks.
        let mut messages: Vec<serde_json::Value> = Vec::new();

        for msg in &prompt.messages {
            let mut content: Vec<serde_json::Value> = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    ContentBlock::Thinking { text, signature } => {
                        // Anthropic requires a thinking block to be replayed with
                        // its signature and to appear first in the assistant turn
                        // (history is already ordered thinking-first). Only include
                        // when thinking is enabled AND we have a non-empty signature
                        // — a thinking block without a valid signature returns a 400.
                        // Otherwise omit (safe: we never end on an assistant turn, so
                        // the "must start with thinking" rule never applies here).
                        if thinking_enabled {
                            if let Some(sig) = signature {
                                if !sig.is_empty() && !text.is_empty() {
                                    content.push(json!({
                                        "type": "thinking",
                                        "thinking": text,
                                        "signature": sig,
                                    }));
                                }
                            }
                        }
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        // `input` must be a JSON object.
                        let input = if arguments.is_object() {
                            arguments.clone()
                        } else if let Some(s) = arguments.as_str() {
                            serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
                        } else {
                            json!({})
                        };
                        content.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    ContentBlock::ToolResult {
                        call_id, content: c, ..
                    } => {
                        let result_str = if c.is_string() {
                            c.as_str().unwrap_or("").to_string()
                        } else {
                            serde_json::to_string(c).unwrap_or_else(|_| "{}".to_string())
                        };
                        content.push(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": result_str,
                        }));
                    }
                    ContentBlock::Image { mime_type, data } => {
                        content.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": mime_type,
                                "data": data,
                            }
                        }));
                    }
                }
            }

            if content.is_empty() {
                continue;
            }

            // Map neutral role → Anthropic role. Tool results and any System-role
            // messages are folded into the "user" turn.
            let role = match msg.role {
                ChatRole::Assistant => "assistant",
                ChatRole::User | ChatRole::System | ChatRole::Tool => "user",
            };

            messages.push(json!({ "role": role, "content": content }));
        }

        let mut request = json!({
            "model": self.config.model,
            "max_tokens": self.effective_max_tokens(),
            "messages": messages,
            "stream": streaming,
        });

        // System prompt is a top-level field on Anthropic (not a message).
        if let Some(system) = &prompt.system {
            if !system.is_empty() {
                request["system"] = json!(system);
            }
        }

        // Adaptive thinking (GA on modern models; auto-enables interleaved
        // thinking with tools). budget_tokens is intentionally NOT used — it is
        // removed on current models and returns 400.
        if thinking_enabled {
            request["thinking"] = json!({ "type": "adaptive" });
        }

        // Tools — sent with prompt_builder-consistent prefixed names so the
        // streamed `tool_use.name` resolves back via the same map.
        if !prompt.tools.is_empty() {
            let tools: Vec<serde_json::Value> = prompt
                .tools
                .iter()
                .map(|t| {
                    let prefixed =
                        crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);
                    json!({
                        "name": prefixed,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            request["tools"] = json!(tools);
        }

        if self.config.model.is_empty() {
            tracing::error!("Claude request: model is EMPTY — this will 400. Check settings.");
        }

        tracing::debug!(
            "Claude request: model='{}', max_tokens={}, {} messages, {} tools",
            self.config.model,
            self.effective_max_tokens(),
            messages.len(),
            prompt.tools.len(),
        );

        request
    }

    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for line in chunk.lines() {
            let line = line.trim();
            // We only care about `data:` lines; `event:` and blank lines are ignored.
            let json_str = match line.strip_prefix("data:") {
                Some(s) => s.trim(),
                None => continue,
            };
            let val: Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match val["type"].as_str() {
                Some("message_start") => {
                    // Initial usage: input tokens (+ cache reads). output is ~1 here.
                    let usage = &val["message"]["usage"];
                    let input = usage["input_tokens"].as_i64().unwrap_or(0) as i32;
                    let cached = usage["cache_read_input_tokens"].as_i64().map(|v| v as i32);
                    if input > 0 || cached.is_some() {
                        events.push(StreamEvent::Usage(UsageData {
                            prompt_tokens: input,
                            completion_tokens: 0,
                            total_tokens: input,
                            cached_content_tokens: cached,
                            thoughts_tokens: None,
                            cost: None,
                        }));
                    }
                }
                Some("content_block_start") => {
                    let block = &val["content_block"];
                    if block["type"] == "tool_use" {
                        // Declaration: carries id + name; input arrives via deltas.
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        if !name.is_empty() {
                            events.push(StreamEvent::ToolCall {
                                id,
                                name,
                                server_name: None,
                                arguments: json!(""),
                                signature: None,
                            });
                        }
                    }
                }
                Some("content_block_delta") => {
                    let delta = &val["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                events.push(StreamEvent::Text {
                                    content: text.to_string(),
                                });
                            }
                        }
                        Some("input_json_delta") => {
                            // Argument fragment for the current tool_use block.
                            let frag = delta["partial_json"].as_str().unwrap_or("");
                            events.push(StreamEvent::ToolCall {
                                id: String::new(),
                                name: String::new(), // empty = continuation
                                server_name: None,
                                arguments: json!(frag),
                                signature: None,
                            });
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta["thinking"].as_str() {
                                events.push(StreamEvent::Thinking {
                                    text: text.to_string(),
                                    signature: None,
                                });
                            }
                        }
                        Some("signature_delta") => {
                            // Arrives once at the end of a thinking block; carries
                            // the signature needed to replay the block next turn.
                            if let Some(sig) = delta["signature"].as_str() {
                                events.push(StreamEvent::Thinking {
                                    text: String::new(),
                                    signature: Some(sig.to_string()),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    let output = val["usage"]["output_tokens"].as_i64().unwrap_or(0) as i32;
                    if output > 0 {
                        events.push(StreamEvent::Usage(UsageData {
                            prompt_tokens: 0,
                            completion_tokens: output,
                            total_tokens: output,
                            cached_content_tokens: None,
                            thoughts_tokens: None,
                            cost: None,
                        }));
                    }
                }
                Some("message_stop") => events.push(StreamEvent::Done),
                Some("error") => {
                    let msg = val["error"]["message"].as_str().unwrap_or("Unknown Claude error");
                    events.push(StreamEvent::Error {
                        message: format!("Claude API error: {}", msg),
                    });
                }
                // "ping", "content_block_stop", etc. — nothing to emit.
                _ => {}
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

    // sanitize_tool_name: uses trait default (alphanumeric + underscore)

    fn original_tool_name(&self, sanitized: &str) -> Option<String> {
        Some(sanitized.to_string())
    }

    fn max_tools(&self) -> usize {
        // Anthropic supports a large tool list; cap conservatively.
        128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ChatMessage, ToolDefinition};

    fn connector() -> ClaudeConnector {
        ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            max_tokens: Some(4096),
            ..Default::default()
        })
    }

    #[test]
    fn request_puts_system_at_top_level_not_in_messages() {
        let prompt = LlmPrompt {
            system: Some("You are Hobbes.".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }],
            tools: vec![],
        };
        let req = connector().to_native_request(&prompt, true);
        assert_eq!(req["system"], "You are Hobbes.");
        assert_eq!(req["model"], "claude-opus-4-8");
        assert_eq!(req["max_tokens"], 4096);
        assert_eq!(req["stream"], true);
        // System must NOT appear as a message.
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn max_tokens_clamped_to_model_ceiling() {
        let c = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-1".to_string(), // 32K ceiling
            max_tokens: Some(999_999),
            ..Default::default()
        });
        assert_eq!(c.effective_max_tokens(), 32_000);
    }

    #[test]
    fn tool_result_maps_to_user_turn_block() {
        let prompt = LlmPrompt {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_1".to_string(),
                    name: "search".to_string(),
                    content: json!("results here"),
                }],
            }],
            tools: vec![],
        };
        let req = connector().to_native_request(&prompt, true);
        let msg = &req["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"][0]["type"], "tool_result");
        assert_eq!(msg["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(msg["content"][0]["content"], "results here");
    }

    #[test]
    fn tools_use_prefixed_names_and_input_schema() {
        let prompt = LlmPrompt {
            system: None,
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "list_files".to_string(),
                server_name: "fs".to_string(),
                description: "List files".to_string(),
                parameters: json!({ "type": "object", "properties": {} }),
            }],
        };
        let req = connector().to_native_request(&prompt, true);
        let tool = &req["tools"][0];
        assert_eq!(
            tool["name"],
            crate::gemini::convert::get_prefixed_tool_name("fs", "list_files")
        );
        assert_eq!(tool["input_schema"]["type"], "object");
    }

    #[test]
    fn parses_text_delta() {
        let chunk = "event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n";
        let events = connector().parse_stream_chunk(chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Text { content } => assert_eq!(content, "Hello"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn parses_tool_use_start_and_input_delta() {
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"fs_list_files\",\"input\":{}}}\n";
        let delta = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\".\\\"}\"}}\n";

        let start_events = connector().parse_stream_chunk(start);
        match &start_events[0] {
            StreamEvent::ToolCall { name, .. } => assert_eq!(name, "fs_list_files"),
            other => panic!("expected ToolCall decl, got {:?}", other),
        }

        let delta_events = connector().parse_stream_chunk(delta);
        match &delta_events[0] {
            StreamEvent::ToolCall { name, arguments, .. } => {
                assert!(name.is_empty(), "continuation has empty name");
                assert_eq!(arguments.as_str().unwrap(), "{\"path\":\".\"}");
            }
            other => panic!("expected ToolCall fragment, got {:?}", other),
        }
    }

    #[test]
    fn parses_usage_and_message_stop() {
        let start = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n";
        let delta = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":17}}\n";
        let stop = "data: {\"type\":\"message_stop\"}\n";
        let c = connector();

        let su = c.parse_stream_chunk(start);
        match &su[0] {
            StreamEvent::Usage(u) => assert_eq!(u.prompt_tokens, 42),
            other => panic!("expected Usage, got {:?}", other),
        }
        let du = c.parse_stream_chunk(delta);
        match &du[0] {
            StreamEvent::Usage(u) => assert_eq!(u.completion_tokens, 17),
            other => panic!("expected Usage, got {:?}", other),
        }
        assert!(matches!(c.parse_stream_chunk(stop)[0], StreamEvent::Done));
    }

    fn thinking_connector(enabled: bool) -> ClaudeConnector {
        ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            max_tokens: Some(4096),
            extended_thinking: enabled,
            ..Default::default()
        })
    }

    #[test]
    fn thinking_param_sent_only_when_enabled() {
        let prompt = LlmPrompt {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
        };
        let off = thinking_connector(false).to_native_request(&prompt, true);
        assert!(off.get("thinking").is_none());

        let on = thinking_connector(true).to_native_request(&prompt, true);
        assert_eq!(on["thinking"]["type"], "adaptive");
    }

    #[test]
    fn thinking_block_serialized_only_with_signature() {
        let with_sig = LlmPrompt {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: Some("sig-abc".into()),
                    },
                    ContentBlock::Text { text: "answer".into() },
                ],
            }],
            tools: vec![],
        };
        let req = thinking_connector(true).to_native_request(&with_sig, true);
        let blocks = req["messages"][0]["content"].as_array().unwrap();
        // thinking block first, then text
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "reasoning");
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[1]["type"], "text");

        // Without a signature, the thinking block is omitted (would 400).
        let no_sig = LlmPrompt {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: None,
                    },
                    ContentBlock::Text { text: "answer".into() },
                ],
            }],
            tools: vec![],
        };
        let req2 = thinking_connector(true).to_native_request(&no_sig, true);
        let blocks2 = req2["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks2.len(), 1);
        assert_eq!(blocks2[0]["type"], "text");

        // Even with a signature, thinking is omitted when the feature is off.
        let req3 = thinking_connector(false).to_native_request(&with_sig, true);
        let blocks3 = req3["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks3.len(), 1);
        assert_eq!(blocks3[0]["type"], "text");
    }

    #[test]
    fn parses_signature_delta() {
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n";
        let events = connector().parse_stream_chunk(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Thinking { text, signature } => {
                assert!(text.is_empty());
                assert_eq!(signature.as_deref(), Some("sig-xyz"));
            }
            other => panic!("expected Thinking signature, got {:?}", other),
        }
    }
}
