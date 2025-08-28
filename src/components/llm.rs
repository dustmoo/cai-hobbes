use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::env;
use futures_util::StreamExt;
use tokio::sync::mpsc;

const API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent";
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
    let client = Client::new();

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

    let mut buffer = String::new();
    let mut has_sent_data = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                let chunk = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                for line in chunk.lines() {
                    if line.starts_with("data: ") {
                        buffer.push_str(&line[6..]);
                        if let Ok(parsed) = serde_json::from_str::<GeminiResponse>(&buffer) {
                            if let Some(candidate) = parsed.candidates.get(0) {
                                if let Some(reason) = &candidate.finish_reason {
                                    if reason != "STOP" {
                                        tracing::warn!("Gemini stream finished with reason: {}", reason);
                                    }
                                }
                                if let Some(part) = candidate.content.parts.get(0) {
                                    if !part.text.is_empty() {
                                        if tx.send(part.text.clone()).is_err() {
                                            tracing::error!("Failed to send content chunk to UI.");
                                            return;
                                        }
                                        has_sent_data = true;
                                    }
                                }
                            }
                            buffer.clear();
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

    if !has_sent_data {
        let default_message = "[Hobbes did not provide a response. This could be due to a safety filter or an internal error.]".to_string();
        if tx.send(default_message).is_err() {
            tracing::error!("Failed to send default message to UI.");
        }
    }
}

pub async fn summarize_conversation(recent_messages: String) -> Result<serde_json::Value, reqwest::Error> {
    let api_key = env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set");
    let client = Client::new();

    let system_prompt = r#"
You are an AI assistant that processes a conversation and extracts key entities and a brief summary.
Analyze the following conversation snippet. Identify key facts, entities, or user-stated preferences.
Format your response as a single, clean JSON object with two keys: "summary" and "entities".
- "summary": A concise, one-sentence summary of the conversation.
- "entities": An object containing key-value pairs of extracted information.
For example, if a user mentions a password, you could extract: {"password": "the_password"}.
If no specific entities are found, return an empty "entities" object.

Conversation:
---
"#;

    let full_prompt = format!("{}{}", system_prompt, recent_messages);

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