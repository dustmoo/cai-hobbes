use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Resolved authentication strategy for a toolkit
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResolvedAuth {
    /// No authentication required (Instant Connect)
    NoAuth,
    /// Connect via Managed OAuth (Preferred Fallback)
    Managed,
    /// Connect via Local Credentials (BYOA Primacy)
    Byoa,
    /// Requires Setup (BYOA mandatory, no local keys)
    RequiresSetup,
}

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

/// Expected input field for authentication schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedInputField {
    /// Field name (e.g., "api_key", "client_id")
    pub name: String,
    /// Human-readable description
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the field is mandatory
    #[serde(default)]
    pub required: bool,
    /// Field type (e.g., "string", "password")
    #[serde(rename = "type", default)]
    pub field_type: Option<String>,
    /// Default value if any
    #[serde(default)]
    pub default: Option<Value>,
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
    /// Expected input fields for this auth config (BYOA schema)
    #[serde(default)]
    pub expected_input_fields: Option<Vec<ExpectedInputField>>,
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
    /// Authentication configuration (schema/inputs)
    #[serde(default)]
    pub auth_config: Option<AuthConfigInfo>,
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
    #[allow(dead_code)]
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
        let scheme = self.auth_schemes.as_ref()?.first().cloned();
        if scheme.as_deref() == Some("NO_AUTH") {
            None
        } else {
            scheme
        }
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

impl ToolExecuteResponse {
    /// Single authority for detecting auth errors in a Composio tool response.
    ///
    /// Checks all known patterns the Composio API uses to signal auth failures:
    /// 1. `data.status_code` — numeric 401/403
    /// 2. `data.statusCode` — string or numeric 401/403
    /// 3. `data.ECODE` — `AUTH_*` / `OAUTH_*` prefixes
    /// 4. `data.http_error` — substring "401"/"403"
    /// 5. `data.data.status_code` / `data.data.statusCode` — nested variants
    /// 6. `error` string — deterministic status code patterns only
    ///
    /// CYCLE GUARD: Returns false if `data.redirectUrl` is present — this means
    /// the response is our own generated auth redirect, not an upstream error.
    ///
    /// NOTE: This does NOT cover the Double-MCP `result.content[].text` path,
    /// which operates on the raw JSON-RPC envelope before deserialization.
    /// That check lives in `execution.rs` as a separate lifecycle stage.
    pub fn is_auth_error(&self) -> bool {
        if self.successful {
            return false;
        }

        let data = &self.data;

        // CYCLE GUARD: If the response already contains a redirectUrl, this is
        // our own generated auth redirect (from initiate_connection or
        // reconnect_toolkit). It is NOT a new upstream auth error.
        // NOTE: Do NOT check for `data.status` here — that field is ubiquitous
        // in Composio API responses (e.g. ConnectedAccount.status = "ACTIVE")
        // and would suppress real auth errors. (Review: 2026-02-25)
        if data.get("redirectUrl").is_some() {
            return false;
        }

        // 1. data.status_code (numeric) — HARD signal
        let status_code_num = data.get("status_code").is_some_and(|v| {
            v.as_u64().is_some_and(|n| n == 401 || n == 403)
                || v.as_i64().is_some_and(|n| n == 401 || n == 403)
        });

        // 2. data.statusCode (string "401"/"403" or numeric) — HARD signal
        let status_code_str = data.get("statusCode").is_some_and(|v| {
            v.as_str().is_some_and(|s| s == "401" || s == "403")
                || v.as_u64().is_some_and(|n| n == 401 || n == 403)
        });

        // 3. data.ECODE (AUTH_018, OAUTH_018, etc.) — HARD signal
        let ecode_match = data
            .get("ECODE")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.starts_with("AUTH_") || e.starts_with("OAUTH_"));

