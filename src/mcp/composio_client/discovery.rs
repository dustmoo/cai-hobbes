use super::ComposioClient;
use super::models::*;
use super::utils::write_to_debug_file;
use super::meta::get_meta_tools;


/// List all available tools from Composio (via MCP Protocol endpoint)
pub async fn list_tools(client: &ComposioClient) -> Result<Vec<ComposioTool>, String> {
    // Use the MCP Protocol endpoint - returns only tools configured for this specific server
    // CRITICAL: Do NOT include x-api-key header - the URL itself (containing server UUID) is the auth
    // See KI troubleshooting item #58 for details on why x-api-key causes 401 errors here
    
    let mcp_url = client.build_mcp_url("");
    
    tracing::debug!("Fetching tools via MCP Protocol from {}", mcp_url);
    
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": "1",
        "params": {}
    });
    
    let response = client.client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        // No x-api-key header - MCP URL is the authentication (see troubleshooting #58)
        .json(&request)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
         let status = response.status();
         let text = response.text().await.unwrap_or_default();
         tracing::error!("Failed to fetch tools via MCP Protocol. Status: {}, Body: {}", status, text);
         return Err(format!("MCP Protocol error ({}): {}", status, text));
    }
    
    tracing::debug!("Got MCP Protocol response with status: {}", response.status());
    
    // Get the response body as text
    let response_text = response.text().await.map_err(|e| e.to_string())?;
    
    // Write the response to a file for detailed analysis
    if let Err(e) = write_to_debug_file("composio_tools_response.json", &response_text) {
        tracing::error!("Failed to write response to debug file: {}", e);
    } else {
        tracing::debug!("Wrote response to debug_logs/composio_tools_response.json");
    }
    
    // Parse tool response and cache - parse_tools_response handles SSE format
    match parse_tools_response(client, &response_text) {
        Ok(tools) => {
            tracing::trace!("Loaded {} tools from MCP Protocol", tools.len());
            cache_tools(client, &tools);
            Ok(tools)
        },
        Err(e) => Err(e),
    }
}

/// Get information about connected toolkits for UI display
/// 
/// IMPORTANT: This function derives toolkit info purely from the MCP `tools/list` response,
/// which is scoped to the active MCP server. It does NOT use the REST `list_connected_accounts`
/// endpoint, which returns global accounts across all profiles.
/// 
/// See COMPOSIO_ENDPOINTS.md mandate: "MCP-First for Status"
/// 
/// NOTE: Results are cached. Call `invalidate_toolkit_cache()` on profile change.
pub async fn list_connected_toolkits(client: &ComposioClient) -> Result<Vec<ToolkitInfo>, String> {
    // Return cached data if available
    if let Some(cached) = client.get_cached_toolkit_info() {
        tracing::debug!("Returning {} cached toolkit infos", cached.len());
        return Ok(cached);
    }
    
    // MCP-First: Get tools from MCP endpoint (profile-scoped)
    let all_tools = match list_tools(client).await {
        Ok(tools) => tools,
        Err(e) => return Err(format!("Failed to list tools: {}", e)),
    };
    
    // Aggregate tools by toolkit slug
    let mut toolkit_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    
    for tool in &all_tools {
        // Get toolkit slug from tool metadata or infer from name prefix
        let slug = tool.toolkit.as_ref().map(|tk| tk.slug.to_lowercase())
            .or_else(|| tool.app.as_ref().map(|a| a.slug.to_lowercase()))
            .or_else(|| {
                // Infer from tool name: TOOLKIT_ACTION -> toolkit
                tool.name.split('_').next().map(|s| s.to_lowercase())
            });
        
        if let Some(s) = slug {
            *toolkit_map.entry(s).or_insert(0) += 1;
        }
    }
    
    // Build ToolkitInfo from aggregated data
    let mut toolkit_infos: Vec<ToolkitInfo> = toolkit_map.into_iter().map(|(slug, tool_count)| {
        // Format display name: "slack" -> "Slack", "news_api" -> "News_api"
        let display_name = slug.chars().next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default() + &slug[1..];
        
        ToolkitInfo {
            slug,
            display_name,
            tool_count,
            is_connected: true, // All tools from MCP are connected by definition
        }
    }).collect();
    
    // Sort for consistent UI ordering
    toolkit_infos.sort_by(|a, b| a.slug.cmp(&b.slug));
    
    // Cache the result
    client.set_cached_toolkit_info(toolkit_infos.clone());
    tracing::debug!("Cached {} toolkit infos", toolkit_infos.len());
    
    Ok(toolkit_infos)
}

