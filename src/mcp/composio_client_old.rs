use serde::{Deserialize, Serialize};
use serde_json::Value;
use rmcp::model::Tool;
use std::sync::Arc;
use std::fs::File;
use std::io::Write;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::mcp::oauth_flow::{
    find_available_port, start_callback_server, open_browser,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ComposioTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub toolkit: Option<ComposioToolkit>,
    pub app: Option<ComposioToolkit>, // Sometimes usage is 'app' instead of 'toolkit' in responses
    // Additional fields from the API
    pub slug: Option<String>,
    pub input_parameters: Option<Value>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Option<Value>,
    pub output_parameters: Option<Value>,
    pub tags: Option<Vec<String>>,
    pub version: Option<String>,
    pub available_versions: Option<Vec<String>>,
    #[serde(rename = "deprecated")]
    pub is_deprecated: Option<Value>,
    #[serde(rename = "no_auth")]
    pub is_no_auth: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ComposioToolkit {
    pub slug: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectedAccount {
    pub id: String,
    pub status: String,
    #[serde(alias = "userId", alias = "user_id")]
    pub user_id: Option<String>,
    #[serde(alias = "appName", alias = "app_name")]
    pub app_name: Option<String>,
    #[serde(alias = "providerId", alias = "provider_id")]
    pub provider_id: Option<String>,
    pub toolkit: Option<ConnectedAccountToolkit>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectedAccountToolkit {
    pub slug: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct ConnectedAccountsResponse {
    items: Vec<ConnectedAccount>,
}

/// Information about a Composio toolkit for UI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolkitInfo {
    /// Toolkit slug (e.g., "gmail", "clickup")
    pub slug: String,
    /// Human-readable display name
    pub display_name: String,
    /// Number of tools in this toolkit
    pub tool_count: usize,
    /// Whether the toolkit is connected (has authenticated account)
    pub is_connected: bool,
}

/// Nested metadata for a toolkit listing from the Composio API
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolkitMeta {
    /// Description of what this toolkit does
    #[serde(default)]
    pub description: Option<String>,
    /// Logo/icon URL
    #[serde(default)]
    pub logo: Option<String>,
    /// Number of tools in this toolkit
    #[serde(default)]
    pub tools_count: Option<usize>,
    /// Number of triggers in this toolkit
    #[serde(default)]
    pub triggers_count: Option<usize>,
    /// App URL (for external link)
    #[serde(default)]
    pub app_url: Option<String>,
}

/// A toolkit listing from the Composio API for marketplace display
/// This represents all available toolkits, not just connected ones
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioToolkitListing {
    /// Toolkit slug (e.g., "gmail", "linear", "github")
    pub slug: String,
    /// Human-readable name
    pub name: String,
    /// Nested metadata containing description, logo, counts, etc.
    #[serde(default)]
    pub meta: Option<ToolkitMeta>,
    /// Categories this toolkit belongs to (may be at top-level or in meta)
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Available authentication schemes for this toolkit (e.g., ["OAUTH2"], ["API_KEY"])
    #[serde(default)]
    pub auth_schemes: Option<Vec<String>>,
    /// Auth schemes that Composio can manage (for use_composio_managed_auth)
    #[serde(default)]
    pub composio_managed_auth_schemes: Option<Vec<String>>,
    /// True if toolkit requires no authentication
    #[serde(default)]
    pub no_auth: Option<bool>,
}

impl ComposioToolkitListing {
    /// Get the description, preferring the nested meta.description
    pub fn description(&self) -> Option<String> {
        self.meta.as_ref().and_then(|m| m.description.clone())
    }
    
    /// Get the logo URL from meta
    #[allow(dead_code)]
    pub fn logo(&self) -> Option<String> {
        self.meta.as_ref().and_then(|m| m.logo.clone())
    }
    
    /// Get the app URL from meta
    pub fn app_url(&self) -> Option<String> {
        self.meta.as_ref().and_then(|m| m.app_url.clone())
    }
    
    /// Get the tools count from meta
    pub fn tools_count(&self) -> Option<usize> {
        self.meta.as_ref().and_then(|m| m.tools_count)
    }
    
    /// Determine if Composio managed auth is available for this toolkit
    pub fn supports_managed_auth(&self) -> bool {
        self.composio_managed_auth_schemes
            .as_ref()
            .map(|schemes| !schemes.is_empty())
            .unwrap_or(false)
    }
    
    /// Get the primary auth scheme for this toolkit (uppercase, e.g., "OAUTH2", "API_KEY")
    pub fn primary_auth_scheme(&self) -> Option<String> {
        self.auth_schemes.as_ref()?.first().cloned()
    }
    
    /// Check if this toolkit requires no authentication
    #[allow(dead_code)]
    pub fn requires_no_auth(&self) -> bool {
        self.no_auth.unwrap_or(false)
    }
}

/// Response from GET /api/v3/toolkits
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolkitListResponse {
    #[serde(default)]
    pub items: Vec<ComposioToolkitListing>,
    #[serde(rename = "totalPages", alias = "total_pages", default)]
    pub total_pages: Option<i32>,
    #[serde(rename = "currentPage", alias = "current_page", default)]
    pub current_page: Option<i32>,
    #[serde(rename = "totalItems", alias = "total_items", default)]
    pub total_items: Option<i32>,
    #[serde(rename = "nextCursor", alias = "next_cursor", default)]
    pub next_cursor: Option<String>,
}

/// A category for toolkit filtering in the marketplace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioCategory {
    /// Category identifier - API returns this as 'id' but we use 'slug' internally
    #[serde(alias = "id")]
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "displayName", alias = "display_name", default)]
    pub display_name: Option<String>,
}

impl ComposioCategory {
    /// Get the display-friendly name for this category
    #[allow(dead_code)]
    pub fn display(&self) -> String {
        self.display_name.clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.slug.clone())
    }
}

#[derive(Clone)]

pub struct ComposioClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    entity_id: Option<String>,
    pub user_id: Option<String>,
    // Cache of tool name -> toolkit slug
    tool_toolkit_map: Arc<RwLock<HashMap<String, String>>>,
    // Cache of toolkit slug -> (connected_account_id, user_id)
    toolkit_account_map: Arc<RwLock<HashMap<String, (String, String)>>>,
    // Cache of toolkit slug -> auth_config_id for dynamic per-toolkit lookups
    auth_config_cache: Arc<RwLock<HashMap<String, String>>>,
}

/// Validate a Composio API key by making a lightweight API call
/// Returns Ok(()) if valid, Err with message if invalid
pub async fn validate_composio_api_key(api_key: &str) -> Result<(), String> {
    if api_key.trim().is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    let client = reqwest::Client::new();
    let url = format!("{}/toolkits?limit=1", ComposioClient::MARKETPLACE_API_BASE);
    
    match client
        .get(&url)
        .header("x-api-key", api_key)
        .header("Accept", "application/json")
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
                Err(format!("API error ({}): {}", status, error_text))
            }
        }
        Err(e) => Err(format!("Network error: {}", e)),
    }
}

// Response types for Composio API
#[derive(Debug, Serialize, Deserialize)]
struct ToolListResponse {
    #[serde(default)]
    items: Vec<ComposioTool>,
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(rename = "totalPages", default)]
    total_pages: Option<i32>,
    // Add additional fields that might be in the response
    #[serde(rename = "tools", default)]
    tools: Option<Vec<ComposioTool>>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

// JSON-RPC 2.0 response format
#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcResponse<T> {
    jsonrpc: String,
    id: Option<Value>,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

