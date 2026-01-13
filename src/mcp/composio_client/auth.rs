use super::ComposioClient;
use super::models::*;
use super::utils::write_to_debug_file;
use crate::mcp::oauth_flow::{find_available_port, start_callback_server, open_browser};
use serde_json::Value;
use std::collections::HashMap;

/// List connected accounts for the current user/entity
pub async fn list_connected_accounts(client: &ComposioClient) -> Result<Vec<ConnectedAccount>, String> {
    let user_uuid = client.user_id.clone().or(client.entity_id.clone());
    tracing::trace!("Listing connected accounts for user_uuid: {:?}", user_uuid);
    
    let base_url = format!("{}/connected_accounts", client.get_api_base_url());
    let mut all_accounts: Vec<ConnectedAccount> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page_count = 0;
    const MAX_PAGES: usize = 10; // Safety limit to prevent infinite loops
    
    loop {
        page_count += 1;
        if page_count > MAX_PAGES {
            tracing::warn!("Reached max page limit ({}) for connected accounts", MAX_PAGES);
            break;
        }
        
        let mut params: HashMap<&str, String> = HashMap::new();
        if let Some(ref uid) = user_uuid {
            params.insert("user_uuid", uid.clone());
        }
        if let Some(ref c) = cursor {
            params.insert("cursor", c.clone());
        }
        
        tracing::debug!("Fetching connected accounts page {} from {}", page_count, base_url);
        
        let response = client
            .client
            .get(&base_url)
            .header("x-api-key", &client.api_key)
            .query(&params)
            .send()
            .await.map_err(|e| format!("Failed to send request to list connected accounts: {}", e))?;
            
        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!("Failed to fetch connected accounts: status {}", status);
            let text = response.text().await.unwrap_or_default();
            if let Err(e) = write_to_debug_file("composio_account_error.txt", &format!("Status: {}\nBody: {}", status, text)) {
                tracing::warn!("Failed to log account error: {}", e);
            }
            // Return what we have so far rather than failing completely
            break;
        }

        let response_text = response.text().await.map_err(|e| format!("Failed to get response text for connected accounts: {}", e))?;
        
        // Debug log only the first page
        if page_count == 1 {
            if let Err(e) = write_to_debug_file("composio_connected_accounts.json", &response_text) {
                tracing::warn!("Failed to debug log connected accounts: {}", e);
            }
        }

        // Parse response and extract next_cursor
        let json: serde_json::Value = match serde_json::from_str(&response_text) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to parse connected accounts response: {}", e);
                break;
            }
        };
        
        // Extract items
        if let Some(items) = json.get("items").and_then(|v| v.as_array()) {
            for item in items {
                if let Ok(acc) = serde_json::from_value::<ConnectedAccount>(item.clone()) {
                    all_accounts.push(acc);
                }
            }
        }
        
        // Check for next_cursor to continue pagination
        if let Some(next) = json.get("next_cursor").and_then(|v| v.as_str()) {
            if !next.is_empty() {
                cursor = Some(next.to_string());
            } else {
                break; // Empty cursor means no more pages
            }
        } else {
            break; // No cursor means no more pages
        }
    }
    
    tracing::trace!("Found {} connected accounts across {} pages", all_accounts.len(), page_count);
    
    // Debug: Log all accounts after pagination
    let all_slugs: Vec<String> = all_accounts.iter()
        .map(|a| format!("{} ({})", a.toolkit.as_ref().map(|t| t.slug.as_str()).unwrap_or("?"), a.status.as_str()))
        .collect();
    let _ = write_to_debug_file("composio_all_accounts.txt", &format!(
        "Total: {} accounts across {} pages\nAccounts: {:?}", 
        all_accounts.len(), page_count, all_slugs
    ));
    
    Ok(all_accounts)
}

