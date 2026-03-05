use super::models::*;
use super::utils::write_to_debug_file;
use super::ComposioClient;
use crate::mcp::oauth_flow::{find_available_port, open_browser, start_callback_server};
use serde_json::Value;
use std::collections::HashMap;

/// List connected accounts for the current user/entity
pub async fn list_connected_accounts(
    client: &ComposioClient,
) -> Result<Vec<ConnectedAccount>, String> {
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
            tracing::warn!(
                "Reached max page limit ({}) for connected accounts",
                MAX_PAGES
            );
            break;
        }

        let mut params: HashMap<&str, String> = HashMap::new();
        if let Some(ref uid) = user_uuid {
            params.insert("user_id", uid.clone());
            params.insert("user_uuid", uid.clone());
        }
        if let Some(ref c) = cursor {
            params.insert("cursor", c.clone());
        }

        tracing::debug!(
            "Fetching connected accounts page {} from {}",
            page_count,
            base_url
        );

        let response = client
            .client
            .get(&base_url)
            .header("x-api-key", &client.api_key)
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("Failed to send request to list connected accounts: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!("Failed to fetch connected accounts: status {}", status);
            let text = response.text().await.unwrap_or_default();
            if let Err(e) = write_to_debug_file(
                "composio_account_error.txt",
                &format!("Status: {}\nBody: {}", status, text),
            ) {
                tracing::warn!("Failed to log account error: {}", e);
            }
            // Return what we have so far rather than failing completely
            break;
        }

        let response_text = response
            .text()
            .await
            .map_err(|e| format!("Failed to get response text for connected accounts: {}", e))?;

        // Debug log only the first page
        if page_count == 1 {
            if let Err(e) = write_to_debug_file("composio_connected_accounts.json", &response_text)
            {
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

    tracing::trace!(
        "Found {} connected accounts across {} pages",
        all_accounts.len(),
        page_count
    );

    // PATTERN 122: Proactive Cache Hydration
    // Populate the toolkit_account_map used by execution.rs for "Proactive Verification"
    let current_user_id = client.user_id.clone().or(client.entity_id.clone());
    if let Ok(mut map) = client.toolkit_account_map.write() {
        // Clear old entries to ensure freshness on each hydration
        map.clear();

        for acc in &all_accounts {
            // Only cache ACTIVE connections
            if !acc.status.eq_ignore_ascii_case("ACTIVE") {
                continue;
            }

            // CRITICAL: Only accept accounts whose user_id matches the current profile's
            // user_id. Without this filter, stale ACTIVE entries from old UUIDs (e.g. after
            // API key rotation) pollute the map and cause toolkits to appear "connected"
            // when they are not authenticated for the current user.
            let uid_matches = match (&acc.user_id, &current_user_id) {
                (Some(acc_uid), Some(client_uid)) => acc_uid == client_uid,
                (None, None) => true, // Both unset — legacy/default entity
                _ => false,           // Mismatch — skip
            };

            if !uid_matches {
                tracing::trace!(
                    "[AUTH] Skipping account '{}' — user_id {:?} does not match current {:?}",
                    acc.id,
                    acc.user_id,
                    current_user_id
                );
                continue;
            }

            let account_id = acc.id.clone();

            // Map both Toolkit Slug and App Name if available
            // CRITICAL: Always lowercase keys. All lookups use .to_lowercase()
            // (see execution.rs line 590). Without this, API casing like "CLICKUP"
            // won't match the lookup key "clickup", causing false cache misses
            // that trigger unnecessary initiate_connection and duplicate connections.
            if let Some(toolkit) = &acc.toolkit {
                map.insert(toolkit.slug.to_lowercase(), account_id.clone());
            }

            if let Some(app_name) = &acc.app_name {
                map.insert(app_name.to_lowercase(), account_id.clone());
            }
        }
        tracing::debug!(
            "Hydrated toolkit_account_map with {} entries (filtered by user_id: {:?})",
            map.len(),
            current_user_id
        );
    } else {
        tracing::warn!("Failed to acquire write lock for toolkit_account_map");
    }

    // Debug: Log all accounts after pagination
    let all_slugs: Vec<String> = all_accounts
        .iter()
        .map(|a| {
            format!(
                "{} ({})",
                a.toolkit.as_ref().map(|t| t.slug.as_str()).unwrap_or("?"),
                a.status.as_str()
            )
        })
        .collect();
    let _ = write_to_debug_file(
        "composio_all_accounts.txt",
        &format!(
            "Total: {} accounts across {} pages\nAccounts: {:?}",
            all_accounts.len(),
            page_count,
            all_slugs
        ),
    );

    Ok(all_accounts)
}

/// Delete a connected account by ID
pub async fn delete_connected_account(
    client: &ComposioClient,
    account_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/connected_accounts/{}",
        client.get_api_base_url(),
        account_id
    );
    tracing::info!("Deleting connected account: {}", account_id);

    let response = client
        .client
        .delete(&url)
        .header("x-api-key", &client.api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to delete connected account: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "Failed to delete connected account ({}): {}",
            status, text
        ));
    }

    tracing::info!("Successfully deleted connected account: {}", account_id);
    Ok(())
}

