use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

// ============================================================================
// RESPONSE TYPES
// ============================================================================

/// OpenAI-style model entry
#[derive(Debug, Deserialize)]
struct OaiModelEntry {
    id: String,
    /// vLLM includes this in model info
    #[serde(default)]
    #[allow(dead_code)]
    max_model_len: Option<usize>,
}

/// Discovered model with optional context length
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub id: String,
    pub context_length: Option<usize>,
}

/// OpenAI-style models response
#[derive(Debug, Deserialize)]
struct OaiModelsResponse {
    data: Vec<OaiModelEntry>,
}

/// Ollama-style model entry (from /api/tags)
#[derive(Debug, Deserialize)]
struct OllamaModelEntry {
    name: String,
}

/// Ollama-style tags response
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModelEntry>,
}

// ============================================================================
// PUBLIC API
// ============================================================================

/// Fetch available models from an OpenAI-compatible endpoint.
///
/// Tries the following in order:
/// 1. `{endpoint}/v1/models` (standard OpenAI format)
/// 2. `{endpoint}/api/tags`  (Ollama native format)
///
/// Returns a sorted list of model IDs on success.
pub async fn fetch_openai_compat_models(
    endpoint: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    if endpoint.trim().is_empty() {
        return Err("Endpoint URL cannot be empty".to_string());
    }

    let base = endpoint.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // Try OpenAI-format first
    let oai_url = if base.ends_with("/v1") {
        format!("{}/models", base)
    } else {
        format!("{}/v1/models", base)
    };

    let mut request = client.get(&oai_url);
    if let Some(key) = api_key {
        if !key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", key));
        }
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(models) = response.json::<OaiModelsResponse>().await {
                let mut ids: Vec<String> = models.data.into_iter().map(|m| m.id).collect();
                if !ids.is_empty() {
                    ids.sort();
                    return Ok(ids);
                }
            }
            // Fall through to Ollama if parsing failed
        }
        Ok(response) if response.status().as_u16() == 401 || response.status().as_u16() == 403 => {
            return Err("Authentication failed — check your API key".to_string());
        }
        _ => {
            // Fall through to Ollama
        }
    }

    // Try Ollama /api/tags
    let ollama_url = format!("{}/api/tags", base);
    match client.get(&ollama_url).send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(tags) = response.json::<OllamaTagsResponse>().await {
                let mut ids: Vec<String> = tags.models.into_iter().map(|m| m.name).collect();
                if !ids.is_empty() {
                    ids.sort();
                    return Ok(ids);
                }
            }
        }
        _ => {}
    }

    Err(format!("Cannot reach {} — is the server running?", base))
}

/// Fetch models with context length information.
/// Returns DiscoveredModel structs that include max_model_len when available.
pub async fn fetch_openai_compat_models_with_context(
    endpoint: &str,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>, String> {
    if endpoint.trim().is_empty() {
        return Err("Endpoint URL cannot be empty".to_string());
    }

    let base = endpoint.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // Try OpenAI-format first (vLLM includes max_model_len)
    let oai_url = if base.ends_with("/v1") {
        format!("{}/models", base)
    } else {
        format!("{}/v1/models", base)
    };

    let mut request = client.get(&oai_url);
    if let Some(key) = api_key {
        if !key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", key));
        }
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(models) = response.json::<OaiModelsResponse>().await {
                let mut discovered: Vec<DiscoveredModel> = models
                    .data
                    .into_iter()
                    .map(|m| DiscoveredModel {
                        id: m.id,
                        context_length: m.max_model_len,
                    })
                    .collect();
                if !discovered.is_empty() {
                    discovered.sort_by(|a, b| a.id.cmp(&b.id));
                    return Ok(discovered);
                }
            }
        }
        _ => {}
    }

    // Fallback to fetch_openai_compat_models (no context info)
    let ids = fetch_openai_compat_models(endpoint, api_key).await?;
    Ok(ids
        .into_iter()
        .map(|id| DiscoveredModel {
            id,
            context_length: None,
        })
        .collect())
}

/// Validate an OpenAI-compatible endpoint by checking reachability.
/// Alias for fetch_openai_compat_models — validation IS model discovery.
pub async fn validate_openai_compat_endpoint(
    endpoint: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    fetch_openai_compat_models(endpoint, api_key).await
}

// ============================================================================
// TOKENIZER CALIBRATION
// ============================================================================

/// Response from the vLLM /tokenize endpoint
#[derive(Debug, Deserialize)]
struct TokenizeResponse {
    count: usize,
    #[allow(dead_code)]
    max_model_len: Option<usize>,
}

/// Calibration probe: a mix of English prose and JSON tool schema, representative
/// of real prompts. ~500 chars gives a stable measurement without wasting time.
const CALIBRATION_PROBE: &str = r#"You are a helpful assistant. Answer the user's questions thoughtfully and accurately.

The user has access to the following tools for file system operations:
{"type": "function", "function": {"name": "read_file", "description": "Read the contents of a file from the filesystem. Returns the file content as a string.", "parameters": {"type": "object", "properties": {"path": {"type": "string", "description": "Absolute path to the file to read"}, "encoding": {"type": "string", "description": "Character encoding (default: utf-8)"}}, "required": ["path"]}}}

Please help the user with their request. When using tools, provide the exact arguments."#;

/// Probe the server's tokenizer to measure the actual chars-per-token ratio
/// for the given model. This enables accurate context budget estimation without
/// relying on a hardcoded heuristic.
///
/// Returns `Some(ratio)` on success, `None` if the endpoint doesn't exist or fails.
/// The ratio is clamped to [1.5, 6.0] to prevent pathological values.
///
/// Works with vLLM's `/tokenize` endpoint. Servers that don't expose it
/// (OpenAI, Ollama) will return None and fall back to the default ratio.
pub async fn calibrate_tokenizer(
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
) -> Option<f64> {
    if endpoint.trim().is_empty() || model.trim().is_empty() {
        return None;
    }

    let base = endpoint.trim_end_matches('/');
    // vLLM exposes /tokenize at the base URL (NOT under /v1/)
    let tokenize_url = if base.ends_with("/v1") {
        format!("{}/tokenize", base.trim_end_matches("/v1"))
    } else {
        format!("{}/tokenize", base)
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let mut request = client
        .post(&tokenize_url)
        .json(&serde_json::json!({
            "model": model,
            "prompt": CALIBRATION_PROBE,
        }));

    if let Some(key) = api_key {
        if !key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", key));
        }
    }

    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        tracing::debug!(
            "Tokenizer calibration: /tokenize returned {} — endpoint may not support it",
            response.status()
        );
        return None;
    }

    let result: TokenizeResponse = response.json().await.ok()?;
    if result.count == 0 {
        return None;
    }

    let probe_chars = CALIBRATION_PROBE.chars().count() as f64;
    let ratio = probe_chars / result.count as f64;

    // Clamp to sane range: 1.5 (very dense tokenizer) to 6.0 (very sparse)
    let clamped = ratio.clamp(1.5, 6.0);

    tracing::info!(
        "Tokenizer calibration: {} chars / {} tokens = {:.2} chars/token (model: {})",
        probe_chars as usize,
        result.count,
        clamped,
        model
    );

    Some(clamped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_validate_empty_endpoint() {
        let result = validate_openai_compat_endpoint("", None).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Endpoint URL cannot be empty");
    }

    #[tokio::test]
    async fn test_validate_unreachable_endpoint() {
        let result = validate_openai_compat_endpoint("http://127.0.0.1:19999", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Cannot reach"));
    }
}
