use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::config::OpenAiCompatConfig;
use super::convert::{LlmFormatConverter, StreamEvent};
use super::types::{ChatRole, ContentBlock, LlmPrompt};
use super::LlmConnector;
use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use crate::mcp::manager::McpContext;

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
        mut prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
    ) {
        let base = self.config.endpoint.trim_end_matches('/');
        let endpoint = if base.ends_with("/v1") {
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        };

        // Enforce context window budget if configured.
        // Silently trims oldest messages to fit; logs a warning when trimming occurs.
        // Protected window of 6 messages ensures recent context is always preserved.
        if let Some(max_tokens) = self.config.max_context_tokens {
            // Use the per-provider chars_per_token ratio for accurate estimation
            let chars_per_token = self.config.context_tuning.chars_per_token
                .unwrap_or(crate::context::token_estimator::DEFAULT_CHARS_PER_TOKEN);

            // Log token budget breakdown for diagnostics
            let system_tokens = prompt_data
                .system
                .as_ref()
                .map_or(0, |s| crate::context::token_estimator::estimate_tokens_with_ratio(s, chars_per_token));
            let message_tokens: usize = prompt_data
                .messages
                .iter()
                .map(|m| crate::context::token_estimator::estimate_message_tokens_with_ratio(m, chars_per_token))
                .sum();
            let json_ratio = chars_per_token.min(2.5);
            let tool_tokens: usize = prompt_data
                .tools
                .iter()
                .map(|t| {
                    crate::context::token_estimator::estimate_tokens_with_ratio(&t.name, chars_per_token)
                        + (t.description.chars().count() as f64 / json_ratio).ceil() as usize
                        + (t.parameters.to_string().chars().count() as f64 / json_ratio).ceil() as usize
                        + 10
                })
                .sum();
            let total = system_tokens + message_tokens + tool_tokens;
            tracing::debug!(
                "Context budget: {}/{} tokens @ {:.1} chars/tok (system: {}, messages: {} ({} msgs), tools: {} ({} tools))",
                total, max_tokens, chars_per_token, system_tokens, message_tokens, prompt_data.messages.len(), tool_tokens, prompt_data.tools.len()
            );

            let dropped = prompt_data.enforce_context_budget(max_tokens, 6, chars_per_token);
            if dropped > 0 {
                tracing::warn!(
                    "OpenAI Compat: Trimmed {} oldest messages to fit {} token context window",
                    dropped,
                    max_tokens
                );
            }
        }

        let native_request = self.to_native_request(&prompt_data, true);

        // Build a reverse-map from prefixed tool names → (server_name, tool_name).
        // Uses get_prefixed_tool_name to match the EXACT same format that prompt_builder
        // uses for tool calls in conversation history. This ensures definitions and
        // history names are identical, preventing name mismatch on continuation turns.
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

        // Autorecovery: retry once on transient stream decode errors (e.g. "error decoding response body").
        // These are likely vLLM hiccups, not real model errors. If the error repeats, surface it.
        const MAX_STREAM_RETRIES: u32 = 1;

        'retry: for attempt in 0..=MAX_STREAM_RETRIES {
            let client = Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("Failed to build reqwest client");

            let body_str = serde_json::to_string(&native_request)
                .expect("Failed to serialize OpenAI request body");

            let mut request_builder = client
                .post(&endpoint)
                .header("Content-Type", "application/json")
                .body(body_str);

            if let Some(api_key) = &self.config.api_key {
                request_builder =
                    request_builder.header("Authorization", format!("Bearer {}", api_key));
            } else {
                tracing::warn!("OpenAI Compat: No API key configured — request will be unauthenticated");
            }

            let response = match request_builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: format!("Network error: {}", e),
                    });
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let friendly_message = Self::format_api_error(status.as_u16(), &body, &self.config);
                let _ = tx.send(StreamMessage::Error {
                    message: friendly_message,
                });
                return;
            }

            let mut stream = response.bytes_stream();

            // State for tracking <think>/<thinking> tag boundaries across chunks
            // Disabled for real OpenAI — GPT-5.x uses hidden reasoning tokens, never <think> tags
            let is_real_openai = self.is_real_openai();
            let mut inside_think = false;
            let mut think_buffer = String::new();

            // State for tracking <|channel>thought / <|channel>response token boundaries.
            // These leak from Gemma 4 and similar models when vLLM doesn't separate
            // thinking into the reasoning_content field. Persisted across chunk boundaries.
            let mut inside_channel_think = false;

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

            // Track whether we've sent any data to the UI — if so, retrying would duplicate output
            let mut has_sent_data = false;

            // Helper closure: flush all pending tool calls as StreamMessage::ToolCall
            let flush_tool_calls =
                |pending: &mut Vec<PendingToolCall>,
                 tx: &mpsc::UnboundedSender<StreamMessage>,
                 thought_summary: &mut Option<String>| {
                    for tc in pending.drain(..) {
                        let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                        let tool_call = ToolCall::new(
                            tc.server_name,
                            tc.name,
                            args,
                            None, // thought_signature - not available from OpenAI compat
                            thought_summary.take(), // attach thinking to the first tool call
                        );
                        tracing::debug!(
                            "OpenAI Compat: Sending tool call '{}' on server '{}'",
                            tool_call.tool_name,
                            tool_call.server_name
                        );
                        let _ = tx.send(StreamMessage::ToolCall(tool_call));
                    }
                };

            let mut last_raw_chunk = String::new();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => {
                        let chunk = String::from_utf8_lossy(&bytes);
                        last_raw_chunk = chunk.to_string();
                        let events = self.parse_stream_chunk(&chunk);
                        for event in events {
                            match event {
                                StreamEvent::Thinking { text, .. } => {
                                    // Native reasoning from provider (reasoning_content or reasoning field)
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
                                    has_sent_data = true;
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
                                            if let Some(end_pos) = text_to_process.find("</tool_call>")
                                            {
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
                                            if let Some(start_pos) = text_to_process.find("<tool_call>")
                                            {
                                                // Emit text before the tag
                                                remaining_text.push_str(&text_to_process[..start_pos]);
                                                inside_tool_call = true;
                                                text_to_process =
                                                    text_to_process[start_pos + 11..].to_string(); // skip "<tool_call>"
                                                continue;
                                            } else {
                                                // No tool_call tag — pass through
                                                remaining_text.push_str(&text_to_process);
                                                break;
                                            }
                                        }
                                    }

                                    // Process remaining (non-tool-call) text through think tag detection
                                    // Skip for real OpenAI — GPT-5.x uses hidden reasoning tokens,
                                    // never produces <think>/<thinking> tags. Parsing them would
                                    // only catch echoed tags from our own history injection.
                                    if !remaining_text.is_empty() {
                                        // Phase 1: Strip leaked <|channel>thought / <|channel>response
                                        // special tokens (Gemma 4, etc.) BEFORE XML tag parsing.
                                        // These can appear even when thinking mode is disabled.
                                        let channel_segments = super::convert::strip_channel_tokens(
                                            &remaining_text,
                                            &mut inside_channel_think,
                                        );

                                        for (is_channel_thinking, segment_text) in channel_segments {
                                            if is_channel_thinking {
                                                // Route to thinking display
                                                if let Some(ref mut summary) = accumulated_thought_summary {
                                                    summary.push_str(&segment_text);
                                                } else {
                                                    accumulated_thought_summary = Some(segment_text.clone());
                                                }
                                                let _ = tx.send(StreamMessage::Text {
                                                    content: String::new(),
                                                    thought_signature: None,
                                                    thought_summary: Some(segment_text),
                                                });
                                            } else if !segment_text.is_empty() {
                                                // Phase 2: Process non-thinking segments through
                                                // XML think tag detection (<think>/<thinking>)
                                                if is_real_openai {
                                                    // Real OpenAI: pass text straight through, no tag parsing
                                                    let _ = tx.send(StreamMessage::Text {
                                                        content: segment_text,
                                                        thought_signature: None,
                                                        thought_summary: None,
                                                    });
                                                } else {
                                                    let processed = self.split_think_tags(
                                                        &segment_text,
                                                        &mut inside_think,
                                                        &mut think_buffer,
                                                    );
                                                    for (is_thinking, text) in processed {
                                                        if is_thinking {
                                                            // Accumulate thinking for potential tool call attachment
                                                            if let Some(ref mut summary) =
                                                                accumulated_thought_summary
                                                            {
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
                                        }
                                        has_sent_data = true;
                                    }
                                }
                                StreamEvent::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                    ..
                                } => {
                                    // OpenAI streams tool calls incrementally:
                                    //   Chunk 1: id + name (arguments empty)
                                    //   Chunk 2+: arguments fragments
                                    // We match by index position in our pending_tool_calls vec.
                                    // If `name` is non-empty, it's a new tool call declaration.
                                    // If `name` is empty, it's an argument fragment for the last pending call.
                                    if !name.is_empty() {
                                        // Resolve server/tool name using the pre-built map.
                                        // The map uses get_prefixed_tool_name format (same as prompt_builder)
                                        // to ensure definitions and history names are consistent.
                                        let (resolved_server, resolved_tool) = if let Some((
                                            server,
                                            tool,
                                        )) =
                                            tool_name_map.get(&name)
                                        {
                                            (server.clone(), tool.clone())
                                        } else {
                                            // Fallback: split at first underscore for names not in the map
                                            tracing::warn!(
                                                "OpenAI Compat: Tool name '{}' not found in tool_name_map, falling back to underscore split",
                                                name
                                            );
                                            self.resolve_tool_call(&name)
                                        };
                                        pending_tool_calls.push(PendingToolCall {
                                            id,
                                            name: resolved_tool,
                                            server_name: resolved_server,
                                            // CRITICAL: Use .as_str() not .to_string()!
                                            // arguments is a serde_json::Value wrapping a string fragment.
                                            // .to_string() produces quoted JSON repr ("\"\""), but we need
                                            // raw string for accumulation with subsequent fragments.
                                            arguments: arguments.as_str().unwrap_or("").to_string(),
                                        });
                                    } else if let Some(last) = pending_tool_calls.last_mut() {
                                        // Argument fragment — append to last pending tool call
                                        let frag = arguments.as_str().unwrap_or("");
                                        last.arguments.push_str(frag);
                                    }
                                    has_sent_data = true;
                                }
                                StreamEvent::Usage(usage) => {
                                    let _ = tx.send(StreamMessage::Usage(usage));
                                }
                                StreamEvent::Done => {
                                    // Flush any pending tool calls before returning
                                    flush_tool_calls(
                                        &mut pending_tool_calls,
                                        &tx,
                                        &mut accumulated_thought_summary,
                                    );
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
                        // Dump the exact payload returned by vLLM before error decoding failure
                        tracing::error!(
                            "OpenAI Compat Stream Error! Last received chunk before drop:\n---\n{}\n---",
                            last_raw_chunk
                        );

                        // Autorecovery: if no data has been sent yet and we have retries left,
                        // retry the full request. This handles transient stream decode errors
                        // (e.g. "error decoding response body") from vLLM.
                        if !has_sent_data && attempt < MAX_STREAM_RETRIES {
                            tracing::warn!(
                                "OpenAI Compat: Stream error on attempt {} (no data sent yet), retrying: {}",
                                attempt + 1, e
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue 'retry;
                        }
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Stream error: {}", e),
                        });
                        return;
                    }
                }
            }

            // Stream ended without explicit [DONE] — flush any pending tool calls
            flush_tool_calls(
                &mut pending_tool_calls,
                &tx,
                &mut accumulated_thought_summary,
            );
            break; // success — don't retry
        }
    }

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let summary_model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.model)
            .clone();

        if summary_model.is_empty() {
            tracing::error!(
                "OpenAI Compat summarizer: model slug is EMPTY (summary_model={:?}, chat_model='{}'). \
                Cannot summarize without a model.",
                self.config.summary_model, self.config.model
            );
            return Err(Box::new(std::io::Error::other(
                "No model configured for summarization. Please select a model in OpenAI settings."
            )) as Box<dyn std::error::Error + Send + Sync>);
        }

        tracing::debug!(model = %summary_model, "LLM: Summarizing (OpenAI Compat)");

        let base = self.config.endpoint.trim_end_matches('/');
        let endpoint = if base.ends_with("/v1") {
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        };

        let system_prompt = r#"You are an AI assistant that refines a conversation summary.