/// List all available toolkits from Composio (for marketplace discovery)
pub async fn list_all_toolkits(
    client: &ComposioClient,
    search: Option<&str>,
    cursor: Option<&str>,
    limit: Option<i32>,
    categories: Option<Vec<String>>,
    sort_by: Option<&str>,
) -> Result<(Vec<ComposioToolkitListing>, i32, Option<String>), String> {
    // Use the fixed marketplace API base URL for toolkit listings (refer to constants)
    let mut url = format!("{}/toolkits", super::constants::MARKETPLACE_API_BASE);
    
    // Build query parameters
    let mut params = Vec::new();
    if let Some(q) = search {
        if !q.is_empty() {
            params.push(format!("search={}", urlencoding::encode(q)));
        }
    }
    if let Some(cats) = categories {
        for cat in cats {
            if !cat.is_empty() {
                params.push(format!("category={}", urlencoding::encode(&cat)));
            }
        }
    }
    // Use cursor for pagination (not page number)
    if let Some(c) = cursor {
        if !c.is_empty() {
            params.push(format!("cursor={}", urlencoding::encode(c)));
        }
    }
    if let Some(l) = limit {
        params.push(format!("limit={}", l));
    } else {
        // Default to 20 per page
        params.push("limit=20".to_string());
    }
    // Add sort parameter (defaults to "usage" on the API side)
    if let Some(s) = sort_by {
        if !s.is_empty() {
            params.push(format!("sortBy={}", urlencoding::encode(s)));
        }
    }
    
    if !params.is_empty() {
        url = format!("{}?{}", url, params.join("&"));
    }
    
    tracing::debug!("Fetching all toolkits from: {}", url);
    
    let response = client.client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch toolkits: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Failed to fetch toolkits: {} - {}", status, body));
    }
    
    let response_text = response.text().await
        .map_err(|e| format!("Failed to read toolkits response: {}", e))?;
    
    // Log response for debugging
    if let Err(e) = write_to_debug_file("composio_toolkits.json", &response_text) {
        tracing::warn!("Failed to write toolkits debug file: {}", e);
    }
    
    // Parse the response
    let parsed: ToolkitListResponse = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse toolkits response: {}", e))?;
        
    let total_pages = parsed.total_pages.unwrap_or(1);
    let current_page = parsed.current_page.unwrap_or(1);
    
    tracing::trace!("Fetched {} toolkits (page {} of {}, next_cursor: {:?})", 
        parsed.items.len(), 
        current_page,
        total_pages,
        parsed.next_cursor
    );
    
    Ok((parsed.items, total_pages, parsed.next_cursor))
}

/// List all available toolkit categories from Composio
pub async fn list_toolkit_categories(client: &ComposioClient) -> Result<Vec<ComposioCategory>, String> {
    // Use the fixed marketplace API base URL for categories (refer to constants)
    let url = format!("{}/toolkits/categories", super::constants::MARKETPLACE_API_BASE);
    
    tracing::debug!("Fetching toolkit categories from: {}", url);
    
    let response = client.client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch categories: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Failed to fetch categories: {} - {}", status, body));
    }
    
    let response_text = response.text().await
        .map_err(|e| format!("Failed to read categories response: {}", e))?;
    
    // Log response for debugging
    if let Err(e) = write_to_debug_file("composio_categories.json", &response_text) {
        tracing::warn!("Failed to write categories debug file: {}", e);
    }

    #[derive(serde::Deserialize)]
    struct CategoriesResponse {
        items: Vec<ComposioCategory>,
    }
    
    let parsed: CategoriesResponse = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse categories response: {}", e))?;
    
    Ok(parsed.items)
}

