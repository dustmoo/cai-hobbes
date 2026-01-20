use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

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
    #[serde(alias = "createdAt", alias = "created_at")]
    pub created_at: Option<String>,
    pub toolkit: Option<ConnectedAccountToolkit>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectedAccountToolkit {
    pub slug: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct ConnectedAccountsResponse {
    pub items: Vec<ConnectedAccount>,
}

/// Auth config information from GET /api/v3/auth_configs
/// Represents an authentication blueprint for a toolkit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfigInfo {
    /// Auth config ID (e.g., "ac_K5UVzcW8cJsX")
    pub id: String,
    /// Toolkit information
    #[serde(default)]
    pub toolkit: Option<AuthConfigToolkit>,
    /// Auth scheme (e.g., "OAUTH2", "API_KEY")
    #[serde(default)]
    pub auth_scheme: Option<String>,
    /// Whether this uses Composio's managed auth
    #[serde(default)]
    pub is_composio_managed: Option<bool>,
    /// Status (e.g., "ENABLED")
    #[serde(default)]
    pub status: Option<String>,
    /// Number of active connections using this config
    #[serde(default)]
    pub no_of_connections: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfigToolkit {
    pub slug: String,
    #[serde(default)]
    pub logo: Option<String>,
}

impl AuthConfigInfo {
    /// Get the toolkit slug, if available
    pub fn toolkit_slug(&self) -> Option<&str> {
        self.toolkit.as_ref().map(|t| t.slug.as_str())
    }
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
        self.display_name
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.slug.clone())
    }
}

// Response types for Composio API
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolListResponse {
    #[serde(default)]
    pub items: Vec<ComposioTool>,
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
    #[serde(rename = "totalPages", default)]
    pub total_pages: Option<i32>,
    // Add additional fields that might be in the response
    #[serde(rename = "tools", default)]
    pub tools: Option<Vec<ComposioTool>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// JSON-RPC 2.0 response format
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<T>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl ToolListResponse {
    // Helper method to get all tools, whether they're in 'items' or 'tools'
    pub fn get_all_tools(&self) -> Vec<ComposioTool> {
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