/// Create an auth config for a toolkit.
pub(crate) async fn create_auth_config(
    client: &ComposioClient,
    toolkit_slug: &str,
    auth_scheme: Option<&str>,
    use_managed: bool,
) -> Result<String, String> {
    tracing::error!("DEBUG: create_auth_config called for '{}'. Auth Scheme: {:?}, Use Managed: {}", 
        toolkit_slug, auth_scheme, use_managed);
    let url = format!("{}/auth_configs", client.get_api_base_url());
    
    // Build the payload based on auth type
    // IMPORTANT: The field is "auth_config" not "options" - matches Composio Python SDK
    let payload = if use_managed {
        // Try Composio managed auth (OAuth apps that Composio has pre-configured)
        tracing::info!("Creating managed auth config for toolkit '{}'", toolkit_slug);
        serde_json::json!({
            "toolkit": { "slug": toolkit_slug },
            "auth_config": { "type": "use_composio_managed_auth" }
        })
    } else if let Some(scheme) = auth_scheme {
        // Use custom auth with explicit scheme (API_KEY, BEARER_TOKEN, etc.)
        tracing::info!("Creating custom auth config for toolkit '{}' with scheme '{}'", toolkit_slug, scheme);
        serde_json::json!({
            "toolkit": { "slug": toolkit_slug },
            "auth_config": {
                "type": "use_custom_auth",
                "authScheme": scheme.to_uppercase(),
                "credentials": {}
            }
        })
    } else {
        // Fallback: basic config (for no_auth toolkits or when scheme is unknown)
        tracing::info!("Creating basic auth config for toolkit '{}' (no auth scheme specified)", toolkit_slug);
        serde_json::json!({
            "toolkit": { "slug": toolkit_slug }
        })
    };
    
    tracing::debug!("Auth config payload: {:?}", payload);
    tracing::error!("DEBUG: Sending to URL: {}", url);
    tracing::error!("DEBUG: Payload: {}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    
    let response = client.client
        .post(&url)
        .header("x-api-key", &client.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Failed to create auth config: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("Failed to create auth config ({}): {}", status, text));
    }
    
    let response_text = response.text().await.map_err(|e| e.to_string())?;
    
    if let Err(e) = write_to_debug_file("composio_create_auth_config.json", &response_text) {
        tracing::warn!("Failed to debug log create auth config response: {}", e);
    }
    
    // Parse response to get auth_config_id
    // Response format: {"toolkit":{...}, "auth_config":{"id":"...", ...}}
    let json: Value = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse auth config response: {}", e))?;
    
    // Try auth_config.id first (new API format), then fall back to top-level id
    let auth_config_id = json.get("auth_config")
        .and_then(|ac| ac.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| json.get("id").and_then(|v| v.as_str()))
        .ok_or_else(|| format!("No id in auth config response: {}", response_text))?;
    
    tracing::info!("Created auth config '{}' for toolkit '{}'", auth_config_id, toolkit_slug);
    
    // Cache the new auth config
    {
        let mut cache = client.auth_config_cache.write().unwrap();
        cache.insert(toolkit_slug.to_string(), auth_config_id.to_string());
    }
    
    Ok(auth_config_id.to_string())
}