/// Get the set of connected toolkit slugs
/// 
/// IMPORTANT: This function derives toolkit slugs from the MCP `tools/list` response,
/// which is scoped to the active MCP server. See COMPOSIO_ENDPOINTS.md mandate.
pub async fn get_connected_toolkit_slugs(client: &ComposioClient) -> Result<std::collections::HashSet<String>, String> {
    // MCP-First: Get tools from MCP endpoint (profile-scoped)
    let tools = list_tools(client).await?;
    
    let slugs: std::collections::HashSet<String> = tools
        .iter()
        .filter_map(|tool| {
            tool.toolkit.as_ref().map(|t| t.slug.to_lowercase())
                .or_else(|| tool.app.as_ref().map(|a| a.slug.to_lowercase()))
                .or_else(|| {
                    // Infer from tool name: TOOLKIT_ACTION -> toolkit
                    tool.name.split('_').next().map(|s| s.to_lowercase())
                })
        })
        .collect();
    Ok(slugs)
}

/// Fetch all tool slugs for a specific toolkit from Composio API.
pub async fn get_toolkit_tools(client: &ComposioClient, toolkit_slug: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/tools/enum", client.get_api_base_url());
    
    tracing::debug!("Fetching tools enum to filter for toolkit '{}'", toolkit_slug);
    
    let response = client.client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch tools enum: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Failed to fetch tools enum ({}): {}", status, body));
    }
    
    let all_tools: Vec<String> = response.json().await
        .map_err(|e| format!("Failed to parse tools enum response: {}", e))?;
    
    // Filter tools by toolkit prefix convention: TOOLKIT_SLUG_UPPERCASE_*
    let prefix = format!("{}_", toolkit_slug.to_uppercase());
    let filtered_tools: Vec<String> = all_tools.into_iter()
        .filter(|tool| tool.starts_with(&prefix))
        .collect();
    
    tracing::info!("Found {} tools for toolkit '{}' (prefix: {})", filtered_tools.len(), toolkit_slug, prefix);
    
    Ok(filtered_tools)
}

/// Fetch tools for a toolkit with descriptions for LLM-based selection.
pub async fn get_toolkit_tools_detailed(client: &ComposioClient, toolkit_slug: &str) -> Result<Vec<(String, Option<String>)>, String> {
    // NOTE: The /tools?appNames= endpoint has issues returning wrong toolkit's tools.
    // Instead, use /tools/enum which correctly returns all tool slugs, then filter by prefix.
    let url = format!("{}/tools/enum", client.get_api_base_url());
    
    tracing::debug!("Fetching tools enum for detailed list, filtering for toolkit '{}'", toolkit_slug);
    
    let response = client.client
        .get(&url)
        .header("x-api-key", &client.api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch tools enum: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Failed to fetch tools enum ({}): {}", status, body));
    }
    
    let all_tools: Vec<String> = response.json().await
        .map_err(|e| format!("Failed to parse tools enum response: {}", e))?;
    
    // Filter tools by toolkit prefix convention: TOOLKIT_SLUG_UPPERCASE_*
    let prefix = format!("{}_", toolkit_slug.to_uppercase());
    let filtered_tools: Vec<(String, Option<String>)> = all_tools.into_iter()
        .filter(|tool| tool.starts_with(&prefix))
        .map(|tool| (tool, None)) // No description available from enum endpoint
        .collect();
    
    tracing::info!("Found {} tools for toolkit '{}' (prefix: {})", filtered_tools.len(), toolkit_slug, prefix);
    
    Ok(filtered_tools)
}