/// Prune connections for a toolkit to ensure "One Active Connection" per user.
/// Keeps the most recent ACTIVE connection and removes duplicates/stale ones.
pub async fn prune_connections(
    client: &ComposioClient,
    toolkit_slug: &str,
    user_id: &str,
) -> Result<(), String> {
    tracing::info!(
        "[PRUNE] Starting connection pruning for toolkit '{}' (user: {})",
        toolkit_slug,
        user_id
    );

    // 1. Fetch all accounts
    let accounts = list_connected_accounts(client).await?;

    // 2. Filter for this user and toolkit
    let mut target_accounts: Vec<&ConnectedAccount> = accounts
        .iter()
        .filter(|acc| {
            let matches_user = acc.user_id.as_deref() == Some(user_id);
            let matches_toolkit = acc
                .toolkit
                .as_ref()
                .map(|t| t.slug.eq_ignore_ascii_case(toolkit_slug))
                .unwrap_or(false)
                || acc
                    .app_name
                    .as_ref()
                    .map(|n| n.eq_ignore_ascii_case(toolkit_slug))
                    .unwrap_or(false);
            matches_user && matches_toolkit
        })
        .collect();

    // 3. Sort by created_at descending (newest first)
    // If created_at is missing, they will just maintain relative order or act as "old" depending on sort stability
    target_accounts.sort_by(|a, b| {
        b.created_at
            .as_deref()
            .unwrap_or("")
            .cmp(a.created_at.as_deref().unwrap_or(""))
    });

    let mut active_found = false;

    for acc in target_accounts {
        let is_active = acc.status.eq_ignore_ascii_case("ACTIVE");
        let is_initiated = acc.status.eq_ignore_ascii_case("INITIATED");
        let is_failed = acc.status.eq_ignore_ascii_case("FAILED");
        let created_at = acc.created_at.as_deref().unwrap_or("");

        if is_active {
            if !active_found {
                // Keep the FIRST (newest) active connection
                tracing::info!(
                    "[PRUNE] Keeping newest ACTIVE connection: {} (created: {})",
                    acc.id,
                    created_at
                );
                active_found = true;
            } else {
                // Remove duplicates
                tracing::warn!(
                    "[PRUNE] Deleting duplicate ACTIVE connection: {} (created: {})",
                    acc.id,
                    created_at
                );
                if let Err(e) = delete_connected_account(client, &acc.id).await {
                    tracing::error!(
                        "[PRUNE] Failed to delete duplicate account {}: {}",
                        acc.id,
                        e
                    );
                }
            }
        } else if is_failed {
            // Always remove failed
            tracing::warn!(
                "[PRUNE] Deleting FAILED connection: {} (created: {})",
                acc.id,
                created_at
            );
            if let Err(e) = delete_connected_account(client, &acc.id).await {
                tracing::error!("[PRUNE] Failed to delete failed account {}: {}", acc.id, e);
            }
        } else if is_initiated {
            // Remove stale INITIATED (older than 24h)
            // Simple heuristic: if we are pruning, we are likely about to create a NEW initiated one.
            // So getting rid of old pending ones is generally safe.
            // For now, let's just delete them if they aren't the absolute newest thing
            // (effectively cleaning up abandoned flows).
            tracing::warn!(
                "[PRUNE] Deleting stale INITIATED connection: {} (created: {})",
                acc.id,
                created_at
            );
            if let Err(e) = delete_connected_account(client, &acc.id).await {
                tracing::error!("[PRUNE] Failed to delete stale account {}: {}", acc.id, e);
            }
        }
    }

    Ok(())
}