        // 4. data.http_error (substring) — HARD signal
        let http_error_match = data
            .get("http_error")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("401") || s.contains("403"));

        // 5. Nested data.data.status_code / data.data.statusCode — HARD signal
        let nested_status = data.get("data").is_some_and(|inner| {
            inner
                .get("status_code")
                .is_some_and(|v| v.as_u64().is_some_and(|n| n == 401 || n == 403))
                || inner.get("statusCode").is_some_and(|v| {
                    v.as_str().is_some_and(|s| s == "401" || s == "403")
                        || v.as_u64().is_some_and(|n| n == 401 || n == 403)
                })
        });

        // 6. Fallback: error string — SOFT signal (restricted patterns only)
        // CAUTION: Do NOT match on "Authentication required" — our own code
        // generates messages containing this string in auth redirect responses.
        // Only match on deterministic HTTP status codes in the error string.
        let error_str = self.error.as_deref().unwrap_or("");
        let error_fallback = error_str.contains("401") || error_str.contains("403 Forbidden");

        status_code_num
            || status_code_str
            || ecode_match
            || http_error_match
            || nested_status
            || error_fallback
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PaginatedToolResult {
    #[serde(alias = "items")]
    pub tools: Option<Vec<ComposioTool>>,
    #[serde(alias = "nextCursor", alias = "next_cursor")]
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a failed (`successful: false`) tool response with the given `data`
    /// payload and optional `error` string — the two inputs `is_auth_error`
    /// inspects.
    fn failed(data: Value, error: Option<&str>) -> ToolExecuteResponse {
        ToolExecuteResponse {
            data,
            error: error.map(|s| s.to_string()),
            successful: false,
            log_id: None,
            session_info: None,
        }
    }

    // ── is_auth_error: this is the single authority for auth-error detection
    // (P-007). If it under-detects, auth failures silently bypass the reconnect
    // lifecycle; if it over-detects, our own auth-redirect responses are
    // mistaken for upstream errors and trigger reconnect loops. Both failure
    // modes have shipped as production regressions, so each documented pattern
    // and guard gets a locked-in case here.

    #[test]
    fn successful_response_is_never_an_auth_error() {
        // The short-circuit must win even if the payload looks like a 401.
        let resp = ToolExecuteResponse {
            data: json!({ "status_code": 401 }),
            error: Some("401 Unauthorized".to_string()),
            successful: true,
            log_id: None,
            session_info: None,
        };
        assert!(!resp.is_auth_error());
    }

    #[test]
    fn pattern_1_numeric_status_code() {
        assert!(failed(json!({ "status_code": 401 }), None).is_auth_error());
        assert!(failed(json!({ "status_code": 403 }), None).is_auth_error());
        // Non-auth statuses must not match.
        assert!(!failed(json!({ "status_code": 200 }), None).is_auth_error());
        assert!(!failed(json!({ "status_code": 500 }), None).is_auth_error());
    }

    #[test]
    fn pattern_2_statuscode_string_or_numeric() {
        assert!(failed(json!({ "statusCode": "401" }), None).is_auth_error());
        assert!(failed(json!({ "statusCode": 403 }), None).is_auth_error());
        assert!(!failed(json!({ "statusCode": "200" }), None).is_auth_error());
    }

    #[test]
    fn pattern_3_ecode_auth_prefixes() {
        assert!(failed(json!({ "ECODE": "AUTH_018" }), None).is_auth_error());
        assert!(failed(json!({ "ECODE": "OAUTH_401" }), None).is_auth_error());
        // Non-auth ECODEs (e.g. rate limiting) must not trigger reconnect.
        assert!(!failed(json!({ "ECODE": "RATE_LIMIT" }), None).is_auth_error());
    }

    #[test]
    fn pattern_4_http_error_substring() {
        assert!(failed(json!({ "http_error": "HTTP 401 Unauthorized" }), None).is_auth_error());
        assert!(failed(json!({ "http_error": "403 Forbidden" }), None).is_auth_error());
        assert!(!failed(json!({ "http_error": "500 Internal Server Error" }), None).is_auth_error());
    }

    #[test]
    fn pattern_5_nested_data_status_code() {
        assert!(failed(json!({ "data": { "status_code": 403 } }), None).is_auth_error());
        assert!(failed(json!({ "data": { "statusCode": "401" } }), None).is_auth_error());
        assert!(failed(json!({ "data": { "statusCode": 403 } }), None).is_auth_error());
        assert!(!failed(json!({ "data": { "status_code": 404 } }), None).is_auth_error());
    }

    #[test]
    fn pattern_6_error_string_fallback() {
        assert!(failed(json!({}), Some("Request failed with 401")).is_auth_error());
        assert!(failed(json!({}), Some("403 Forbidden")).is_auth_error());
        assert!(!failed(json!({}), Some("500 Internal Server Error")).is_auth_error());
    }

    #[test]
    fn cycle_guard_our_own_redirect_is_not_an_error() {
        // A response carrying a redirectUrl is our own generated auth redirect,
        // not a fresh upstream failure — even alongside a 401 status code it
        // must return false, or reconnect would destroy the new credentials.
        let resp = failed(
            json!({ "redirectUrl": "https://composio.dev/auth", "status_code": 401 }),
            Some("401 Unauthorized"),
        );
        assert!(!resp.is_auth_error());
    }

    #[test]
    fn false_positive_guard_status_active_is_not_auth_error() {
        // `data.status` is ubiquitous in Composio payloads (e.g. account
        // status = "ACTIVE") and must never be read as an auth signal.
        assert!(!failed(json!({ "status": "ACTIVE" }), None).is_auth_error());
    }

    #[test]
    fn false_positive_guard_authentication_required_text_alone() {
        // Our own redirect messages contain "Authentication required"; the
        // error-string fallback matches only deterministic status codes, so
        // this phrase alone must not trip detection.
        assert!(!failed(json!({}), Some("Authentication required to continue")).is_auth_error());
    }

    #[test]
    fn no_signal_at_all_is_not_an_auth_error() {
        assert!(!failed(json!({ "message": "tool ran but returned nothing" }), None).is_auth_error());
        assert!(!failed(json!({}), None).is_auth_error());
    }

    // ── Pure accessor helpers ────────────────────────────────────────────────

    fn listing_from(value: Value) -> ComposioToolkitListing {
        serde_json::from_value(value).expect("valid toolkit listing fixture")
    }

    #[test]
    fn primary_auth_scheme_skips_no_auth() {
        let oauth = listing_from(json!({
            "slug": "gmail", "name": "Gmail", "auth_schemes": ["OAUTH2", "API_KEY"]
        }));
        assert_eq!(oauth.primary_auth_scheme().as_deref(), Some("OAUTH2"));

        let no_auth = listing_from(json!({
            "slug": "weather", "name": "Weather", "auth_schemes": ["NO_AUTH"]
        }));
        assert_eq!(no_auth.primary_auth_scheme(), None);

        let none = listing_from(json!({ "slug": "x", "name": "X" }));
        assert_eq!(none.primary_auth_scheme(), None);
    }

    #[test]
    fn supports_managed_auth_requires_nonempty_schemes() {
        let managed = listing_from(json!({
            "slug": "gmail", "name": "Gmail", "composio_managed_auth_schemes": ["OAUTH2"]
        }));
        assert!(managed.supports_managed_auth());

        let empty = listing_from(json!({
            "slug": "gmail", "name": "Gmail", "composio_managed_auth_schemes": []
        }));
        assert!(!empty.supports_managed_auth());

        let absent = listing_from(json!({ "slug": "gmail", "name": "Gmail" }));
        assert!(!absent.supports_managed_auth());
    }

    #[test]
    fn description_prefers_nested_meta() {
        let listing = listing_from(json!({
            "slug": "gmail", "name": "Gmail", "meta": { "description": "Send email" }
        }));
        assert_eq!(listing.description().as_deref(), Some("Send email"));

        let no_meta = listing_from(json!({ "slug": "gmail", "name": "Gmail" }));
        assert_eq!(no_meta.description(), None);
    }

    #[test]
    fn category_display_falls_back_through_name_then_slug() {
        let display = serde_json::from_value::<ComposioCategory>(
            json!({ "id": "prod", "name": "Productivity", "displayName": "Productivity Tools" }),
        )
        .unwrap();
        assert_eq!(display.display(), "Productivity Tools");

        let name_only =
            serde_json::from_value::<ComposioCategory>(json!({ "id": "prod", "name": "Productivity" }))
                .unwrap();
        assert_eq!(name_only.display(), "Productivity");

        let slug_only = serde_json::from_value::<ComposioCategory>(json!({ "id": "prod" })).unwrap();
        assert_eq!(slug_only.display(), "prod");
    }

    #[test]
    fn get_all_tools_merges_items_and_tools() {
        let resp: ToolListResponse = serde_json::from_value(json!({
            "items": [{ "name": "A" }],
            "tools": [{ "name": "B" }, { "name": "C" }]
        }))
        .unwrap();
        let names: Vec<_> = resp.get_all_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }
}
