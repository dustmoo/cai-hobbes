//! OpenAI **Responses API** connector (`POST /v1/responses`).
//!
//! This is additive to [`OpenAiCompatConnector`](super::openai_compat::OpenAiCompatConnector),
//! which speaks Chat Completions. The newest OpenAI models (gpt-5 / o-series)
//! are served *only* by the Responses API, which uses a different request shape
//! (`instructions` + typed `input` items), a different tool-call representation
//! (`function_call` / `function_call_output` items), and semantic SSE events
//! (`response.output_text.delta`, `response.function_call_arguments.delta`, …)
//! instead of `choices[].delta` chunks.
//!
//! Routing lives in [`build_openai_connector`]: under `ApiStyle::Auto` we only
//! use Responses for the gpt-5/o family on `api.openai.com`; every other
//! endpoint/model keeps using Chat Completions, so nothing existing regresses.
//!
//! Conversations are sent **statelessly** (full history as `input` each turn);
//! we never use `previous_response_id` server-side state.

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::config::{ApiStyle, OpenAiCompatConfig};
use super::types::{ChatRole, ContentBlock, LlmPrompt};
use super::LlmConnector;
use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use crate::mcp::manager::McpContext;

// ── Routing ──────────────────────────────────────────────────────────────────

/// True if the model belongs to OpenAI's Responses-first family (gpt-5 / o-series).
/// The Responses API works for *all* current OpenAI models, but only this family
/// *requires* it (Chat Completions rejects them), so Auto routes the family here.
pub fn is_responses_family(model: &str) -> bool {
    let m = model.trim();
    m.starts_with("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
}

fn endpoint_is_openai(endpoint: &str) -> bool {
    endpoint.contains("api.openai.com")
}

/// Decide whether the configured endpoint/model should use the Responses API.
pub fn should_use_responses(config: &OpenAiCompatConfig, model: &str) -> bool {
    match config.api_style {
        ApiStyle::Responses => true,
        ApiStyle::ChatCompletions => false,
        ApiStyle::Auto => endpoint_is_openai(&config.endpoint) && is_responses_family(model),
    }
}

/// Single construction site for the OpenAI provider: returns the Responses
/// connector or the Chat Completions connector based on [`should_use_responses`].
/// `config.model` must already be set to the effective model.
pub fn build_openai_connector(config: OpenAiCompatConfig) -> Arc<dyn LlmConnector> {
    if should_use_responses(&config, &config.model) {
        tracing::info!(
            "OpenAI: routing model '{}' to the Responses API",
            config.model
        );
        Arc::new(OpenAiResponsesConnector::new(config))
    } else {
        Arc::new(super::openai_compat::OpenAiCompatConnector::new(config))
    }
}

// ── Connector ────────────────────────────────────────────────────────────────

pub struct OpenAiResponsesConnector {
    config: OpenAiCompatConfig,
}

impl OpenAiResponsesConnector {
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self { config }
    }

    fn responses_endpoint(&self) -> String {
        let base = self.config.endpoint.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{}/responses", base)
        } else {
            format!("{}/v1/responses", base)
        }
    }

    /// Build the `input` array of typed Responses items from neutral messages.
    /// A single neutral message may expand into several items (assistant text +
    /// `function_call`s, or a `function_call_output`).
    fn build_input_items(&self, prompt: &LlmPrompt) -> Vec<Value> {
        let mut items: Vec<Value> = Vec::new();

        for msg in &prompt.messages {
            let mut text_parts: Vec<String> = Vec::new();
            let mut image_parts: Vec<(String, String)> = Vec::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (call_id, name, args)
            let mut tool_result: Option<(String, String)> = None; // (call_id, output)

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    // Reasoning is hidden server-side; never re-inject it into history.
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolCall {
                        id, name, arguments, ..
                    } => {
                        let args = if arguments.is_string() {
                            arguments.as_str().unwrap_or("{}").to_string()
                        } else {
                            serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_calls.push((id.clone(), name.clone(), args));
                    }
                    ContentBlock::ToolResult {
                        call_id, content, ..
                    } => {
                        let output = if content.is_string() {
                            content.as_str().unwrap_or("").to_string()
                        } else {
                            serde_json::to_string(content).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_result = Some((call_id.clone(), output));
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
                ChatRole::Tool => "user",
            };
            let joined = text_parts.join("\n");

            if let Some((call_id, output)) = tool_result {
                // Tool results are standalone items correlated by call_id.
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            } else if !tool_calls.is_empty() {
                // Optional assistant prose, then one function_call item per call.
                if !joined.is_empty() {
                    items.push(json!({ "role": "assistant", "content": joined }));
                }
                for (call_id, name, args) in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": args,
                    }));
                }
            } else if !image_parts.is_empty() {
                // Multimodal message: input_text + input_image parts.
                let mut content_array: Vec<Value> = Vec::new();
                if !joined.is_empty() {
                    content_array.push(json!({ "type": "input_text", "text": joined }));
                }
                for (mime_type, data) in &image_parts {
                    content_array.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", mime_type, data),
                    }));
                }
                items.push(json!({ "role": role_str, "content": content_array }));
            } else if !joined.is_empty() {
                // Plain text message — Responses accepts a string `content`.
                items.push(json!({ "role": role_str, "content": joined }));
            }
        }

        items
    }

    /// Build the full `/v1/responses` request body.
    fn to_responses_request(&self, prompt: &LlmPrompt, streaming: bool) -> Value {
        let input = self.build_input_items(prompt);

        let tools: Option<Vec<Value>> =
            if !self.config.tools_enabled || prompt.tools.is_empty() {
                None
            } else {
                Some(
                    prompt
                        .tools
                        .iter()
                        .map(|t| {
                            let name = crate::gemini::convert::get_prefixed_tool_name(
                                &t.server_name,
                                &t.name,
                            );
                            // Responses flattens the function shape (no nested
                            // `function` object). `strict: false` avoids strict
                            // JSON-schema validation our MCP tools may not satisfy.
                            json!({
                                "type": "function",
                                "name": name,
                                "description": t.description,
                                "parameters": t.parameters,
                                "strict": false,
                            })
                        })
                        .collect(),
                )
            };

        let mut request = json!({
            "model": self.config.model,
            "input": input,
            "stream": streaming,
        });

        if let Some(system) = &prompt.system {
            request["instructions"] = json!(system);
        }
        if let Some(tools) = tools {
            request["tools"] = json!(tools);
        }
        // Opt into reasoning summaries so the thinking UI has something to show.
        // (Raw reasoning tokens are never exposed by the API — only summaries.)
        if self.config.thinking_enabled {
            request["reasoning"] = json!({ "summary": "auto" });
        }

        if self.config.model.is_empty() {
            tracing::error!(
                "OpenAI Responses request: model is EMPTY — this will cause a 400 error."
            );
        }

        request
    }

    /// Reverse-map prefixed tool names → (server, tool), matching prompt_builder.
    fn build_tool_name_map(
        &self,
        prompt: &LlmPrompt,
    ) -> std::collections::HashMap<String, (String, String)> {
        let mut map = std::collections::HashMap::new();
        for t in &prompt.tools {
            let prefixed =
                crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);
            map.insert(prefixed, (t.server_name.clone(), t.name.clone()));
        }
        map
    }

    /// Resolve a prefixed tool name; fall back to splitting at the first `_`.
    fn resolve_tool_name(
        name: &str,
        map: &std::collections::HashMap<String, (String, String)>,
    ) -> (String, String) {
        if let Some(pair) = map.get(name) {
            return pair.clone();
        }
        if let Some(pos) = name.find('_') {
            (name[..pos].to_string(), name[pos + 1..].to_string())
        } else {
            ("unknown".to_string(), name.to_string())
        }
    }
}