/// Fetch the auth config ID for a given toolkit from Composio API.
pub(crate) async fn get_auth_config_id(client: &ComposioClient, toolkit_slug: &str) -> Result<String, String> {
    // Check cache first
    {
        let cache = client.auth_config_cache.read().unwrap();
        if let Some(cached_id) = cache.get(toolkit_slug) {
            tracing::debug!("Using cached auth_config_id '{}' for toolkit '{}'", cached_id, toolkit_slug);
            return Ok(cached_id.clone());
        }
    }
    
    let url = format!("{}/auth_configs", client.get_api_base_url());
    
    tracing::debug!("Fetching auth configs for toolkit '{}' from {}", toolkit_slug, url);
    
    let response = client
        .client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .query(&[("toolkit_slug", toolkit_slug)])
        .send()
        .await
        .map_err(|e| format!("Failed to fetch auth configs: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        tracing::warn!("Failed to fetch auth configs: status {} - {}", status, text);
        return Err(format!("Failed to fetch auth configs: {}", status));
    }
    
    let response_text = response.text().await.map_err(|e| e.to_string())?;
    
    if let Err(e) = write_to_debug_file("composio_auth_configs.json", &response_text) {
        tracing::warn!("Failed to debug log auth configs: {}", e);
    }
    
    // Parse response - expect { items: [{ id: "...", toolkitSlug: "...", ... }] }
    let json: Value = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse auth configs response: {}", e))?;
    
    // Look for matching auth config
    let mut found_id: Option<String> = None;
    
    if let Some(items) = json.get("items").and_then(|v| v.as_array()) {
        for item in items {
            // Match by toolkit slug (could be in different field names)
            let item_slug = item.get("toolkitSlug")
                .or_else(|| item.get("toolkit_slug"))
                .or_else(|| item.get("appName"))
                .or_else(|| item.get("app_name"))
                .and_then(|v| v.as_str());
            
            if let Some(slug) = item_slug {
                if slug.to_lowercase() == toolkit_slug.to_lowercase() {
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        tracing::info!("Found auth config ID '{}' for toolkit '{}'", id, toolkit_slug);
                        found_id = Some(id.to_string());
                        break;
                    }
                }
            }
        }
        
        // If no exact match, try first available for this toolkit
        if found_id.is_none() {
            if let Some(first) = items.first() {
                if let Some(id) = first.get("id").and_then(|v| v.as_str()) {
                    tracing::info!("Using first available auth config ID '{}' for toolkit '{}'", id, toolkit_slug);
                    found_id = Some(id.to_string());
                }
            }
        }
    }
    
    // Cache the result if found
    if let Some(ref id) = found_id {
        let mut cache = client.auth_config_cache.write().unwrap();
        cache.insert(toolkit_slug.to_string(), id.clone());
        return Ok(id.clone());
    }
    
    // No auth config found - create one programmatically
    // Try managed auth first (most common case), the create function will handle the details
    tracing::info!("No auth config found for toolkit '{}', creating one with managed auth...", toolkit_slug);
    create_auth_config(client, toolkit_slug, None, true).await
}

/// Fetch ALL auth configs for this project and populate the cache.
pub async fn list_auth_configs(client: &ComposioClient) -> Result<Vec<AuthConfigInfo>, String> {
    let url = format!("{}/auth_configs", client.get_api_base_url());
    
    tracing::debug!("Fetching all auth configs from {}", url);
    
    let response = client
        .client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch auth configs: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        tracing::warn!("Failed to fetch auth configs: status {} - {}", status, text);
        return Err(format!("Failed to fetch auth configs: {}", status));
    }
    
    let response_text = response.text().await.map_err(|e| e.to_string())?;
    
    // Parse response - API returns { items: [...] }
    let json: Value = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse auth configs response: {}", e))?;
    
    let mut configs: Vec<AuthConfigInfo> = Vec::new();
    
    if let Some(items) = json.get("items").and_then(|v| v.as_array()) {
        for item in items {
            if let Ok(config) = serde_json::from_value::<AuthConfigInfo>(item.clone()) {
                configs.push(config);
            }
        }
    }
    
    tracing::info!("Loaded {} auth configs", configs.len());
    
    // Populate the cache as a side effect (calling local helper)
    cache_auth_configs(client, &configs);
    
    Ok(configs)
}

/// Populate auth_config_cache from a list of AuthConfigInfo
fn cache_auth_configs(client: &ComposioClient, configs: &[AuthConfigInfo]) {
    let mut cache = client.auth_config_cache.write().unwrap();
    cache.clear();
    
    for config in configs {
        if let Some(slug) = config.toolkit_slug() {
            // Only cache ENABLED configs
            let is_enabled = config.status.as_deref() == Some("ENABLED");
            if is_enabled {
                tracing::debug!("Caching auth_config '{}' for toolkit '{}'", config.id, slug);
                cache.insert(slug.to_lowercase(), config.id.clone());
            }
        }
    }
    
    tracing::debug!("Cached {} auth configs", cache.len());
}

