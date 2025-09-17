use serde::{Deserialize, Serialize};
use reqwest::Client;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::components::shared::ToolCall;
use crate::components::shared::StreamMessage;
const BASE_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

use crate::session::Tool;

#[derive(Serialize, Deserialize)]
pub(crate) struct GeminiRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction>,
}

#[derive(Serialize, Deserialize)]
#[derive(Debug)]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Serialize, Deserialize)]
#[derive(Debug)]
pub struct Part {
    pub text: String,
}

#[derive(Deserialize, Debug)]
struct GeminiErrorResponse {
    error: GeminiError,
}

#[derive(Deserialize, Debug)]
struct GeminiError {
    message: String,
}

#[derive(Serialize, Deserialize)]
#[derive(Debug)]
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
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PartResponse {
    #[serde(default)]
    text: String,
    function_call: Option<FunctionCall>,
}

use crate::context::prompt_builder::LlmPrompt;

pub async fn generate_content_stream(
    api_key: String,
    model: String,
    prompt_data: LlmPrompt,
    tx: mpsc::UnboundedSender<StreamMessage>,
    mcp_context: Option<crate::mcp::manager::McpContext>,
) {
    const MAX_RETRIES: u32 = 2;
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("Failed to build reqwest client");

    let request_body = GeminiRequest {
        contents: prompt_data.contents,
        tools: prompt_data.tools,
        system_instruction: prompt_data.system_instruction,
    };
    tracing::info!("Using chat model: {}", model);
    let url = format!("{}/{}:streamGenerateContent?key={}&alt=sse", BASE_API_URL, model, api_key);

    for attempt in 0..MAX_RETRIES {
        let response = match client.post(&url).json(&request_body).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Error sending request on attempt {}: {}", attempt + 1, e);
                if attempt + 1 == MAX_RETRIES { return; }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
            if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
                tracing::error!("Gemini API Error [{}]: {}", status, error_response.error.message);
            } else {
                tracing::error!("Gemini API Error [{}]: {}", status, body_text);
            }
            return;
        }

        let mut stream = response.bytes_stream();
        let mut has_sent_data = false;
        let mut finish_reason: Option<String> = None;
        let mut buffer = Vec::<u8>::new();
        let mut malformed_call_detected = false;

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
                                            if reason != "STOP" {
                                                tracing::warn!("Gemini stream finished with reason: {}", reason);
                                            }
                                        }
                                        if let Some(part) = candidate.content.parts.get(0) {
                                            if let Some(function_call) = &part.function_call {
                                                let mut found_tool = false;
                                                if let Some(context) = &mcp_context {
                                                    for server in &context.servers {
                                                        if server.tools.iter().any(|t| t.name == function_call.name) {
                                                            let tool_call = ToolCall::new(
                                                                server.name.clone(),
                                                                function_call.name.clone(),
                                                                function_call.args.clone(),
                                                            );
                                                            if tx.send(StreamMessage::ToolCall(tool_call)).is_err() {
                                                                return;
                                                            }
                                                            has_sent_data = true;
                                                            found_tool = true;
                                                            break;
                                                        }
                                                    }
                                                }
                                                if !found_tool {
                                                    tracing::error!("LLM requested tool '{}' which was not found in the provided context.", function_call.name);
                                                }
                                            } else if !part.text.is_empty() {
                                                if tx.send(StreamMessage::Text(part.text.clone())).is_err() {
                                                    return;
                                                }
                                                has_sent_data = true;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to parse JSON chunk from stream: {}. Chunk: '{}'", e, json_str);
                                    // Check if this error is due to a malformed call finish reason
                                    if json_str.contains("MALFORMED_FUNCTION_CALL") {
                                        tracing::warn!("Malformed function call detected via string search on attempt {}. Retrying...", attempt + 1);
                                        malformed_call_detected = true;
                                        break; // Break from inner while to retry
                                    }
                                    let error_message = "[Hobbes encountered a stream error. Please check the logs for details.]";
                                    if tx.send(StreamMessage::Text(error_message.to_string())).is_err() {
                                        tracing::error!("Failed to send stream error message to UI.");
                                    }
                                    return;
                                }
                            }
                        }
                    }
                    if malformed_call_detected { break; }
                }
                Err(e) => {
                    tracing::error!("Error in stream: {}", e);
                    break;
                }
            }
        }

        if malformed_call_detected {
            if attempt + 1 < MAX_RETRIES {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue; // Go to the next iteration of the for loop
            } else {
                tracing::error!("Malformed function call persisted after {} retries. Aborting.", MAX_RETRIES);
                let _ = tx.send(StreamMessage::Text("[Hobbes failed to process a tool call after multiple retries.]".to_string()));
                return;
            }
        }

        if !has_sent_data {
            let default_message = match finish_reason.as_deref() {
                Some("SAFETY") => "[Hobbes did not provide a response due to the safety filter.]".to_string(),
                Some(reason) => format!("[Hobbes did not provide a response. Finish Reason: {}]", reason),
                None => "[Hobbes did not provide a response due to an internal error.]".to_string(),
            };
            if tx.send(StreamMessage::Text(default_message)).is_err() {
                tracing::error!("Failed to send default message to UI.");
            }
        }
        // If we've successfully processed the stream without a malformed call, break the retry loop.
        break;
    }
}

pub async fn summarize_conversation(
    api_key: String,
    model: String,
    previous_summary: String,
    recent_messages: String,
) -> Result<serde_json::Value, reqwest::Error> {
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
            parts: vec![Part { text: full_prompt }],
        }],
        tools: None,
        system_instruction: None,
    };

    tracing::info!("Using summary model: {}", model);
    let url = format!("{}/{}:generateContent?key={}", BASE_API_URL, model, api_key);

    let response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        if let Ok(error_response) = serde_json::from_str::<GeminiErrorResponse>(&body_text) {
            tracing::error!("Gemini API Error [{}]: {}", status, error_response.error.message);
        } else {
            tracing::error!("Gemini API Error [{}]: {}", status, body_text);
        }
        // Return a structured error instead of panicking or returning a generic reqwest::Error
        return Ok(serde_json::json!({
            "error": "API request failed",
            "status": status.as_u16(),
            "body": body_text
        }));
    }

    let response_json: GeminiResponse = response.json().await?;

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