/// A function call being assembled from streamed events, keyed by its item id.
struct PendingCall {
    item_id: String,
    call_id: String,
    server_name: String,
    tool_name: String,
    arguments: String,
}

/// Semantic Responses streaming event, classified from one SSE `data:` object.
/// Pure (no I/O) so it can be unit-tested against captured payloads.
#[derive(Debug, PartialEq)]
enum RespEvent {
    TextDelta(String),
    ReasoningDelta(String),
    FunctionCallAdded {
        item_id: String,
        call_id: String,
        name: String,
        initial_args: String,
    },
    FunctionCallArgsDelta {
        item_id: String,
        delta: String,
    },
    FunctionCallArgsDone {
        item_id: String,
        arguments: String,
    },
    Completed {
        usage: Option<UsageData>,
        /// Full assistant text from the final response object. Used as a fallback
        /// when a model emits no incremental `output_text.delta` events.
        final_text: String,
    },
    Failed(String),
    Ignored,
}

fn parse_usage(response: &Value) -> Option<UsageData> {
    let usage = response.get("usage")?;
    let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as i32;
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as i32;
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or((input + output) as u64) as i32;
    Some(UsageData {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cached_content_tokens: None,
        thoughts_tokens: None,
        cost: Some(0.0),
    })
}

/// Classify a single parsed SSE `data:` JSON object into a [`RespEvent`].
fn classify_event(v: &Value) -> RespEvent {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "response.output_text.delta" => {
            RespEvent::TextDelta(v.get("delta").and_then(|d| d.as_str()).unwrap_or("").to_string())
        }
        "response.reasoning_summary_text.delta" => RespEvent::ReasoningDelta(
            v.get("delta").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        ),
        "response.output_item.added" | "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                RespEvent::FunctionCallAdded {
                    item_id: item.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    call_id: item
                        .get("call_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: item.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    initial_args: item
                        .get("arguments")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            } else {
                RespEvent::Ignored
            }
        }
        "response.function_call_arguments.delta" => RespEvent::FunctionCallArgsDelta {
            item_id: v.get("item_id").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            delta: v.get("delta").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        },
        "response.function_call_arguments.done" => RespEvent::FunctionCallArgsDone {
            item_id: v.get("item_id").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            arguments: v
                .get("arguments")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "response.completed" => {
            let response = v.get("response");
            RespEvent::Completed {
                usage: response.and_then(parse_usage),
                final_text: response.map(extract_output_text).unwrap_or_default(),
            }
        }
        "response.failed" | "response.incomplete" => {
            let msg = v
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Response failed")
                .to_string();
            RespEvent::Failed(msg)
        }
        "error" => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()))
                .unwrap_or("Stream error")
                .to_string();
            RespEvent::Failed(msg)
        }
        _ => RespEvent::Ignored,
    }
}

