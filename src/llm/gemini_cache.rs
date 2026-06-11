use std::collections::HashMap;
use std::hash::Hasher;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use reqwest::Client;
use crate::session::Tool;
use super::gemini::{Content, SystemInstruction, GeminiErrorResponse};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCacheEntry {
    /// The resource name returned by the API (e.g., "cachedContents/abc123")
    pub cache_id: String,
    /// Model this cache was created for
    pub model: String,
    /// Number of messages from the conversation that were included in the cache
    pub cached_message_count: usize,
    /// Hash of the system instruction + tools + cached contents prefix
    pub prefix_hash: u64,
    /// When this cache expires (UTC)
    pub expires_at: DateTime<Utc>,
    /// Token count of the cached content (from API response)
    pub token_count: Option<i32>,
}

#[derive(Debug, Default)]
pub struct GeminiCacheStore {
    /// Active cache entry, if any. Keyed by session_id.
    entries: HashMap<String, GeminiCacheEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateCacheRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    ttl: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    contents: Vec<Content>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CachedContentResponse {
    name: String,
    model: String,
    expire_time: String,
    ttl: String,
    #[serde(default)]
    usage_metadata: Option<CacheUsageMetadata>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct CacheUsageMetadata {
    total_token_count: i32,
}

impl GeminiCacheStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Retrieve a valid cached entry for the given session_id, model, and prefix hash.
    pub fn get_valid_cache(&self, session_id: &str, model: &str, prefix_hash: u64) -> Option<&GeminiCacheEntry> {
        if let Some(entry) = self.entries.get(session_id) {
            // Must match model, prefix hash, and not be expired
            if entry.model == model && entry.prefix_hash == prefix_hash && entry.expires_at > Utc::now() {
                return Some(entry);
            }
        }
        None
    }

    /// Store a cache entry for the session.
    pub fn insert_entry(&mut self, session_id: String, entry: GeminiCacheEntry) {
        self.entries.insert(session_id, entry);
    }

    /// Invalidate any cached context for the given session.
    pub fn invalidate(&mut self, session_id: &str) -> Option<GeminiCacheEntry> {
        self.entries.remove(session_id)
    }

    /// Remove expired entries.
    pub fn cleanup_expired(&mut self) {
        self.entries.retain(|_, entry| entry.expires_at > Utc::now());
    }
}

/// Compute a stable hash for system instruction + tools + prefix messages.
pub fn compute_prefix_hash(
    system_instruction: Option<&SystemInstruction>,
    tools: Option<&[Tool]>,
    contents: &[Content],
) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    if let Some(si) = system_instruction {
        if let Ok(s) = serde_json::to_string(si) {
            hasher.write(s.as_bytes());
        }
    }
    if let Some(t) = tools {
        if let Ok(s) = serde_json::to_string(t) {
            hasher.write(s.as_bytes());
        }
    }
    if let Ok(s) = serde_json::to_string(contents) {
        hasher.write(s.as_bytes());
    }
    hasher.finish()
}

/// Call the Gemini API to create a cache for the prefix.
pub async fn api_create_cache(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    session_id: &str,
    system_instruction: Option<SystemInstruction>,
    tools: Option<Vec<Tool>>,
    contents: Vec<Content>,
    ttl_seconds: u32,
    prefix_hash: u64,
) -> Result<GeminiCacheEntry, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/v1beta/cachedContents?key={}", base_url.trim_end_matches('/'), api_key);
    let ttl_str = format!("{}s", ttl_seconds);

    // If model name already has models/ prefix, make sure we use it as is
    let model_path = if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{}", model)
    };

    let display_name = Some(format!("cai-hobbes-session-{}", session_id));

    let request_body = CreateCacheRequest {
        model: model_path.clone(),
        display_name,
        ttl: ttl_str,
        system_instruction,
        tools,
        contents: contents.clone(),
    };

    tracing::debug!("POST /v1beta/cachedContents request: model={}, messages_count={}", model, contents.len());

    let response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();
        let error_msg = if let Ok(err) = serde_json::from_str::<GeminiErrorResponse>(&error_body) {
            err.error.message
        } else {
            error_body
        };
        tracing::warn!("Failed to create Gemini cache [{}]: {}", status, error_msg);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create Gemini cache [{}]: {}", status, error_msg),
        ).into());
    }

    let cache_resp: CachedContentResponse = response.json().await?;
    let expires_at = DateTime::parse_from_rfc3339(&cache_resp.expire_time)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now() + chrono::Duration::seconds(ttl_seconds as i64));

    let token_count = cache_resp.usage_metadata.map(|meta| meta.total_token_count);

    tracing::info!(
        "Successfully created Gemini cache: name={}, model={}, expires_at={}, tokens={:?}",
        cache_resp.name,
        cache_resp.model,
        expires_at,
        token_count
    );

    Ok(GeminiCacheEntry {
        cache_id: cache_resp.name,
        model: model_path,
        cached_message_count: contents.len(),
        prefix_hash,
        expires_at,
        token_count,
    })
}