You will be given a previous summary (which may be empty) and the most recent messages in a conversation.
Your primary task is to integrate the new information from the recent messages into the previous summary, updating and extending it.
Preserve existing information while incorporating new facts, entities, or user preferences.

You MUST respond with valid JSON containing exactly these fields:
- "summary": A concise, updated summary of the entire conversation so far.
- "sentiment": A brief string describing the user's current mood (e.g., "curious and collaborative", "frustrated but focused").
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
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": user_message }
            ],
            "response_format": { "type": "json_object" },
            "stream": false
        });

        // Dump truncated JSON to verify model is actually in the serialized body
        let body_str = serde_json::to_string(&request_body).unwrap_or_default();
        tracing::debug!(
            "Summarizer request: endpoint='{}', body_start='{}'",
            endpoint,
            &body_str[..body_str.len().min(200)],
        );

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let mut request_builder = client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .body(body_str);

        if let Some(api_key) = &self.config.api_key {
            request_builder =
                request_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!("OpenAI Compat summarization error [{}]: {}", status, body);
            return Err(Box::new(std::io::Error::other(format!(
                "Summarization API request failed with status {}: {}",
                status, body
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: Value = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Extract content from OpenAI response format:
        // { "choices": [{ "message": { "content": "..." } }] }
        if let Some(content) = response_json["choices"][0]["message"]["content"].as_str() {
            // Try parsing as JSON
            if let Ok(json_value) = serde_json::from_str(content) {
                return Ok(json_value);
            }

            // Fallback: extract JSON from markdown code block
            if let Some(start) = content.find('{') {
                if let Some(end) = content.rfind('}') {
                    let potential_json = &content[start..=end];
                    if let Ok(json_value) = serde_json::from_str(potential_json) {
                        tracing::warn!(
                            "OpenAI Compat: Parsed summary JSON from markdown code block."
                        );
                        return Ok(json_value);
                    }
                }
            }

            // Last resort: wrap raw text as summary
            tracing::warn!(
                "OpenAI Compat: Failed to parse summary response as JSON. Using raw text."
            );
            return Ok(json!({
                "summary": content,
                "entities": {},
                "sentiment": "neutral"
            }));
        }

        tracing::error!("OpenAI Compat: Summarization response had no content.");
        Ok(serde_json::Value::Null)
    }
}

impl LlmFormatConverter for OpenAiCompatConnector {
    fn to_native_request(&self, prompt: &LlmPrompt, streaming: bool) -> serde_json::Value {
        // Build messages as serde_json::Value directly — the OpenAI API requires
        // different message shapes for text, tool calls, and tool results.
        // A flat struct cannot represent all three.
        let mut messages: Vec<serde_json::Value> = Vec::new();

        if let Some(system) = &prompt.system {
            messages.push(json!({
                "role": "system",
                "content": system,
            }));
        }

        for msg in &prompt.messages {
            // Separate content blocks by type: text/thinking go in `content`,
            // tool calls go in `tool_calls`, tool results are separate messages.
            let mut text_parts: Vec<String> = Vec::new();
            let mut image_parts: Vec<(String, String)> = Vec::new(); // (mime_type, base64_data)
            let mut tool_calls_array: Vec<serde_json::Value> = Vec::new();
            let mut tool_result: Option<(String, String, String)> = None; // (call_id, name, content)

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::Thinking { text, .. } => {
                        if self.is_real_openai() || self.config.thinking_enabled {
                            // Real OpenAI / thinking-enabled models (Gemma 4, Qwen3):
                            // STRIP thinking from history. These models use native tokens
                            // (<|channel>thought for Gemma, <|think|> for Qwen) — not XML
                            // tags. Injecting <thinking> tags confuses the model and can
                            // cause thinking text to leak into the response content.
                        } else {
                            // Local models without native thinking: wrap in tags so
                            // models can parse them back if they use <think>/<thinking>.
                            text_parts.push(format!("<thinking>\n{}\n</thinking>", text));
                        }
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        // OpenAI assistant tool call format:
                        // {"id": "call_123", "type": "function", "function": {"name": "...", "arguments": "..."}}
                        let args_string = if arguments.is_string() {
                            arguments.as_str().unwrap_or("{}").to_string()
                        } else {
                            serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_calls_array.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                // DO NOT re-sanitize: prompt_builder already sanitizes the name
                                // with get_prefixed_tool_name. Double-sanitizing can alter the
                                // server__tool separator and break resolve_tool_call on the response.
                                "name": name,
                                "arguments": args_string,
                            }
                        }));
                        tracing::debug!(
                            "OpenAI Compat request: Including ToolCall id='{}' name='{}' in assistant message",
                            id, name
                        );
                    }
                    ContentBlock::ToolResult {
                        call_id,
                        name,
                        content,
                    } => {
                        // OpenAI tool result format: separate message with role "tool"
                        let content_string = if content.is_string() {
                            content.as_str().unwrap_or("").to_string()
                        } else {
                            serde_json::to_string(content).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_result = Some((call_id.clone(), name.clone(), content_string));
                        tracing::debug!(
                            "OpenAI Compat request: Including ToolResult call_id='{}' name='{}'",
                            call_id,
                            name
                        );
                    }
                    ContentBlock::Image { mime_type, data } => {
                        image_parts.push((mime_type.clone(), data.clone()));
                    }
                }
            }

            let role_str = match msg.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::System => "system",
                ChatRole::Tool => "tool",
            };

            // Build the message based on what we found
            if let Some((call_id, _name, content_str)) = tool_result {
                // Tool result message — OpenAI requires role=tool + tool_call_id
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content_str,
                }));
            } else if !tool_calls_array.is_empty() {
                // Assistant message with tool calls
                let mut msg_value = json!({
                    "role": role_str,
                    "tool_calls": tool_calls_array,
                });
                // Include text content alongside tool calls if present (thinking + call)
                if !text_parts.is_empty() {
                    msg_value["content"] = json!(text_parts.join("\n"));
                }
                messages.push(msg_value);
            } else if image_parts.is_empty() {
                // Regular text message (no images)
                messages.push(json!({
                    "role": role_str,
                    "content": text_parts.join("\n"),
                }));
            } else {
                // Multimodal message — use structured content array (OpenAI Vision format)
                // Supported by vLLM, Ollama, LM Studio, and the real OpenAI API.
                let mut content_array: Vec<serde_json::Value> = Vec::new();
                let joined_text = text_parts.join("\n");
                if !joined_text.is_empty() {
                    content_array.push(json!({
                        "type": "text",
                        "text": joined_text,
                    }));
                }
                for (mime_type, data) in &image_parts {
                    content_array.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", mime_type, data),
                        }
                    }));
                }
                tracing::debug!(
                    "OpenAI Compat: Multimodal message with {} text + {} image parts",
                    if joined_text.is_empty() { 0 } else { 1 },
                    image_parts.len()
                );
                messages.push(json!({
                    "role": role_str,
                    "content": content_array,
                }));
            }
        }

        // === Defensive Sanitizer ===
        // OpenAI strictly requires: messages with role 'tool' must follow a preceding
        // message with role 'assistant' that contains 'tool_calls'. Context budget
        // trimming or history windowing can break these pairs. Strip orphans.
        let pre_sanitize_len = messages.len();
        let mut i = 0;
        while i < messages.len() {
            if messages[i].get("role").and_then(|r| r.as_str()) == Some("tool") {
                // Check if the preceding message is an assistant with tool_calls
                let has_preceding_tool_calls = i > 0
                    && messages[i - 1].get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && messages[i - 1].get("tool_calls").is_some();
                // Also allow consecutive tool results (multiple tools in one turn)
                let follows_another_tool = i > 0
                    && messages[i - 1].get("role").and_then(|r| r.as_str()) == Some("tool");

                if !has_preceding_tool_calls && !follows_another_tool {
                    tracing::warn!(
                        "OpenAI sanitizer: Removing orphan tool result at position {} (call_id={:?})",
                        i,
                        messages[i].get("tool_call_id")
                    );
                    messages.remove(i);
                    continue; // Don't increment, recheck this position
                }
            }
            i += 1;
        }
        if messages.len() < pre_sanitize_len {
            tracing::warn!(
                "OpenAI sanitizer: Removed {} orphan tool result(s) from prompt",
                pre_sanitize_len - messages.len()
            );
        }

        let tools = if !self.config.tools_enabled || prompt.tools.is_empty() {
            None
        } else {
            // OpenAI tool format — use get_prefixed_tool_name to match the names
            // that prompt_builder uses for tool calls in conversation history.
            // This ensures the model sees consistent names in both definitions and history.
            let openai_tools: Vec<serde_json::Value> = prompt
                .tools
                .iter()
                .map(|t| {
                    let prefixed_name =
                        crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);
                    json!({
                        "type": "function",
                        "function": {
                            "name": prefixed_name,
                            "description": t.description,
                            "parameters": t.parameters
                        }
                    })
                })
                .collect();
            Some(openai_tools)
        };

        let mut request = json!({
            "model": self.config.model,
            "messages": messages,
            "tools": tools,
            "stream": streaming,
        });

        // When thinking mode is enabled, configure the request so vLLM correctly
        // separates thinking content into the `reasoning`/`reasoning_content` delta
        // fields instead of leaking it into the text content.
        //
        // Two parameters are required:
        //   1. chat_template_kwargs: {"enable_thinking": true}
        //      Activates the model's thinking template (e.g. Gemma 4's <|channel> tokens).
        //   2. skip_special_tokens: false
        //      CRITICAL: vLLM defaults to stripping special tokens. Gemma 4 uses
        //      <|channel>thought / <|channel>response tokens as delimiters. If these
        //      are stripped (default), the reasoning parser cannot separate thinking
        //      from response content, causing thinking text to leak into `content`
        //      as raw text with no way to parse it.
        if self.config.thinking_enabled && !self.is_real_openai() {
            request["chat_template_kwargs"] = json!({"enable_thinking": true});
            request["skip_special_tokens"] = json!(false);
            tracing::debug!("OpenAI Compat: Thinking mode enabled — injecting chat_template_kwargs + skip_special_tokens=false");
        }

        if self.config.model.is_empty() {
            tracing::error!(
                "OpenAI Compat request: model is EMPTY — this will cause a 400 error. \
                Check that openai_compat_config.model is set in settings."
            );
        }

        tracing::debug!(
            "OpenAI Compat request: model='{}', {} messages, {} tools, endpoint='{}', thinking={}",
            self.config.model,
            messages.len(),
            tools.as_ref().map_or(0, |t| t.len()),
            self.config.endpoint,
            self.config.thinking_enabled,
        );

        request
    }

    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for line in chunk.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

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
                                events.push(StreamEvent::Text {
                                    content: content.to_string(),
                                });
                            }

                            // Tool calls delta — OpenAI streams these incrementally:
                            //   First chunk:  {"index": 0, "id": "call_xxx", "function": {"name": "fn_name", "arguments": ""}}
                            //   Later chunks: {"index": 0, "function": {"arguments": "{\"key\": "}}
                            //   More chunks:  {"index": 0, "function": {"arguments": "\"value\"}"}}
                            // We emit each delta as a StreamEvent::ToolCall.
                            // The accumulation happens in generate_content_stream.
                            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                                for tc in tool_calls {
                                    let id = tc
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string();
                                    let name_raw = tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default();
                                    let args_fragment = tc
                                        .get("function")
                                        .and_then(|f| f.get("arguments"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    if !name_raw.is_empty() {
                                        // Pass raw name through — resolution happens in
                                        // generate_content_stream using the tool_name_map.
                                        events.push(StreamEvent::ToolCall {
                                            id,
                                            name: name_raw.to_string(),
                                            server_name: None,
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

                            // Native reasoning content (multiple field names across providers):
                            // - "reasoning_content": DeepSeek API
                            // - "reasoning": vLLM with Qwen 3.5 (--enable-reasoning)
                            let reasoning = choice["delta"]["reasoning_content"]
                                .as_str()
                                .or_else(|| choice["delta"]["reasoning"].as_str());
                            if let Some(reasoning) = reasoning {
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
                        if let (Some(p), Some(c), Some(t)) = (
                            usage["prompt_tokens"].as_u64(),
                            usage["completion_tokens"].as_u64(),
                            usage["total_tokens"].as_u64(),
                        ) {
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
        128
    }
}

impl OpenAiCompatConnector {
    /// Returns true if the configured endpoint points to the real OpenAI API.
    /// Used to gate behavior that only applies to official OpenAI models (GPT-5.x):
    /// - Skip <thinking> tag injection in history (GPT-5.x uses hidden reasoning tokens)
    /// - Disable split_think_tags parser (GPT-5.x never produces <think> tags)
    fn is_real_openai(&self) -> bool {
        self.config.endpoint.contains("api.openai.com")
    }

    /// Resolve a sanitized tool name back to (server_name, tool_name).
    ///
    /// Tool names use get_prefixed_tool_name format: `{sanitized_server}_{tool_name}`.
    /// Since server names may contain underscores after sanitization (e.g. "composio_native"),
    /// we try matching from the first underscore position outward.
    /// This is a last-resort fallback — the tool_name_map lookup should handle most cases.
    fn resolve_tool_call(&self, sanitized_name: &str) -> (String, String) {
        // Try splitting at each underscore position to find a valid server/tool pair
        if let Some(pos) = sanitized_name.find('_') {
            let server = sanitized_name[..pos].to_string();
            let tool = sanitized_name[pos + 1..].to_string();
            (server, tool)
        } else {
            tracing::warn!(
                "OpenAI Compat: Tool name '{}' has no underscore separator — treating as raw tool name.",
                sanitized_name
            );
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
        tracing::trace!("OpenAI Compat: Parsing inline tool_call: {}", trimmed);

        match serde_json::from_str::<Value>(trimmed) {
            Ok(val) => {
                // Hermes format: {"name": "fn_name", "arguments": {"key": "value"}}
                // Also handle: {"function": {"name": "...", "arguments": {...}}}
                let (name_raw, args) = if let Some(name) = val.get("name").and_then(|v| v.as_str())
                {
                    let arguments = val.get("arguments").cloned().unwrap_or(json!({}));
                    (name.to_string(), arguments)
                } else if let Some(func) = val.get("function") {
                    let name = func
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let arguments = func.get("arguments").cloned().unwrap_or(json!({}));
                    (name.to_string(), arguments)
                } else {
                    tracing::warn!(
                        "OpenAI Compat: Inline tool_call JSON missing 'name': {}",
                        trimmed
                    );
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
                tracing::debug!(
                    "OpenAI Compat: Inline tool call '{}' on server '{}'",
                    tool_name,
                    server_name
                );
                let _ = tx.send(StreamMessage::ToolCall(tool_call));
            }
            Err(e) => {
                tracing::warn!(
                    "OpenAI Compat: Failed to parse inline tool_call JSON: {} — raw: {}",
                    e,
                    trimmed
                );
                // Fall back to emitting as text so the user at least sees it
                let _ = tx.send(StreamMessage::Text {
                    content: format!("<tool_call>{}</tool_call>", trimmed),
                    thought_signature: None,
                    thought_summary: None,
                });
            }
        }
    }

    /// Parse common OpenAI-compatible API error patterns and produce user-friendly
    /// error messages that guide users to the correct settings.
    fn format_api_error(status: u16, body: &str, config: &OpenAiCompatConfig) -> String {
        let body_lower = body.to_lowercase();

        // Context length / input token overflow
        if body_lower.contains("context length")
            || body_lower.contains("input_tokens")
            || body_lower.contains("maximum context")
            || body_lower.contains("reduce the length")
        {
            let max_ctx = config
                .max_context_tokens
                .map(|t| format!("{}", t))
                .unwrap_or_else(|| "not set".to_string());
            return format!(
                "⚠️ **Prompt Too Large**\n\n\
                Your prompt exceeds this model's context window \
                (auto-detected: **{}** tokens).\n\n\
                Try reducing conversation history, loaded tools, \
                or switch to a model with a larger context window.",
                max_ctx
            );
        }

        // Model not found / invalid model
        if body_lower.contains("model_not_found")
            || body_lower.contains("model not found")
            || body_lower.contains("does not exist")
        {
            return format!(
                "⚠️ **Model Not Found**\n\n\
                The model '{}' was not found on this server.\n\n\
                **→ Go to Settings → LLM Configuration** and click \
                **Refresh** to see available models.",
                config.model
            );
        }

        // Authentication errors
        if status == 401
            || status == 403
            || body_lower.contains("unauthorized")
            || body_lower.contains("invalid api key")
            || body_lower.contains("authentication")
        {
            return "⚠️ **Authentication Failed**\n\n\
                The server rejected your API key.\n\n\
                **→ Go to Settings → LLM Configuration** and check your API Key."
                .to_string();
        }

        // Fallback: show raw error with guidance
        format!(
            "⚠️ **Server Error [{}]**\n\n{}\n\n\
            If this persists, check **Settings → LLM Configuration**.",
            status, body
        )
    }
}

