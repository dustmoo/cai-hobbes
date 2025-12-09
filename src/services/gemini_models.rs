use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODELS_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModel {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    pub supported_generation_methods: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsListResponse {
    models: Vec<GeminiModel>,
    #[serde(default)]
    next_page_token: Option<String>,
}

struct ModelsCache {
    models: Vec<GeminiModel>,
    fetched_at: Instant,
}

lazy_static::lazy_static! {
    static ref MODELS_CACHE: Arc<Mutex<Option<ModelsCache>>> = Arc::new(Mutex::new(None));
}

#[derive(Debug, thiserror::Error)]
pub enum ModelFetchError {
    #[error("HTTP request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("No API key provided")]
    NoApiKey,
    #[error("Failed to parse response: {0}")]
    ParseError(String),
}

/// Fetch available Gemini models from the API
/// Returns cached results if available and not expired
pub async fn fetch_gemini_models(api_key: Option<&str>) -> Result<Vec<GeminiModel>, ModelFetchError> {
    // Check if we have a valid cached result
    {
        let cache = MODELS_CACHE.lock().unwrap();
        if let Some(cached) = cache.as_ref() {
            if cached.fetched_at.elapsed() < CACHE_TTL {
                tracing::debug!("Returning cached models list");
                return Ok(cached.models.clone());
            }
        }
    }

    // Require API key for fetching
    let api_key = api_key.ok_or(ModelFetchError::NoApiKey)?;

    tracing::info!("Fetching Gemini models from API");
    
    let client = Client::new();
    let mut all_models = Vec::new();
    let mut page_token: Option<String> = None;

    // Fetch all pages of models
    loop {
        let mut url = format!("{}?key={}", MODELS_API_URL, api_key);
        if let Some(token) = &page_token {
            url.push_str(&format!("&pageToken={}", token));
        }

        let response = client.get(&url)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(ModelFetchError::ParseError(
                format!("API returned status {}: {}", status, error_text)
            ));
        }

        let list_response: ModelsListResponse = response.json().await?;
        all_models.extend(list_response.models);

        if list_response.next_page_token.is_none() {
            break;
        }
        page_token = list_response.next_page_token;
    }

    // Filter to only models that support generateContent
    let filtered_models: Vec<GeminiModel> = all_models
        .into_iter()
        .filter(|model| {
            model.supported_generation_methods.contains(&"generateContent".to_string())
        })
        .collect();

    tracing::info!("Fetched {} models supporting generateContent", filtered_models.len());

    // Update cache
    {
        let mut cache = MODELS_CACHE.lock().unwrap();
        *cache = Some(ModelsCache {
            models: filtered_models.clone(),
            fetched_at: Instant::now(),
        });
    }

    Ok(filtered_models)
}

/// Clear the models cache, forcing a fresh fetch on next request
pub fn clear_models_cache() {
    let mut cache = MODELS_CACHE.lock().unwrap();
    *cache = None;
    tracing::debug!("Cleared models cache");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_models_no_api_key() {
        let result = fetch_gemini_models(None).await;
        assert!(matches!(result, Err(ModelFetchError::NoApiKey)));
    }

    #[test]
    fn test_clear_cache() {
        // This test just ensures the function doesn't panic
        clear_models_cache();
    }
}
