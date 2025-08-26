use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::env;
use futures_util::StreamExt;
use tokio::sync::mpsc;

const API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro-preview-06-05:streamGenerateContent";

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
struct GeminiResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize, Debug)]
struct Candidate {
    content: ContentResponse,
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

    let res = client
        .post(&url)
        .json(&request_body)
        .send()
        .await;

    let mut stream = match res {
        Ok(r) => r.bytes_stream(),
        Err(e) => {
            eprintln!("Error sending request: {}", e);
            return;
        }
    };

    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                let chunk = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                for line in chunk.lines() {
                    if line.starts_with("data: ") {
                        let json_str = &line[6..];
                        if let Ok(parsed) = serde_json::from_str::<GeminiResponse>(json_str) {
                            if let Some(candidate) = parsed.candidates.get(0) {
                                if let Some(part) = candidate.content.parts.get(0) {
                                    if tx.send(part.text.clone()).is_err() {
                                        eprintln!("Failed to send content chunk to UI.");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error in stream: {}", e);
                break;
            }
        }
    }
}