/// List tools, optionally filtering by specific toolkit slugs
pub async fn list_tools_filtered(client: &ComposioClient, toolkit_filter: Option<&[String]>) -> Result<Vec<ComposioTool>, String> {
    let all_tools = match list_tools(client).await {
        Ok(tools) => tools,
        Err(e) => return Err(format!("Failed to list tools: {}", e)),
    };

    let Some(filter_slugs) = toolkit_filter else {
        return Ok(all_tools);
    };

    if filter_slugs.is_empty() {
        return Ok(all_tools);
    }

    // Filter tools to only those from specified toolkits
    let filtered: Vec<ComposioTool> = all_tools.into_iter().filter(|tool| {
        let tool_toolkit = tool.toolkit.as_ref().map(|tk| &tk.slug)
            .or(tool.app.as_ref().map(|a| &a.slug));
        
        // Check against explicit toolkit field first
        if let Some(tk) = tool_toolkit {
            return filter_slugs.iter().any(|slug| tk.eq_ignore_ascii_case(slug));
        }
        
        // Fallback: check if tool name starts with any of the filter slugs
        // e.g., "NEWS_API_GET_HEADLINES" starts with "NEWS_API_" for slug "news_api"
        let tool_name_upper = tool.name.to_uppercase();
        filter_slugs.iter().any(|slug| {
            let prefix = format!("{}_", slug.to_uppercase().replace("-", "_"));
            tool_name_upper.starts_with(&prefix)
        })
    }).collect();

    Ok(filtered)
}

/// Search for tools matching a natural language query within specified toolkits
pub async fn search_tools(client: &ComposioClient, query: &str, toolkit_slugs: &[String]) -> Result<Vec<ComposioTool>, String> {
    let url = client.build_mcp_url("");
    
    tracing::info!("Searching tools via MCP: query='{}', toolkits={:?}", query, toolkit_slugs);
    
    let mut params = serde_json::Map::new();
    params.insert("search".to_string(), serde_json::Value::String(query.to_string()));
    
    if !toolkit_slugs.is_empty() {
        let uppercase_slugs: Vec<serde_json::Value> = toolkit_slugs
            .iter()
            .map(|s| serde_json::Value::String(s.to_uppercase()))
            .collect();
        params.insert("toolkits".to_string(), serde_json::Value::Array(uppercase_slugs));
    }
    
    let json_rpc_request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": "search_tools",
        "params": params
    });
    
    let response = client.client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&json_rpc_request)
        .send()
        .await
        .map_err(|e| format!("Search request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Search failed with status {}: {}", status, body));
    }

    let response_text = response.text().await
        .map_err(|e| format!("Failed to read search response: {}", e))?;
    
    match parse_tools_response(client, &response_text) {
        Ok(tools) => {
            tracing::info!("Search returned {} tools", tools.len());
            Ok(tools)
        },
        Err(e) => Err(format!("Failed to parse search response: {}", e)),
    }
}

/// List tools for a session with Tool Router pattern
pub async fn list_tools_for_session(client: &ComposioClient, force_load_slugs: &[String]) -> Result<Vec<ComposioTool>, String> {
    let mut tools = Vec::new();
    
    // 1. Add meta-tools for on-demand discovery (these are always included)
    tools.extend(get_meta_tools());
    
    // 2. Only add tools from force-loaded toolkits
    if !force_load_slugs.is_empty() {
        tracing::trace!("Loading tools from force-loaded toolkits: {:?}", force_load_slugs);
        let force_loaded = list_tools_filtered(client, Some(force_load_slugs)).await?;
        tracing::trace!("Loaded {} tools from force-loaded toolkits", force_loaded.len());
        tools.extend(force_loaded);
    } else {
        tracing::info!("No force-loaded toolkits configured - only meta-tools available");
    }
    
    tracing::trace!("Total tools for session: {} (including {} meta-tools)", tools.len(), 2);
    Ok(tools)
}

fn cache_tools(client: &ComposioClient, tools: &[ComposioTool]) {
    let mut map = client.tool_toolkit_map.write().unwrap();
    // eprintln!("DEBUG: caching tools, count: {}", tools.len());
    for tool in tools {
        if let Some(toolkit) = &tool.toolkit {
            // Preserve toolkit slug casing as returned from API
            map.insert(tool.name.clone(), toolkit.slug.clone());
            if let Some(slug) = &tool.slug {
                map.insert(slug.clone(), toolkit.slug.clone());
            }
        } else if let Some(app) = &tool.app {
            map.insert(tool.name.clone(), app.slug.clone());
            if let Some(slug) = &tool.slug {
                map.insert(slug.clone(), app.slug.clone());
            }
        } else {
             // Fallback: Try to infer toolkit slug from tool name
             let inferred_slug = tool.name.split('_').next().map(|s| s.to_lowercase());
             if let Some(slug) = inferred_slug {
                 tracing::debug!("Inferred toolkit '{}' for tool '{}'", slug, tool.name);
                 map.insert(tool.name.clone(), slug.clone());
                 if let Some(tool_slug) = &tool.slug {
                     map.insert(tool_slug.clone(), slug);
                 }
             } else {
                 tracing::warn!("Could not determine toolkit for tool {}", tool.name);
             }
        }
    }
    tracing::debug!("Cached {} tool-to-toolkit mappings", map.len());
}