/// Initiate the OAuth connection flow
pub async fn initiate_connection(client: &ComposioClient, toolkit_slug: &str, user_id: &str) -> Result<String, String> {
    // Use the internal Proxy Link endpoint (original working pattern from 889718d)
    // This is different from the public /api/v3/connected_accounts endpoint.
    let url = format!("{}/connected_accounts/link", client.get_api_base_url());
    
    let final_user_id = client.user_id.clone().unwrap_or_else(|| user_id.to_string());
    
    // We need the auth_config_id to tell the API WHICH toolkit to link.
    let auth_config_id = get_auth_config_id(client, toolkit_slug).await?;

    // Find a random port for the callback
    let port = find_available_port().ok_or_else(|| "Failed to find available port for callback".to_string())?;
    let callback_url = format!("http://localhost:{}/callback", port);

    // Payload for the link endpoint (original keys: user_id, callback_url)
    let payload = serde_json::json!({
        "auth_config_id": auth_config_id, 
        "user_id": final_user_id,
        "callback_url": callback_url
    });
    
    tracing::info!("[REST] Initiating connection via {}: {:?}", url, payload);

    let response = client.client.post(&url)
        .header("x-api-key", &client.api_key) // REST API requires x-api-key
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Connection initiation error: {}", e))?;

    let status = response.status();
    let response_text = response.text().await.map_err(|e| e.to_string())?;
    
    // Debug log the response
    let _ = write_to_debug_file("composio_initiate_resp.json", &response_text);
    
    if !status.is_success() {
         return Err(format!("Failed to initiate connection ({}): {}", status, response_text));
    }

    // Parse response to find redirect URL
    let json: Value = serde_json::from_str(&response_text).map_err(|e| e.to_string())?;
    
    // Check for redirectUrl at various locations in response structure
    // REST API typically returns it in 'redirectUrl' or 'connectionRequest.redirectUrl'
    let redirect_url = if let Some(url) = json.get("redirectUrl").and_then(|v| v.as_str()) {
        Some(url.to_string())
    } else if let Some(url) = json.get("redirect_url").and_then(|v| v.as_str()) {
         Some(url.to_string())
    } else if let Some(url) = json.get("data").and_then(|d| d.get("redirectUrl")).and_then(|v| v.as_str()) {
         Some(url.to_string())
    } else if let Some(url) = json.get("connectionRequest").and_then(|d| d.get("redirectUrl")).and_then(|v| v.as_str()) {
        Some(url.to_string())
    } else {
        None
    };

    if let Some(auth_url) = redirect_url {
        tracing::info!("Initiating local auth flow. Listening on port {}", port);
        
        // Start the callback server to receive the OAuth callback
        let mut rx = start_callback_server(port);
        
        // Open the browser for user to authenticate
        if let Err(e) = open_browser(&auth_url) {
            tracing::error!("Failed to open browser: {}", e);
            return Ok(format!("Please visit this URL to authenticate: {}", auth_url));
        }
        
        // Wait for the callback (with timeout)
        match tokio::time::timeout(tokio::time::Duration::from_secs(300), rx.recv()).await {
            Ok(Some(result)) => {
                if result.success {
                    tracing::info!("Authentication successful!");
                    
                    // Log the connectedAccountId for debugging
                    if let Some(acc_id) = result.params.get("connectedAccountId")
                        .or_else(|| result.params.get("connected_account_id")) {
                        tracing::info!("Received connectedAccountId: {} for toolkit: {} (user: {})", 
                            acc_id, toolkit_slug, final_user_id);
                    }

                    // CAPTURE CONTEXT KEYS
                    // Save any non-standard parameters to the ContextStore (e.g., team_id, workspace_id)
                    let standard_keys = ["code", "state", "scope", "error", "error_description", "error_uri", "status"];
                    for (key, value) in &result.params {
                        if !standard_keys.contains(&key.as_str()) {
                            tracing::info!("[CONTEXT] Capturing context param '{}' for toolkit '{}'", key, toolkit_slug);
                            client.context_store.save_param(toolkit_slug, &final_user_id, key, value);
                        }
                    }
                    
                    // Refresh auth configs cache to pick up any new configs
                    let _ = list_auth_configs(client).await;
                    
                    return Ok("Authentication successful! You can now use the tool.".to_string());
                } else {
                    let error = result.error.unwrap_or_else(|| "Unknown error".to_string());
                    return Err(format!("Authentication failed: {}", error));
                }
            },
            Ok(None) => {
                return Err("Callback server closed unexpectedly".to_string());
            },
            Err(_) => {
                return Err("Authentication timed out (5 minutes)".to_string());
            }
        }
    } else {
        Err(format!("Could not find redirectUrl in response: {}", response_text))
    }
}
