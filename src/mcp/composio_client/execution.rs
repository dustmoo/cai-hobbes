use super::auth::{list_auth_configs, list_connected_accounts};
use super::models::*;
use super::utils::write_to_debug_file;
use super::ComposioClient;
use serde_json::Value;

/// Add a toolkit to the MCP server configuration via PATCH API.
/// Returns the new server URL if a server was created, so caller can save it.
pub async fn add_toolkit_to_server(
    client: &ComposioClient,
    toolkit_slug: &str,
    auth_config_id: &str,
    selected_tools: Option<Vec<String>>,
) -> Result<Option<String>, String> {
    // Extract target server ID from base_url/settings for verification
    let target_server_id = client
        .base_url
        .split("/mcp/")
        .nth(1)
        .map(|s| s.split('?').next().unwrap_or(s))
        .map(|s| s.trim_end_matches("/mcp"))
        .unwrap_or_default();

    tracing::info!(
        "Adding toolkit '{}' to MCP server. Target ID from settings: '{}'",
        toolkit_slug,
        target_server_id
    );

    // CRITICAL FIX: Use V3 Registry API to LIST and discover the correct server
    // Endpoint: GET /api/v3/mcp/servers
    let api_base = client.get_api_base_url();
    let registry_base_url = format!("{}/mcp/servers", api_base);

    tracing::debug!("Discovering servers via Registry: {}", registry_base_url);

    let list_response = client
        .client
        .get(registry_base_url)
        .header("x-api-key", &client.api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to list MCP servers: {}", e))?;

    if !list_response.status().is_success() {
        let status = list_response.status();
        let text = list_response.text().await.unwrap_or_default();
        return Err(format!("Failed to list MCP servers ({}): {}", status, text));
    }

    let list_json: Value = list_response
        .json()
        .await
        .map_err(|e| format!("Failed to parse server list: {}", e))?;

    let items = list_json
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or("Invalid server list response")?;

    // Find matching server or use the first one, OR CREATE if none exist
    let (server_id, server_obj_owned, newly_created_url): (String, Value, Option<String>) = if items.is_empty() {
        // No servers exist - create one and an instance for this user
        tracing::info!("No MCP servers found. Creating new server for first toolkit connection.");
        
        let new_server = create_mcp_server(client, toolkit_slug, auth_config_id).await?;
        let new_server_id = new_server
            .get("id")
            .and_then(|s| s.as_str())
            .ok_or("Created server missing ID")?
            .to_string();
        
        // Construct the new MCP URL for the caller to save
        let base_domain = api_base.replace("/api/v3", "");
        let new_url = format!("{}/v3/mcp/{}/mcp", base_domain, new_server_id);
        tracing::info!("Created new server with URL: {}", new_url);
        
        // Create instance to bind user to this server
        if let Some(ref user_id) = client.user_id {
            if let Err(e) = create_mcp_instance(client, &new_server_id, user_id).await {
                tracing::warn!("Failed to create instance (user may already exist): {}", e);
            }
        }
        
        (new_server_id, new_server, Some(new_url))
    } else {
        // MATCHING LOGIC: Prioritize the exact target_server_id if provided
        let mut target_server = None;
        if !target_server_id.is_empty() {
            target_server = items
                .iter()
                .find(|s| s.get("id").and_then(|id| id.as_str()) == Some(target_server_id));
        }

        // Fallback to first if not found, or matching by toolkit if possible? 
        // For now, let's just be more logging-heavy about the choice.
        let found = target_server
            .or_else(|| {
                if !target_server_id.is_empty() {
                    tracing::warn!("Target server ID '{}' not found in Registry list. Falling back to first available.", target_server_id);
                }
                items.first()
            })
            .ok_or_else(|| {
                tracing::error!("Registry returned empty items list after successful list call");
                "No MCP servers found for this account".to_string()
            })?;
        
        let id = found
            .get("id")
            .and_then(|s| s.as_str())
            .ok_or("Server object missing ID")?
            .to_string();
            
        // Return URL for settings update
        let base_domain = api_base.replace("/api/v3", "");
        let existing_url = format!("{}/v3/mcp/{}/mcp", base_domain, id);
        
        (id, found.clone(), Some(existing_url))
    };
    
    // Ensure user is bound to the server (required for tool visibility)
    // Call this regardless of whether the server is new or existing.
    if let Some(ref user_id) = client.user_id {
        if let Err(e) = create_mcp_instance(client, &server_id, user_id).await {
            // Log but don't fail - user may already be bound
            tracing::debug!("Instance binding note (likely already exists): {}", e);
        }
    }

    let server_obj = &server_obj_owned;

    tracing::info!("Resolved MCP Server ID: {}", server_id);

    // Construct the specific config URL for this server
    // Endpoint: GET/PATCH /api/v3/mcp/{server_id}
    // NOTE: The endpoint for CONFIGURATION is /api/v3/mcp/{id} (singular 'mcp', no 'servers')
    // The endpoint for LISTING is /api/v3/mcp/servers
    let config_url = format!("{}/mcp/{}", api_base, server_id);

    // Extract existing toolkits as strings (API requires string format)
    let mut final_toolkits: Vec<String> = server_obj
        .get("toolkits")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    // Handle both string and object formats returned by API
                    t.as_str().map(|s| s.to_string()).or_else(|| {
                        t.get("toolkit")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Extract existing auth_config_ids
    let existing_auth_ids: Vec<String> = server_obj
        .get("auth_config_ids")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // RECONCILIATION STEP: Fetch actual active auth configs to prune stale IDs
    // We must do this to avoid sending deleted IDs back to the server, which causes 400 Bad Request.
    let mut auth_config_ids = Vec::new();
    let mut auth_updated = false;

    // 1. Fetch all active auth configs for this user/client
    // This gives us the "source of truth" for which IDs are valid.
    tracing::debug!("Fetching active auth configs for reconciliation...");
    match list_auth_configs(client).await {
        Ok(active_configs) => {
            // Build a map of valid IDs and which toolkit they belong to
            // Map<AuthConfigID, ToolkitSlug>
            let valid_id_map: std::collections::HashMap<String, String> = active_configs
                .iter()
                .filter_map(|ac| {
                    ac.toolkit.as_ref().map(|t| (ac.id.clone(), t.slug.clone().to_lowercase()))
                })
                .collect();
            
            let target_slug_lower = toolkit_slug.to_lowercase();

            // 2. Filter existing IDs
            for existing_id in existing_auth_ids {
                // Keep the ID if:
                // A) It is present in our list of active configs (it's valid)
                // AND
                // B) It matches a DIFFERENT toolkit OR it is the specific one we are trying to add (unlikely, but safe)
                // effectively removing all OLD configs for THIS toolkit.
                if let Some(associated_slug) = valid_id_map.get(&existing_id) {
                    if associated_slug == &target_slug_lower {
                        // This is an config for OUR target toolkit.
                        // We will be adding the NEW authoritative one below.
                        // So we drop this one (pruning old configs for this tool).
                        tracing::debug!("Pruning stale/old auth config '{}' for current toolkit '{}'", existing_id, target_slug_lower);
                        auth_updated = true; // We changed the list
                    } else {
                        // It belongs to another toolkit, keep it.
                        auth_config_ids.push(existing_id);
                    }
                } else {
                    // ID not found in active list -> it's stale/deleted. Drop it.
                    tracing::warn!("Pruning invalid/deleted auth config ID '{}'", existing_id);
                    auth_updated = true;
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to fetch active auth configs for reconciliation: {}. Falling back to append-only (risky).", e);
            auth_config_ids = existing_auth_ids;
        }
    }

    // 3. Add the NEW authoritative auth config for this toolkit
    if !auth_config_ids.contains(&auth_config_id.to_string()) {
        auth_config_ids.push(auth_config_id.to_string());
        tracing::info!("Binding new auth_config '{}' for toolkit '{}'", auth_config_id, toolkit_slug);
        auth_updated = true;
    } else {
         tracing::debug!("Auth config '{}' already present in reconciled list", auth_config_id);
    }

    // Check if toolkit already exists
    let normalized_slug = toolkit_slug.to_lowercase();
    let toolkit_already_exists = final_toolkits
        .iter()
        .any(|t| t.to_lowercase() == normalized_slug);

    // Add new toolkit if not present
    if !toolkit_already_exists {
        final_toolkits.push(toolkit_slug.to_lowercase());
        tracing::info!("Adding toolkit '{}' to MCP server", toolkit_slug);
    }

    // Get existing allowed_tools from the server config
    let mut custom_tools: Vec<String> = server_obj
        .get("allowed_tools")
        .or_else(|| server_obj.get("custom_tools"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Determine which tools to add: use pre-selected or fetch all
    let mut use_all_tools = false;
    let tools_added = if let Some(pre_selected) = selected_tools {
        // Use pre-selected tools (from LLM smart selection)
        
        // PRUNING FIX: Remove existing tools for THIS toolkit before adding selection.
        // This ensures the AI's selection actually replaces the "all tools" default.
        let prefix = format!("{}_", toolkit_slug.to_uppercase().replace("-", "_"));
        custom_tools.retain(|t| !t.to_uppercase().starts_with(&prefix));

        let mut added = 0;
        for tool in pre_selected {
            if !custom_tools.contains(&tool) {
                custom_tools.push(tool);
                added += 1;
            }
        }
        tracing::info!(
            "Using {} pre-selected tools for toolkit '{}' (total: {})",
            added,
            toolkit_slug,
            custom_tools.len()
        );
        added
    } else {
        // Step 3 path: No tools specified - do NOT fetch/add all tools here.
        // We set use_all_tools=true to OMIT the allowed_tools field from the PATCH payload,
        // which lets the Composio backend default to allowing all tools for this toolkit
        // until Step 4 (Smart Selection) runs and applies a specific filter.
        tracing::info!("No tools specified for '{}'. Toolkit/auth binding only.", toolkit_slug);
        use_all_tools = true;
        0
    };

    // Skip PATCH if no changes needed (toolkit exists, auth bound, and no new tools)
    // CRITICAL FIX: We must check `!auth_updated` instead of checking if the ID exists in the list
    // because we just added it to the list above!
    let should_patch = !toolkit_already_exists || tools_added > 0 || auth_updated || use_all_tools;

    if should_patch {
        // Build PATCH payload with strict String Array types as per Mandate 5
        let mut patch_payload = serde_json::json!({
            "toolkits": final_toolkits,
            "auth_config_ids": auth_config_ids
        });

        // Only include allowed_tools if we are NOT in "all" mode
        if !use_all_tools {
            if let Some(obj) = patch_payload.as_object_mut() {
                obj.insert("allowed_tools".to_string(), serde_json::json!(custom_tools));
            }
        }

        tracing::debug!("PATCH {} with payload: {:?}", config_url, patch_payload);

        let patch_response = client
            .client
            .patch(&config_url)
            .header("x-api-key", &client.api_key)
            .header("Content-Type", "application/json")
            .json(&patch_payload)
            .send()
            .await
            .map_err(|e| format!("Failed to update MCP server: {}", e))?;

        if !patch_response.status().is_success() {
            let status = patch_response.status();
            let text = patch_response.text().await.unwrap_or_default();
            
            // FAIL FAST: This configuration is critical. If it fails, tools won't work correctly.
            // This prevents "hollow" user generation and the "0 tools" vacuum.
            return Err(format!("Failed to configure toolkit on server ({}): {}", status, text));
        }
    } else {
         tracing::info!(
            "Toolkit '{}' already configured, skipping PATCH",
            toolkit_slug
        );
    }
    
    // Step 4: Generate/register user with the MCP server
    // This is required for the user to see the tools
    // API: POST /api/v3/mcp/servers/generate
    // NOTE: This might be the one place that still uses v3? Docs are unclear, but standard practice says stick to v1 for registry if possible.
    // However, 'generate' implies runtime credential creation. Let's keep it as is unless it fails.
    if let Some(ref user_id) = client.user_id {
        let generate_url = format!("{}/mcp/servers/generate", client.get_api_base_url());
        // CRITICAL: API requires user_ids (plural, array) NOT user_id (singular)
        // SDK pattern: user_ids=[user_id], managed_auth_by_composio=True
        // Pattern 110: Ensure managed_auth_by_composio is explicitly true
        let generate_payload = serde_json::json!({
            "user_ids": [user_id],
            "mcp_server_id": server_id,
            "managed_auth_by_composio": true
        });

        tracing::debug!(
            "Registering user '{}' with MCP server '{}'",
            user_id,
            server_id
        );

        let generate_response = client
            .client
            .post(&generate_url)
            .header("x-api-key", &client.api_key)
            .header("Content-Type", "application/json")
            .json(&generate_payload)
            .send()
            .await;

        match generate_response {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("User '{}' registered with MCP server", user_id);
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!("Failed to register user with MCP server: {}", text);
            }
            Err(e) => {
                tracing::warn!("Error registering user with MCP server: {}", e);
            }
        }
    }

    tracing::info!(
        "Successfully added toolkit '{}' with {} tools to MCP server",
        toolkit_slug,
        custom_tools.len()
    );
    Ok(newly_created_url)
}

/// Create a new MCP server for the user's first toolkit.
/// Called when no servers exist for the account.
/// Endpoint: POST /api/v3/mcp/servers/custom
async fn create_mcp_server(
    client: &ComposioClient,
    toolkit_slug: &str,
    auth_config_id: &str,
) -> Result<Value, String> {
    let api_base = client.get_api_base_url();
    let url = format!("{}/mcp/servers/custom", api_base);

    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let server_name = format!("hobbes-mcp-{}", epoch);

    // Build payload with initial toolkit binding
    // CRITICAL: Use String Arrays for toolkits and auth_config_ids
    // Object binding ([{toolkit:..., auth_config:...}]) is FORBIDDEN by API
    let payload = serde_json::json!({
        "name": server_name,
        "toolkits": [toolkit_slug.to_lowercase()],
        "auth_config_ids": [auth_config_id]
    });

    tracing::info!(
        "Creating new MCP server with initial toolkit '{}': {}",
        toolkit_slug,
        url
    );
    tracing::debug!("POST {} with payload: {:?}", url, payload);

    let response = client
        .client
        .post(&url)
        .header("x-api-key", &client.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Failed to create MCP server: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("Failed to create MCP server ({}): {}", status, text));
    }

    let server: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse create server response: {}", e))?;

    let server_id = server
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");

    tracing::info!("Created new MCP server with ID: {}", server_id);
    Ok(server)
}

/// Create an MCP instance to bind a user to a server.
/// This is required for the user to see tools on the operational proxy.
/// Endpoint: POST /api/v3/mcp/servers/{id}/instances
async fn create_mcp_instance(
    client: &ComposioClient,
    server_id: &str,
    user_id: &str,
) -> Result<Value, String> {
    let api_base = client.get_api_base_url();
    let url = format!("{}/mcp/servers/{}/instances", api_base, server_id);

    let payload = serde_json::json!({
        "user_id": user_id
    });

    tracing::info!(
        "Creating MCP instance for user '{}' on server '{}': {}",
        user_id,
        server_id,
        url
    );

    let response = client
        .client
        .post(&url)
        .header("x-api-key", &client.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Failed to create MCP instance: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "Failed to create MCP instance ({}): {}",
            status, text
        ));
    }

    let instance: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse create instance response: {}", e))?;

    tracing::info!("Created MCP instance for user '{}'", user_id);
    Ok(instance)
}

/// Execute a Composio tool via the MCP server.
pub async fn execute_tool(
    client: &ComposioClient,
    slug: &str,
    args: serde_json::Value,
) -> Result<ToolExecuteResponse, String> {
    // Ensure arguments are wrapped in an object
    let mut arguments = if args.is_object() {
        args
    } else {
        serde_json::json!({ "value": args })
    };

    // Determine the target user_id (prioritize Settings/Profile ID)
    let profile_user_id = client
        .user_id
        .clone()
        .or(client.entity_id.clone())
        .unwrap_or_else(|| "default".to_string());

    let user_id = profile_user_id.clone();

    // 1. Resolve toolkit slug (needed for manual connection initiation)
    let toolkit_slug = match client.tool_toolkit_map.read() {
        Ok(map) => map.get(slug).cloned(),
        Err(e) => {
            tracing::error!(
                "[PANIC PREVENTION] Failed to acquire read lock on tool_toolkit_map: {}",
                e
            );
            None
        }
    };

    // If no mapping, try to infer (heuristic)
    let toolkit_slug = toolkit_slug.or_else(|| {
        let guessed = slug.split('_').next()?.to_lowercase();
        tracing::debug!("Guessed toolkit '{}' for tool '{}'", guessed, slug);
        Some(guessed)
    });

    // CONTEXT INJECTION (Pattern 123)
    // Check if we have stored context keys (e.g. team_id) and inject them if missing.
    // NOTE: This must run AFTER heuristic resolution to ensure we don't miss injection for guessed toolkits.
    if let Some(ref tk_slug) = toolkit_slug {
        if let Some(context) = client.context_store.get_context(tk_slug, &user_id) {
            if let Some(obj) = arguments.as_object_mut() {
                for (k, v) in context {
                    if !obj.contains_key(&k) {
                        tracing::info!(
                            "[CONTEXT] Injecting context param '{}' for toolkit '{}'",
                            k,
                            tk_slug
                        );
                        obj.insert(k, serde_json::Value::String(v));
                    }
                }
            }
        }
    }

    // BYOA CREDENTIAL INJECTION
    // Custom Tool Credentials (Settings → Credentials) injected as missing tool arguments.
    // ContextStore injection above takes priority — BYOA only fills remaining gaps.
    if let Some(ref tk_slug) = toolkit_slug {
        if let Ok(creds) = client.custom_auth_creds.read() {
            if let Some(toolkit_creds) = creds.get(tk_slug) {
                if let Some(obj) = arguments.as_object_mut() {
                    for (k, v) in toolkit_creds {
                        if !obj.contains_key(k) {
                            tracing::debug!("[BYOA] Injecting '{}' for '{}'", k, tk_slug);
                            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
                        }
                    }
                }
            }
        }
    }

    // PROACTIVE AUTH CHECK (restored from v0.9.4 pattern)
    // Check for an ACTIVE connected account for this toolkit BEFORE calling the proxy.
    // If no active connection, trigger initiate_connection immediately.
    if let Some(ref tk_slug) = toolkit_slug {
        tracing::debug!(
            "[AUTH] Checking for active connection for toolkit '{}'",
            tk_slug
        );

        // OPTIMIZED PATTERN 122: Check cache first, then fetch
        let has_active_connection = {
            // 1. Check Cache
            let cached = client.toolkit_account_map.read().ok().and_then(|map| map.get(tk_slug).cloned());
            
            if let Some(acc_id) = cached {
                tracing::debug!("[AUTH] Found cached ACTIVE connection '{}' for toolkit '{}'", acc_id, tk_slug);
                true
            } else {
                // 2. Cache Miss - Fetch & Refresh
                tracing::debug!("[AUTH] Cache miss for toolkit '{}'. Fetching connected accounts...", tk_slug);
                match list_connected_accounts(client).await {
                    Ok(_) => {
                        // 3. Check Cache Again (populated by list_connected_accounts)
                         client.toolkit_account_map.read().ok().map(|map| map.contains_key(tk_slug)).unwrap_or(false)
                    },
                    Err(e) => {
                        tracing::warn!("[AUTH] Failed to fetch connected accounts: {}", e);
                        false
                    }
                }
            }
        };

        // If no active connection, trigger OAuth flow proactively
        if !has_active_connection {
            tracing::warn!(
                "[OAUTH TRIGGER] No active connection for toolkit '{}'. Initiating auth flow.",
                tk_slug
            );

            match client.reconnect_toolkit(tk_slug).await {
                Ok(result_msg) => {
                    if result_msg.contains("Authentication successful") {
                        tracing::info!("[AUTH] 5-point reconnect successful, proceeding with tool execution.");
                        // FALLTHROUGH: Don't return, let the code proceed to step 2 (URL construction & execution)
                    } else {
                        let url = result_msg
                            .split_whitespace()
                            .last()
                            .unwrap_or(&result_msg)
                            .to_string();
                        let redirect_url = if url.starts_with("http") {
                            url
                        } else {
                            result_msg.clone()
                        };
                        return Ok(ToolExecuteResponse {
                            data: serde_json::json!({ "redirectUrl": redirect_url }),
                            error: Some(format!("Authentication required. {}", result_msg)),
                            successful: false,
                            log_id: None,
                            session_info: None,
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("[AUTH] 5-point reconnect failed: {}", e);
                    return Ok(ToolExecuteResponse {
                        data: serde_json::Value::Null,
                        error: Some(format!(
                            "No active connection for toolkit '{}' and failed to start auth: {}",
                            tk_slug, e
                        )),
                        successful: false,
                        log_id: None,
                        session_info: None,
                    });
                }
            }
        }
    }

    // 2. Build URL AFTER resolving the correct user_id
    // Uses the standardized build_mcp_url which handles Double-MCP and user_id mapping
    let url = client.build_mcp_url("");

    // 3. Prepare Request Properties (STRICT MCP PROXY payload)
    // We rely exclusively on the URL query string for user_id routing.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": "1",
        "params": {
            "name": slug,
            "arguments": arguments
        }
    });

    // Log the request for debugging
    tracing::debug!("Executing tool {} at {}", slug, url);
    let request_body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
    tracing::debug!("Request body: {}", request_body_str);

    // Write request to debug file
    let req_filename = format!("composio_exec_req_{}.json", slug);
    if let Err(e) = write_to_debug_file(&req_filename, &request_body_str) {
        tracing::warn!("Failed to write request debug file: {}", e);
    }

    let response = client
        .client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .query(&[("user_id", &user_id)]) // Keep query param for Proxy routing
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();

    // Handle 401/403 at the HTTP level immediately
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // HTTP 401/403 = deterministic auth failure - trigger managed flow
        if let Some(tk_slug) = &toolkit_slug {
            tracing::info!(
                "[AUTH] HTTP {} for toolkit '{}', triggering connection flow",
                status.as_u16(),
                tk_slug
            );
            match client.reconnect_toolkit(tk_slug).await {
                Ok(result_msg) => {
                    if result_msg.contains("Authentication successful") {
                        return Ok(ToolExecuteResponse {
                            data: serde_json::Value::Null,
                            error: Some(
                                "Authentication successful! Please try the tool again.".to_string(),
                            ),
                            successful: false,
                            log_id: None,
                            session_info: None,
                        });
                    } else {
                        let url_candidate = result_msg
                            .split_whitespace()
                            .last()
                            .unwrap_or(&result_msg)
                            .to_string();
                        let redirect_url = if url_candidate.starts_with("http") {
                            url_candidate
                        } else {
                            result_msg.clone()
                        };
                        return Ok(ToolExecuteResponse {
                            data: serde_json::json!({ "redirectUrl": redirect_url }),
                            error: Some(format!("Authentication required. {}", result_msg)),
                            successful: false,
                            log_id: None,
                            session_info: None,
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("[AUTH] 5-point reconnect failed for HTTP {}: {}", status.as_u16(), e);
                    return Err(format!(
                        "Authentication required but connection failed: {}",
                        e
                    ));
                }
            }
        }
    }

    if !status.is_success() {
        match response.error_for_status() {
            Ok(_) => unreachable!(),
            Err(e) => return Err(e.to_string()),
        }
    }

    let response_text = response.text().await.map_err(|e| e.to_string())?;
    tracing::trace!(
        "Raw tool execution response body len: {}",
        response_text.len()
    );

    let resp_filename = format!("composio_exec_resp_{}.txt", slug);
    let _ = write_to_debug_file(&resp_filename, &response_text);

    // Handle SSE response - Simple stripping
    let trimmed_response = response_text.trim();
    let json_text = if trimmed_response.contains("data:") {
        tracing::debug!("Detected SSE format response");
        // Simple heuristic to grab the payload after the first data: marker
        // This is safer than the complex multi-line parsing we tried earlier
        let parts: Vec<&str> = response_text.split("data:").collect();
        if parts.len() > 1 {
            parts.last().unwrap_or(&"").trim()
        } else {
            trimmed_response
        }
    } else {
        &response_text
    };

    // 1. Try to parse as ToolExecuteResponse directly (The Simple Path)
    let json_value: Value = match serde_json::from_str::<ToolExecuteResponse>(json_text) {
        Ok(result) => {
            tracing::debug!("Successfully parsed ToolExecuteResponse directly");
            // Convert back to Value for unified auth checking, or just use it
            // We'll hydrate a Value from it to reuse the auth logic below
            serde_json::to_value(result).unwrap_or(Value::Null)
        }
        Err(_) => {
            // 2. Fallback: Parse as generic JSON
            match serde_json::from_str::<Value>(json_text) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Failed to parse response as JSON: {}", e);
                    return Ok(ToolExecuteResponse {
                        data: Value::String(response_text), // Return raw text as data
                        error: Some(format!("Failed to parse JSON response: {}", e)),
                        successful: false,
                        log_id: None,
                        session_info: None,
                    });
                }
            }
        }
    };

    // Compatibility Normalization:
    // If the parsed value IS a ToolExecuteResponse structure (has "successful" and "data" fields),
    // we use it. If not (it's just a raw result object), we wrap it.
    // This handles cases where the API returns the result directly vs wrapped.
    let (mut successful, data, mut error_msg) =
        if let Some(success) = json_value.get("successful").and_then(|b| b.as_bool()) {
            let d = json_value.get("data").cloned().unwrap_or(Value::Null);
            let e = json_value
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            (success, d, e)
        } else {
            // Assume success if no explicit error field, wrap the whole things as data
            let e = json_value
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            (e.is_empty(), json_value.clone(), e)
        };

    // Extract log_id and session_info for UI observability
    let log_id = json_value
        .get("log_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let session_info = json_value.get("session_info").cloned();

    // ----------------------------------------------------------------
    // AUTH DETECTION — delegates to ToolExecuteResponse::is_auth_error()
    // ----------------------------------------------------------------
    // Build a temporary response to use the shared detection method.
    // The final ToolExecuteResponse is returned at the bottom; here we
    // only need the bool to decide whether to bust the cache.
    let temp_response = ToolExecuteResponse {
        data: data.clone(),
        error: if error_msg.is_empty() { None } else { Some(error_msg.clone()) },
        successful,
        log_id: None,
        session_info: None,
    };
    let mut needs_auth = temp_response.is_auth_error();

    // EXTENSION: Double-MCP `result.content[].text` check.
    // This operates on the raw JSON-RPC envelope (`json_value`), NOT on the
    // deserialized ToolExecuteResponse — a different lifecycle stage.
    // The MCP proxy sometimes wraps errors inside a JSON-RPC result object.
    if !needs_auth {
        if let Some(result) = json_value.get("result") {
            if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            // Try to parse the nested text as JSON for deterministic signals
                            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                                let inner_response = ToolExecuteResponse {
                                    data: inner,
                                    error: Some(text.to_string()),
                                    successful: false,
                                    log_id: None,
                                    session_info: None,
                                };
                                if inner_response.is_auth_error() {
                                    needs_auth = true;
                                    error_msg = text.to_string();
                                    successful = false;
                                    break;
                                }
                            }
                            // FALLBACK: Substring matching for unstructured errors
                            if !needs_auth
                                && (text.contains("No connected account")
                                    || text.contains("valid connection not found"))
                            {
                                tracing::info!(
                                    "[AUTH] Detected MCP Protocol error (substring match): {}",
                                    text
                                );
                                needs_auth = true;
                                error_msg = text.to_string();
                                successful = false;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Auth recovery is handled by `try_auth_recovery` in manager.rs which has access to
    // the full 5-point connection lifecycle (auth config lookup, OAuth, server patching,
    // tool selection, reload). We just pass the raw error through with clear logging.
    if needs_auth {
        if let Some(tk_slug) = &toolkit_slug {
            tracing::warn!(
                "[AUTH] Auth error detected for toolkit '{}'. Raw error will propagate to manager.rs for 5-point recovery.",
                tk_slug
            );
            // Defense-in-depth: bust the stale toolkit_account_map EAGERLY here
            // so that even if the manager-level recovery doesn't fire (e.g. tool
            // called via COMPOSIO_EXECUTE_TOOL path), the next execute_tool call
            // won't short-circuit on a cached ACTIVE entry.
            // `reconnect_toolkit` in mod.rs performs the same bust during its
            // Step 3 — both locations are intentional. (Review: 2026-02-21)
            if let Ok(mut map) = client.toolkit_account_map.write() {
                if map.remove(tk_slug).is_some() {
                    tracing::info!("[AUTH] Cleared stale '{}' from toolkit_account_map", tk_slug);
                }
            }
        }
    }

    Ok(ToolExecuteResponse {
        data,
        error: if error_msg.is_empty() {
            None
        } else {
            Some(error_msg)
        },
        successful,
        log_id,
        session_info,
    })
}

