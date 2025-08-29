use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::env;
use futures_util::StreamExt;
use tokio::sync::mpsc;

const API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:streamGenerateContent";
const FLASH_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:generateContent";

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<Content>,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Serialize)]
struct Part {
    text: String,
}

#[derive(Deserialize, Debug)]
struct GeminiErrorResponse {
    error: GeminiError,
}

#[derive(Deserialize, Debug)]
struct GeminiError {
    message: String,
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
    parts: Vec<PartResponse>,
}

#[derive(Deserialize, Debug)]
struct PartResponse {
    text: String,
}

pub async fn generate_content_stream(prompt: String, tx: mpsc::UnboundedSender<String>) {
    let api_key = env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set");
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("Failed to build reqwest client");

    let request_body = GeminiRequest {
        contents: vec![Content {
            parts: vec![Part { text: prompt }],
        }],
    };

    let url = format!("{}?key={}&alt=sse", API_URL, api_key);

    let response = match client.post(&url).json(&request_body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Error sending request: {}", e);
            return;
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

    // With the `:streamGenerateContent` endpoint, we receive Server-Sent Events (SSE).
    // This is a much simpler and more robust way to handle streaming compared to
    // manually buffering bytes and searching for JSON objects. The previous implementation
    // was unnecessarily complex because it was trying to stream from a non-streaming endpoint.
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                // The SSE format sends data in chunks, often line-by-line.
                // We process each line that starts with "data: ".
                for line in std::str::from_utf8(&bytes).unwrap_or("").lines() {
                    if line.starts_with("data: ") {
                        let json_str = &line["data: ".len()..];
                        match serde_json::from_str::<GeminiResponse>(json_str) {
                            Ok(parsed) => {
                                if let Some(candidate) = parsed.candidates.get(0) {
                                    // Capture the finish reason if the API provides it.
                                    if let Some(reason) = &candidate.finish_reason {
                                        finish_reason = Some(reason.clone());
                                        if reason != "STOP" {
                                            tracing::warn!("Gemini stream finished with reason: {}", reason);
                                        }
                                    }
                                    // Extract the text part and send it to the UI.
                                    if let Some(part) = candidate.content.parts.get(0) {
                                        if !part.text.is_empty() {
                                            if tx.send(part.text.clone()).is_err() {
                                                tracing::error!("Failed to send content chunk to UI.");
                                                return; // Exit if the UI receiver is gone.
                                            }
                                            has_sent_data = true;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                // If we receive malformed JSON, it indicates a problem with the stream.
                                // We log the detailed error for debugging and send a user-friendly
                                // message to the UI, then terminate the stream.
                                let error_message = "[Hobbes encountered a stream error. Please check the logs for details.]";
                                tracing::error!("Failed to parse JSON chunk from stream: {}. Chunk: '{}'", e, json_str);
                                if tx.send(error_message.to_string()).is_err() {
                                    tracing::error!("Failed to send stream error message to UI.");
                                }
                                return; // Stop processing the stream.
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error in stream: {}", e);
                break;
            }
        }
    }

    // After the stream, if no actual content was sent, we send a default message
    // based on the finish reason we captured.
    if !has_sent_data {
        let default_message = match finish_reason.as_deref() {
            Some("SAFETY") => "[Hobbes did not provide a response due to the safety filter.]".to_string(),
            Some(reason) => format!("[Hobbes did not provide a response. Finish Reason: {}]", reason),
            None => "[Hobbes did not provide a response due to an internal error.]".to_string(),
        };
        if tx.send(default_message).is_err() {
            tracing::error!("Failed to send default message to UI.");
        }
    }
}

pub async fn summarize_conversation(
    previous_summary: String,
    recent_messages: String,
) -> Result<serde_json::Value, reqwest::Error> {
    let api_key = env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set");
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
- "entities": An object containing all key-value pairs of extracted information from the whole conversation.
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
            parts: vec![Part { text: full_prompt }],
        }],
    };

    let url = format!("{}?key={}", FLASH_API_URL, api_key);

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