#[async_trait]
impl LlmConnector for OpenAiResponsesConnector {
    async fn generate_content_stream(
        &self,
        mut prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
        _session_id: Option<String>,
    ) {
        // Enforce context budget (mirrors the Chat Completions connector).
        if let Some(max_tokens) = self.config.max_context_tokens {
            let chars_per_token = self
                .config
                .context_tuning
                .chars_per_token
                .unwrap_or(crate::context::token_estimator::DEFAULT_CHARS_PER_TOKEN);
            let dropped = prompt_data.enforce_context_budget(max_tokens, 6, chars_per_token);
            if dropped > 0 {
                tracing::warn!(
                    "OpenAI Responses: trimmed {} oldest messages to fit {} token window",
                    dropped,
                    max_tokens
                );
            }
        }

        let endpoint = self.responses_endpoint();
        let tool_name_map = self.build_tool_name_map(&prompt_data);
        let request = self.to_responses_request(&prompt_data, true);
        let body_str = match serde_json::to_string(&request) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Failed to serialize request: {}", e),
                });
                return;
            }
        };

        // Reasoning models (gpt-5-pro especially) can think silently for minutes
        // before the first token, so allow a generous total stream duration.
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("Failed to build reqwest client");

        let mut request_builder = client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .body(body_str);
        if let Some(api_key) = &self.config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            tracing::warn!("OpenAI Responses: no API key configured — request will be unauthenticated");
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
            let _ = tx.send(StreamMessage::Error {
                message: format!(
                    "⚠️ **Server Error [{}]**\n\n{}\n\nIf this persists, check **Settings → LLM Configuration**.",
                    status.as_u16(),
                    body
                ),
            });
            return;
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut pending: Vec<PendingCall> = Vec::new();
        // Track whether any text streamed, so we can fall back to the final
        // response text if a model emits no incremental deltas (some reasoning
        // models go silent until completion).
        let mut text_emitted = false;

        // Flush all assembled function calls as ToolCall messages. Preserves the
        // OpenAI call_id as the ToolCall execution_id so the function_call_output
        // we send on the next turn correlates exactly.
        let flush =
            |pending: &mut Vec<PendingCall>, tx: &mpsc::UnboundedSender<StreamMessage>| {
                for pc in pending.drain(..) {
                    let args: Value =
                        serde_json::from_str(&pc.arguments).unwrap_or_else(|_| json!({}));
                    let mut call = ToolCall::new(pc.server_name, pc.tool_name, args, None, None);
                    if !pc.call_id.is_empty() {
                        call.execution_id = pc.call_id;
                    }
                    let _ = tx.send(StreamMessage::ToolCall(call));
                }
            };

        while let Some(item) = stream.next().await {
            let bytes = match item {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: format!("Stream error: {}", e),
                    });
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process complete lines; keep any partial trailing line buffered.
            while let Some(nl) = buffer.find('\n') {
                let line = buffer[..nl].trim().to_string();
                buffer.drain(..=nl);

                let Some(data) = line.strip_prefix("data:") else {
                    continue; // skip `event:` lines, comments, blanks
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };

                match classify_event(&value) {
                    RespEvent::TextDelta(text) => {
                        if !text.is_empty() {
                            text_emitted = true;
                            let _ = tx.send(StreamMessage::Text {
                                content: text,
                                thought_signature: None,
                                thought_summary: None,
                            });
                        }
                    }
                    RespEvent::ReasoningDelta(text) => {
                        if !text.is_empty() {
                            let _ = tx.send(StreamMessage::Text {
                                content: String::new(),
                                thought_signature: None,
                                thought_summary: Some(text),
                            });
                        }
                    }
                    RespEvent::FunctionCallAdded {
                        item_id,
                        call_id,
                        name,
                        initial_args,
                    } => {
                        let (server_name, tool_name) =
                            Self::resolve_tool_name(&name, &tool_name_map);
                        // output_item.added then output_item.done can both fire for
                        // the same item — only register once.
                        if !pending.iter().any(|p| p.item_id == item_id) {
                            pending.push(PendingCall {
                                item_id,
                                call_id,
                                server_name,
                                tool_name,
                                arguments: initial_args,
                            });
                        }
                    }
                    RespEvent::FunctionCallArgsDelta { item_id, delta } => {
                        if let Some(pc) = pending.iter_mut().find(|p| p.item_id == item_id) {
                            pc.arguments.push_str(&delta);
                        }
                    }
                    RespEvent::FunctionCallArgsDone { item_id, arguments } => {
                        // Authoritative full arguments — replace the accumulation.
                        if let Some(pc) = pending.iter_mut().find(|p| p.item_id == item_id) {
                            pc.arguments = arguments;
                        }
                    }
                    RespEvent::Completed { usage, final_text } => {
                        // Fallback: if nothing streamed incrementally, emit the
                        // final assistant text so the turn isn't silently empty.
                        if !text_emitted && !final_text.is_empty() {
                            tracing::warn!(
                                "OpenAI Responses: no text deltas streamed; emitting final response text ({} chars)",
                                final_text.len()
                            );
                            let _ = tx.send(StreamMessage::Text {
                                content: final_text,
                                thought_signature: None,
                                thought_summary: None,
                            });
                        }
                        if let Some(mut usage) = usage {
                            // Billed only for the real OpenAI API with a key.
                            usage.cost = crate::llm::openai_pricing::turn_cost(
                                &self.config.endpoint,
                                self.config.api_key.is_some(),
                                &self.config.model,
                                usage.prompt_tokens as i64,
                                usage.completion_tokens as i64,
                            );
                            let _ = tx.send(StreamMessage::Usage(usage));
                        }
                        flush(&mut pending, &tx);
                        return;
                    }
                    RespEvent::Failed(message) => {
                        let _ = tx.send(StreamMessage::Error { message });
                        return;
                    }
                    RespEvent::Ignored => {}
                }
            }
        }

        // Stream ended without an explicit completed event — flush what we have.
        flush(&mut pending, &tx);
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
            return Err(Box::new(std::io::Error::other(
                "No model configured for summarization.",
            )) as Box<dyn std::error::Error + Send + Sync>);
        }

        let system_prompt = r#"You are an AI assistant that refines a conversation summary.