/// Call the Gemini API to delete a cache.
pub async fn api_delete_cache(
    client: &Client,
    base_url: &str,
    api_key: &str,
    cache_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "{}/v1beta/{}?key={}",
        base_url.trim_end_matches('/'),
        cache_id.trim_start_matches('/'),
        api_key
    );

    tracing::debug!("DELETE /v1beta/{}", cache_id);

    let response = client.delete(&url).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();
        tracing::warn!("Failed to delete Gemini cache [{}]: {}", status, error_body);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to delete Gemini cache [{}]: {}", status, error_body),
        ).into());
    }

    tracing::info!("Successfully deleted Gemini cache: {}", cache_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_prefix_hash_stability() {
        let sys = Some(SystemInstruction {
            parts: vec![super::super::gemini::Part::Text {
                text: "test system prompt".to_string(),
                thought: None,
            }],
        });
        let tools = vec![];
        let contents = vec![Content {
            role: "user".to_string(),
            parts: vec![super::super::gemini::Part::Text {
                text: "hello".to_string(),
                thought: None,
            }],
        }];

        let hash1 = compute_prefix_hash(sys.as_ref(), Some(&tools), &contents);
        let hash2 = compute_prefix_hash(sys.as_ref(), Some(&tools), &contents);
        assert_eq!(hash1, hash2);

        let contents_diff = vec![Content {
            role: "user".to_string(),
            parts: vec![super::super::gemini::Part::Text {
                text: "hello world".to_string(),
                thought: None,
            }],
        }];
        let hash3 = compute_prefix_hash(sys.as_ref(), Some(&tools), &contents_diff);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_cache_store_lookup_and_expiration() {
        let mut store = GeminiCacheStore::new();
        let session_id = "test-session".to_string();
        let model = "models/gemini-2.5-flash".to_string();
        let prefix_hash = 12345;

        // Insert non-expired entry
        let entry = GeminiCacheEntry {
            cache_id: "cachedContents/abc".to_string(),
            model: model.clone(),
            cached_message_count: 2,
            prefix_hash,
            expires_at: Utc::now() + chrono::Duration::seconds(60),
            token_count: Some(500),
        };
        store.insert_entry(session_id.clone(), entry);

        // Verify valid lookup
        assert!(store.get_valid_cache(&session_id, &model, prefix_hash).is_some());
        // Verify mismatch model yields None
        assert!(store.get_valid_cache(&session_id, "models/gemini-2.5-pro", prefix_hash).is_none());
        // Verify mismatch hash yields None
        assert!(store.get_valid_cache(&session_id, &model, 9999).is_none());

        // Expire the entry
        let expired_entry = GeminiCacheEntry {
            cache_id: "cachedContents/expired".to_string(),
            model: model.clone(),
            cached_message_count: 2,
            prefix_hash,
            expires_at: Utc::now() - chrono::Duration::seconds(10),
            token_count: Some(500),
        };
        store.insert_entry(session_id.clone(), expired_entry);

        // Verify expired entry is not returned
        assert!(store.get_valid_cache(&session_id, &model, prefix_hash).is_none());

        // Test cleanup
        store.cleanup_expired();
        assert!(store.entries.is_empty());
    }
}
