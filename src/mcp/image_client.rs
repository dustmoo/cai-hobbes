use rmcp::model::{CallToolResult, Content, Tool};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Native virtual MCP client for image generation via the Gemini API.
/// Fully stateless — both API key and model are passed at call time
/// from global signals (Pattern 30 compliance).
#[derive(Clone)]
pub struct ImageClient;

impl ImageClient {
    pub fn new() -> Self {
        Self
    }

    pub fn list_tools(&self) -> Vec<Tool> {
        use std::sync::Arc;
        
        let schema_val = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The detailed description of the image to generate or how to modify the reference image"
                },
                "reference_image": {
                    "type": "string",
                    "description": "Optional file:// path to a previously generated image to use as a reference for editing/riffing. When provided, the prompt describes how to modify this image."
                }
            },
            "required": ["prompt"]
        });
        
        vec![Tool {
            name: "generate_image".into(),
            description: Some("Generate or edit an image. Use with just a prompt for new images, or include a reference_image path to modify a previously generated image.".into()),
            input_schema: Arc::new(serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(schema_val).unwrap()),
            title: Some("generate_image".to_string()),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }]
    }

    /// Execute the generate_image tool via the Gemini REST API.
    /// Both `model` and `api_key` are passed at call time from global signals.
    pub async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        model: &str,
        api_key: Option<&str>,
    ) -> Result<CallToolResult, String> {
        if name != "generate_image" {
            return Err(format!("Unknown tool: {}", name));
        }

        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or_default();
        if prompt.is_empty() {
            return Err("Missing 'prompt' argument for image generation".to_string());
        }

        if model.is_empty() {
            return Err("No image generation model selected. Please choose one in Settings -> Model -> Image Generation Model.".to_string());
        }

        let api_key = api_key
            .filter(|k| !k.is_empty())
            .ok_or_else(|| "Image Generation requires the Gemini API Key to be configured in Settings -> Credentials.".to_string())?;

        tracing::info!("Generating image via Gemini model '{}' for prompt: {}", model, prompt);

        // The model name from the API comes as "models/gemini-2.5-flash-image"
        // The REST URL expects: /v1beta/models/{slug}:generateContent
        // Handle both with and without the "models/" prefix.
        let model_path = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{}", model)
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{}:generateContent",
            model_path
        );

        // Build the parts array: text prompt + optional reference image
        let mut parts = vec![serde_json::json!({ "text": prompt })];

        // If a reference image path is provided, read + base64-encode it
        if let Some(ref_path) = args.get("reference_image").and_then(|v| v.as_str()) {
            if !ref_path.is_empty() {
                // Security: validate the resolved path is inside a known safe directory.
                // This prevents a malicious model from exfiltrating arbitrary files
                // (e.g. ~/.ssh/id_rsa) by injecting them as "reference_image" paths.
                if let Some(safe_path) = crate::security::validate_safe_file_path(ref_path) {
                    match std::fs::read(&safe_path) {
                        Ok(bytes) => {
                            use base64::Engine;
                            let mime = crate::security::mime_from_extension(
                                safe_path.to_str().unwrap_or("")
                            );
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            parts.push(serde_json::json!({
                                "inlineData": { "mimeType": mime, "data": b64 }
                            }));
                            tracing::info!("Attached reference image from {} ({} bytes)", safe_path.display(), bytes.len());
                        }
                        Err(e) => {
                            tracing::warn!("Could not read reference image at {}: {}", safe_path.display(), e);
                            // Continue without the reference — fall back to text-only generation
                        }
                    }
                } else {
                    tracing::warn!(
                        "ImageClient: rejecting reference_image outside safe directories: {}",
                        ref_path
                    );
                    // Fall through to text-only generation
                }
            }
        }

        let body = serde_json::json!({
            "contents": [{
                "parts": parts
            }],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"]
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("x-goog-api-key", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Image generation network error: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(format!("Gemini image API error ({}): {}", status, error_text));
        }

        let resp_json: Value = response.json().await.map_err(|e| format!("Failed to parse image response: {}", e))?;

        // Extract parts from the response
        let parts = resp_json
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .ok_or_else(|| "Unexpected response format from Gemini image API".to_string())?;

        let mut result_contents: Vec<Content> = Vec::new();

        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                result_contents.push(Content::text(text.to_string()));
            } else if let Some(inline_data) = part.get("inlineData") {
                let mime_type = inline_data.get("mimeType").and_then(|m| m.as_str()).unwrap_or("image/png");
                let data_b64 = inline_data.get("data").and_then(|d| d.as_str()).unwrap_or_default();

                let extension = match mime_type {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    _ => "png",
                };

                let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                let file_name = format!("hobbes_gen_{}.{}", timestamp, extension);
                // Save to persistent app directory (not temp) so images survive reboots
                let mut path = dirs::config_dir()
                    .unwrap_or_else(|| std::env::temp_dir())
                    .join("com.hobbes.app")
                    .join("generated_images");
                if let Err(e) = std::fs::create_dir_all(&path) {
                    tracing::warn!("Could not create generated_images dir: {}", e);
                }
                path.push(&file_name);

                match base64_decode_and_write(data_b64, &path).await {
                    Ok(_) => {
                        // URL-encode the path (spaces in 'Application Support' break markdown URL parsing)
                        let encoded_path = path.to_string_lossy().replace(' ', "%20");
                        let markdown = format!("![Generated Image](file://{})", encoded_path);
                        result_contents.push(Content::text(markdown));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to decode/save generated image: {}", e);
                        result_contents.push(Content::text(format!("Image generated but failed to save: {}", e)));
                    }
                }
            }
        }

        if result_contents.is_empty() {
            return Err("Gemini API returned no image or text content".to_string());
        }

        Ok(CallToolResult {
            content: result_contents,
            is_error: Some(false),
            structured_content: None,
            meta: None,
        })
    }
}

/// Decode a base64 string and write the raw bytes to a file.
async fn base64_decode_and_write(b64: &str, path: &std::path::Path) -> Result<(), String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("Base64 decode error: {}", e))?;
    tokio::fs::write(path, &bytes)
        .await
        .map_err(|e| format!("File write error: {}", e))
}