fn parse_tools_response(_client: &ComposioClient, response_text: &str) -> Result<Vec<ComposioTool>, String> {
    // Check if the response is in SSE format
    let trimmed_response = response_text.trim();
    if trimmed_response.starts_with("event:") || trimmed_response.starts_with("data:") {
        tracing::debug!("Detected SSE format response");
        
        let data_start = response_text.find("data:").unwrap_or(0) + "data:".len();
        let json_text = response_text[data_start..].trim();
        
        match serde_json::from_str::<serde_json::Value>(json_text) {
            Ok(json_value) => {
                if json_value.is_object() {
                    let json_obj = json_value.clone();
                    if json_obj.get("jsonrpc").is_some() {
                        if let Ok(rpc_response) = serde_json::from_value::<JsonRpcResponse<Vec<ComposioTool>>>(json_obj.clone()) {
                            if let Some(error) = rpc_response.error {
                                return Err(format!("Composio API error: {}", error.message));
                            }
                            if let Some(tools) = rpc_response.result {
                                return Ok(tools);
                            }
                        }
                    }
                    if let Some(result) = json_obj.get("result") {
                         if let Some(tools) = result.get("tools").and_then(|t| serde_json::from_value::<Vec<ComposioTool>>(t.clone()).ok()) {
                             return Ok(tools);
                         }
                    }
                    if let Some(tools) = json_obj.get("tools").and_then(|t| serde_json::from_value::<Vec<ComposioTool>>(t.clone()).ok()) {
                        return Ok(tools);
                    }
                } else if json_value.is_array() {
                    if let Ok(tools) = serde_json::from_value::<Vec<ComposioTool>>(json_value) {
                         return Ok(tools);
                    }
                }
            },
            Err(e) => return Err(e.to_string()),
        }
        return Err("Failed to parse Composio SSE response into a usable format".to_string());
    } else {
        // Regular JSON
        match serde_json::from_str::<serde_json::Value>(response_text) {
             Ok(json_value) => {
                 if json_value.is_object() {
                     let json_obj = json_value.clone();
                     if json_obj.get("jsonrpc").is_some() {
                        if let Ok(rpc_response) = serde_json::from_value::<JsonRpcResponse<Vec<ComposioTool>>>(json_obj.clone()) {
                            if let Some(error) = rpc_response.error {
                                return Err(format!("Composio API error: {}", error.message));
                            }
                            if let Some(tools) = rpc_response.result {
                                return Ok(tools);
                            }
                        }
                     }
                     if let Some(result) = json_obj.get("result") {
                         if let Some(tools) = result.get("tools").and_then(|t| serde_json::from_value::<Vec<ComposioTool>>(t.clone()).ok()) {
                             return Ok(tools);
                         }
                    }
                    if let Some(tools) = json_obj.get("tools").and_then(|t| serde_json::from_value::<Vec<ComposioTool>>(t.clone()).ok()) {
                        return Ok(tools);
                    }
                    if let Some(items) = json_obj.get("items").and_then(|t| serde_json::from_value::<Vec<ComposioTool>>(t.clone()).ok()) {
                        return Ok(items);
                    }
                    // Legacy fallback
                    if let Ok(tools_response) = serde_json::from_value::<ToolListResponse>(json_obj) {
                         let tools = tools_response.get_all_tools();
                         if !tools.is_empty() { return Ok(tools); }
                    }
                 } else if json_value.is_array() {
                      if let Ok(tools) = serde_json::from_value::<Vec<ComposioTool>>(json_value) {
                           return Ok(tools);
                      }
                 }
             },
             Err(e) => return Err(e.to_string()),
        }
        Err("Failed to parse Composio response into a usable format".to_string())
    }
}
