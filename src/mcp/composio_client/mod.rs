pub mod auth;
pub mod constants;
pub mod context_store;
pub mod discovery;
pub mod execution;
pub mod meta;
pub mod models;
pub mod utils;

use serde_json::Value;
use crate::mcp::composio_client::discovery::DiscoveryResult;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// Re-export models and utils for convenience
pub use context_store::ContextStore;
pub use models::*;
pub use utils::*;

use reqwest::Client;

#[derive(Clone)]
pub struct ComposioClient {
    pub(crate) client: reqwest::Client,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) entity_id: Option<String>,
    pub user_id: Option<String>,
    // Cache of tool name -> toolkit slug
    pub(crate) tool_toolkit_map: Arc<RwLock<HashMap<String, String>>>,
    // Cache of toolkit slug -> auth_config_id for dynamic per-toolkit lookups
    pub(crate) auth_config_cache: Arc<RwLock<HashMap<String, String>>>,
    // Cache of toolkit slug -> account_id for dynamic per-toolkit lookups
    pub(crate) toolkit_account_map: Arc<RwLock<HashMap<String, String>>>,
    // Secure Context Store for tool-specific keys
    pub(crate) context_store: Arc<ContextStore>,
    /// Custom auth credentials map: Toolkit Slug -> Map of field names to values (ClientID, Secret, etc.)
    pub(crate) custom_auth_creds: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    /// Cached connected toolkit info for Status panel (ephemeral, invalidated on profile change)
    pub(crate) cached_toolkit_info: Arc<RwLock<Option<Vec<ToolkitInfo>>>>,
}

#[allow(dead_code)]
impl ComposioClient {
    /// Initialize a new ComposioClient.
    ///
    /// # Arguments
    /// * `api_key` - The API key for REST API access.
    /// * `base_url` - The base URL for the registry API.
    /// * `entity_id` - Optional entity ID for scoping.
    /// * `user_id` - Optional user ID for MCP proxy routing.
    /// * `profile_id` - The UUID of the active profile for context isolation.
    pub fn new(
        api_key: String,
        base_url: String,
        entity_id: Option<String>,
        user_id: Option<String>,
        profile_id: String,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();

        // Pattern 123: Initialize Profile-Scoped Context Store
        let context_store = Arc::new(ContextStore::new(&profile_id));

        Self {
            api_key,
            base_url,
            entity_id,
            user_id,
            client,
            tool_toolkit_map: Arc::new(RwLock::new(HashMap::new())),
            toolkit_account_map: Arc::new(RwLock::new(HashMap::new())),
            auth_config_cache: Arc::new(RwLock::new(HashMap::new())),
            custom_auth_creds: Arc::new(RwLock::new(HashMap::new())),
            context_store,
            cached_toolkit_info: Arc::new(RwLock::new(None)),
        }
    }

    /// Set custom authentication credentials for BYOA toolkits.
    pub fn set_custom_creds(&self, creds: HashMap<String, HashMap<String, String>>) {
        if let Ok(mut lock) = self.custom_auth_creds.write() {
            *lock = creds;
            tracing::info!("Updated custom auth credentials for {} toolkits", lock.len());
        } else {
            tracing::error!("Failed to acquire write lock for custom_auth_creds");
        }
    }

    pub(crate) fn get_api_base_url(&self) -> String {
        let base = self
            .base_url
            .split("/v3/mcp")
            .next()
            .unwrap_or(&self.base_url);
        let base = base.trim_end_matches('/');
        format!("{}/api/v3", base)
    }

    pub(crate) fn build_mcp_url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let full_base = if base.ends_with("/mcp") {
            format!("{}{}", base, path)
        } else {
            format!("{}/mcp{}", base, path)
        };