impl ToolListResponse {
    // Helper method to get all tools, whether they're in 'items' or 'tools'
    fn get_all_tools(&self) -> Vec<ComposioTool> {
        let mut all_tools = self.items.clone();
        if let Some(ref tools) = self.tools {
            all_tools.extend(tools.clone());
        }
        all_tools
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolExecuteResponse {
    pub data: Value,
    pub error: Option<String>,
    pub successful: bool,
    #[serde(rename = "log_id")]
    pub log_id: Option<String>,
    #[serde(rename = "session_info")]
    pub session_info: Option<Value>,
}

// Helper function to write content to a file for debugging
// Only writes files when DEBUG or TRACE log level is enabled
fn write_to_debug_file(filename: &str, content: &str) -> std::io::Result<()> {
    // Only write debug files if DEBUG or TRACE level is enabled
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return Ok(());
    }
    
    // Use system temp directory for logs to avoid triggering hot-reload watchers
    let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
    if !debug_dir.exists() {
        std::fs::create_dir_all(&debug_dir)?;
    }
    
    let file_path = debug_dir.join(filename);
    let mut file = File::create(&file_path)?;
    file.write_all(content.as_bytes())?;
    
    // Log the absolute path for clarity
    tracing::debug!("Wrote debug file to: {}", file_path.display());
    
    Ok(())
}

impl ComposioClient {
    pub fn new(api_key: String, base_url: String, entity_id: Option<String>, user_id: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url,
            entity_id,
            user_id,
            tool_toolkit_map: Arc::new(RwLock::new(HashMap::new())),
            toolkit_account_map: Arc::new(RwLock::new(HashMap::new())),
            auth_config_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The marketplace API base URL - always uses the official Composio backend
    /// This is separate from the MCP server URL which may be user-configured
    const MARKETPLACE_API_BASE: &'static str = "https://backend.composio.dev/api/v3";

    fn get_api_base_url(&self) -> String {
        // The base_url is likely specifically for the MCP endpoint (e.g. backend.../v3/mcp)
        // We need the general API base.
        let base = self.base_url.split("/v3/mcp").next().unwrap_or(&self.base_url);
        let base = base.trim_end_matches('/');
        // Composio API for connected accounts is typically v1
        format!("{}/api/v3", base)
    }

    /// Build MCP endpoint URL with user_id query parameter.
    /// Per Composio docs, the MCP URL must include user_id as a query param
    /// for the server to resolve connected accounts correctly.
    fn build_mcp_url(&self, path: &str) -> String {
        let base = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        if let Some(uid) = self.user_id.as_ref().or(self.entity_id.as_ref()) {
            let separator = if base.contains('?') { "&" } else { "?" };
            format!("{}{}user_id={}", base, separator, uid)
        } else {
            base
        }
    }

    async fn list_connected_accounts(&self) -> Result<Vec<ConnectedAccount>, String> {
        let user_uuid = self.user_id.clone().or(self.entity_id.clone());
        tracing::info!("Listing connected accounts for user_uuid: {:?}", user_uuid);
        
        let base_url = format!("{}/connected_accounts", self.get_api_base_url());
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
            
            let response = self
                .client
                .get(&base_url)
                .header("x-api-key", &self.api_key)
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
        
        tracing::info!("Found {} connected accounts across {} pages", all_accounts.len(), page_count);
        
        // Debug: Log all accounts after pagination
        let all_slugs: Vec<String> = all_accounts.iter()
            .map(|a| format!("{} ({})", a.toolkit.as_ref().map(|t| t.slug.as_str()).unwrap_or("?"), a.status.as_str()))
            .collect();
        let _ = write_to_debug_file("composio_all_accounts.txt", &format!(
            "Total: {} accounts across {} pages\nAccounts: {:?}", 
            all_accounts.len(), page_count, all_slugs
        ));
        
        self.cache_accounts(&all_accounts);
        Ok(all_accounts)
    }

    fn cache_accounts(&self, accounts: &[ConnectedAccount]) {
        let mut map = self.toolkit_account_map.write().unwrap();
        // Clear old mappings to prevent stale data
        map.clear();
        
        for account in accounts {
            if account.status == "ACTIVE" {
                // Try toolkit slug first (most specific), then app_name, then provider_id
                // Preserve original casing as returned from Composio API
                let slug = account.toolkit.as_ref().map(|t| t.slug.as_str())
                    .or(account.app_name.as_deref())
                    .or(account.provider_id.as_deref())
                    .map(|s| s.to_string());
                
                if let Some(slug) = slug {
                    let uid = account.user_id.as_deref().unwrap_or("unknown");
                    let account_id = account.id.clone();
                    
                    // Determine the target user we're filtering for
                    let target_id = self.entity_id.as_deref().or(self.user_id.as_deref());
                    
                    // Logic to prioritize the configured entity_id or user_id
                    let should_insert = if let Some(target) = target_id {
                        // ONLY insert if this account belongs to our target user
                        if uid != target {
                            tracing::debug!("Skipping account {} for toolkit '{}' - user '{}' doesn't match target '{}'", 
                                account_id, slug, uid, target);
                            false
                        } else if let Some(existing) = map.get(&slug) {
                            // We have a match for our user - only overwrite if the existing one is for a different user
                            let overwrite = existing.1 != target;
                            if overwrite {
                                tracing::debug!("Overwriting account {} for toolkit '{}' - existing user '{}' doesn't match target '{}'",
                                    account_id, slug, existing.1, target);
                            }
                            overwrite
                        } else {
                            // No existing entry, and user matches - insert it
                            true
                        }
                    } else {
                        // No target configured - accept any account (fallback behavior)
                        true
                    };

                    if should_insert {
                        tracing::debug!("Mapping toolkit '{}' to account '{}' for user '{}'", 
                            slug, account_id, uid);
                        map.insert(slug, (account_id, uid.to_string()));
                    }
                }
            }
        }
        tracing::debug!("Cached {} toolkit-to-account mappings", map.len());
    }

    /// Create an auth config for a toolkit.
    /// 
    /// - `auth_scheme`: The auth type (e.g., "OAUTH2", "API_KEY", "BEARER_TOKEN")
    /// - `use_managed`: Whether to try Composio managed auth (for OAuth apps that Composio supports)
    pub(crate) async fn create_auth_config(
        &self, 
        toolkit_slug: &str,
        auth_scheme: Option<&str>,
        use_managed: bool,
    ) -> Result<String, String> {
        tracing::error!("DEBUG: create_auth_config called for '{}'. Auth Scheme: {:?}, Use Managed: {}", 
            toolkit_slug, auth_scheme, use_managed);
        let url = format!("{}/auth_configs", self.get_api_base_url());
        
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
        
        let response = self.client
            .post(&url)
            .header("x-api-key", &self.api_key)
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
        let mut cache = self.auth_config_cache.write().unwrap();
        cache.insert(toolkit_slug.to_string(), auth_config_id.to_string());
        
        Ok(auth_config_id.to_string())
    }

    /// Fetch the auth config ID for a given toolkit from Composio API.
    /// This is required for initiating OAuth connections properly for any toolkit.
    /// Uses caching to avoid repeated API calls for the same toolkit.
    /// If no auth config exists, creates one programmatically using Composio managed auth.
    async fn get_auth_config_id(&self, toolkit_slug: &str) -> Result<String, String> {
        // Check cache first
        {
            let cache = self.auth_config_cache.read().unwrap();
            if let Some(cached_id) = cache.get(toolkit_slug) {
                tracing::debug!("Using cached auth_config_id '{}' for toolkit '{}'", cached_id, toolkit_slug);
                return Ok(cached_id.clone());
            }
        }
        
        let url = format!("{}/auth_configs", self.get_api_base_url());
        
        tracing::debug!("Fetching auth configs for toolkit '{}' from {}", toolkit_slug, url);
        
        let response = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
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
            let mut cache = self.auth_config_cache.write().unwrap();
            cache.insert(toolkit_slug.to_string(), id.clone());
            return Ok(id.clone());
        }
        
        // No auth config found - create one programmatically
        // Try managed auth first (most common case), the create function will handle the details
        tracing::info!("No auth config found for toolkit '{}', creating one with managed auth...", toolkit_slug);
        self.create_auth_config(toolkit_slug, None, true).await
    }

    pub async fn list_tools(&self) -> Result<Vec<ComposioTool>, String> {
        // Use the MCP Protocol endpoint - returns only tools configured for this specific server
        // CRITICAL: Do NOT include x-api-key header - the URL itself (containing server UUID) is the auth
        // See KI troubleshooting item #58 for details on why x-api-key causes 401 errors here
        
        let mcp_url = self.build_mcp_url("");
        
        tracing::debug!("Fetching tools via MCP Protocol from {}", mcp_url);
        
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": "1",
            "params": {}
        });
        
