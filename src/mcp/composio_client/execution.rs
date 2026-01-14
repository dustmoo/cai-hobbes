use super::ComposioClient;
use super::models::*;
use super::utils::write_to_debug_file;
use super::discovery::get_toolkit_tools;
use super::auth::{initiate_connection, list_auth_configs, list_connected_accounts};
use serde_json::Value;

/// Add a toolkit to the MCP server configuration via PATCH API.
pub async fn add_toolkit_to_server(client: &ComposioClient, toolkit_slug: &str, auth_config_id: &str, selected_tools: Option<Vec<String>>) -> Result<(), String> {
    // Extract server ID from base_url
    // base_url format: "https://backend.composio.dev/v3/mcp/{server_id}/mcp" or with query params
    let server_id = client.base_url
        .split("/mcp/")
        .nth(1)
        .map(|s| s.split('?').next().unwrap_or(s))
        .map(|s| s.trim_end_matches("/mcp"))  // Handle .../v3/mcp/{uuid}/mcp format
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Cannot extract server ID from base_url. Expected format: .../v3/mcp/{server_id}/mcp".to_string())?;
    
    tracing::info!("Adding toolkit '{}' with auth_config '{}' to MCP server '{}' (pre-selected: {})", 
        toolkit_slug, auth_config_id, server_id, selected_tools.is_some());
    
    // First, get current server config to preserve existing toolkits
    // API endpoint: GET /api/v3/mcp/{server_id} (NOT /mcp/servers/{server_id})
    let get_url = format!("{}/mcp/{}", client.get_api_base_url(), server_id);
    tracing::debug!("GET MCP server config from: {}", get_url);
    
    let get_response = client.client
        .get(&get_url)
        .header("x-api-key", &client.api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to get MCP server config: {}", e))?;
    
    if !get_response.status().is_success() {
        let status = get_response.status();
        let text = get_response.text().await.unwrap_or_default();
        return Err(format!("Failed to get MCP server ({}): {}", status, text));
    }
    
    let server_json: Value = get_response.json().await
        .map_err(|e| format!("Failed to parse server response: {}", e))?;
    
    // Get existing toolkits as objects (API expects array of {toolkit, auth_config} objects)
    // Per docs: toolkits=[{"toolkit": "gmail", "auth_config": "ac_xyz123"}, ...]
    let toolkits: Vec<Value> = server_json
        .get("toolkits")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().cloned().collect())
        .unwrap_or_default();
    
    // NOTE: Do NOT accumulate auth_config_ids - this causes 400 errors when old IDs become stale.
    // The API manages auth_config associations at the toolkit level, not server-wide.
    // Anti-pattern: Merging all old auth_config_ids and sending them in PATCH.
    let _ = auth_config_id; // Used in logging only now
    
    // Check if toolkit already exists (handle both string and object formats from API)
    let normalized_slug = toolkit_slug.to_lowercase();
    let toolkit_already_exists = toolkits.iter().any(|t| {
        // Handle object format {"toolkit": "..."} from API response
        if let Some(obj_slug) = t.get("toolkit").and_then(|s| s.as_str()) {
            return obj_slug.to_lowercase() == normalized_slug;
        }
        // Handle string format "toolkit_name"
        if let Some(str_slug) = t.as_str() {
            return str_slug.to_lowercase() == normalized_slug;
        }
        false
    });
    
    // Get existing allowed_tools from the server config
    let mut custom_tools: Vec<String> = server_json
        .get("allowed_tools")
        .or_else(|| server_json.get("custom_tools"))
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    
    // Determine which tools to add: use pre-selected or fetch all
    let tools_added = if let Some(pre_selected) = selected_tools {
        // Use pre-selected tools (from LLM smart selection)
        let mut added = 0;
        for tool in pre_selected {
            if !custom_tools.contains(&tool) {
                custom_tools.push(tool);
                added += 1;
            }
        }
        tracing::info!("Using {} pre-selected tools for toolkit '{}' (total: {})", added, toolkit_slug, custom_tools.len());
        added
    } else {
        // Fetch all tools for the toolkit and add any missing ones
        match get_toolkit_tools(client, toolkit_slug).await {
            Ok(new_tools) => {
                let mut added = 0;
                for tool in new_tools {
                    if !custom_tools.contains(&tool) {
                        custom_tools.push(tool);
                        added += 1;
                    }
                }
                tracing::info!("Auto-enabling {} new tools for toolkit '{}' (total: {})", added, toolkit_slug, custom_tools.len());
                added
            }
            Err(e) => {
                // Log warning but continue - tools can be added manually later
                tracing::warn!("Could not auto-fetch tools for toolkit '{}': {}", toolkit_slug, e);
                0
            }
        }
    };
    
    // Skip PATCH if no changes needed (toolkit exists and no new tools)
    if toolkit_already_exists && tools_added == 0 {
        tracing::info!("No changes needed for toolkit '{}' - all tools already present", toolkit_slug);
        return Ok(());
    }

    // Convert existing toolkits to objects to preserve/update auth_config
    let mut final_toolkits: Vec<serde_json::Value> = Vec::new();
    let mut found = false;
    
    for t in toolkits {
        let mut obj = if t.is_string() {
            serde_json::json!({ "toolkit": t.as_str().unwrap() })
        } else {
            t.clone()
        };
        
        // Check if this is the toolkit we are updating
        let slug = obj.get("toolkit").and_then(|s| s.as_str()).unwrap_or_default();
        if slug.eq_ignore_ascii_case(toolkit_slug) {
            // UPDATE: Set the correct auth_config_id
            obj["auth_config"] = serde_json::Value::String(auth_config_id.to_string());
            found = true;
        }
        
        final_toolkits.push(obj);
    }
    
    // If not found (new toolkit), add it
    if !found {
        final_toolkits.push(serde_json::json!({
            "toolkit": toolkit_slug,
            "auth_config": auth_config_id
        }));
        tracing::info!("Adding new toolkit '{}' to payload", toolkit_slug);
    }
    
    let patch_url = format!("{}/mcp/{}", client.get_api_base_url(), server_id);
    
    // NOTE: We now send 'toolkits' as objects to strictly bind the auth_config
    // according to the "One Auth Config per tool per MCP Server" rule.
    let patch_payload = serde_json::json!({
        "toolkits": final_toolkits,
        "allowed_tools": custom_tools
    });
    
    tracing::debug!("PATCH {} with payload: {:?}", patch_url, patch_payload);
    
    let patch_response = client.client
        .patch(&patch_url)
        .header("x-api-key", &client.api_key)
        .header("Content-Type", "application/json")
        .json(&patch_payload)
        .send()
        .await
        .map_err(|e| format!("Failed to update MCP server: {}", e))?;
    
    if !patch_response.status().is_success() {
        let status = patch_response.status();
        let text = patch_response.text().await.unwrap_or_default();
        return Err(format!("Failed to add toolkit to server ({}): {}", status, text));
    }
    
    // Step 4: Generate/register user with the MCP server
    // This is required for the user to see the tools
    // API: POST /api/v3/mcp/servers/generate
    if let Some(ref user_id) = client.user_id {
        let generate_url = format!("{}/mcp/servers/generate", client.get_api_base_url());
        let generate_payload = serde_json::json!({
            "user_id": user_id,
            "mcp_server_id": server_id
        });
        
        tracing::debug!("Registering user '{}' with MCP server '{}'", user_id, server_id);
        
        let generate_response = client.client
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
    
    tracing::info!("Successfully added toolkit '{}' with {} tools to MCP server", toolkit_slug, custom_tools.len());
    Ok(())
}

/// Execute a Composio tool via the MCP server.
pub async fn execute_tool(client: &ComposioClient, slug: &str, args: serde_json::Value) -> Result<ToolExecuteResponse, String> {
    // Ensure arguments are wrapped in an object
    let mut arguments = if args.is_object() {
        args
    } else {
        serde_json::json!({ "value": args })
    };

    // Determine the target user_id (prioritize Settings/Profile ID)
    let profile_user_id = client.user_id.clone()
        .or(client.entity_id.clone())
        .unwrap_or_else(|| "default".to_string());
        
    let user_id = profile_user_id.clone();

    // 1. Resolve toolkit slug (needed for manual connection initiation)
    let toolkit_slug = {
        let map = client.tool_toolkit_map.read().unwrap();
        map.get(slug).cloned()
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
                         tracing::info!("[CONTEXT] Injecting context param '{}' for toolkit '{}'", k, tk_slug);
                         obj.insert(k, serde_json::Value::String(v));
                     }
                 }
             }
        }
    }

    // PROACTIVE AUTH CHECK (restored from v0.9.4 pattern)
    // Check for an ACTIVE connected account for this toolkit BEFORE calling the proxy.
    // If no active connection, trigger initiate_connection immediately.
    if let Some(ref tk_slug) = toolkit_slug {
        tracing::debug!("[AUTH] Checking for active connection for toolkit '{}'", tk_slug);
        
        // Fetch connected accounts and check for ACTIVE status
        let has_active_connection = match list_connected_accounts(client).await {
            Ok(accounts) => {
                // Find an ACTIVE account for this toolkit
                let active = accounts.iter().find(|acc| {
                    let matches_toolkit = acc.toolkit.as_ref()
                        .map(|t| t.slug.eq_ignore_ascii_case(tk_slug))
                        .unwrap_or(false)
                        || acc.app_name.as_ref()
                            .map(|n| n.eq_ignore_ascii_case(tk_slug))
                            .unwrap_or(false);
                    let is_active = acc.status.eq_ignore_ascii_case("ACTIVE");
                    matches_toolkit && is_active
                });
                if let Some(acc) = active {
                    tracing::debug!("[AUTH] Found ACTIVE connection '{}' for toolkit '{}'", acc.id, tk_slug);
                    

                    
                    true
                } else {
                    tracing::info!("[AUTH] No ACTIVE connection found for toolkit '{}'. Available accounts: {:?}", 
                        tk_slug, 
                        accounts.iter()
                            .filter(|a| a.toolkit.as_ref().map(|t| t.slug.eq_ignore_ascii_case(tk_slug)).unwrap_or(false))
                            .map(|a| format!("{}:{}", a.id, a.status))
                            .collect::<Vec<_>>()
                    );
                    false
                }
            },
            Err(e) => {
                tracing::warn!("[AUTH] Failed to fetch connected accounts: {}", e);
                // Assume no connection to be safe
                false
            }
        };
        
        // If no active connection, trigger OAuth flow proactively
        if !has_active_connection {
            tracing::warn!("[OAUTH TRIGGER] No active connection for toolkit '{}'. Initiating auth flow.", tk_slug);
            
            match initiate_connection(client, tk_slug, &user_id).await {
                Ok(result_msg) => {
                    if result_msg.contains("Authentication successful") {
                        tracing::info!("[AUTH] Authentication successful, proceeding with tool execution immediately.");
                        // FALLTHROUGH: Don't return, let the code proceed to step 2 (URL construction & execution)
                        // This prevents the "Auth Loop" where a retry might hit a stale cache check.
                    } else {
                        let url = result_msg.split_whitespace().last().unwrap_or(&result_msg).to_string();
                        let redirect_url = if url.starts_with("http") { url } else { result_msg.clone() };
                        return Ok(ToolExecuteResponse {
                            data: serde_json::json!({ "redirectUrl": redirect_url }),
                            error: Some(format!("Authentication required. {}", result_msg)),
                            successful: false,
                            log_id: None,
                            session_info: None,
                        });
                    }
                },
                Err(e) => {
                    tracing::error!("[AUTH] Failed to initiate connection: {}", e);
                    return Ok(ToolExecuteResponse {
                        data: serde_json::Value::Null,
                        error: Some(format!("No active connection for toolkit '{}' and failed to start auth: {}", tk_slug, e)),
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

    let response = client.client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .query(&[("user_id", &user_id)]) // Keep query param for Proxy routing
        .json(&body)
        .send()
        .await.map_err(|e| e.to_string())?;
        
    let status = response.status();
    
    // Handle 401/403 at the HTTP level immediately
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
         // HTTP 401/403 = deterministic auth failure - trigger managed flow
         if let Some(tk_slug) = &toolkit_slug {
             tracing::info!("[AUTH] HTTP {} for toolkit '{}', triggering connection flow", status.as_u16(), tk_slug);
             match initiate_connection(client, tk_slug, &user_id).await {
                 Ok(result_msg) => {
                     if result_msg.contains("Authentication successful") {
                         return Ok(ToolExecuteResponse {
                             data: serde_json::Value::Null,
                             error: Some("Authentication successful! Please try the tool again.".to_string()),
                             successful: false,
                             log_id: None,
                             session_info: None,
                         });
                     } else {
                         let url_candidate = result_msg.split_whitespace().last().unwrap_or(&result_msg).to_string();
                         let redirect_url = if url_candidate.starts_with("http") { url_candidate } else { result_msg.clone() };
                         return Ok(ToolExecuteResponse {
                             data: serde_json::json!({ "redirectUrl": redirect_url }),
                             error: Some(format!("Authentication required. {}", result_msg)),
                             successful: false,
                             log_id: None,
                             session_info: None,
                         });
                     }
                 },
                 Err(e) => {
                     tracing::error!("Failed to initiate connection: {}", e);
                     return Err(format!("Authentication required but connection failed: {}", e));
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
    tracing::trace!("Raw tool execution response body len: {}", response_text.len());
    
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
        },
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
    let (mut successful, data, mut error_msg) = if let Some(success) = json_value.get("successful").and_then(|b| b.as_bool()) {
         let d = json_value.get("data").cloned().unwrap_or(Value::Null);
         let e = json_value.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_default();
         (success, d, e)
    } else {
         // Assume success if no explicit error field, wrap the whole things as data
         let e = json_value.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_default();
         (e.is_empty(), json_value.clone(), e)
    };
    
    // Extract log_id and session_info for UI observability
    let log_id = json_value.get("log_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let session_info = json_value.get("session_info").cloned();

    // ----------------------------------------------------------------
    // ROBUST AUTH DETECTION (Preserved from recent improvements)
    // ----------------------------------------------------------------
    let mut needs_auth = false;
    let mut is_hard_auth_failure = false; // Flag to override "soft" refresh signals
    
    // Check data.status_code
    if let Some(status_code) = data.get("status_code") {
        let code = status_code.as_u64()
            .or_else(|| status_code.as_i64().map(|i| i as u64));
        if code == Some(401) || code == Some(403) {
            tracing::info!("[AUTH] Detected status_code {} in data", code.unwrap());
            needs_auth = true;
            is_hard_auth_failure = true;
        }
    }
    
    // Check ECODEs
    if !needs_auth {
        if let Some(ecode) = data.get("ECODE").and_then(|v| v.as_str()) {
            if ecode.starts_with("AUTH_") || ecode.starts_with("OAUTH_") {
                tracing::info!("[AUTH] Detected ECODE {}", ecode);
                needs_auth = true;
                is_hard_auth_failure = true;
            }
        }
    }
    
    // Check http_error
    if !needs_auth {
         if let Some(http_error) = data.get("http_error").and_then(|v| v.as_str()) {
             if http_error.contains("401") || http_error.contains("403") {
                 tracing::info!("[AUTH] Detected http_error {}", http_error);
                 needs_auth = true;
                 is_hard_auth_failure = true;
             }
         }
    }

    // Check MCP-protocol Level Error (Double-MCP Pattern)
    // Format: { "result": { "content": [{ "type": "text", "text": "{\"ECODE\":\"OAUTH_018\",...}" }], "isError": true } }
    if !needs_auth {
        if let Some(result) = json_value.get("result") {
            if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                // Extract error message from content array
                if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            // FIRST: Try to parse the nested text as JSON for deterministic signals
                            // The text field often contains stringified JSON with ECODE and status_code
                            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                                // Check for ECODE in inner JSON (OAUTH_018, OAUTH_023, AUTH_018, etc.)
                                if let Some(ecode) = inner.get("ECODE").and_then(|v| v.as_str()) {
                                    if ecode.starts_with("AUTH_") || ecode.starts_with("OAUTH_") {
                                        tracing::info!("[AUTH] Detected ECODE {} in result.content[0].text", ecode);
                                        needs_auth = true;
                                        is_hard_auth_failure = true;
                                        error_msg = text.to_string();
                                        successful = false;
                                        break;
                                    }
                                }
                                // Check for status_code in inner JSON (direct field)
                                if !needs_auth {
                                    if let Some(status_code) = inner.get("status_code") {
                                        let code = status_code.as_u64()
                                            .or_else(|| status_code.as_i64().map(|i| i as u64));
                                        if code == Some(401) || code == Some(403) {
                                            tracing::info!("[AUTH] Detected status_code {} in result.content[0].text", code.unwrap());
                                            needs_auth = true;
                                            is_hard_auth_failure = true;
                                            error_msg = text.to_string();
                                            successful = false;
                                            break;
                                        }
                                    }
                                }
                                // Check for data.status_code in inner JSON (nested field)
                                if !needs_auth {
                                    if let Some(inner_data) = inner.get("data") {
                                        if let Some(status_code) = inner_data.get("status_code") {
                                            let code = status_code.as_u64()
                                                .or_else(|| status_code.as_i64().map(|i| i as u64));
                                            if code == Some(401) || code == Some(403) {
                                                tracing::info!("[AUTH] Detected data.status_code {} in result.content[0].text", code.unwrap());
                                                needs_auth = true;
                                                is_hard_auth_failure = true;
                                                error_msg = text.to_string();
                                                successful = false;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            // FALLBACK: Substring matching for unstructured errors
                            if !needs_auth && (text.contains("No connected account") || text.contains("valid connection not found")) {
                                tracing::info!("[AUTH] Detected MCP Protocol error (substring match): {}", text);
                                needs_auth = true;
                                is_hard_auth_failure = true;
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


    // Check auth_refresh_required (Critical for avoiding loops)
    // REGRESSION FIX: Only respect this flag if we check explicit HARD auth failure.
    // If we have an OAUTH_018 or 401, we MUST trigger auth regardless of what this flag says.
    if needs_auth && !is_hard_auth_failure {
        let auth_refresh = data.get("auth_refresh_required")
            .or_else(|| json_value.get("auth_refresh_required"));
        if let Some(refresh) = auth_refresh.and_then(|v| v.as_bool()) {
            if !refresh {
                tracing::info!("[AUTH] auth_refresh_required=false, ignoring auth error");
                needs_auth = false;
            }
        }
    }
    
    // Trigger managed flow if needed
    if needs_auth {
        if let Some(tk_slug) = &toolkit_slug {
             tracing::info!("[AUTH] Auth failure detected for toolkit '{}', initiating self-repair", tk_slug);
             
             // SELF-REPAIR: Hydrate auth_config_cache before triggering OAuth.
             // This ensures the cache is fresh for potential LLM retry scenarios.
             // If the issue was stale cache, the next execution may succeed without OAuth.
             if let Err(e) = list_auth_configs(client).await {
                 tracing::warn!("[AUTH] Failed to hydrate auth configs during self-repair: {}", e);
             } else {
                 tracing::info!("[AUTH] Successfully hydrated auth_config_cache");
             }
             
             // Now trigger the managed connection flow (opens browser for OAuth if needed)
             match initiate_connection(client, tk_slug, &user_id).await {
                 Ok(res) => {
                     if res.contains("Authentication successful") {
                         return Ok(ToolExecuteResponse {
                             data: Value::Null,
                             error: Some("Authentication successful! Please retry.".to_string()),
                             successful: false,
                             log_id: None,
                             session_info: None
                         });
                     }
                     let url = res.split_whitespace().last().unwrap_or(&res).to_string();
                     let redirect = if url.starts_with("http") { url } else { res.clone() };
                     
                     return Ok(ToolExecuteResponse {
                         data: serde_json::json!({ "redirectUrl": redirect }),
                         error: Some(format!("Authentication required. {}", res)),
                         successful: false,
                         log_id: None,
                         session_info: None
                     });
                 },
                 Err(e) => {
                     tracing::error!("Auth trigger failed: {}", e);
                 }
             }
        }
    }

    Ok(ToolExecuteResponse {
        data,
        error: if error_msg.is_empty() { None } else { Some(error_msg) },
        successful,
        log_id,
        session_info,
    })
}