        if let Some(uid) = self.user_id.as_ref().or(self.entity_id.as_ref()) {
            let separator = if full_base.contains('?') { "&" } else { "?" };
            format!("{}{}user_id={}", full_base, separator, uid)
        } else {
            full_base
        }
    }

    // --- Auth Module Delegates ---

    pub async fn list_auth_configs(&self) -> Result<Vec<AuthConfigInfo>, String> {
        auth::list_auth_configs(self).await
    }

    pub(crate) async fn get_auth_config_id(&self, toolkit_slug: &str) -> Result<String, String> {
        auth::get_auth_config_id(self, toolkit_slug).await
    }

    pub(crate) async fn list_connected_accounts(&self) -> Result<Vec<ConnectedAccount>, String> {
        auth::list_connected_accounts(self).await
    }

    pub(crate) async fn create_auth_config(
        &self,
        toolkit_slug: &str,
        auth_scheme: Option<&str>,
        use_managed: bool,
    ) -> Result<String, String> {
        auth::create_auth_config(self, toolkit_slug, auth_scheme, use_managed).await
    }

    pub async fn initiate_connection(
        &self,
        toolkit_slug: &str,
        user_id: &str,
    ) -> Result<String, String> {
        auth::initiate_connection(self, toolkit_slug, user_id).await
    }

    // --- Discovery Module Delegates ---

    pub async fn list_tools(&self) -> Result<DiscoveryResult, String> {
        discovery::list_tools(self).await
    }

    pub async fn list_connected_toolkits(&self) -> Result<Vec<ToolkitInfo>, String> {
        discovery::list_connected_toolkits(self).await
    }

    pub async fn list_all_toolkits(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
        categories: Option<Vec<String>>,
        sort_by: Option<&str>,
    ) -> Result<(Vec<ComposioToolkitListing>, i32, Option<String>), String> {
        discovery::list_all_toolkits(self, search, cursor, limit, categories, sort_by).await
    }

    pub async fn list_toolkit_categories(&self) -> Result<Vec<ComposioCategory>, String> {
        discovery::list_toolkit_categories(self).await
    }

    pub async fn get_connected_toolkit_slugs(
        &self,
    ) -> Result<std::collections::HashSet<String>, String> {
        discovery::get_connected_toolkit_slugs(self).await
    }

    pub async fn get_toolkit_tools(&self, toolkit_slug: &str) -> Result<Vec<String>, String> {
        discovery::get_toolkit_tools(self, toolkit_slug).await
    }

    pub async fn get_toolkit_tools_detailed(
        &self,
        toolkit_slug: &str,
    ) -> Result<Vec<(String, Option<String>)>, String> {
        discovery::get_toolkit_tools_detailed(self, toolkit_slug).await
    }

    pub async fn search_tools(
        &self,
        query: &str,
        toolkit_slugs: &[String],
    ) -> Result<Vec<ComposioTool>, String> {
        discovery::search_tools(self, query, toolkit_slugs).await
    }

    pub async fn list_tools_for_session(
        &self,
        force_load_slugs: &[String],
    ) -> Result<discovery::DiscoveryResult, String> {
        discovery::list_tools_for_session(self, force_load_slugs).await
    }

    pub async fn list_tools_filtered(
        &self,
        apps: Option<&[String]>,
    ) -> Result<discovery::DiscoveryResult, String> {
        discovery::list_tools_filtered(self, apps).await
    }

    // --- Execution Module Delegates ---

    pub async fn execute_tool(
        &self,
        name: &str,
        args: Value,
    ) -> Result<ToolExecuteResponse, String> {
        execution::execute_tool(self, name, args).await
    }

    pub async fn add_toolkit_to_server(
        &self,
        toolkit_slug: &str,
        auth_config_id: &str,
        selected_tools: Option<Vec<String>>,
    ) -> Result<Option<String>, String> {
        execution::add_toolkit_to_server(self, toolkit_slug, auth_config_id, selected_tools).await
    }

    // --- Cache Methods ---

    /// Get cached toolkit info if available
    pub fn get_cached_toolkit_info(&self) -> Option<Vec<ToolkitInfo>> {
        self.cached_toolkit_info.read().ok()?.clone()
    }

    /// Set cached toolkit info
    pub fn set_cached_toolkit_info(&self, info: Vec<ToolkitInfo>) {
        if let Ok(mut cache) = self.cached_toolkit_info.write() {
            *cache = Some(info);
        }
    }

    /// Invalidate the toolkit info cache (call on profile change or toolkit install/remove)
    pub fn invalidate_toolkit_cache(&self) {
        if let Ok(mut cache) = self.cached_toolkit_info.write() {
            *cache = None;
            tracing::debug!("Invalidated toolkit info cache");
        }
    }
}