/// Create an auth config for a toolkit.
pub(crate) async fn create_auth_config(
    client: &ComposioClient,
    toolkit_slug: &str,
    auth_scheme: Option<&str>,
    use_managed: bool,
) -> Result<String, String> {
    tracing::error!(
        "DEBUG: create_auth_config called for '{}'. Auth Scheme: {:?}, Use Managed: {}",
        toolkit_slug,
        auth_scheme,
        use_managed
    );
    let url = format!("{}/auth_configs", client.get_api_base_url());

    // Build the payload based on auth type
    // IMPORTANT: The field is "auth_config" not "options" - matches Composio Python SDK
    // Check for custom credentials first (BYOA - Local Primacy)
    let custom_creds = {
        let lock = client
            .custom_auth_creds
            .read()
            .unwrap_or_else(|e| e.into_inner());
        lock.get(toolkit_slug).cloned()
    };

    // Determine the strategy:
    // 1. If we have custom credentials, ALWAYS use them (User override).
    // 2. If explicitly asked for managed auth, try that (unless overridden by #1? No, explicit flag wins if we want to force it).
    //    Actually, for now, if custom creds exist, we assume the user WANTS to use them.
    // 3. Fallback to managed or basic.

    let payload = if let Some(creds) = custom_creds {
        tracing::info!(
            "Using custom credentials for toolkit '{}' (Fields: {:?})",
            toolkit_slug,
            creds.keys()
        );

        // Convert HashMap<String, String> to Map<String, Value>
        let mut credentials_json = serde_json::Map::new();
        for (k, v) in creds {
            credentials_json.insert(k, serde_json::Value::String(v));
        }

        serde_json::json!({
            "toolkit": { "slug": toolkit_slug },
            "auth_config": {
                "type": "use_custom_auth",
                 // Default to OAUTH2 if not specified, but usually custom auth implies protocol awareness.
                 // Ideally we should know the scheme. For now, we trust the API to infer or we assume OAUTH2/API_KEY based on content?
                 // The `auth_scheme` param might give us a hint if provided.
                "authScheme": auth_scheme.unwrap_or("OAUTH2").to_uppercase(),
                "credentials": credentials_json
            }
        })
    } else if use_managed {
        // Try Composio managed auth (OAuth apps that Composio has pre-configured)
        tracing::info!(
            "Creating managed auth config for toolkit '{}'",
            toolkit_slug
        );
        serde_json::json!({
            "toolkit": { "slug": toolkit_slug },
            "auth_config": { "type": "use_composio_managed_auth" }
        })
    } else if let Some(scheme) = auth_scheme {
        // Use custom auth with explicit scheme (API_KEY, BEARER_TOKEN, etc.) but NO credentials provided?
        // This path is likely for when the user hasn't set up keys yet, or we want to trigger a flow that asks for them?
        // Or maybe for "Basic" auth where we just need the scheme setup.
        tracing::info!(
            "Creating custom auth config for toolkit '{}' with scheme '{}' (No local credentials found)",
            toolkit_slug,
            scheme
        );
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
        tracing::info!(
            "Creating basic auth config for toolkit '{}' (no auth scheme specified)",
            toolkit_slug
        );
        serde_json::json!({
            "toolkit": { "slug": toolkit_slug }
        })
    };

    tracing::debug!("Auth config payload: {:?}", payload);
    tracing::error!("DEBUG: Sending to URL: {}", url);
    tracing::error!(
        "DEBUG: Payload: {}",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );

    let response = client
        .client
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
        return Err(format!(
            "Failed to create auth config ({}): {}",
            status, text
        ));
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
    let auth_config_id = json
        .get("auth_config")
        .and_then(|ac| ac.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| json.get("id").and_then(|v| v.as_str()))
        .ok_or_else(|| format!("No id in auth config response: {}", response_text))?;

    tracing::info!(
        "Created auth config '{}' for toolkit '{}'",
        auth_config_id,
        toolkit_slug
    );

    {
        match client.auth_config_cache.write() {
            Ok(mut cache) => {
                cache.insert(toolkit_slug.to_string(), auth_config_id.to_string());
            }
            Err(e) => {
                tracing::error!(
                    "[PANIC PREVENTION] Failed to acquire write lock to cache auth config: {}",
                    e
                );
            }
        }
    }

    Ok(auth_config_id.to_string())
}