        let response = self
            .client
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
        match self.parse_tools_response(&response_text) {
            Ok(tools) => {
                tracing::info!("Loaded {} tools from MCP Protocol", tools.len());
                self.cache_tools(&tools);
                Ok(tools)
            },
            Err(e) => Err(e),
        }
    }

    /// Get information about connected toolkits for UI display
    /// Returns toolkit slugs with their tool counts
    pub async fn list_connected_toolkits(&self) -> Result<Vec<ToolkitInfo>, String> {
        // First get all connected accounts to know which toolkits are connected
        let accounts = self.list_connected_accounts().await?;
        
        // Extract unique toolkit slugs from connected accounts
        let mut toolkit_slugs: Vec<String> = accounts
            .iter()
            .filter_map(|acc| {
                acc.toolkit.as_ref().map(|t| t.slug.clone())
                    .or(acc.app_name.clone())
            })
            .collect();
        toolkit_slugs.sort();
        toolkit_slugs.dedup();

        // Get all tools to count per toolkit
        let all_tools = match self.list_tools().await {
            Ok(tools) => tools,
            Err(e) => return Err(format!("Failed to list tools: {}", e)),
        };

        // Count tools per toolkit
        let toolkit_infos = toolkit_slugs.iter().map(|slug| {
            let tool_count = all_tools.iter().filter(|t| {
                // Get explicit toolkit slug
                let explicit_toolkit = t.toolkit.as_ref().map(|tk| tk.slug.clone())
                    .or_else(|| t.app.as_ref().map(|a| a.slug.clone()));
                
                if let Some(ref tk) = explicit_toolkit {
                    return tk.eq_ignore_ascii_case(slug);
                }
                
                // Fallback: check if tool name starts with TOOLKIT_SLUG_ prefix
                // e.g., NEWS_API_GET_EVERYTHING starts with "NEWS_API_" matches news_api toolkit
                let prefix = format!("{}_", slug.to_uppercase());
                t.name.starts_with(&prefix)
            }).count();

            ToolkitInfo {
                slug: slug.clone(),
                display_name: slug.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default() + &slug[1..],
                tool_count,
                is_connected: true,
            }
        }).collect();

        Ok(toolkit_infos)
    }

    /// List all available toolkits from Composio (for marketplace discovery)
    /// This returns ALL toolkits (~500+), not just connected ones
    /// Supports search and cursor-based pagination for efficient loading
    /// 
    /// Returns: (items, total_pages, next_cursor)
    pub async fn list_all_toolkits(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
        categories: Option<Vec<String>>,
        sort_by: Option<&str>,
    ) -> Result<(Vec<ComposioToolkitListing>, i32, Option<String>), String> {
        // Use the fixed marketplace API base URL for toolkit listings
        let mut url = format!("{}/toolkits", Self::MARKETPLACE_API_BASE);
        
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
        
        let response = self.client
            .get(&url)
            .header("x-api-key", &self.api_key)
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
            
        // DEBUG: Check news_api auth schemes
        if let Some(news_api) = parsed.items.iter().find(|t| t.slug == "news_api") {
            tracing::error!("DEBUG: Found news_api in list_all_toolkits. Auth schemes: {:?}, Managed: {:?}, No Auth: {:?}", 
                news_api.auth_schemes, 
                news_api.composio_managed_auth_schemes,
                news_api.no_auth);
        } else {
            tracing::error!("DEBUG: news_api NOT found in list_all_toolkits response");
        }
        
        let total_pages = parsed.total_pages.unwrap_or(1);
        let current_page = parsed.current_page.unwrap_or(1);
        
        tracing::info!("Fetched {} toolkits (page {} of {}, next_cursor: {:?})", 
            parsed.items.len(), 
            current_page,
            total_pages,
            parsed.next_cursor
        );
        
        Ok((parsed.items, total_pages, parsed.next_cursor))
    }

    /// List all available toolkit categories from Composio
    pub async fn list_toolkit_categories(&self) -> Result<Vec<ComposioCategory>, String> {
        // Use the fixed marketplace API base URL for categories
        let url = format!("{}/toolkits/categories", Self::MARKETPLACE_API_BASE);
        
        tracing::debug!("Fetching toolkit categories from: {}", url);
        
        let response = self.client
            .get(&url)
            .header("x-api-key", &self.api_key)
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

    /// Get the set of connected toolkit slugs (for showing connection status in marketplace)
    pub async fn get_connected_toolkit_slugs(&self) -> Result<std::collections::HashSet<String>, String> {
        let accounts = self.list_connected_accounts().await?;
        let slugs: std::collections::HashSet<String> = accounts
            .iter()
            .filter_map(|acc| {
                acc.toolkit.as_ref().map(|t| t.slug.to_lowercase())
                    .or(acc.app_name.as_ref().map(|n| n.to_lowercase()))
            })
            .collect();
        Ok(slugs)
    }

    /// Fetch all tool slugs for a specific toolkit from Composio API.
    /// Uses the /tools/enum endpoint and filters by toolkit prefix convention:
    /// e.g., toolkit "news_api" → tools starting with "NEWS_API_"
    pub async fn get_toolkit_tools(&self, toolkit_slug: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/tools/enum", self.get_api_base_url());
        
        tracing::debug!("Fetching tools enum to filter for toolkit '{}'", toolkit_slug);
        
        let response = self.client
            .get(&url)
            .header("x-api-key", &self.api_key)
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
    /// Returns a list of (tool_name, description) tuples.
    pub async fn get_toolkit_tools_detailed(&self, toolkit_slug: &str) -> Result<Vec<(String, Option<String>)>, String> {
        // NOTE: The /tools?appNames= endpoint has issues returning wrong toolkit's tools.
        // Instead, use /tools/enum which correctly returns all tool slugs, then filter by prefix.
        // This matches the pattern used in get_toolkit_tools() which works correctly.
        
        let url = format!("{}/tools/enum", self.get_api_base_url());
        
        tracing::debug!("Fetching tools enum for detailed list, filtering for toolkit '{}'", toolkit_slug);
        
        let response = self.client
            .get(&url)
            .header("x-api-key", &self.api_key)
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

    /// Convert detailed tools to ToolCandidate format for LLM selection
    #[allow(dead_code)]
    pub fn tools_to_candidates(&self, tools: Vec<(String, Option<String>)>) -> Vec<super::tool_selection::ToolCandidate> {
        tools.into_iter()
            .map(|(name, description)| super::tool_selection::ToolCandidate { name, description })
            .collect()
    }

    /// Add a toolkit to the MCP server configuration via PATCH API.
    /// This is used when the user clicks "Connect" in the marketplace to add a toolkit
    /// to their MCP server before initiating OAuth.
    /// 
    /// The `auth_config_id` binds this toolkit to a specific auth configuration.
    /// The server_id is extracted from the base_url (e.g., ".../v3/mcp/0a4474b3-d8..." -> "0a4474b3-d8...")
    /// 
    /// If `selected_tools` is provided, only those tools will be enabled.
    /// Otherwise, all tools for the toolkit will be fetched and enabled.
    pub async fn add_toolkit_to_server(&self, toolkit_slug: &str, auth_config_id: &str, selected_tools: Option<Vec<String>>) -> Result<(), String> {
        // Extract server ID from base_url
        // base_url format: "https://backend.composio.dev/v3/mcp/{server_id}/mcp" or with query params
        let server_id = self.base_url
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
        let get_url = format!("{}/mcp/{}", self.get_api_base_url(), server_id);
        tracing::debug!("GET MCP server config from: {}", get_url);
        
        let get_response = self.client
            .get(&get_url)
            .header("x-api-key", &self.api_key)
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
        
        // Convert existing toolkits to strings (API returns objects but expects strings in PATCH)
        let mut toolkit_strings: Vec<String> = toolkits.iter()
            .filter_map(|t| {
                // Extract slug from object format
                if let Some(slug) = t.get("toolkit").and_then(|s| s.as_str()) {
                    return Some(slug.to_string());
                }
                // Use string format as-is
                if let Some(slug) = t.as_str() {
                    return Some(slug.to_string());
                }
                None
            })
            .collect();
        
        // Add the toolkit as a string if not already present
        if !toolkit_already_exists {
            toolkit_strings.push(toolkit_slug.to_string());
            tracing::info!("Adding toolkit '{}' with auth_config '{}' to server", toolkit_slug, auth_config_id);
        } else {
            tracing::info!("Toolkit '{}' already exists on server, will add tools", toolkit_slug);
        }
        
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
            match self.get_toolkit_tools(toolkit_slug).await {
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
        
        // PATCH the server with updated toolkits, auth_config, and allowed_tools
        // Note: API uses 'allowed_tools' (not 'custom_tools' which SDK uses internally)
        // Note: auth_config_ids links the auth config to the MCP server
        // API endpoint: PATCH /api/v3/mcp/{server_id}
        let patch_url = format!("{}/mcp/{}", self.get_api_base_url(), server_id);
        
        // NOTE: Do NOT include auth_config_ids - accumulating stale IDs causes 400 errors
        // The auth_config is managed separately per toolkit during initiate_connection
        let patch_payload = serde_json::json!({
            "toolkits": toolkit_strings,
            "allowed_tools": custom_tools
        });
        
        tracing::debug!("PATCH {} with payload: {:?}", patch_url, patch_payload);
        
        let patch_response = self.client
            .patch(&patch_url)
            .header("x-api-key", &self.api_key)
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
        if let Some(ref user_id) = self.user_id {
            let generate_url = format!("{}/mcp/servers/generate", self.get_api_base_url());
            let generate_payload = serde_json::json!({
                "user_id": user_id,
                "mcp_server_id": server_id
            });
            
            tracing::debug!("Registering user '{}' with MCP server '{}'", user_id, server_id);
            
            let generate_response = self.client
                .post(&generate_url)
                .header("x-api-key", &self.api_key)
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

    /// List tools, optionally filtering by specific toolkit slugs
    /// If `toolkit_filter` is provided, only tools from those toolkits are returned
    pub async fn list_tools_filtered(&self, toolkit_filter: Option<&[String]>) -> Result<Vec<ComposioTool>, String> {
        let all_tools = match self.list_tools().await {
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
    /// Uses Composio's MCP server with JSON-RPC (same as list_tools)
    #[allow(dead_code)]
    pub async fn search_tools(&self, query: &str, toolkit_slugs: &[String]) -> Result<Vec<ComposioTool>, String> {
        let url = self.build_mcp_url("");
        
        tracing::info!("Searching tools via MCP: query='{}', toolkits={:?}", query, toolkit_slugs);
        
        // Build params with search query and optional toolkit filter
        // Toolkit slugs should be UPPERCASE per Composio convention
        let mut params = serde_json::Map::new();
        params.insert("search".to_string(), serde_json::Value::String(query.to_string()));
        
        if !toolkit_slugs.is_empty() {
            // Uppercase the slugs as Composio expects them in uppercase
            let uppercase_slugs: Vec<serde_json::Value> = toolkit_slugs
                .iter()
                .map(|s| serde_json::Value::String(s.to_uppercase()))
                .collect();
            params.insert("toolkits".to_string(), serde_json::Value::Array(uppercase_slugs));
        }
        
        if let Some(entity_id) = &self.entity_id {
            params.insert("user_id".to_string(), serde_json::Value::String(entity_id.clone()));
        }
        
        // Use JSON-RPC like list_tools
        let json_rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": "search_tools",
            "params": params
        });
        
        tracing::debug!("Search request: {:?}", json_rpc_request);
        
        let response = self.client
            .post(&url)
            .header("x-api-key", &self.api_key)
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
        
        tracing::debug!("Search response: {}", response_text);

        // Parse the response using the same parser as list_tools
        match self.parse_tools_response(&response_text) {
            Ok(tools) => {
                tracing::info!("Search returned {} tools", tools.len());
                Ok(tools)
            },
            Err(e) => Err(format!("Failed to parse search response: {}", e)),
        }
    }

    /// List tools for a session with Tool Router pattern:
    /// - Always includes meta-tools for on-demand discovery
    /// - Only includes tools from force-loaded toolkits (not all 253+ tools)
    pub async fn list_tools_for_session(&self, force_load_slugs: &[String]) -> Result<Vec<ComposioTool>, String> {
        let mut tools = Vec::new();
        
        // 1. Add meta-tools for on-demand discovery (these are always included)
        tools.extend(Self::get_meta_tools());
        
        // 2. Only add tools from force-loaded toolkits
        if !force_load_slugs.is_empty() {
            tracing::info!("Loading tools from force-loaded toolkits: {:?}", force_load_slugs);
            let force_loaded = self.list_tools_filtered(Some(force_load_slugs)).await?;
            tracing::info!("Loaded {} tools from force-loaded toolkits", force_loaded.len());
            tools.extend(force_loaded);
        } else {
            tracing::info!("No force-loaded toolkits configured - only meta-tools available");
        }
        
        tracing::info!("Total tools for session: {} (including {} meta-tools)", tools.len(), 2);
        Ok(tools)
    }

    /// Get meta-tools for Tool Router pattern
    /// These allow the AI to search for and execute tools on-demand
    fn get_meta_tools() -> Vec<ComposioTool> {
        vec![
            // Meta-tool 1: Discover available apps/toolkits (e.g., "Gmail", "GitHub")
            ComposioTool {
                name: "COMPOSIO_DISCOVER_APPS".to_string(),
                description: Some(
                    "Discover available Composio applications and toolkits. \
                    Use this to find which apps match your needs (e.g., query 'email' to find 'Gmail'). \
                    Returns app names, descriptions, and tool counts. \
                    After finding the right app, use COMPOSIO_GET_APP_TOOLS to list its specific tools.".to_string()
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural language query to search for apps (e.g., 'email', 'crm', 'calendar', 'github')"
                        }
                    },
                    "required": []
                })),
                toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
                app: None,
                slug: Some("COMPOSIO_DISCOVER_APPS".to_string()),
                input_parameters: None,
                input_schema: None,
                output_parameters: None,
                tags: None,
                version: None,
                available_versions: None,
                is_deprecated: None,
                is_no_auth: Some(true),
            },
            // Meta-tool 2: Get specific tools for a chosen app
            ComposioTool {
                name: "COMPOSIO_GET_APP_TOOLS".to_string(),
                description: Some(
                    "List all available tools for a specific Composio app/toolkit. \
                    Use this AFTER discovering the app name with COMPOSIO_DISCOVER_APPS. \
                    Returns tool names, descriptions, and parameter schemas for the selected app.".to_string()
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "The name or slug of the app to list tools for (e.g., 'Gmail', 'GitHub', 'Google Sheets')"
                        }
                    },
                    "required": ["app_name"]
                })),
                toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
                app: None,
                slug: Some("COMPOSIO_GET_APP_TOOLS".to_string()),
                input_parameters: None,
                input_schema: None,
                output_parameters: None,
                tags: None,
                version: None,
                available_versions: None,
                is_deprecated: None,
                is_no_auth: Some(true),
            },
            // Meta-tool 3: Execute a specific tool
            ComposioTool {
                name: "COMPOSIO_EXECUTE_TOOL".to_string(),
                description: Some(
                    "Execute a Composio tool by name. Use COMPOSIO_GET_APP_TOOLS first to find the correct \
                    tool name and required parameters. Pass the exact tool name and arguments as JSON.".to_string()
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool_name": {
                            "type": "string",
                            "description": "The exact name of the tool to execute (e.g., 'GMAIL_SEND_EMAIL', 'GITHUB_CREATE_ISSUE')"
                        },
                        "arguments": {
                            "type": "object",
                            "description": "The arguments to pass to the tool, matching the tool's parameter schema"
                        }
                    },
                    "required": ["tool_name", "arguments"]
                })),
                toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
                app: None,
                slug: Some("COMPOSIO_EXECUTE_TOOL".to_string()),
                input_parameters: None,
                input_schema: None,
                output_parameters: None,
                tags: None,
                version: None,
                available_versions: None,
                is_deprecated: None,
                is_no_auth: Some(true),
            }
        ]
    }

    fn cache_tools(&self, tools: &[ComposioTool]) {
        let mut map = self.tool_toolkit_map.write().unwrap();
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
                 // Fallback: Try to infer toolkit slug from tool name (e.g., GMAIL_FETCH_EMAILS -> gmail)
                 // This is common for API responses that return flat tool lists without toolkit metadata
                 // Infer toolkit from tool name (e.g., GMAIL_FETCH_EMAILS -> gmail)
                 // Keep the inferred slug lowercase as that's what Composio typically uses
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

    fn parse_tools_response(&self, response_text: &str) -> Result<Vec<ComposioTool>, String> {
        // Check if the response is in SSE format (starts with "event: " or "data: ")
        let trimmed_response = response_text.trim();
        if trimmed_response.starts_with("event:") || trimmed_response.starts_with("data:") {
            tracing::debug!("Detected SSE format response");
            
            // Extract the JSON data from the SSE message
            // SSE format is typically "event: message\ndata: {json_data}\n\n"
            let data_start = response_text.find("data:").unwrap_or(0) + "data:".len();
            let json_text = response_text[data_start..].trim();
            
            tracing::debug!("Extracted JSON from SSE: {}", json_text);
            
            // First, try to parse the JSON as a generic Value to determine its type
            match serde_json::from_str::<serde_json::Value>(json_text) {
                Ok(json_value) => {
                    // Check if it's an object
                    if json_value.is_object() {
                        let json_obj = json_value.clone();
                        tracing::debug!("SSE data is a JSON object");
                        
                        // Check if it's a JSON-RPC response
                        if json_obj.get("jsonrpc").is_some() {
                            // Try to parse as a JSON-RPC response
                            if let Ok(rpc_response) = serde_json::from_value::<JsonRpcResponse<Vec<ComposioTool>>>(json_obj.clone()) {
                                // Check if there's an error in the RPC response
                                if let Some(error) = rpc_response.error {
                                    let error_msg = format!("Composio API error: {}", error.message);
                                    tracing::error!("{}", error_msg);
                                    
                                    // Return a network error
                                    return Err(error_msg);
                                }
                                
                                // If we have a result, return it
                                if let Some(tools) = rpc_response.result {
                                    tracing::debug!("Parsed response with {} tools", tools.len());
                                    return Ok(tools);
                                }
                            }
                        }
                        
                        // Check if it has a "result" field that contains a "tools" field with an array
                        if let Some(result) = json_obj.get("result") {
                            if let Some(result_obj) = result.as_object() {
                                if let Some(tools_field) = result_obj.get("tools") {
                                    if let Some(tools_array) = tools_field.as_array() {
                                        // Try to convert each item in the array to a ComposioTool
                                        let mut tools = Vec::new();
                                        for tool_value in tools_array {
                                            if let Ok(tool) = serde_json::from_value::<ComposioTool>(tool_value.clone()) {
                                                tools.push(tool);
                                            }
                                        }
                                        
                                        if !tools.is_empty() {
                                            tracing::debug!("Extracted {} tools from result.tools field", tools.len());
                                            return Ok(tools);
                                        }
                                    }
                                }
                            }
                        }
                        
                        // Check if it has a "tools" field that contains an array
                        if let Some(tools_field) = json_obj.get("tools") {
                            if let Some(tools_array) = tools_field.as_array() {
                                // Try to convert each item in the array to a ComposioTool
                                let mut tools = Vec::new();
                                for tool_value in tools_array {
                                    if let Ok(tool) = serde_json::from_value::<ComposioTool>(tool_value.clone()) {
                                        tools.push(tool);
                                    }
                                }
                                
                                if !tools.is_empty() {
                                    tracing::debug!("Extracted {} tools from JSON object's tools field", tools.len());
                                    return Ok(tools);
                                }
                            }
                        }
                        
                        // Log the actual JSON structure for debugging
                        tracing::error!("SSE data parsed as JSON object but couldn't extract tools: {}",
                            serde_json::to_string_pretty(&json_obj).unwrap_or_default());
                    }
                    // Check if it's an array
                    else if json_value.is_array() {
                        // Try to parse as a direct array of ComposioTool objects
                        if let Ok(tools) = serde_json::from_value::<Vec<ComposioTool>>(json_value.clone()) {
                            tracing::debug!("Parsed SSE data as direct array with {} tools", tools.len());
                            return Ok(tools);
                        }
                    }
                    
                    // If we got here, we couldn't extract tools from the JSON
                    tracing::error!("Could not extract tools from SSE data: {}",
                        serde_json::to_string_pretty(&json_value).unwrap_or_default());
                },
                Err(parse_err) => {
                    // Failed to parse as JSON
                    tracing::error!("Failed to parse SSE data as JSON: {}", parse_err);
                    return Err(parse_err.to_string());
                }
            }
            
            // If we got here, all parsing attempts failed
            let err_msg = "Failed to parse Composio SSE response into a usable format";
            tracing::error!("{}", err_msg);
            
            // Return a network error
            return Err(err_msg.to_string());
        } else {
            // Regular JSON response (not SSE)
            // First, try to parse the JSON as a generic Value to determine its type
            match serde_json::from_str::<serde_json::Value>(response_text) {
                Ok(json_value) => {
                    // Check if it's an object
                    if json_value.is_object() {
                        let json_obj = json_value.clone();
                        tracing::debug!("Response is a JSON object");
                        
                        // Check if it's a JSON-RPC response
                        if json_obj.get("jsonrpc").is_some() {
                            // Try to parse as a JSON-RPC response
                            if let Ok(rpc_response) = serde_json::from_value::<JsonRpcResponse<Vec<ComposioTool>>>(json_obj.clone()) {
                                // Check if there's an error in the RPC response
                                if let Some(error) = rpc_response.error {
                                    let error_msg = format!("Composio API error: {}", error.message);
                                    tracing::error!("{}", error_msg);
                                    
                                    // Return a network error
                                    return Err(error_msg);
                                }
                                
                                // If we have a result, return it
                                if let Some(tools) = rpc_response.result {
                                    tracing::debug!("Parsed response with {} tools", tools.len());
                                    return Ok(tools);
                                }
                            }
                        }
                        
                        // Check if it has a "result" field that contains a "tools" field with an array
                        if let Some(result) = json_obj.get("result") {
                            if let Some(result_obj) = result.as_object() {
                                if let Some(tools_field) = result_obj.get("tools") {
                                    if let Some(tools_array) = tools_field.as_array() {
                                        // Try to convert each item in the array to a ComposioTool
                                        let mut tools = Vec::new();
                                        for tool_value in tools_array {
                                            if let Ok(tool) = serde_json::from_value::<ComposioTool>(tool_value.clone()) {
                                                tools.push(tool);
                                            }
                                        }
                                        
                                        if !tools.is_empty() {
                                            tracing::debug!("Extracted {} tools from result.tools field", tools.len());
                                            return Ok(tools);
                                        }
                                    }
                                }
                            }
                        }
                        
                        // Check if it has a "tools" field that contains an array
                        if let Some(tools_field) = json_obj.get("tools") {
                            if let Some(tools_array) = tools_field.as_array() {
                                // Try to convert each item in the array to a ComposioTool
                                let mut tools = Vec::new();
                                for tool_value in tools_array {
                                    if let Ok(tool) = serde_json::from_value::<ComposioTool>(tool_value.clone()) {
                                        tools.push(tool);
                                    }
                                }
                                
                                if !tools.is_empty() {
                                    tracing::debug!("Extracted {} tools from JSON object's tools field", tools.len());
                                    return Ok(tools);
                                }
                            }
                        }
                        
                        // Check if it has an "items" field (REST API format)
                        if let Some(items_field) = json_obj.get("items") {
                            if let Some(items_array) = items_field.as_array() {
                                // Try to convert each item in the array to a ComposioTool
                                let mut tools = Vec::new();
                                for tool_value in items_array {
                                    if let Ok(tool) = serde_json::from_value::<ComposioTool>(tool_value.clone()) {
                                        tools.push(tool);
                                    }
                                }
                                
                                if !tools.is_empty() {
                                    tracing::debug!("Extracted {} tools from REST API 'items' field", tools.len());
                                    return Ok(tools);
                                }
                            }
                        }

                        // Try to parse as a ToolListResponse (Legacy/Permissive fallback)
                        if let Ok(tools_response) = serde_json::from_value::<ToolListResponse>(json_obj.clone()) {
                            let tools = tools_response.get_all_tools();
                            if !tools.is_empty() {
                                tracing::debug!("Parsed response with {} tools (legacy format)", tools.len());
                                return Ok(tools);
                            }
                        }
                        
                        // Log the actual JSON structure for debugging
                        tracing::error!("Response parsed as JSON object but couldn't extract tools: {}",
                            serde_json::to_string_pretty(&json_obj).unwrap_or_default());
                    }
                    // Check if it's an array
                    else if json_value.is_array() {
                        // Try to parse as a direct array of ComposioTool objects
                        if let Ok(tools) = serde_json::from_value::<Vec<ComposioTool>>(json_value.clone()) {
                            tracing::debug!("Parsed response as direct array with {} tools", tools.len());
                            return Ok(tools);
                        }
                    }
                    
                    // If we got here, we couldn't extract tools from the JSON
                    tracing::error!("Could not extract tools from response: {}",
                        serde_json::to_string_pretty(&json_value).unwrap_or_default());
                },
                Err(parse_err) => {
                    // Failed to parse as JSON
                    tracing::error!("Failed to parse response as JSON: {}", parse_err);
                    return Err(parse_err.to_string());
                }
            }
            
            // If we got here, all parsing attempts failed
            let err_msg = "Failed to parse Composio response into a usable format";
            tracing::error!("{}", err_msg);
            Err(err_msg.to_string())
        }
    }

    pub async fn execute_tool(&self, slug: &str, args: serde_json::Value) -> Result<ToolExecuteResponse, String> {
        // Use the MCP endpoint with user_id in URL for proper account resolution
        let url = self.build_mcp_url("");
        
        // Ensure arguments are wrapped in an object if they aren't already
        let arguments = if args.is_object() {
            args
        } else {
            serde_json::json!({ "value": args })
        };

        // Resolve connected_account_id if entity_id is not set
        let mut connected_account_id: Option<String> = None;
        // Determine the effective user_id or entity_id to use
        let mut user_id = self.user_id.clone()
            .or(self.entity_id.clone())
            .unwrap_or_else(|| "default".to_string());
            
        tracing::info!("Execute Tool: tool={}, internal_user_id={:?}, internal_entity_id={:?}, resolved_target={}", 
            slug, self.user_id, self.entity_id, user_id);
            
        tracing::info!("DEBUG: execute_tool start. Slug: {}, UserID: {}", 
            slug, user_id);

        // Try to find the toolkit for this tool
        let toolkit_slug = {
            let map = self.tool_toolkit_map.read().unwrap();
            let result = map.get(slug).cloned();
            tracing::info!("[TRACE] Toolkit mapping for tool '{}': {:?}", slug, result);
            result
        };

        // If we don't have the toolkit mapping, try refreshing tools
        let toolkit_slug = if toolkit_slug.is_none() {
            if let Ok(_) = self.list_tools().await {
                // cache_tools is called inside list_tools refactor
                let map = self.tool_toolkit_map.read().unwrap();
                match map.get(slug).cloned() {
                    Some(s) => Some(s),
                    None => {
                        // ANTI-PATTERN FIX: Don't use split('_')[0] for multi-word toolkit slugs
                        // e.g., NEWS_API_GET_HEADLINES should match "news_api", not "news"
                        // Use prefix matching against known connected toolkit slugs
                        let tool_upper = slug.to_uppercase();
                        let connected_slugs: Vec<String> = {
                            let account_map = self.toolkit_account_map.read().unwrap();
                            account_map.keys().cloned().collect()
                        };
                        
                        let matched = connected_slugs.iter().find(|tk_slug| {
                            let prefix = format!("{}_", tk_slug.to_uppercase().replace("-", "_"));
                            tool_upper.starts_with(&prefix)
                        }).cloned();
                        
                        if matched.is_some() {
                            tracing::debug!("Matched toolkit slug '{:?}' for tool '{}' via prefix", matched, slug);
                            matched
                        } else {
                            // Fallback to first segment only if no prefix match
                            let parts: Vec<&str> = slug.split('_').collect();
                            if !parts.is_empty() {
                                let guessed = parts[0].to_lowercase();
                                tracing::debug!("Fallback: guessed toolkit slug '{}' from tool '{}'", guessed, slug);
                                Some(guessed)
                            } else {
                                None
                            }
                        }
                    }
                }
            } else {
                 // Even if list_tools failed, try prefix matching against cached accounts
                 let tool_upper = slug.to_uppercase();
                 let connected_slugs: Vec<String> = {
                     let account_map = self.toolkit_account_map.read().unwrap();
                     account_map.keys().cloned().collect()
                 };
                 
                 let matched = connected_slugs.iter().find(|tk_slug| {
                     let prefix = format!("{}_", tk_slug.to_uppercase().replace("-", "_"));
                     tool_upper.starts_with(&prefix)
                 }).cloned();
                 
                 if matched.is_some() {
                     tracing::debug!("Matched toolkit slug '{:?}' for tool '{}' via prefix (fallback path)", matched, slug);
                     matched
                 } else {
                     // Ultimate fallback to first segment
                     let parts: Vec<&str> = slug.split('_').collect();
                     if !parts.is_empty() {
                         Some(parts[0].to_lowercase())
                     } else {
                         None
                     }
                 }
            }
        } else {
            toolkit_slug
        };

        if let Some(raw_slug) = &toolkit_slug {
            let slug = raw_slug.clone();
            tracing::debug!("DEBUG: Found toolkit slug: {}", slug);
            
            // Determine if we need to fetch accounts.
            // We fetch if:
            // 1. It's not in the cache.
            // 2. It IS in the cache, but we have a specific entity_id and the cached one doesn't match.
            //    (Since our new cache_accounts logic helps us find the right one, refreshing gives it a chance to do so).
            let needs_refresh = {
                let map = self.toolkit_account_map.read().unwrap();
                tracing::info!("[TRACE] Cached toolkit→account map: {:?}", map);
                if let Some((_, uid)) = map.get(&slug) {
                     let target_id = self.entity_id.as_deref().or(self.user_id.as_deref());
                     if let Some(target) = target_id {
                        let needs = uid != target;
                        tracing::info!("[TRACE] Toolkit '{}' found in cache with user '{}', target '{}', needs_refresh: {}", 
                            slug, uid, target, needs);
                        needs
                    } else {
                        tracing::info!("[TRACE] Toolkit '{}' found in cache, no target configured, no refresh needed", slug);
                        false
                    }
                } else {
                    tracing::info!("[TRACE] Toolkit '{}' NOT in cache, needs_refresh: true", slug);
                    true
                }
            };

            if needs_refresh {
                // Fetch connected accounts and populate cache
                tracing::info!("[TRACE] Fetching connected accounts to resolve ID for toolkit '{}'", slug);
                if let Ok(accounts) = self.list_connected_accounts().await {
                    tracing::info!("[TRACE] Fetched {} accounts", accounts.len());
                    for acc in &accounts {
                        tracing::debug!("[TRACE] Account: id={}, status={}, toolkit={:?}, app_name={:?}", 
                            acc.id, acc.status, acc.toolkit.as_ref().map(|t| &t.slug), acc.app_name);
                    }
                    // list_connected_accounts updates the cache
                } else {
                    tracing::warn!("[TRACE] Failed to fetch connected accounts");
                }
            }
            
            // Check cache for the result - use case-insensitive lookup
            let map = self.toolkit_account_map.read().unwrap();
            // Try exact match first, then case-insensitive
            let account_info = map.get(&slug)
                .or_else(|| {
                    // Try case-insensitive match
                    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(&slug)).map(|(_, v)| v)
                });
            
            if let Some((acc_id, uid)) = account_info {
                tracing::info!("Resolved connected_account_id: {} for toolkit: {} (user: {})", acc_id, slug, uid);
                connected_account_id = Some(acc_id.clone());
                user_id = uid.clone();
            } else {
                tracing::warn!("No account found in cache for toolkit: {}. Available toolkits: {:?}", 
                    slug, map.keys().collect::<Vec<_>>());
            }
        }

        // NOTE: MCP protocol resolves auth via user_id in URL query param (set by build_mcp_url).
        // connected_account_id is optional - we should NOT trigger auth preemptively here.
        // If auth is actually needed, the tool call response will indicate it and we handle it post-execution.
        // This fixes the reconnection loop for Custom Auth (API_KEY) toolkits like news_api,
        // where the REST API /connected_accounts doesn't return API_KEY accounts but MCP works fine.
        if connected_account_id.is_none() {
            tracing::debug!("[AUTH] No connected_account_id found for toolkit '{:?}', proceeding with user_id '{}' in URL", 
                toolkit_slug, user_id);
        }

        let mut params_obj = serde_json::Map::new();
        params_obj.insert("name".to_string(), serde_json::Value::String(slug.to_string()));
        params_obj.insert("arguments".to_string(), arguments);
        

        if let Some(id) = connected_account_id {
            // If we have a specific connected account, use it and do NOT send the user_id.
            // The connected_account_id is the primary identifier for execution.
            params_obj.insert("connected_account_id".to_string(), serde_json::Value::String(id));
        } else {
            // No connected account - use user_id as fallback for the API to determine account
            // Note: auth_config_id is dynamically looked up per toolkit during initiate_connection
            params_obj.insert("user_id".to_string(), serde_json::Value::String(user_id.clone()));
        }

        let params = serde_json::Value::Object(params_obj);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": "1", 
            "params": params
        });

        // Log the request for debugging
        tracing::debug!("Executing tool {} at {}", slug, url);
        let request_body_str = serde_json::to_string_pretty(&body).unwrap_or_default();
        
        // Write request to debug file
        let req_filename = format!("composio_exec_req_{}.json", slug);
        if let Err(e) = write_to_debug_file(&req_filename, &request_body_str) {
             tracing::warn!("Failed to write request debug file: {}", e);
        } else {
             tracing::debug!("Wrote request to debug_logs/{}", req_filename);
        }

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await.map_err(|e| e.to_string())?;
            
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // HTTP 401/403 = deterministic auth failure - trigger managed flow
            if let Some(tk_slug) = &toolkit_slug {
                tracing::info!("[AUTH] HTTP {} for toolkit '{}', triggering connection flow", status.as_u16(), tk_slug);
                match self.initiate_connection(tk_slug, &user_id).await {
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
            } else {
                return Err(format!("HTTP {}: Authentication required", status.as_u16()));
            }
        } else if !status.is_success() {
            // Other non-success statuses remain as errors
            match response.error_for_status() {
                Ok(_) => unreachable!(),
                Err(e) => return Err(e.to_string()),
            }
        }

        let response_text = response.text().await.map_err(|e| e.to_string())?;
        tracing::info!("Raw tool execution response body len: {}", response_text.len());
        
        // Write response to debug file
        let resp_filename = format!("composio_exec_resp_{}.txt", slug);
        if let Err(e) = write_to_debug_file(&resp_filename, &response_text) {
             tracing::warn!("Failed to write response debug file: {}", e);
        }

        // Handle SSE response
        let trimmed_response = response_text.trim();
        let json_text = if trimmed_response.starts_with("event:") || trimmed_response.starts_with("data:") {
            tracing::debug!("Detected SSE format response for tool execution");
            let data_start = response_text.find("data:").unwrap_or(0) + "data:".len();
            response_text[data_start..].trim()
        } else {
            &response_text
        };

        // Try to parse as ToolExecuteResponse directly first
        if let Ok(result) = serde_json::from_str::<ToolExecuteResponse>(json_text) {
            tracing::debug!("Successfully parsed ToolExecuteResponse");
            
            // CRITICAL FIX: Check for auth errors in nested data.status_code
            // MCP meta-tools wrap HTTP errors inside the response, not at transport layer
            if !result.successful {
                let mut needs_auth = false;
                
                // Check data.status_code for 401/403
                if let Some(status_code) = result.data.get("status_code") {
                    let code = status_code.as_u64()
                        .or_else(|| status_code.as_i64().map(|i| i as u64))
                        .or_else(|| status_code.as_str().and_then(|s| s.parse::<u64>().ok()));
                    if code == Some(401) || code == Some(403) {
                        tracing::info!("[AUTH] Detected nested status_code {} in tool response", code.unwrap());
                        needs_auth = true;
                    }
                }
                
                // Check for ECODE auth patterns (e.g., AUTH_018, OAUTH_023)
                if !needs_auth {
                    if let Some(ecode) = result.data.get("ECODE").and_then(|v| v.as_str()) {
                        if ecode.starts_with("AUTH_") || ecode.starts_with("OAUTH_") {
                            tracing::info!("[AUTH] Detected ECODE {} in tool response", ecode);
                            needs_auth = true;
                        }
                    }
                }
                
                // Check http_error field for 401/403 patterns
                if !needs_auth {
                    if let Some(http_error) = result.data.get("http_error").and_then(|v| v.as_str()) {
                        if http_error.contains("401") || http_error.contains("403") 
                            || http_error.to_lowercase().contains("unauthorized") {
                            tracing::info!("[AUTH] Detected http_error auth pattern in tool response");
                            needs_auth = true;
                        }
                    }
                }
                
                // CRITICAL: Check auth_refresh_required field from Composio
                // If false, the 401/403 is from the downstream API (e.g., wrong team, no permissions)
                // NOT from missing/expired OAuth token - don't trigger reconnection
                if needs_auth {
                    if let Some(auth_refresh) = result.data.get("auth_refresh_required") {
                        if auth_refresh == false {
                            tracing::info!("[AUTH] auth_refresh_required=false, 401 is from downstream API not auth issue - skipping reconnection");
                            needs_auth = false;
                        }
                    }
                }
                
                if needs_auth {
                    if let Some(tk_slug) = &toolkit_slug {
                        tracing::info!("[AUTH] Triggering connection flow for toolkit '{}' due to nested auth error", tk_slug);
                        match self.initiate_connection(tk_slug, &user_id).await {
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
                                // Fall through to return original result
                            }
                        }
                    }
                }
            }
            
            return Ok(result);
        }

        // If direct parsing fails, try to handle other formats (like error-wrapped or raw data)
        let json_value: Value = match serde_json::from_str(json_text) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to parse response as JSON: {}", e);
                // Return a synthetic error response
                return Ok(ToolExecuteResponse {
                    data: Value::Null,
                    error: Some(format!("Failed to parse response body: {}. Raw body: {}", e, response_text)),
                    successful: false,
                    log_id: None,
                    session_info: None,
                });
            }
        };

        // Handle JSON-RPC envelope: extract inner tool response from result.content[0].text
        // The MCP API returns: {"result":{"content":[{"type":"text","text":"<stringified JSON>"}]}}
        let inner_response: Option<Value> = json_value
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .and_then(|text_str| serde_json::from_str(text_str).ok());
        
        // Use inner response if available, otherwise use original json_value
        let effective_value = inner_response.as_ref().unwrap_or(&json_value);
        
        // Check for auth errors in nested data structure
        let mut needs_auth = false;
        let mut error_msg = String::new();
        
        // Check data.status_code for 401/403 (deterministic check)
        if let Some(data) = effective_value.get("data") {
            if let Some(status_code) = data.get("status_code") {
                let code = status_code.as_u64()
                    .or_else(|| status_code.as_i64().map(|i| i as u64))
                    .or_else(|| status_code.as_str().and_then(|s| s.parse::<u64>().ok()));
                if code == Some(401) || code == Some(403) {
                    tracing::info!("[AUTH] Detected nested data.status_code {} in JSON-RPC response", code.unwrap());
                    needs_auth = true;
                }
            }
            
            // Check data.http_error for 401/403
            if !needs_auth {
                if let Some(http_error) = data.get("http_error").and_then(|v| v.as_str()) {
                    if http_error.contains("401") || http_error.contains("403") 
                        || http_error.to_lowercase().contains("unauthorized") {
                        tracing::info!("[AUTH] Detected data.http_error auth pattern: {}", http_error);
                        needs_auth = true;
                        if error_msg.is_empty() { error_msg = http_error.to_string(); }
                    }
                }
            }
            
            // Check data.ECODE for auth patterns
            if !needs_auth {
                if let Some(ecode) = data.get("ECODE").and_then(|v| v.as_str()) {
                    if ecode.starts_with("AUTH_") || ecode.starts_with("OAUTH_") {
                        tracing::info!("[AUTH] Detected data.ECODE {} in response", ecode);
                        needs_auth = true;
                        if error_msg.is_empty() { error_msg = format!("Auth error: {}", ecode); }
                    }
                }
            }
        }
        
        // Also check root-level ECODE (some responses put it there)
        if !needs_auth {
            if let Some(ecode) = effective_value.get("ECODE").and_then(|v| v.as_str()) {
                if ecode.starts_with("AUTH_") || ecode.starts_with("OAUTH_") {
                    tracing::info!("[AUTH] Detected root ECODE {} in response", ecode);
                    needs_auth = true;
                    if error_msg.is_empty() { error_msg = format!("Auth error: {}", ecode); }
                }
            }
        }
        
        // CRITICAL: Check auth_refresh_required field from Composio
        // If false, the 401/403 is from the downstream API (e.g., wrong team) - don't trigger reconnection
        if needs_auth {
            let auth_refresh = effective_value.get("auth_refresh_required")
                .or_else(|| effective_value.get("data").and_then(|d| d.get("auth_refresh_required")));
            if let Some(refresh_val) = auth_refresh {
                if refresh_val == false || refresh_val.as_bool() == Some(false) {
                    tracing::info!("[AUTH] auth_refresh_required=false in fallback path, skipping reconnection");
                    needs_auth = false;
                }
            }
        }

        // Check for "error" field (string matching as last resort)

        // check standard error field
        if let Some(error) = json_value.get("error") {
            let msg = if let Some(m) = error.as_str() { m.to_string() } else { error.to_string() };
            error_msg = msg.clone();
            
            let error_lower = msg.to_lowercase();
            if error_lower.contains("authentication") 
                || error_lower.contains("unauthorized")
                || error_lower.contains("not authorized")
                || error_lower.contains("connection required")
                || error_lower.contains("not connected")
                || error_lower.contains("auth required") {
                needs_auth = true;
            }
        }

        // Check for http_error field (seen in user regression report)
        if !needs_auth {
            if let Some(http_error) = json_value.get("http_error") {
                let msg = if let Some(m) = http_error.as_str() { m.to_string() } else { http_error.to_string() };
                if error_msg.is_empty() { error_msg = msg.clone(); }
                
                let error_lower = msg.to_lowercase();
                if error_lower.contains("401") || error_lower.contains("unauthorized") {
                    tracing::info!("[AUTH] Detected http_error identifying 401/Unauthorized");
                    needs_auth = true;
                }
            }
        }

        // Check for ECODE AUTH_018 (specifically mentioned by user)
        if !needs_auth {
            if let Some(ecode) = json_value.get("ECODE").and_then(|v| v.as_str()) {
                if ecode == "AUTH_018" {
                    tracing::info!("[AUTH] Detected ECODE AUTH_018 (Oauth token not found)");
                    needs_auth = true;
                    if error_msg.is_empty() { error_msg = "OAuth token not found (AUTH_018)".to_string(); }
                }
            }
        }

        if needs_auth {
            if let Some(tk_slug) = toolkit_slug {
                tracing::info!("[POST-EXEC AUTH] Tool returned auth error for toolkit '{}', triggering connection flow", tk_slug);
                
                match self.initiate_connection(&tk_slug, &user_id).await {
                    Ok(result_msg) => {
                        if result_msg.contains("Authentication successful") {
                            // Auth succeeded - tell user to retry
                            return Ok(ToolExecuteResponse {
                                data: serde_json::Value::Null,
                                error: Some("Authentication successful! Please try the tool again.".to_string()),
                                successful: false,
                                log_id: None,
                                session_info: None,
                            });
                        } else {
                            // Browser opened but waiting for user
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
                        tracing::error!("Failed to initiate connection after auth error: {}", e);
                        // Fall through to return the original error
                    }
                }
            }
        }

        if !error_msg.is_empty() {
             return Ok(ToolExecuteResponse {
                data: Value::Null,
                error: Some(error_msg),
                successful: false,
                log_id: None,
                session_info: None,
            });
        }
        
        // Use the whole JSON as data if it doesn't match the specific structure
        // This is a fallback for when the API might return just the result
        Ok(ToolExecuteResponse {
            data: json_value,
            error: None,
            successful: status.is_success(),
            log_id: None,
            session_info: None,
        })
    }
    pub async fn initiate_connection(&self, toolkit_slug: &str, user_id: &str) -> Result<String, String> {
        let url = format!("{}/connected_accounts/link", self.get_api_base_url());
        
        // Use configured user_id if available, otherwise fall back to argument
        let final_user_id = self.user_id.clone().unwrap_or_else(|| user_id.to_string());
        
        // Always dynamically look up the auth config ID for this specific toolkit
        // This ensures each toolkit (Gmail, Google Docs, etc.) uses its own auth config
        let auth_config_id = self.get_auth_config_id(toolkit_slug).await?;

        // Find a random port for the callback
        let port = find_available_port().ok_or_else(|| "Failed to find available port for callback".to_string())?;
        let callback_url = format!("http://localhost:{}/callback", port);

        // Helper to log if write fails
        let _ = write_to_debug_file("composio_auth_req_payload.json", &format!("AuthConfig: {}, User: {}, Callback: {}", auth_config_id, final_user_id, callback_url));
        
        // Payload to create a connected account request
        let payload = serde_json::json!({
            "auth_config_id": auth_config_id, 
            "user_id": final_user_id,
            "callback_url": callback_url
        });
        
        let response = self.client.post(&url)
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let response_text = response.text().await.map_err(|e| e.to_string())?;
        
        let _ = write_to_debug_file("composio_auth_resp.json", &response_text);
        
        // Parse response to find redirect URL
        let json: Value = serde_json::from_str(&response_text).map_err(|e| e.to_string())?;
        
        // Check for redirectUrl at various locations in response structure
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
                        
                        // Check for connectedAccountId in params (both camelCase and snake_case)
                        // CRITICAL FIX: Immediately cache the ID so the next tool call can use it
                        if let Some(acc_id) = result.params.get("connectedAccountId")
                            .or_else(|| result.params.get("connected_account_id")) {
                            tracing::info!("Received connectedAccountId: {} for toolkit: {}", acc_id, toolkit_slug);
                            
                            // Immediately update the toolkit_account_map cache
                            {
                                let mut map = self.toolkit_account_map.write().unwrap();
                                map.insert(toolkit_slug.to_lowercase(), (acc_id.clone(), final_user_id.clone()));
                                tracing::info!("[HANDOFF] Cached connected_account_id '{}' for toolkit '{}' (user: {})", 
                                    acc_id, toolkit_slug, final_user_id);
                            }
                        }
                        
                        // Also refresh from REST API as a backup (may catch additional accounts)
                        let _ = self.list_connected_accounts().await;
                        
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
}