You will be given a previous summary (which may be empty) and the most recent messages in a conversation.
Integrate the new information into the previous summary, preserving existing facts.

You MUST respond with valid JSON containing exactly these fields:
- "summary": A concise, updated summary of the entire conversation so far.
- "sentiment": A brief string describing the user's current mood.
- "current_task": A one-sentence description of the task the user is CURRENTLY working on (empty string if none).
- "entities": An object with "user_name", "project_name", "key_topics" (array), "key_decisions" (array), "active_profile", "blockers" (array)."#;

        let user_message = format!(
            "Previous Summary:\n---\n{}\n---\n\nRecent Messages:\n---\n{}\n---",
            previous_summary, recent_messages
        );

        let request_body = json!({
            "model": summary_model,
            "instructions": system_prompt,
            "input": user_message,
            "text": { "format": { "type": "json_object" } },
            "stream": false,
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let mut request_builder = client
            .post(self.responses_endpoint())
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&request_body).unwrap_or_default());
        if let Some(api_key) = &self.config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Box::new(std::io::Error::other(format!(
                "Summarization request failed with status {}: {}",
                status, body
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: Value = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let content = extract_output_text(&response_json);
        if !content.is_empty() {
            if let Ok(json_value) = serde_json::from_str::<Value>(&content) {
                return Ok(json_value);
            }
            if let (Some(start), Some(end)) = (content.find('{'), content.rfind('}')) {
                if let Ok(json_value) = serde_json::from_str::<Value>(&content[start..=end]) {
                    return Ok(json_value);
                }
            }
            tracing::warn!("OpenAI Responses: summary not valid JSON, using raw text.");
            return Ok(json!({ "summary": content, "entities": {}, "sentiment": "neutral" }));
        }

        tracing::error!("OpenAI Responses: summarization response had no output text.");
        Ok(Value::Null)
    }

    async fn select_tools_for_toolkit(
        &self,
        request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, String> {
        use crate::mcp::tool_selection::{build_selection_prompt, parse_selection_response};

        let model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.model)
            .clone();

        if model.is_empty() {
            return Err("No model configured for tool selection.".to_string());
        }

        tracing::info!(
            model = %model,
            toolkit = %request.toolkit_name,
            tool_count = %request.available_tools.len(),
            "LLM: Selecting tools for toolkit (OpenAI Responses)"
        );

        let request_body = json!({
            "model": model,
            "input": build_selection_prompt(request),
            "text": { "format": { "type": "json_object" } },
            "stream": false,
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;

        let mut request_builder = client
            .post(self.responses_endpoint())
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&request_body).unwrap_or_default());
        if let Some(api_key) = &self.config.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request_builder.send().await.map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Tool selection request failed with status {}: {}",
                status, body
            ));
        }

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;
        let content = extract_output_text(&response_json);
        if content.is_empty() {
            return Err("No response from LLM for tool selection".to_string());
        }

        parse_selection_response(&content)
    }
}