/// Fetch the auth_config_id for a given toolkit, prioritizing local custom credentials.
pub(crate) async fn get_auth_config_id(
    client: &ComposioClient,
    toolkit_slug: &str,
) -> Result<String, String> {
    // Check for custom credentials first (BYOA - Local Primacy)
    // If local keys exist, we MUST ensure they are used by calling create_auth_config.
    let has_custom_creds = {
        let lock = match client.custom_auth_creds.read() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("Failed to acquire read lock on custom_auth_creds: {}", e);
                e.into_inner()
            }
        };
        lock.contains_key(toolkit_slug)
    };

    if has_custom_creds {
        tracing::info!(
            "Custom credentials detected for toolkit '{}'. Checking for existing auth config before creating.",
            toolkit_slug
        );
        // BYOA Primacy: We want to ensure custom creds are used, but we MUST check
        // if an auth config already exists first. Creating a duplicate will fail with
        // 400 ("Missing required field Client Id") because the API rejects duplicates.
        // Only call create_auth_config if no existing config is found.

        // 1. Check cache
        if let Ok(cache) = client.auth_config_cache.read() {
            if let Some(cached_id) = cache.get(toolkit_slug) {
                tracing::info!(
                    "Found cached auth_config_id '{}' for BYOA toolkit '{}'",
                    cached_id,
                    toolkit_slug
                );
                return Ok(cached_id.clone());
            }
        }

        // 2. Check API — do the same filtered lookup as the non-BYOA path
        let url = format!("{}/auth_configs", client.get_api_base_url());
        let req = client
            .client
            .get(&url)
            .header("x-api-key", &client.api_key)
            .query(&[("toolkit_slug", toolkit_slug), ("limit", "50")]);

        if let Ok(response) = req.send().await {
            if response.status().is_success() {
                if let Ok(text) = response.text().await {
                    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(items) = json_value.get("items").and_then(|v| v.as_array()) {
                            for item in items {
                                let item_slug = item
                                    .get("toolkit")
                                    .and_then(|t| t.get("slug"))
                                    .and_then(|s| s.as_str());
                                if let Some(slug) = item_slug {
                                    if slug.eq_ignore_ascii_case(toolkit_slug) {
                                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                                            tracing::info!(
                                                "Found existing auth config '{}' for BYOA toolkit '{}' via API lookup",
                                                id, toolkit_slug
                                            );
                                            // Cache it
                                            if let Ok(mut cache) = client.auth_config_cache.write()
                                            {
                                                cache.insert(
                                                    toolkit_slug.to_lowercase(),
                                                    id.to_string(),
                                                );
                                            }
                                            return Ok(id.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. No existing config found — create with BYOA credentials
        tracing::info!(
            "No existing auth config for BYOA toolkit '{}'. Creating with custom credentials.",
            toolkit_slug
        );
        return create_auth_config(client, toolkit_slug, None, false).await;
    }

    // fallback to existing cache/API lookup for managed or existing configs
    {
        match client.auth_config_cache.read() {
            Ok(cache) => {
                if let Some(cached_id) = cache.get(toolkit_slug) {
                    tracing::debug!(
                        "Using cached auth_config_id '{}' for toolkit '{}'",
                        cached_id,
                        toolkit_slug
                    );
                    return Ok(cached_id.clone());
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[PANIC PREVENTION] Failed to acquire read lock on auth_config_cache: {}",
                    e
                );
                // Continue to fetch from API if cache read fails
            }
        }
    }

    let url = format!("{}/auth_configs", client.get_api_base_url());
    let mut current_cursor: Option<String> = None;
    let mut found_id: Option<String> = None;

    // Pagination Loop
    loop {
        tracing::debug!(
            "Fetching auth configs for toolkit '{}' from {} (cursor: {:?})",
            toolkit_slug,
            url,
            current_cursor
        );

        let mut req = client
            .client
            .get(&url)
            .header("x-api-key", &client.api_key)
            .query(&[("toolkit_slug", toolkit_slug), ("limit", "50")]); // Filter on server side

        if let Some(ref c) = current_cursor {
            req = req.query(&[("cursor", c)]);
        }

        let response = req
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

        // Parse response using the AuthConfigInfo struct for robustness
        let json_value: Value = serde_json::from_str(&response_text)
            .map_err(|e| format!("Failed to parse auth configs response: {}", e))?;

        // Extract items
        if let Some(items) = json_value.get("items").and_then(|v| v.as_array()) {
            for item in items {
                // Correctly resolve the nested toolkit slug structure
                // API returns: { "toolkit": { "slug": "clickup" }, ... }
                let item_slug = item
                    .get("toolkit")
                    .and_then(|t| t.get("slug"))
                    .and_then(|s| s.as_str())
                    .or_else(|| {
                        // Fallback to flat properties if format changes or relies on old schema
                        item.get("toolkitSlug")
                            .or_else(|| item.get("toolkit_slug"))
                            .or_else(|| item.get("appName"))
                            .or_else(|| item.get("app_name"))
                            .and_then(|v| v.as_str())
                    });

                if let Some(slug) = item_slug {
                    tracing::trace!(
                        "[AUTH CONFIG] Comparing API slug '{}' with requested '{}'",
                        slug,
                        toolkit_slug
                    );
                    if slug.eq_ignore_ascii_case(toolkit_slug) {
                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                            tracing::info!(
                                "Found auth config ID '{}' for toolkit '{}'",
                                id,
                                toolkit_slug
                            );
                            found_id = Some(id.to_string());
                            break;
                        }
                    }
                }
            }
        }

        if found_id.is_some() {
            break;
        }

        // Check for next_cursor
        if let Some(next) = json_value.get("next_cursor").and_then(|v| v.as_str()) {
            if !next.is_empty() {
                current_cursor = Some(next.to_string());
            } else {
                break;
            }
        } else {
            break;
        }
    }

    if let Some(ref id) = found_id {
        match client.auth_config_cache.write() {
            Ok(mut cache) => {
                cache.insert(toolkit_slug.to_string(), id.clone());
            }
            Err(e) => {
                tracing::error!(
                    "[PANIC PREVENTION] Failed to acquire write lock to cache auth config id: {}",
                    e
                );
            }
        }
        return Ok(id.clone());
    }

    // No auth config found - create one programmatically
    // Try managed auth first (most common case), the create function will handle the details
    tracing::info!(
        "No auth config found for toolkit '{}', creating one with managed auth...",
        toolkit_slug
    );
    create_auth_config(client, toolkit_slug, None, true).await
}

/// Fetch ALL auth configs for this project and populate the cache.
pub async fn list_auth_configs(client: &ComposioClient) -> Result<Vec<AuthConfigInfo>, String> {
    let url = format!("{}/auth_configs", client.get_api_base_url());
    let mut all_configs: Vec<AuthConfigInfo> = Vec::new();
    let mut current_cursor: Option<String> = None;

    // Limit safety loop
    const MAX_PAGES: usize = 20;
    let mut page_count = 0;

    loop {
        page_count += 1;
        if page_count > MAX_PAGES {
            tracing::warn!(
                "Reached max page limit ({}) for auth configs list",
                MAX_PAGES
            );
            break;
        }

        let mut req = client
            .client
            .get(&url)
            .header("x-api-key", &client.api_key)
            .query(&[("limit", "50")]);

        if let Some(ref c) = current_cursor {
            req = req.query(&[("cursor", c)]);
        }

        tracing::debug!("Fetching all auth configs page {} from {}", page_count, url);

        let response = req
            .send()
            .await
            .map_err(|e| format!("Failed to fetch auth configs: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            tracing::warn!("Failed to fetch auth configs: status {} - {}", status, text);
            // Return what we have so far
            if all_configs.is_empty() {
                return Err(format!("Failed to fetch auth configs: {}", status));
            } else {
                break;
            }
        }

        let response_text = response.text().await.map_err(|e| e.to_string())?;

        // Parse response - API returns { items: [...], next_cursor: ... }
        let json: Value = serde_json::from_str(&response_text)
            .map_err(|e| format!("Failed to parse auth configs response: {}", e))?;

        if let Some(items) = json.get("items").and_then(|v| v.as_array()) {
            for item in items {
                if let Ok(config) = serde_json::from_value::<AuthConfigInfo>(item.clone()) {
                    all_configs.push(config);
                }
            }
        }

        // Check next cursor
        if let Some(next) = json.get("next_cursor").and_then(|v| v.as_str()) {
            if !next.is_empty() {
                current_cursor = Some(next.to_string());
            } else {
                break;
            }
        } else {
            break;
        }
    }

    tracing::info!(
        "Loaded {} auth configs across {} pages",
        all_configs.len(),
        page_count
    );

    // Populate the cache as a side effect (calling local helper)
    cache_auth_configs(client, &all_configs);

    Ok(all_configs)
}

/// Populate auth_config_cache from a list of AuthConfigInfo
fn cache_auth_configs(client: &ComposioClient, configs: &[AuthConfigInfo]) {
    let mut cache = match client.auth_config_cache.write() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "[PANIC PREVENTION] Failed to acquire write lock for bulk auth config cache: {}",
                e
            );
            return;
        }
    };
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

/// Initiate the OAuth connection flow.
/// When `force` is true, skips the ACTIVE connection safety check (used during
/// auth recovery when we KNOW the token is dead despite ACTIVE backend status).
pub async fn initiate_connection(
    client: &ComposioClient,
    toolkit_slug: &str,
    user_id: &str,
    force: bool,
) -> Result<String, String> {
    // Use the internal Proxy Link endpoint (original working pattern from 889718d)
    // This is different from the public /api/v3/connected_accounts endpoint.
    let url = format!("{}/connected_accounts/link", client.get_api_base_url());

    let final_user_id = client
        .user_id
        .clone()
        .unwrap_or_else(|| user_id.to_string());

    // PRUNE: Ensure we start with a clean slate
    // This removes duplicates and stale connections BEFORE we ask for a new one.
    // If there is an existing ACTIVE connection, it will be preserved (logged),
    // but typically initiate_connection is called when the user explicitly needs a NEW one.
    if let Err(e) = prune_connections(client, toolkit_slug, &final_user_id).await {
        tracing::warn!(
            "[PRUNE] Failed to prune connections before initiation: {}",
            e
        );
    }

    // SAFETY CHECK: After pruning, verify if we already have an ACTIVE connection.
    // If so, skip the OAuth flow entirely to prevent unnecessary re-initiation loops.
    // EXCEPTION: When force=true (auth recovery from a confirmed 401), skip this check
    // because the backend reports ACTIVE even though the downstream token is dead.
    if !force {
        match list_connected_accounts(client).await {
            Ok(accounts) => {
                let has_active = accounts.iter().any(|acc| {
                    let matches_toolkit = acc
                        .toolkit
                        .as_ref()
                        .map(|t| t.slug.eq_ignore_ascii_case(toolkit_slug))
                        .unwrap_or(false)
                        || acc
                            .app_name
                            .as_ref()
                            .map(|n| n.eq_ignore_ascii_case(toolkit_slug))
                            .unwrap_or(false);
                    let matches_user = acc.user_id.as_deref() == Some(&final_user_id);
                    let is_active = acc.status.eq_ignore_ascii_case("ACTIVE");
                    matches_toolkit && matches_user && is_active
                });

                if has_active {
                    tracing::info!(
                        "[AUTH] Active connection already exists for toolkit '{}' (user: {}). Skipping OAuth flow.",
                        toolkit_slug,
                        final_user_id
                    );
                    return Ok(
                        "Connection already active. No re-authentication needed.".to_string()
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[AUTH] Failed to check for active connections: {}. Proceeding with OAuth.",
                    e
                );
            }
        }
    } else {
        tracing::info!(
            "[AUTH RECOVERY] Force mode: skipping ACTIVE safety check for '{}'",
            toolkit_slug
        );
    }

    // We need the auth_config_id to tell the API WHICH toolkit to link.
    let auth_config_id = get_auth_config_id(client, toolkit_slug).await?;

    // Find a random port for the callback
    let port = find_available_port()
        .ok_or_else(|| "Failed to find available port for callback".to_string())?;
    let callback_url = format!("http://localhost:{}/callback", port);

    // Payload for the link endpoint (original keys: user_id, callback_url)
    let payload = serde_json::json!({
        "auth_config_id": auth_config_id,
        "user_id": final_user_id,
        "callback_url": callback_url
    });

    tracing::info!("[REST] Initiating connection via {}: {:?}", url, payload);

    let response = client
        .client
        .post(&url)
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
        return Err(format!(
            "Failed to initiate connection ({}): {}",
            status, response_text
        ));
    }

    // Parse response to find redirect URL and connected_account_id
    let json: Value = serde_json::from_str(&response_text).map_err(|e| e.to_string())?;

    // Capture connected_account_id from link response (managed auth returns it here)
    let link_account_id = json
        .get("connected_account_id")
        .or_else(|| json.get("connectedAccountId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Check for redirectUrl at various locations in response structure
    // REST API typically returns it in 'redirectUrl' or 'connectionRequest.redirectUrl'
    let redirect_url = if let Some(url) = json.get("redirectUrl").and_then(|v| v.as_str()) {
        Some(url.to_string())
    } else if let Some(url) = json.get("redirect_url").and_then(|v| v.as_str()) {
        Some(url.to_string())
    } else if let Some(url) = json
        .get("data")
        .and_then(|d| d.get("redirectUrl"))
        .and_then(|v| v.as_str())
    {
        Some(url.to_string())
    } else {
        json.get("connectionRequest")
            .and_then(|d| d.get("redirectUrl"))
            .and_then(|v| v.as_str())
            .map(|url| url.to_string())
    };

    if let Some(auth_url) = redirect_url {
        tracing::info!("Initiating local auth flow. Listening on port {}", port);

        // Start the callback server to receive the OAuth callback
        let mut rx = start_callback_server(port);

        // Open the browser for user to authenticate
        if let Err(e) = open_browser(&auth_url, client.chrome_profile_directory.as_deref()).await {
            tracing::error!("Failed to open browser: {}", e);
            return Ok(format!(
                "Please visit this URL to authenticate: {}",
                auth_url
            ));
        }

        // Try callback first (60s — works for BYOA where localhost receives redirect).
        // If it doesn't fire, fall back to polling the connection status (managed auth
        // where connect.composio.dev handles OAuth internally and never redirects to us).
        let callback_result =
            tokio::time::timeout(tokio::time::Duration::from_secs(60), rx.recv()).await;

        match callback_result {
            Ok(Some(result)) if result.success => {
                tracing::info!("Authentication successful via callback!");

                // Write connectedAccountId to toolkit_account_map
                if let Some(acc_id) = result
                    .params
                    .get("connectedAccountId")
                    .or_else(|| result.params.get("connected_account_id"))
                {
                    tracing::info!(
                        "Received connectedAccountId: {} for toolkit: {} (user: {})",
                        acc_id,
                        toolkit_slug,
                        final_user_id
                    );
                    if let Ok(mut map) = client.toolkit_account_map.write() {
                        map.insert(toolkit_slug.to_lowercase(), acc_id.clone());
                    }
                }

                // CAPTURE CONTEXT KEYS from callback params
                // Save non-standard parameters to ContextStore for injection into tool
                // arguments (Pattern 123). This includes connected_account_id which
                // ClickUp and other routed toolkits need (Pattern 124).
                // Staleness protection: reconnect_toolkit busts these entries via
                // context_store.remove_param before re-auth.
                let standard_keys = [
                    "code",
                    "state",
                    "scope",
                    "error",
                    "error_description",
                    "error_uri",
                    "status",
                ];
                for (key, value) in &result.params {
                    if !standard_keys.contains(&key.as_str()) {
                        tracing::info!(
                            "[CONTEXT] Capturing context param '{}' for toolkit '{}'",
                            key,
                            toolkit_slug
                        );
                        client
                            .context_store
                            .save_param(toolkit_slug, &final_user_id, key, value);
                        // KEY NORMALIZATION (Pattern 123 Extension)
                        if key.contains('_') {
                            let camel_key = super::utils::snake_to_camel(key);
                            if camel_key != *key {
                                tracing::info!(
                                    "[CONTEXT] Normalizing '{}' -> '{}'",
                                    key,
                                    camel_key
                                );
                                client.context_store.save_param(
                                    toolkit_slug,
                                    &final_user_id,
                                    &camel_key,
                                    value,
                                );
                            }
                        }
                    }
                }

                let _ = list_auth_configs(client).await;
                Ok("Authentication successful! You can now use the tool.".to_string())
            }
            Ok(Some(result)) => {
                // Callback fired but reported failure
                let error = result.error.unwrap_or_else(|| "Unknown error".to_string());
                Err(format!("Authentication failed: {}", error))
            }
            _ => {
                // Callback didn't fire within 60s — try polling for managed auth
                if let Some(ref acct_id) = link_account_id {
                    tracing::debug!(
                        "[MANAGED AUTH] Callback didn't fire, polling connection for ACTIVE status"
                    );
                    let poll_url = format!(
                        "{}/connected_accounts/{}",
                        client.get_api_base_url(),
                        acct_id
                    );
                    // Exponential backoff: start at 3s, add 1s every 10 attempts, cap at 10s.
                    // Total wait ≈ 4 minutes (comparable to prior 80 × 3s = 240s).
                    for attempt in 1..=80 {
                        let delay_secs = std::cmp::min(3 + (attempt / 10), 10);
                        tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
                        if let Ok(resp) = client
                            .client
                            .get(&poll_url)
                            .header("x-api-key", &client.api_key)
                            .send()
                            .await
                        {
                            if let Ok(text) = resp.text().await {
                                if let Ok(j) = serde_json::from_str::<Value>(&text) {
                                    let status = j
                                        .get("status")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("unknown");
                                    match status {
                                        "ACTIVE" => {
                                            tracing::info!(
                                                "[MANAGED AUTH] Connection is ACTIVE (poll {})",
                                                attempt
                                            );
                                            if let Ok(mut map) = client.toolkit_account_map.write()
                                            {
                                                map.insert(
                                                    toolkit_slug.to_lowercase(),
                                                    acct_id.clone(),
                                                );
                                            }
                                            let _ = list_auth_configs(client).await;
                                            return Ok("Authentication successful! You can now use the tool.".to_string());
                                        }
                                        "FAILED" | "EXPIRED" | "REVOKED" => {
                                            let reason = j
                                                .get("status_reason")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("No reason given");
                                            return Err(format!(
                                                "Authentication failed ({}): {}",
                                                status, reason
                                            ));
                                        }
                                        _ => {
                                            if attempt % 10 == 0 {
                                                tracing::debug!(
                                                    "[MANAGED AUTH] Still waiting (status: {}, attempt {}/80, interval: {}s)",
                                                    status, attempt, delay_secs
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err("Authentication timed out — connection did not become ACTIVE within 5 minutes".to_string())
                } else {
                    Err("Authentication timed out (callback did not fire)".to_string())
                }
            }
        }
    } else {
        Err(format!(
            "Could not find redirectUrl in response: {}",
            response_text
        ))
    }
}