/// Adapter function to convert a ComposioTool to a standard rmcp::model::Tool
pub fn composio_to_rmcp_tool(composio_tool: &ComposioTool) -> Tool {
    // Prefer input_parameters or inputSchema if available, fall back to parameters
    let schema = if let Some(Value::Object(obj)) = &composio_tool.input_parameters {
        Arc::new(obj.clone())
    } else if let Some(Value::Object(obj)) = &composio_tool.input_schema {
        Arc::new(obj.clone())
    } else if let Some(Value::Object(obj)) = &composio_tool.parameters {
        Arc::new(obj.clone())
    } else {
        // rmcp::model::Tool expects a non-optional Arc, so we provide an empty map if schema is missing/invalid
        Arc::new(serde_json::Map::new())
    };

    // Create metadata with toolkit and version info
    let mut meta_map = serde_json::Map::new();
    if let Some(toolkit) = &composio_tool.toolkit {
        meta_map.insert("toolkit_slug".to_string(), serde_json::Value::String(toolkit.slug.clone()));
    }

    if let Some(version) = &composio_tool.version {
        meta_map.insert("version".to_string(), serde_json::Value::String(version.clone()));
    }

    // Create metadata with toolkit and version info
    let meta = if !meta_map.is_empty() {
        // Convert our map to a HashMap<String, String> for Meta
        let mut string_map = std::collections::HashMap::new();
        for (key, value) in meta_map {
            let string_value = match value {
                serde_json::Value::String(s) => s,
                _ => value.to_string(),
            };
            string_map.insert(key, string_value);
        }
        
        // Create a Meta object from our HashMap
        let mut meta_obj = rmcp::model::Meta::new();
        for (key, value) in string_map {
            meta_obj.insert(key, serde_json::Value::String(value));
        }
        Some(meta_obj)
    } else {
        None
    };

    Tool {
        name: composio_tool.slug.clone().unwrap_or_else(|| composio_tool.name.clone()).into(), // Use slug if available, else name
        description: composio_tool.description.clone().map(|s| s.into()),
        input_schema: schema,
        title: Some(composio_tool.name.clone().into()), // Use display name as title
        output_schema: None,
        annotations: None,
        icons: None,
        meta,
    }
}