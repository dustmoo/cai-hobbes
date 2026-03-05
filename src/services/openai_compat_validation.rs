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
#[allow(dead_code)] // Architectural — used by vLLM context-length-aware model discovery
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
#[allow(dead_code)] // Architectural — context-length-aware model discovery for vLLM
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
