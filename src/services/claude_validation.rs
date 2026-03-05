use reqwest::Client;
use std::time::Duration;

/// Validate a Claude API key by calling GET /v1/models
/// Returns Ok(()) if valid, Err with user-friendly message if invalid
pub async fn validate_claude_api_key(api_key: &str) -> Result<(), String> {
    if api_key.trim().is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    match client
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                Err("Invalid API key".to_string())
            } else {
                let error_text = response.text().await.unwrap_or_default();
                Err(format!("Anthropic API error ({}): {}", status, error_text))
            }
        }
        Err(e) => {
            if e.is_timeout() {
                Err("Connection timed out — cannot reach Anthropic API".to_string())
            } else {
                Err(format!("Network error: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_validate_empty_key() {
        let result = validate_claude_api_key("").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "API key cannot be empty");
    }

    #[tokio::test]
    async fn test_validate_whitespace_key() {
        let result = validate_claude_api_key("   ").await;
        assert!(result.is_err());
    }
}