/// Concatenate the text of all `output_text` content parts in a non-streaming
/// Responses payload (`output[].content[]`).
fn extract_output_text(response: &Value) -> String {
    let mut out = String::new();
    if let Some(items) = response.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for part in parts {
                        if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                out.push_str(text);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ChatMessage, ToolDefinition};

    fn cfg(endpoint: &str, model: &str, style: ApiStyle) -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            api_style: style,
            tools_enabled: true,
            ..Default::default()
        }
    }

    // ── Routing ──────────────────────────────────────────────────────────────

    #[test]
    fn auto_routes_gpt5_and_o_series_on_openai_to_responses() {
        let c = cfg("https://api.openai.com/v1", "gpt-5-pro", ApiStyle::Auto);
        assert!(should_use_responses(&c, "gpt-5-pro"));
        assert!(should_use_responses(&cfg("https://api.openai.com/v1", "o3-pro", ApiStyle::Auto), "o3-pro"));
    }

    #[test]
    fn auto_keeps_older_openai_models_on_chat_completions() {
        let c = cfg("https://api.openai.com/v1", "gpt-4o", ApiStyle::Auto);
        assert!(!should_use_responses(&c, "gpt-4o"));
    }

    #[test]
    fn auto_never_routes_non_openai_endpoints_to_responses() {
        // A local server advertising a gpt-5-ish name must NOT be sent to Responses.
        let c = cfg("http://localhost:11434/v1", "gpt-5-clone", ApiStyle::Auto);
        assert!(!should_use_responses(&c, "gpt-5-clone"));
    }

    #[test]
    fn explicit_overrides_win_regardless_of_endpoint() {
        let forced_on = cfg("http://localhost:1234/v1", "llama3", ApiStyle::Responses);
        assert!(should_use_responses(&forced_on, "llama3"));
        let forced_off = cfg("https://api.openai.com/v1", "gpt-5-pro", ApiStyle::ChatCompletions);
        assert!(!should_use_responses(&forced_off, "gpt-5-pro"));
    }

    #[test]
    fn family_detection_matches_expected_prefixes() {
        for m in ["gpt-5", "gpt-5-mini", "o1", "o1-pro", "o3", "o3-mini", "o4-mini"] {
            assert!(is_responses_family(m), "{m} should be in the Responses family");
        }
        for m in ["gpt-4o", "gpt-4.1", "chatgpt-4o-latest", "llama3"] {
            assert!(!is_responses_family(m), "{m} should NOT be in the family");
        }
    }

    // ── Request building ───────────────────────────────────────────────────────

    fn connector() -> OpenAiResponsesConnector {
        OpenAiResponsesConnector::new(cfg("https://api.openai.com/v1", "gpt-5", ApiStyle::Auto))
    }

    #[test]
    fn system_prompt_maps_to_instructions_not_a_message() {
        let prompt = LlmPrompt {
            system: Some("be terse".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: "hi".to_string() }],
            }],
            tools: vec![],
        };
        let req = connector().to_responses_request(&prompt, true);
        assert_eq!(req["instructions"], json!("be terse"));
        let input = req["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"], json!("hi"));
        // No system message item should be present.
        assert!(input.iter().all(|i| i["role"] != json!("system")));
    }

    #[test]
    fn tools_are_flattened_no_nested_function_object() {
        let prompt = LlmPrompt {
            system: None,
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "send".to_string(),
                server_name: "gmail".to_string(),
                description: "send mail".to_string(),
                parameters: json!({ "type": "object" }),
            }],
        };
        let req = connector().to_responses_request(&prompt, true);
        let tool = &req["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert!(tool["name"].is_string(), "name is top-level, not nested");
        assert!(tool.get("function").is_none(), "must not nest under `function`");
        assert_eq!(tool["parameters"], json!({ "type": "object" }));
    }

    #[test]
    fn tool_call_and_result_become_correlated_items() {
        let prompt = LlmPrompt {
            system: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ContentBlock::ToolCall {
                        id: "call_42".to_string(),
                        name: "gmail_send".to_string(),
                        arguments: json!({ "to": "a@b.c" }),
                        signature: None,
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: "call_42".to_string(),
                        name: "gmail_send".to_string(),
                        content: json!("sent"),
                    }],
                },
            ],
            tools: vec![],
        };
        let req = connector().to_responses_request(&prompt, true);
        let input = req["input"].as_array().unwrap();
        // function_call item
        let fc = input.iter().find(|i| i["type"] == json!("function_call")).unwrap();
        assert_eq!(fc["call_id"], json!("call_42"));
        assert_eq!(fc["name"], json!("gmail_send"));
        // Arguments serialized as a JSON string.
        assert_eq!(fc["arguments"], json!(r#"{"to":"a@b.c"}"#));
        // function_call_output correlated by the same call_id
        let out = input.iter().find(|i| i["type"] == json!("function_call_output")).unwrap();
        assert_eq!(out["call_id"], json!("call_42"));
        assert_eq!(out["output"], json!("sent"));
    }

    #[test]
    fn reasoning_requested_only_when_thinking_enabled() {
        let prompt = LlmPrompt { system: None, messages: vec![], tools: vec![] };
        let off = connector().to_responses_request(&prompt, true);
        assert!(off.get("reasoning").is_none());

        let mut c = cfg("https://api.openai.com/v1", "gpt-5", ApiStyle::Auto);
        c.thinking_enabled = true;
        let on = OpenAiResponsesConnector::new(c).to_responses_request(&prompt, true);
        assert_eq!(on["reasoning"], json!({ "summary": "auto" }));
    }

    // ── Stream event classification ──────────────────────────────────────────

    #[test]
    fn classify_text_and_reasoning_deltas() {
        assert_eq!(
            classify_event(&json!({ "type": "response.output_text.delta", "delta": "hello" })),
            RespEvent::TextDelta("hello".to_string())
        );
        assert_eq!(
            classify_event(&json!({ "type": "response.reasoning_summary_text.delta", "delta": "thinking" })),
            RespEvent::ReasoningDelta("thinking".to_string())
        );
    }

    #[test]
    fn classify_function_call_lifecycle() {
        let added = classify_event(&json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "gmail_send", "arguments": "" }
        }));
        assert_eq!(
            added,
            RespEvent::FunctionCallAdded {
                item_id: "fc_1".to_string(),
                call_id: "call_9".to_string(),
                name: "gmail_send".to_string(),
                initial_args: "".to_string(),
            }
        );
        assert_eq!(
            classify_event(&json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"to\":" })),
            RespEvent::FunctionCallArgsDelta { item_id: "fc_1".to_string(), delta: "{\"to\":".to_string() }
        );
        assert_eq!(
            classify_event(&json!({ "type": "response.function_call_arguments.done", "item_id": "fc_1", "arguments": "{\"to\":\"x\"}" })),
            RespEvent::FunctionCallArgsDone { item_id: "fc_1".to_string(), arguments: "{\"to\":\"x\"}".to_string() }
        );
    }

    #[test]
    fn classify_completed_parses_usage() {
        let ev = classify_event(&json!({
            "type": "response.completed",
            "response": { "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15 } }
        }));
        match ev {
            RespEvent::Completed { usage: Some(u), .. } => {
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 5);
                assert_eq!(u.total_tokens, 15);
            }
            other => panic!("expected Completed with usage, got {:?}", other),
        }
    }

    #[test]
    fn completed_carries_final_text_for_fallback() {
        // A model that streams no deltas still yields its text via the final
        // response object, which the connector emits as a fallback.
        let ev = classify_event(&json!({
            "type": "response.completed",
            "response": {
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 },
                "output": [
                    { "type": "message", "content": [ { "type": "output_text", "text": "hi there" } ] }
                ]
            }
        }));
        match ev {
            RespEvent::Completed { final_text, .. } => assert_eq!(final_text, "hi there"),
            other => panic!("expected Completed, got {:?}", other),
        }
    }

    #[test]
    fn classify_errors_and_ignored() {
        assert!(matches!(
            classify_event(&json!({ "type": "error", "message": "boom" })),
            RespEvent::Failed(m) if m == "boom"
        ));
        assert_eq!(
            classify_event(&json!({ "type": "response.created" })),
            RespEvent::Ignored
        );
    }

    #[test]
    fn extract_output_text_concatenates_message_parts() {
        let resp = json!({
            "output": [
                { "type": "reasoning", "content": [] },
                { "type": "message", "content": [
                    { "type": "output_text", "text": "{\"summary\":" },
                    { "type": "output_text", "text": "\"done\"}" }
                ] }
            ]
        });
        assert_eq!(extract_output_text(&resp), r#"{"summary":"done"}"#);
    }
}
