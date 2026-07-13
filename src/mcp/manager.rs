use crate::components::shared::ToolCallStatus;
use crate::components::smithery_registry::SmitheryServerDetail;
use crate::context::permissions::{PermissionManager, PermissionStatus};
use crate::mcp::authenticated_sse::AuthenticatedClientError;
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use crate::mcp::tool_selection::{ToolCandidate, ToolSelectionRequest, TOOL_SELECTION_THRESHOLD};
use crate::settings::{Settings, SettingsManager};
use crate::SecretManagerTrait;
use dioxus::prelude::spawn;
use dioxus::prelude::Signal;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{CallToolRequestParam, CallToolResult, PaginatedRequestParam, Tool};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::sse_client::SseTransportError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::Mutex;

/// The bare server name for Composio native virtual clients.
pub const COMPOSIO_NATIVE_PREFIX: &str = "composio-native";

/// Check if a server name refers to a Composio native client
/// (bare `"composio-native"` or profiled `"composio-native:{id}"`).
pub fn is_composio_native(name: &str) -> bool {
    name == COMPOSIO_NATIVE_PREFIX || name.starts_with("composio-native:")
}

/// Canonical key for a profile-scoped Composio-native server in the servers map.
fn composio_server_key(profile_id: &str) -> String {
    format!("{}:{}", COMPOSIO_NATIVE_PREFIX, profile_id)
}

/// Resolve the servers-map key for a specific profile's Composio-native client.
/// Prefers the profile-scoped key, then a `profile_id` field match, then the
/// bare prefix, then any Composio client as a last resort.
///
/// Profile-scoping is mandatory: each connected profile has its own
/// `composio-native:{id}` entry, so a bare `find(is_composio_native)` returns an
/// arbitrary client and can operate on the wrong profile (e.g. reloading tools
/// for "Clearmirror" but binding them to "Puget").
fn active_composio_key(
    servers: &std::collections::HashMap<String, ActiveMcpClient>,
    profile_id: Option<&str>,
) -> Option<String> {
    if let Some(pid) = profile_id {
        let scoped = composio_server_key(pid);
        if servers.contains_key(&scoped) {
            return Some(scoped);
        }
        if let Some((k, _)) = servers
            .iter()
            .find(|(k, c)| is_composio_native(k) && c.profile_id.as_deref() == Some(pid))
        {
            return Some(k.clone());
        }
    }
    if servers.contains_key(COMPOSIO_NATIVE_PREFIX) {
        return Some(COMPOSIO_NATIVE_PREFIX.to_string());
    }
    servers.keys().find(|k| is_composio_native(k)).cloned()
}

/// Cache key under which a profile's on-demand Composio tools are stored in
/// `dynamic_composio_tools`. Uses the profile id, falling back to the bare
/// prefix for the legacy singleton (no profile). Mirrors the profile filter in
/// `get_mcp_context` so discovery under one profile stays out of another's context.
fn dyn_composio_key(profile_id: Option<&str>) -> String {
    profile_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| COMPOSIO_NATIVE_PREFIX.to_string())
}

/// Gemini's practical tool limit for FunctionDeclarations.
const GEMINI_TOOL_LIMIT: usize = 128;

/// Canonical server name for built-in Hobbes session-management tools
/// (`HOBBES_PAGE_RESULT`, `HOBBES_UPDATE_SCRATCHPAD`).
/// Registered in `McpManager::servers` so the AI can accurately introspect
/// all built-in tools and their origins.
pub const HOBBES_CORE_SERVER: &str = "hobbes-core";

/// Canonical server name for built-in on-demand MCP management tools
/// (`MCP_LOAD_SERVER_TOOLS`, `MCP_UNLOAD_SERVER_TOOLS`).
/// Registered in `McpManager::servers` so the AI can accurately introspect
/// all built-in tools and their origins.
pub const HOBBES_META_SERVER: &str = "hobbes-meta";



/// Get meta-tools for local MCP server on-demand loading.
/// These allow the AI to load/unload tools from servers set to "On-demand" mode.
fn get_local_meta_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "MCP_LOAD_SERVER_TOOLS".into(),
            description: Some(
                "Load all tools from a local MCP server that is in on-demand mode. \
                After calling this, the server's tools become available as native function calls \
                on the next turn. Use this when you need to use tools from a server marked [ON-DEMAND]."
                    .into(),
            ),
            input_schema: serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "The exact name of the on-demand server to load tools from (e.g., 'playwright', 'graphiti')"
                    }
                },
                "required": ["server_name"]
            }))
            .unwrap_or_default(),
            title: None,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "MCP_UNLOAD_SERVER_TOOLS".into(),
            description: Some(
                "Unload dynamically loaded tools from a local MCP server to free up context space. \
                Use this when you no longer need tools from a server you previously loaded."
                    .into(),
            ),
            input_schema: serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "The exact name of the server whose tools should be unloaded"
                    }
                },
                "required": ["server_name"]
            }))
            .unwrap_or_default(),
            title: None,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
    ]
}
/// Score a tool's relevance by name keywords for budget-aware selection.
/// Higher score = more likely to be included when over budget.
///
/// MAYBE: Consider exposing these priority weights in the Settings UI in the future.
/// For now, these defaults work well and exposing them risks user misconfiguration.
fn score_tool_relevance(name: &str) -> u32 {
    let upper = name.to_uppercase();
    // Root traversal / auth tools — critical for hierarchy navigation
    if upper.contains("_TEAM")
        || upper.contains("_WORKSPACE")
        || upper.contains("_ORGANIZATION")
        || upper.contains("_AUTH")
        || upper.contains("_SPACE")
    {
        return 100;
    }
    // Core read operations
    if upper.contains("_GET_")
        || upper.contains("_LIST_")
        || upper.contains("_SEARCH_")
        || upper.contains("_FIND_")
    {
        return 80;
    }
    // Core write operations
    if upper.contains("_CREATE_") || upper.contains("_ADD_") || upper.contains("_POST_") {
        return 60;
    }
    // Core update operations
    if upper.contains("_UPDATE_")
        || upper.contains("_SET_")
        || upper.contains("_EDIT_")
        || upper.contains("_MODIFY_")
    {
        return 40;
    }
    // Delete operations
    if upper.contains("_DELETE_") || upper.contains("_REMOVE_") {
        return 20;
    }
    // Everything else (specialized, admin, etc.)
    10
}

/// Check a failed Composio `ToolExecuteResponse` for 401/403 auth errors and attempt
/// auto-reconnection via OAuth. Returns `Some(CallToolResult)` if the auth flow
/// handled the error (success or auth-required URL), or `None` to fall through
/// to the normal error response path.
///
/// Pattern 150.8.1: On auth failure, busts the stale `toolkit_account_map` cache entry,
/// re-authenticates, and retries the tool call with the original arguments.
async fn try_auth_recovery(
    response: &crate::mcp::composio_client::models::ToolExecuteResponse,
    tool_name: &str,
    composio_client: &ComposioClient,
) -> Option<CallToolResult> {
    if response.successful {
        return None;
    }

    // Delegate auth detection to the single-authority method on ToolExecuteResponse.
    // This covers status_code, statusCode, ECODE, http_error, nested data, and error string.
    if !response.is_auth_error() {
        return None;
    }

    // CYCLE GUARD: If the response already contains a redirectUrl, it means execute_tool's
    // proactive auth check already triggered reconnect_toolkit and returned an OAuth URL.
    // We must NOT fire recovery again, or we'll enter a double-reconnect loop where
    // is_auth_error() matches our own "Authentication required" error message.
    if response.data.get("redirectUrl").is_some() {
        tracing::debug!(
            "[AUTH RECOVERY] Skipping — response is already an auth redirect from proactive check"
        );
        return None;
    }

    // Extract toolkit slug from tool name (first segment before _)
    let toolkit_slug =
        ComposioClient::normalize_toolkit_key(tool_name.split('_').next().unwrap_or(tool_name));

    tracing::info!("[AUTH RECOVERY] Auth error detected for '{}' (toolkit: '{}'), triggering 6-point reconnect", tool_name, toolkit_slug);

    // Use the full 6-point reconnect lifecycle:
    // 1. Hydrate auth_config_cache (finds existing configs)
    // 2. Resolve auth_config_id (cache → API → create)
    // 3. Delete stale ACTIVE connections + bust cache
    // 4. Initiate OAuth with force=true (opens browser)
    // 5. Re-patch MCP server
    // 6. Re-hydrate toolkit_account_map
    match composio_client.reconnect_toolkit(&toolkit_slug).await {
        Ok(result_msg) => {
            if result_msg.contains("Authentication successful") {
                // ANTI-PATTERN: Do NOT retry the tool call internally!
                // Because reconnect_toolkit uses force=true (which prunes connections),
                // an immediate internal retry that fails (e.g. due to proxy sync delay)
                // would return a 401 -> is_error: Some(true) -> causing the LLM to autonomously
                // retry -> triggering ANOTHER prune and ANOTHER browser popup in an infinite loop!
                // Returning `is_error: Some(false)` breaks the LLM's panic loop and allows token propagation.
                Some(CallToolResult {
                    content: vec![rmcp::model::Content::text(
                        "Authentication successful! Please try the tool again.".to_string(),
                    )],
                    is_error: Some(false),
                    structured_content: None,
                    meta: None,
                })
            } else {
                let url = result_msg
                    .split_whitespace()
                    .last()
                    .unwrap_or(&result_msg)
                    .to_string();
                let auth_msg = format!(
                    "Authentication required. Please connect your account: {}",
                    url
                );
                Some(CallToolResult {
                    content: vec![rmcp::model::Content::text(auth_msg)],
                    is_error: Some(true),
                    structured_content: None,
                    meta: None,
                })
            }
        }
        Err(e) => {
            tracing::error!("[AUTH RECOVERY] 6-point reconnect failed: {}", e);
            None // Fall through to return original error
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct McpServerConfig {
    #[serde(default)]
    pub name: String,
    pub command: Option<String>,
    pub uri: Option<String>,
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub always_allow: Vec<String>,
}

impl McpServerConfig {
    /// Minimal stub config for Composio virtual servers (no external command/URI).
    pub fn composio_stub(name: String) -> Self {
        Self {
            name,
            command: None,
            uri: None,
            args: None,
            description: String::new(),
            env: HashMap::new(),
            disabled: false,
            always_allow: Vec::new(),
        }
    }

    /// Minimal stub config for native virtual servers (image gen, etc.).
    pub fn native_stub(name: String, description: String) -> Self {
        Self {
            name,
            command: None,
            uri: None,
            args: None,
            description,
            env: HashMap::new(),
            disabled: false,
            always_allow: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct McpServersWrapper {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct McpContext {
    pub servers: Vec<McpServerContext>,
    /// Toolkit slugs connected to the active Composio profile (MCP-First, Section 6).
    /// Includes both force-loaded AND on-demand toolkits. Used by the skill executor
    /// to validate capabilities without false warnings for connected-but-not-loaded toolkits.
    #[serde(default)]
    pub connected_toolkit_slugs: Vec<String>,
}

impl McpContext {
    /// Enrich connected_toolkit_slugs from the active Composio profile's toolkit_configs.
    /// This ensures on-demand toolkit slugs are always present even when the runtime
    /// cache (from list_connected_toolkits) hasn't been hydrated yet.
    pub fn enrich_from_settings(&mut self, settings: &crate::settings::Settings) {
        if let Some(profile) = settings.get_active_profile() {
            let existing: HashSet<String> = self.connected_toolkit_slugs.iter().cloned().collect();
            for config in &profile.toolkit_configs {
                let slug = config.slug.to_lowercase();
                if !existing.contains(&slug) {
                    self.connected_toolkit_slugs.push(slug);
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct McpServerContext {
    pub name: String,
    pub description: String,
    pub tools: Vec<Tool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ServerStatus {
    Loaded,        // Green - fully working
    Error,         // Red - configured but failed to load
    Disabled,      // Gray - configured but disabled
    NeedsAuth,     // Yellow - server requires OAuth authentication
    NotConfigured, // Blue - needs initial setup (e.g., missing API key)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub status: ServerStatus,
    pub error_message: Option<String>,
    pub warning_message: Option<String>,
    /// Total tools available on this server (including on-demand tools not yet loaded)
    pub tools: usize,
    /// Tools actually injected into the LLM prompt this turn.
    /// For on-demand servers: only counts tools explicitly loaded via MCP_LOAD_SERVER_TOOLS.
    /// For normal servers: equals `tools`.
    pub loaded_tools: usize,
    pub resources: usize,
    pub prompts: usize,
    /// Whether tools from this server are visible to the AI (false = unloaded at runtime)
    pub is_loaded: bool,
    /// Whether this server is in on-demand mode (tools available via MCP_LOAD_SERVER_TOOLS)
    pub is_on_demand: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl McpServerStatus {
    /// Create a new status with the given identity and status. All other fields
    /// default to zero/None/false. Use struct update syntax to override specifics:
    /// ```ignore
    /// McpServerStatus { tools: 5, is_loaded: true, ..McpServerStatus::new(name, desc, status) }
    /// ```
    pub fn new(name: String, description: String, status: ServerStatus) -> Self {
        Self {
            display_name: name.clone(),
            name,
            description,
            status,
            error_message: None,
            warning_message: None,
            tools: 0,
            loaded_tools: 0,
            resources: 0,
            prompts: 0,
            is_loaded: false,
            is_on_demand: false,
            auth_url: None,
            uri: None,
            profile: None,
        }
    }
}

#[derive(Clone)]
pub enum McpClientType {
    Service(Arc<RunningService<RoleClient, ()>>),
    NativeComposio(Arc<crate::mcp::composio_client::ComposioClient>),
    NativeImage(Arc<crate::mcp::image_client::ImageClient>),
    /// Built-in Hobbes session tools (HOBBES_PAGE_RESULT, HOBBES_UPDATE_SCRATCHPAD).
    /// Dispatch is intercepted upstream in stream_manager.rs (requires SessionState).
    /// This variant exists so the server is visible / introspectable in McpManager.
    NativeCore,
    /// Built-in on-demand MCP management tools (MCP_LOAD_SERVER_TOOLS, MCP_UNLOAD_SERVER_TOOLS).
    /// Dispatch is intercepted in use_mcp_tool() before the match arm runs.
    /// This variant exists so the server is visible / introspectable in McpManager.
    NativeMeta,
}

impl McpClientType {
    /// Check if a service's transport is still alive.
    /// Native clients don't use child-process transports, so they're always "healthy".
    fn is_healthy(&self) -> bool {
        match self {
            McpClientType::Service(arc) => !arc.is_transport_closed(),
            McpClientType::NativeComposio(_)
            | McpClientType::NativeImage(_)
            | McpClientType::NativeCore
            | McpClientType::NativeMeta => true,
        }
    }
}

#[derive(Clone)]
pub struct ActiveMcpClient {
    pub config: McpServerConfig,
    pub service: McpClientType,
    pub tools: Vec<Tool>,
    pub warning_message: Option<String>,
    /// Optional Profile ID for native clients (e.g. "composio-native:{id}")
    pub profile_id: Option<String>,
}

/// Information about a server that requires authentication
#[derive(Clone)]
pub struct AuthRequiredInfo {
    #[allow(dead_code)]
    pub config: McpServerConfig,
    pub auth_url: Option<String>,
    pub error_message: String,
    #[allow(dead_code)]
    pub profile: Option<String>,
}

#[derive(Clone)]
pub struct McpManager {
    config_path: Option<PathBuf>,
    pub servers: Arc<Mutex<HashMap<String, ActiveMcpClient>>>,
    pub failed_servers: Arc<Mutex<HashMap<String, (McpServerConfig, String)>>>,
    /// Servers that require OAuth authentication
    pub auth_required_servers: Arc<Mutex<HashMap<String, AuthRequiredInfo>>>,
    /// Servers whose tools are hidden from the AI (runtime-only state)
    pub unloaded_servers: Arc<Mutex<HashSet<String>>>,
    permission_manager: Signal<PermissionManager>,
    /// Cached server statuses for Status panel (ephemeral, invalidated on profile change)
    cached_server_statuses: Arc<Mutex<Option<Vec<McpServerStatus>>>>,
    /// Shared SecretManager for non-blocking credential access
    secret_manager: Signal<crate::secret_manager::SecretManager>,
    /// Shared settings signal for Pattern 30: read image model etc. at call time
    settings: Signal<crate::settings::Settings>,
    /// Dynamically discovered Composio tools (populated by COMPOSIO_GET_APP_TOOLS),
    /// keyed by Composio profile id (see `dyn_composio_key`). Included as a virtual
    /// server in get_mcp_context() so the prompt builder sends them to Gemini as
    /// real FunctionDeclarations. Profile-keyed so tools discovered under one
    /// profile don't bleed into another profile's context across tabs.
    pub dynamic_composio_tools: Arc<Mutex<HashMap<String, Vec<rmcp::model::Tool>>>>,
    /// Servers whose tools are available on-demand via MCP_LOAD_SERVER_TOOLS meta-tool.
    /// Servers in this set have their process running but tools are NOT included in get_mcp_context()
    /// unless explicitly loaded via the meta-tool into dynamic_local_tools.
    pub on_demand_servers: Arc<Mutex<HashSet<String>>>,
    /// Dynamically loaded local MCP tools (populated by MCP_LOAD_SERVER_TOOLS).
    /// Mirrors dynamic_composio_tools but for local MCP servers in on-demand mode.
    pub dynamic_local_tools: Arc<Mutex<Vec<rmcp::model::Tool>>>,
    /// Reverse-lookup map: tool_name → origin server name.
    /// Populated by MCP_LOAD_SERVER_TOOLS so that use_mcp_tool can resolve the
    /// virtual "local-on-demand" server back to the real server for execution.
    pub dynamic_local_tool_sources: Arc<Mutex<HashMap<String, String>>>,
}

impl McpManager {
    pub fn new(
        config_path: PathBuf,
        permission_manager: Signal<PermissionManager>,
        secret_manager: Signal<crate::secret_manager::SecretManager>,
        settings: Signal<crate::settings::Settings>,
    ) -> Self {
        Self {
            servers: Arc::new(Mutex::new(HashMap::new())),
            failed_servers: Arc::new(Mutex::new(HashMap::new())),
            auth_required_servers: Arc::new(Mutex::new(HashMap::new())),
            unloaded_servers: Arc::new(Mutex::new(HashSet::new())),
            permission_manager,
            config_path: Some(config_path),
            cached_server_statuses: Arc::new(Mutex::new(None)),
            secret_manager,
            settings,
            dynamic_composio_tools: Arc::new(Mutex::new(HashMap::new())),
            on_demand_servers: Arc::new(Mutex::new(HashSet::new())),
            dynamic_local_tools: Arc::new(Mutex::new(Vec::new())),
            dynamic_local_tool_sources: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Helper: Inject custom credentials from SecretManager (BYOA)
    fn inject_custom_credentials(
        client: &ComposioClient,
        profile_id: &str,
        secret_manager: &Signal<crate::secret_manager::SecretManager>,
    ) {
        // Inject Custom Tool Credentials (BYOA) - Read from memory cache (INSTANT)
        // This avoids blocking keychain I/O on the main thread
        // Pattern 153: Scope injected credentials to the target profile
        let custom_creds = secret_manager
            .peek()
            .get_custom_tool_credentials(Some(profile_id));

        if !custom_creds.is_empty() {
            tracing::info!(
                "Injecting {} custom tool credentials for profile '{}'",
                custom_creds.len(),
                profile_id
            );
            client.set_custom_creds(custom_creds);
        }
    }

    /// Helper: Initialize a native Composio client for a specific profile.
    pub async fn initialize_native_composio_for_profile(
        &self,
        profile: &crate::settings::ComposioProfile,
    ) -> Result<ActiveMcpClient, String> {
        let Some(api_key) = profile.api_key.clone() else {
            return Err("No Composio API key configured".to_string());
        };

        let base_url = profile
            .base_url
            .clone()
            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
        // Guard: bare /v3/mcp URL has no server UUID — MCP endpoint will 10401
        let has_server_uuid = profile.base_url.as_ref()
            .is_some_and(|u| u.contains("/v3/mcp/"));
        let entity_id = profile.entity_id.clone();
        let user_id = profile.user_id.clone();
        let force_load_slugs = profile.get_force_load_toolkit_slugs();
        let profile_id = profile.id.clone();

        tracing::info!(
            "Initializing native Composio client for profile '{}' (ID: {}). API Key: {}... Entity: {:?}, User: {:?}, has_server_uuid: {}",
            profile.name,
            profile_id,
            api_key.get(..8).unwrap_or(""),
            entity_id,
            user_id,
            has_server_uuid
        );

        let composio_client = Arc::new(ComposioClient::new(
            api_key,
            base_url,
            entity_id,
            user_id,
            profile_id,
            profile.chrome_profile_directory.clone(),
        ));

        // Inject credentials
        Self::inject_custom_credentials(&composio_client, &profile.name, &self.secret_manager);

        let description = "Composio Integration Hub - provides access to 100+ external apps (Gmail, GitHub, Slack, etc.) via a Tool Router workflow. Use COMPOSIO_DISCOVER_APPS to search for apps, then COMPOSIO_GET_APP_TOOLS to list that app's tools, then COMPOSIO_EXECUTE_TOOL to run a specific action. Force-loaded toolkits have their tools available directly.".to_string();

        let server_name = format!("{}:{}", COMPOSIO_NATIVE_PREFIX, profile.name); // Using name for UI display in LLM prompt

        let mut composio_config = McpServerConfig::composio_stub(server_name);
        composio_config.description = description;

        // Only call list_tools (MCP endpoint) when we have a valid server UUID in the URL.
        // Without a UUID, the bare /v3/mcp URL causes 10401 "MCP server not found".
        // Meta-tools are always loaded so the user can still connect toolkits.
        let discovery_result = if has_server_uuid {
            composio_client
                .list_tools_for_session(&force_load_slugs)
                .await
                .map_err(|e| format!("Failed to list Composio tools: {}", e))?
        } else {
            tracing::warn!(
                "No MCP server UUID in base_url for profile '{}' — loading meta-tools only. \
                 Connect a toolkit via Marketplace to resolve.",
                profile.name
            );
            use crate::mcp::composio_client::discovery::DiscoveryResult;
            DiscoveryResult {
                tools: crate::mcp::composio_client::meta::get_meta_tools(),
                warning: Some("No MCP server configured. Connect a toolkit to get started.".into()),
            }
        };

        // Convert Composio tools to rmcp::model::Tool for the prompt builder.
        // This conversion appears at multiple lifecycle stages (init, hot-reload,
        // dynamic injection, marketplace connect, auth recovery) — each call site
        // operates on a different tool set, so consolidation is not beneficial.
        let tools = discovery_result
            .tools
            .iter()
            .map(composio_to_rmcp_tool)
            .collect();

        Ok(ActiveMcpClient {
            config: composio_config,
            service: McpClientType::NativeComposio(composio_client),
            tools,
            warning_message: discovery_result.warning,
            profile_id: Some(profile.id.clone()),
        })
    }

    /// Ensure a native Composio client is loaded for a specific profile name or ID.
    /// If not loaded, it fetches the profile from settings and initializes it.
    /// This now supports both Name-based (legacy) and ID-based lookups, prioritizing ID.
    pub async fn ensure_native_client_for_profile(
        &self,
        name_or_id: &str,
        settings: &Settings,
    ) -> Result<(), String> {
        // Resolve to a stable ID first
        let profile = settings
            .composio_profiles
            .iter()
            .find(|p| p.id == name_or_id)
            .or_else(|| {
                settings
                    .composio_profiles
                    .iter()
                    .find(|p| p.name == name_or_id)
            })
            .ok_or_else(|| format!("Composio profile '{}' not found in settings", name_or_id))?;

        let server_key = composio_server_key(&profile.id);

        {
            let servers = self.servers.lock().await;
            if servers.contains_key(&server_key) {
                return Ok(());
            }
        }

        let client = self.initialize_native_composio_for_profile(profile).await?;

        {
            let mut servers = self.servers.lock().await;
            servers.insert(server_key, client);
        }

        Ok(())
    }

    /// Initialize unloaded servers from persisted state
    pub async fn set_initial_unloaded_servers(&self, servers: Vec<String>) {
        let mut unloaded = self.unloaded_servers.lock().await;
        for server in servers {
            unloaded.insert(server);
        }
        tracing::debug!(
            "Restored {} unloaded servers from persisted state",
            unloaded.len()
        );
    }

    /// Initialize on-demand servers from persisted state
    pub async fn set_initial_on_demand_servers(&self, servers: Vec<String>) {
        let mut on_demand = self.on_demand_servers.lock().await;
        for server in servers {
            on_demand.insert(server);
        }
        tracing::debug!(
            "Restored {} on-demand servers from persisted state",
            on_demand.len()
        );
    }

    /// Set a server to on-demand mode (tools hidden, discoverable via meta-tool)
    pub async fn set_server_on_demand(&self, server_name: &str) {
        // Remove from unloaded if present
        {
            let mut unloaded = self.unloaded_servers.lock().await;
            unloaded.remove(server_name);
        }
        // Add to on-demand
        {
            let mut on_demand = self.on_demand_servers.lock().await;
            on_demand.insert(server_name.to_string());
        }
        // Clear any dynamically loaded tools for this server
        self.clear_dynamic_tools_for(server_name).await;
        self.invalidate_status_cache();
        tracing::info!(
            "Set server '{}' to on-demand mode - tools discoverable via MCP_LOAD_SERVER_TOOLS",
            server_name
        );
    }

    /// Set a server back to loaded mode (all tools in every prompt)
    pub async fn set_server_loaded(&self, server_name: &str) {
        // Remove from on-demand
        {
            let mut on_demand = self.on_demand_servers.lock().await;
            on_demand.remove(server_name);
        }
        // Remove from unloaded
        {
            let mut unloaded = self.unloaded_servers.lock().await;
            unloaded.remove(server_name);
        }
        // Clear dynamically loaded tools for this server (they'll now be in the main set)
        self.clear_dynamic_tools_for(server_name).await;
        self.invalidate_status_cache();
        tracing::info!(
            "Set server '{}' to loaded mode - all tools visible in every prompt",
            server_name
        );
    }

    /// Remove dynamically loaded tools belonging to `server_name` from the
    /// `dynamic_local_tools` cache and `dynamic_local_tool_sources` reverse-lookup.
    ///
    /// LOCK ORDERING INVARIANT (P-010): Always acquire `servers` before
    /// `dynamic_local_tools` to match `call_tool`'s acquisition order
    /// and prevent ABBA deadlocks.
    async fn clear_dynamic_tools_for(&self, server_name: &str) {
        let servers = self.servers.lock().await;
        let mut dynamic = self.dynamic_local_tools.lock().await;
        let mut sources = self.dynamic_local_tool_sources.lock().await;
        if let Some(client) = servers.get(server_name) {
            let server_tool_names: HashSet<String> =
                client.tools.iter().map(|t| t.name.to_string()).collect();
            dynamic.retain(|t| !server_tool_names.contains(t.name.as_ref()));
            for name in &server_tool_names {
                sources.remove(name);
            }
        }
    }

    /// Reloads the MCP configuration and restarts servers.
    /// This is used for hot-reloading when mcp_servers.json is modified.
    pub async fn reload_config(
        &self,
        mcp_context_signal: dioxus::prelude::Signal<McpContext>,
        settings: crate::settings::Settings,
    ) {
        tracing::info!("Reloading MCP configuration...");

        // 1. Stop and clear existing servers
        // Dropping the active clients should effectively stop the running services
        {
            let mut servers = self.servers.lock().await;
            // We want to keep composio-native if it's managed separately,
            // but launch_servers re-initializes it based on settings anyway.
            // So clearing everything is safer to avoid duplicates.
            servers.clear();
            tracing::info!("Cleared existing MCP servers for reload.");
        }

        // Clear dynamic tool caches
        {
            let mut dynamic = self.dynamic_composio_tools.lock().await;
            if !dynamic.is_empty() {
                let tool_count: usize = dynamic.values().map(|v| v.len()).sum();
                tracing::info!(
                    "Clearing {} stale dynamic Composio tools on config reload",
                    tool_count
                );
                dynamic.clear();
            }
        }
        {
            let mut dynamic_local = self.dynamic_local_tools.lock().await;
            let mut sources = self.dynamic_local_tool_sources.lock().await;
            if !dynamic_local.is_empty() {
                tracing::info!(
                    "Clearing {} stale dynamic local tools on config reload",
                    dynamic_local.len()
                );
                dynamic_local.clear();
                sources.clear();
            }
        }

        // 2. Clear failed servers map
        self.failed_servers.lock().await.clear();

        // 3. Flush stale status cache (Pattern 150.3B) so next Status tab
        //    fetch rebuilds from the new backend state.
        self.invalidate_status_cache_async().await;

        // 4. Launch servers with new config
        self.launch_servers(mcp_context_signal, settings).await;

        tracing::info!("MCP configuration reload initiated.");
    }

    pub async fn reinitialize_composio_client(
        &self,
        mut mcp_context_signal: dioxus::prelude::Signal<McpContext>,
        settings: crate::settings::Settings,
        profile_id: Option<String>,
    ) {
        // Pattern 123: Identity Trinity - force specific profile lookup
        let profile = if let Some(ref val) = profile_id {
            settings
                .composio_profiles
                .iter()
                .find(|p| &p.id == val || &p.name == val)
                .cloned()
        } else {
            settings.get_active_profile().cloned()
        };

        let Some(profile) = profile else {
            tracing::warn!(
                "Failed to find profile for reinitialization: {:?}",
                profile_id
            );
            return;
        };

        // Use consistent key format with profile ID for stability
        let server_key = composio_server_key(&profile.id);

        // EAGER CLEANUP: Remove ALL composio-native clients immediately (Pattern 151)
        // This ensures that during the initialization window (which may involve network calls),
        // we are not "accidentally" picking up stale clients from the old profile.
        {
            let mut servers = self.servers.lock().await;
            // Clear specific target key if exists
            servers.remove(&server_key);

            // Also clear by name for legacy cleanup if needed
            let name_key = format!("{}:{}", COMPOSIO_NATIVE_PREFIX, profile.name);
            servers.remove(&name_key);

            let native_keys: Vec<String> = servers
                .keys()
                .filter(|k| is_composio_native(k))
                .cloned()
                .collect();
            for k in native_keys {
                servers.remove(&k);
            }
            tracing::info!("Cleared existing Composio native clients for reinitialization.");
        }

        // Clear dynamic tool caches for the profile being reinitialized only —
        // other profiles' on-demand tools (e.g. in other tabs) must survive.
        {
            let mut dynamic = self.dynamic_composio_tools.lock().await;
            if let Some(removed) = dynamic.remove(&dyn_composio_key(Some(&profile.id))) {
                tracing::info!(
                    "Clearing {} stale dynamic Composio tools for reinitialized profile '{}'",
                    removed.len(),
                    profile.id
                );
            }
        }
        {
            let mut dynamic_local = self.dynamic_local_tools.lock().await;
            let mut sources = self.dynamic_local_tool_sources.lock().await;
            if !dynamic_local.is_empty() {
                tracing::info!(
                    "Clearing {} stale dynamic local tools from previous profile",
                    dynamic_local.len()
                );
                dynamic_local.clear();
                sources.clear();
            }
        }

        tracing::info!(
            "Profile switch in progress for '{}'. Composio tools temporarily unavailable.",
            profile.name
        );

        match self.initialize_native_composio_for_profile(&profile).await {
            Ok(active_client) => {
                // Insert the new client cleanly
                {
                    let mut servers = self.servers.lock().await;
                    servers.insert(server_key.clone(), active_client);
                    self.failed_servers.lock().await.remove(&server_key);
                }

                self.invalidate_status_cache_async().await;
                mcp_context_signal.set(self.get_mcp_context(Some(profile.id.clone())).await);
                tracing::info!(
                    "Successfully reinitialized Composio client for profile: {}",
                    profile.name
                );
            }
            Err(e) => {
                let error_msg = format!("Failed to reinitialize Composio: {}", e);
                {
                    let mut servers = self.servers.lock().await;
                    // Remove ALL composio-native clients
                    let native_keys: Vec<String> = servers
                        .keys()
                        .filter(|k| is_composio_native(k))
                        .cloned()
                        .collect();
                    for key in native_keys {
                        servers.remove(&key);
                    }

                    let mut dummy_config = McpServerConfig::composio_stub(server_key.clone());
                    dummy_config.description = "Composio Integration Hub".to_string();
                    self.failed_servers
                        .lock()
                        .await
                        .insert(server_key.clone(), (dummy_config, error_msg.clone()));
                }

                if Self::is_needs_setup_error(&error_msg) {
                    tracing::debug!("Composio needs setup: {}", e);
                } else {
                    tracing::error!("{}", error_msg);
                }

                mcp_context_signal.set(self.get_mcp_context(Some(profile.id)).await);
            }
        }
    }

    /// Check if an error message indicates authentication is required
    fn is_auth_error(error_msg: &str) -> bool {
        let lower_msg = error_msg.to_lowercase();
        lower_msg.contains("401")
            || lower_msg.contains("unauthorized")
            || lower_msg.contains("invalid_token")
            || lower_msg.contains("authentication required")
            || lower_msg.contains("not authenticated")
    }

    /// Check if an error indicates the user needs to complete initial setup
    /// (e.g., connect their first tool through the marketplace)
    fn is_needs_setup_error(error_msg: &str) -> bool {
        let lower_msg = error_msg.to_lowercase();
        // 405 Method Not Allowed often means no tools/server configured yet
        lower_msg.contains("405")
            || lower_msg.contains("method not allowed")
            || lower_msg.contains("no tools")
            || lower_msg.contains("empty server")
            || lower_msg.contains("server not found")
    }

    /// Construct a sane PATH for child processes, including common dev directories
    fn get_sane_path() -> String {
        let mut paths = vec![
            "/usr/local/bin".to_string(),
            "/opt/homebrew/bin".to_string(),
            "/usr/bin".to_string(),
            "/bin".to_string(),
            "/usr/sbin".to_string(),
            "/sbin".to_string(),
        ];

        // Add cargo bin and local bin if they exist
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".cargo/bin").to_string_lossy().to_string());
            paths.push(home.join(".local/bin").to_string_lossy().to_string());
        }

        paths.join(":")
    }

    /// Get critical environment variables needed for child processes.
    /// When launched from Finder/open, the app gets a minimal environment from launchd
    /// that's missing HOME, USER, SHELL, etc. which tools like `uvx` require.
    fn get_critical_env_vars() -> HashMap<String, String> {
        let mut vars = HashMap::new();

        // HOME is critical for uvx/uv to find its cache
        if let Some(home) = dirs::home_dir() {
            vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        }

        // USER is needed by some tools
        if let Ok(user) = std::env::var("USER") {
            vars.insert("USER".to_string(), user);
        } else if let Some(home) = dirs::home_dir() {
            // Fallback: extract username from home path
            if let Some(username) = home.file_name() {
                vars.insert("USER".to_string(), username.to_string_lossy().to_string());
            }
        }

        // SHELL - default to zsh on macOS if not set
        if let Ok(shell) = std::env::var("SHELL") {
            vars.insert("SHELL".to_string(), shell);
        } else {
            vars.insert("SHELL".to_string(), "/bin/zsh".to_string());
        }

        // TMPDIR - some tools need this
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            vars.insert("TMPDIR".to_string(), tmpdir);
        }

        vars
    }

    pub async fn launch_servers(
        &self,
        mcp_context_signal: dioxus::prelude::Signal<McpContext>,
        settings: crate::settings::Settings,
    ) {
        let configs = if let Some(config_path) = &self.config_path {
            self.load_configs(config_path.clone()).await
        } else {
            Vec::new()
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ActiveMcpClient>();
        let servers_map_clone = self.servers.clone();
        let self_clone_for_receiver = self.clone();
        let mut mcp_context_signal_clone_for_receiver = mcp_context_signal;

        // Spawn a dedicated receiver task to serialize context updates
        spawn(async move {
            while let Some(active_client) = rx.recv().await {
                // Determine the storage key for the servers map.
                // For native Composio, use the stable ID if available.
                // For standard servers, use the config name.
                let storage_key = if let Some(ref pid) = active_client.profile_id {
                    composio_server_key(pid)
                } else {
                    active_client.config.name.clone()
                };

                let display_name = active_client.config.name.clone();
                tracing::info!(
                    "Received initialized client for: {} (Storage Key: {})",
                    display_name,
                    storage_key
                );

                // Lock and insert the new client
                servers_map_clone
                    .lock()
                    .await
                    .insert(storage_key, active_client);

                // Invalidate status cache BEFORE updating context signal (Pattern 150.8)
                self_clone_for_receiver
                    .invalidate_status_cache_async()
                    .await;

                // Get the full, updated context and set the signal
                let new_context = self_clone_for_receiver.get_mcp_context(None).await;
                mcp_context_signal_clone_for_receiver.set(new_context);
                tracing::info!(
                    "Successfully added '{}' and updated MCP context atomically.",
                    display_name
                );
            }
            tracing::info!("MCP context update receiver task finished.");
        });

        // Initialize the native Composio client if an active profile is configured.
        // This is decoupled from mcp_servers.json - the native client is ephemeral.
        if let Some(profile) = settings.get_active_profile() {
            let tx_clone = tx.clone();
            let profile_clone = profile.clone();
            let self_clone = self.clone();

            spawn(async move {
                tracing::info!(
                    "Initializing virtual Composio client for profile '{}'",
                    profile_clone.name
                );
                match self_clone
                    .initialize_native_composio_for_profile(&profile_clone)
                    .await
                {
                    Ok(active_client) => {
                        if tx_clone.send(active_client).is_err() {
                            tracing::error!("Failed to send initialized virtual Composio client");
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to initialize Composio: {}", e);
                        if McpManager::is_needs_setup_error(&error_msg) {
                            tracing::debug!("Composio needs initial setup: {}", e);
                        } else {
                            tracing::error!("{}", error_msg);
                        }
                    }
                }
            });
        }

        // Initialize Native Image Client
        {
            let tx_clone = tx.clone();

            spawn(async move {
                tracing::info!("Initializing virtual Image Generation client");

                let image_client = Arc::new(crate::mcp::image_client::ImageClient::new());

                let tools = image_client.list_tools();

                let config = McpServerConfig::native_stub(
                    "hobbes-native-image".to_string(),
                    "Native integration for Image Generation models.".to_string(),
                );

                let active_client = ActiveMcpClient {
                    config,
                    service: McpClientType::NativeImage(image_client),
                    tools,
                    warning_message: None,
                    profile_id: None,
                };

                if tx_clone.send(active_client).is_err() {
                    tracing::error!("Failed to send initialized virtual Image client");
                }
            });
        }

        // Initialize Native Core Client (HOBBES_PAGE_RESULT, HOBBES_UPDATE_SCRATCHPAD)
        {
            let tx_clone = tx.clone();
            spawn(async move {
                tracing::info!("Initializing Hobbes Core built-in tools server");
                let core_client = Arc::new(crate::mcp::core_client::CoreClient::new());
                let tools = core_client.list_tools();
                let config = McpServerConfig::native_stub(
                    HOBBES_CORE_SERVER.to_string(),
                    "Built-in Hobbes session tools (scratchpad, pagination).".to_string(),
                );
                let active_client = ActiveMcpClient {
                    config,
                    service: McpClientType::NativeCore,
                    tools,
                    warning_message: None,
                    profile_id: None,
                };
                if tx_clone.send(active_client).is_err() {
                    tracing::error!("Failed to send initialized Hobbes Core client");
                }
            });
        }

        // Initialize Native Meta Client (MCP_LOAD_SERVER_TOOLS, MCP_UNLOAD_SERVER_TOOLS)
        {
            let tx_clone = tx.clone();
            spawn(async move {
                tracing::info!("Initializing Hobbes Meta built-in tools server");
                let meta_tools = get_local_meta_tools();
                let config = McpServerConfig::native_stub(
                    HOBBES_META_SERVER.to_string(),
                    "Built-in tools to load/unload on-demand MCP server tools.".to_string(),
                );
                let active_client = ActiveMcpClient {
                    config,
                    service: McpClientType::NativeMeta,
                    tools: meta_tools,
                    warning_message: None,
                    profile_id: None,
                };
                if tx_clone.send(active_client).is_err() {
                    tracing::error!("Failed to send initialized Hobbes Meta client");
                }
            });
        }

        // Skip composio-native in the config loop — it is initialized via the
        // dedicated `initialize_native_composio_for_profile` path above (lines 537-559).
        for server_config in configs
            .iter()
            .filter(|sc| !sc.disabled && sc.name != COMPOSIO_NATIVE_PREFIX)
        {
            let mut server_config_clone = server_config.clone();
            if let Some(key) = &settings.smithery_api_key {
                server_config_clone
                    .env
                    .insert("SMITHERY_API_KEY".to_string(), key.trim().to_string());
            }
            // Composio API Key is handled by inserting it into env or as a header
            if let Some(key) = &settings.composio_api_key {
                server_config_clone
                    .env
                    .insert("COMPOSIO_API_KEY".to_string(), key.trim().to_string());
            }
            let tx_clone = tx.clone();
            let settings_clone = settings.clone();
            let failed_servers_clone = self.failed_servers.clone();
            let auth_required_servers_clone = self.auth_required_servers.clone();
            let secret_manager = self.secret_manager; // Capture signal (Copy) for the closure

            spawn(async move {
                let server_name = server_config_clone.name.clone();
                tracing::info!("Initializing MCP server: {}", server_name);

                if server_name == COMPOSIO_NATIVE_PREFIX {
                    if let Some(profile) = settings_clone.get_active_profile() {
                        if let Some(api_key) = &profile.api_key {
                            let base_url = profile.base_url.clone().unwrap_or_else(|| {
                                "https://backend.composio.dev/v3/mcp".to_string()
                            });
                            let has_server_uuid = profile.base_url.as_ref()
                                .is_some_and(|u| u.contains("/v3/mcp/"));

                            let entity_id = profile.entity_id.clone();
                            let user_id = profile.user_id.clone();
                            // Pattern 123: Pass profile_id for Context isolation
                            let composio_client = Arc::new(ComposioClient::new(
                                api_key.clone(),
                                base_url,
                                entity_id,
                                user_id,
                                profile.id.clone(),
                                profile.chrome_profile_directory.clone(),
                            ));

                            McpManager::inject_custom_credentials(
                                &composio_client,
                                &profile.name,
                                &secret_manager,
                            );

                            let client_for_tools = composio_client.clone();

                            // Tool Router pattern: only load force-loaded toolkit tools + meta-tools
                            let force_load_slugs = profile.get_force_load_toolkit_slugs();

                            // Guard: skip MCP list_tools when no server UUID exists (prevents 10401)
                            let tool_result = if has_server_uuid {
                                client_for_tools
                                    .list_tools_for_session(&force_load_slugs)
                                    .await
                            } else {
                                tracing::warn!("No MCP server UUID in base_url (launch_servers) — meta-tools only");
                                use crate::mcp::composio_client::discovery::DiscoveryResult;
                                Ok(DiscoveryResult {
                                    tools: crate::mcp::composio_client::meta::get_meta_tools(),
                                    warning: Some("No MCP server configured. Connect a toolkit to get started.".into()),
                                })
                            };
                            match tool_result
                            {
                                Ok(discovery_result) => {
                                    let tools = discovery_result
                                        .tools
                                        .iter()
                                        .map(composio_to_rmcp_tool)
                                        .collect();
                                    let active_client = ActiveMcpClient {
                                        config: server_config_clone.clone(),
                                        service: McpClientType::NativeComposio(
                                            composio_client.clone(),
                                        ),
                                        tools,
                                        warning_message: discovery_result.warning,
                                        profile_id: Some(profile.id.clone()),
                                    };
                                    if tx_clone.send(active_client).is_err() {
                                        tracing::error!(
                                            "Failed to send initialized Composio client"
                                        );
                                    }

                                    if !force_load_slugs.is_empty() {
                                        tracing::trace!(
                                            "Force-loaded toolkits: {:?}",
                                            force_load_slugs
                                        );
                                    }
                                }
                                Err(e) => {
                                    let error_msg = format!("Failed to list Composio tools: {}", e);
                                    // Check if this is a "needs setup" error (405, no tools, etc.)
                                    if McpManager::is_needs_setup_error(&error_msg) {
                                        tracing::debug!("Composio needs initial setup (connect first tool via Marketplace): {}", e);
                                    } else {
                                        tracing::error!("{}", error_msg);
                                        failed_servers_clone
                                            .lock()
                                            .await
                                            .insert(server_name, (server_config_clone, error_msg));
                                    }
                                }
                            }
                        } else {
                            // API key not configured - skip silently, status builder will show NotConfigured
                            tracing::debug!("Composio API key not configured for active profile - skipping initialization");
                        }
                    } else {
                        let error_msg = "No active Composio profile found".to_string();
                        tracing::error!("{}", error_msg);
                        failed_servers_clone
                            .lock()
                            .await
                            .insert(server_name, (server_config_clone, error_msg));
                    }
                    return; // End of composio-specific logic
                }

                let service_result = if let Some(uri) = server_config_clone.uri.clone() {
                    // If a command is provided for a network server, launch it as a background process.
                    if let Some(command_string) = server_config_clone.command.clone() {
                        let server_name_clone = server_name.clone();
                        let server_config_clone_for_spawn = server_config_clone.clone();
                        spawn(async move {
                            let mut cmd = Command::new(&command_string);
                            if let Some(args) = server_config_clone_for_spawn.args {
                                for arg in args {
                                    cmd.arg(arg);
                                }
                            }

                            // Inject sane PATH and critical environment variables
                            let mut envs = server_config_clone_for_spawn.env.clone();

                            // Add critical env vars first (HOME, USER, SHELL, TMPDIR)
                            for (key, value) in Self::get_critical_env_vars() {
                                envs.entry(key).or_insert(value);
                            }

                            let current_path = std::env::var("PATH").unwrap_or_default();
                            let sane_path = Self::get_sane_path();
                            let final_path = if current_path.is_empty() {
                                sane_path
                            } else {
                                format!("{}:{}", sane_path, current_path)
                            };
                            envs.insert("PATH".to_string(), final_path);

                            cmd.envs(&envs);
                            // We run this as a detached process. We don't care if it fails,
                            // as the connection logic will handle that.
                            if let Err(e) = cmd.status().await {
                                tracing::error!(
                                    "Failed to launch command for MCP server '{}': {}",
                                    server_name_clone,
                                    e
                                );
                            }
                        });
                    }
                    // Network-based server (SSE)
                    tracing::info!(
                        "Connecting to network MCP server '{}' at {}",
                        server_name,
                        uri
                    );
                    // For SSE servers, auth tokens should be provided via env vars by the CLI
                    // or directly in the server config as needed

                    // Use authenticated transport for Bearer token support (API keys, etc.)
                    // Check if there is a COMPOSIO_API_KEY in the env and use it
                    let auth_token = server_config_clone.env.get("COMPOSIO_API_KEY").cloned();

                    // If connecting to Composio, ensure transport=sse param is present
                    // Using POST as confirmed by manual curl test
                    let mut final_uri = uri.clone();
                    let use_post = if uri.contains("composio.dev") {
                        if !final_uri.contains("transport=sse") {
                            let separator = if final_uri.contains('?') { "&" } else { "?" };
                            final_uri = format!("{}{}transport=sse", final_uri, separator);
                        }
                        true // Use POST for Composio
                    } else {
                        false
                    };

                    // For Composio POST requests, we need both POST method AND correct headers/body
                    // authenticated_sse handles the headers/body logic when use_post is true
                    let auth_header = if uri.contains("composio.dev") {
                        Some("x-api-key".to_string())
                    } else {
                        None
                    };
                    let auth_prefix = if uri.contains("composio.dev") {
                        Some("".to_string())
                    } else {
                        None
                    };

                    let transport =
                        match crate::mcp::authenticated_sse::create_authenticated_transport(
                            &final_uri,
                            auth_token,
                            use_post,
                            auth_header,
                            auth_prefix,
                        )
                        .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                let mut auth_url = None;
                                let mut is_auth_error = false;

                                // Check specific error type
                                if let SseTransportError::Client(
                                    AuthenticatedClientError::AuthRequired(url),
                                ) = &e
                                {
                                    auth_url = Some(url.clone());
                                    is_auth_error = true;
                                }

                                let error_msg = format!("Failed to start SSE transport: {}", e);
                                if !is_auth_error {
                                    is_auth_error = Self::is_auth_error(&error_msg);
                                }

                                tracing::error!("{}", error_msg);

                                if is_auth_error {
                                    auth_required_servers_clone.lock().await.insert(
                                        server_name.clone(),
                                        AuthRequiredInfo {
                                            config: server_config_clone,
                                            auth_url,
                                            error_message: error_msg,
                                            profile: None,
                                        },
                                    );
                                } else {
                                    failed_servers_clone
                                        .lock()
                                        .await
                                        .insert(server_name, (server_config_clone, error_msg));
                                }
                                return;
                            }
                        };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(300),
                        ().serve(transport),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            tracing::error!(
                                "Timeout waiting for MCP server '{}' to initialize",
                                server_name
                            );
                            // Return a compatible error type or handle failure.
                            // Since serve returns Result<RunningService, InitializeError>, we need to match that.
                            // initialize error for RoleClient is likely ServiceError or similar.
                            // We'll return Err(rmcp::service::ServiceError::Timeout { timeout: std::time::Duration::from_secs(300) }.into()) if convertible,
                            // or just log and essentially fail.
                            // actually 'serve' returns Result<RunningService<RoleClient, ()>, ...>
                            // RoleClient::InitializeError is likely Infallible or ServiceError.
                            // Let's check matching. For now, assuming standard error flow.
                            return;
                        }
                    }
                } else {
                    // Stdio-based server
                    tracing::trace!("Launching stdio MCP server: {}", server_name);

                    let command_base = server_config_clone.command.clone().unwrap_or_default();
                    let mut cmd = Command::new(&command_base);

                    if let Some(ref args) = server_config_clone.args {
                        for arg in args {
                            cmd.arg(arg);
                        }
                    }

                    if server_name == "filesystem" {
                        if let Some(project_folder) = &settings_clone.project_folder {
                            cmd.arg(project_folder);
                            tracing::trace!(
                                "Adding project folder to filesystem MCP command: {}",
                                project_folder
                            );
                        }
                    }

                    // Inject sane PATH and critical environment variables
                    let mut envs = server_config_clone.env.clone();

                    // Add critical env vars first (HOME, USER, SHELL, TMPDIR)
                    // These may be missing when launched from Finder/open
                    for (key, value) in Self::get_critical_env_vars() {
                        envs.entry(key).or_insert(value);
                    }

                    let current_path = std::env::var("PATH").unwrap_or_default();
                    let sane_path = Self::get_sane_path();
                    let final_path = if current_path.is_empty() {
                        sane_path
                    } else {
                        format!("{}:{}", sane_path, current_path)
                    };
                    envs.insert("PATH".to_string(), final_path);

                    cmd.envs(&envs)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped());

                    // Set a sane working directory. macOS .app bundles launched from
                    // Finder inherit cwd="/" which causes tools like Playwright to
                    // attempt mkdir at the root (e.g. '/.playwright-mcp').
                    if let Some(home) = dirs::home_dir() {
                        cmd.current_dir(home);
                    }

                    match TokioChildProcess::new(cmd) {
                        Ok(transport) => {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(300),
                                ().serve(transport),
                            )
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => {
                                    tracing::error!(
                                        "Timeout waiting for stdio MCP server '{}' to initialize",
                                        server_name
                                    );
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to launch stdio MCP server '{}': {}",
                                server_name,
                                e
                            );
                            return;
                        }
                    }
                };

                match service_result {
                    Ok(service) => {
                        tracing::trace!("Connected to MCP server: {}", server_name);

                        // Fetch all pages of tools
                        let mut all_tools = Vec::new();
                        let mut next_cursor: Option<String> = None;

                        loop {
                            let cursor = next_cursor.clone();
                            let request_param =
                                cursor.map(|c| PaginatedRequestParam { cursor: Some(c) });

                            match service.list_tools(request_param).await {
                                Ok(result) => {
                                    all_tools.extend(result.tools);

                                    if let Some(cursor) = result.next_cursor {
                                        if !cursor.is_empty() {
                                            next_cursor = Some(cursor);
                                            continue;
                                        }
                                    }
                                    break;
                                }
                                Err(e) => {
                                    let error_msg = format!("Failed to list tools: {}", e);
                                    tracing::error!(
                                        "Failed to list tools for '{}': {}",
                                        server_name,
                                        e
                                    );
                                    // Check if this is an auth error
                                    if Self::is_auth_error(&error_msg) {
                                        tracing::info!(
                                            "Server '{}' requires authentication",
                                            server_name
                                        );
                                        auth_required_servers_clone.lock().await.insert(
                                            server_name.clone(),
                                            AuthRequiredInfo {
                                                config: server_config_clone,
                                                auth_url: None, // TODO: Extract from error if available
                                                error_message: error_msg,
                                                profile: None,
                                            },
                                        );
                                    } else {
                                        failed_servers_clone.lock().await.insert(
                                            server_name.clone(),
                                            (server_config_clone, error_msg),
                                        );
                                    }
                                    return;
                                }
                            }
                        }

                        tracing::trace!(
                            "Discovered {} capabilities for MCP server: {}",
                            all_tools.len(),
                            server_name
                        );
                        let active_client = ActiveMcpClient {
                            config: server_config_clone.clone(),
                            service: McpClientType::Service(Arc::new(service)),
                            tools: all_tools,
                            warning_message: None,
                            profile_id: None,
                        };
                        if tx_clone.send(active_client).is_err() {
                            tracing::error!(
                                "Failed to send initialized MCP client for '{}' to receiver task.",
                                server_name
                            );
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to serve: {}", e);
                        tracing::error!("Failed to serve MCP server '{}': {}", server_name, e);
                        // Check if this is an auth error
                        if Self::is_auth_error(&error_msg) {
                            tracing::info!("Server '{}' requires authentication", server_name);
                            auth_required_servers_clone.lock().await.insert(
                                server_name,
                                AuthRequiredInfo {
                                    config: server_config_clone,
                                    auth_url: None, // TODO: Extract from error if available
                                    error_message: error_msg,
                                    profile: None,
                                },
                            );
                        } else {
                            failed_servers_clone
                                .lock()
                                .await
                                .insert(server_name, (server_config_clone, error_msg));
                        }
                    }
                }
            });
        }
        tracing::info!("All MCP server launch tasks initiated.");
    }
    async fn load_configs(&self, config_path: PathBuf) -> Vec<McpServerConfig> {
        if !config_path.exists() {
            if let Some(parent) = config_path.parent() {
                if !parent.exists() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        tracing::error!("Failed to create config directory: {}", e);
                    }
                }
            }
            // Use a valid default JSON structure
            if let Err(e) = fs::write(&config_path, r#"{ "mcpServers": {} }"#) {
                tracing::error!("Failed to write default mcp_servers.json: {}", e);
            }
        }

        match fs::read_to_string(&config_path) {
            Ok(content) => {
                let mut wrapper: McpServersWrapper =
                    serde_json::from_str(&content).unwrap_or_else(|e| {
                        tracing::error!("Failed to parse mcp_servers.json: {}", e);
                        McpServersWrapper {
                            mcp_servers: HashMap::new(),
                        }
                    });

                // MIGRATION: Check for stale "composio" native config (no command) and remove it.
                // This prevents conflicts with the new COMPOSIO_NATIVE_PREFIX virtual client.
                let mut needs_save = false;
                if let Some(config) = wrapper.mcp_servers.get("composio") {
                    if config.command.is_none() {
                        tracing::info!(
                            "Migrating: Removing stale 'composio' native config from persistence."
                        );
                        wrapper.mcp_servers.remove("composio");
                        needs_save = true;
                    }
                }

                let configs_vec: Vec<McpServerConfig> = wrapper
                    .mcp_servers
                    .into_iter()
                    .map(|(name, mut config)| {
                        config.name = name;
                        config
                    })
                    .collect();

                tracing::info!(
                    "Successfully parsed {} MCP server configs.",
                    configs_vec.len()
                );

                if needs_save {
                    if let Err(e) = self.save_configs(&config_path, configs_vec.clone()).await {
                        tracing::error!("Failed to save migrated configs: {}", e);
                    }
                }

                configs_vec
            }
            Err(e) => {
                tracing::error!("Failed to read mcp_servers.json: {}", e);
                Vec::new()
            }
        }
    }
    #[allow(dead_code)]
    pub async fn add_or_update_mcp_server(
        &self,
        config_path: &std::path::Path,
        new_config: McpServerConfig,
    ) -> Result<(), String> {
        let mut configs = self.load_configs(config_path.to_path_buf()).await;

        if let Some(existing_config) = configs.iter_mut().find(|c| c.name == new_config.name) {
            *existing_config = new_config;
        } else {
            configs.push(new_config);
        }

        self.save_configs(config_path, configs).await
    }

    #[allow(dead_code)]
    async fn save_configs(
        &self,
        config_path: &std::path::Path,
        configs: Vec<McpServerConfig>,
    ) -> Result<(), String> {
        let mcp_servers_map: HashMap<String, McpServerConfig> = configs
            .into_iter()
            .filter(|c| !is_composio_native(&c.name)) // Never persist the virtual native client
            .map(|c| (c.name.clone(), c))
            .collect();

        let wrapper = McpServersWrapper {
            mcp_servers: mcp_servers_map,
        };

        let content = serde_json::to_string_pretty(&wrapper)
            .map_err(|e| format!("Failed to serialize MCP servers: {}", e))?;

        let path = config_path.to_path_buf();
        tokio::task::spawn_blocking(move || fs::write(path, content))
            .await
            .map_err(|e| format!("Save task panicked: {}", e))?
            .map_err(|e| format!("Failed to write to mcp_servers.json: {}", e))
    }

    /// Execute a tool on an MCP server.
    ///
    /// # Error Type: `Result<..., String>` — Architectural Decision
    ///
    /// This function (and the MCP boundary in general) deliberately uses `String`
    /// for error types rather than a structured `thiserror` enum. This is intentional:
    ///
    /// 1. **MCP is cross-language**: Tool responses come from servers written in Python,
    ///    Node.js, Go, etc. Errors are free-form strings with no guaranteed schema.
    /// 2. **AI consumption**: Errors are forwarded to the LLM for self-correction.
    ///    The AI needs human-readable context, not Rust enum variants.
    /// 3. **Display-final**: Most errors are shown directly to the user or AI — `String`
    ///    is the terminal format regardless.
    /// 4. **Normalization is impractical**: Each MCP server produces unique error formats.
    ///    A catch-all `Other(String)` variant would contain 90%+ of cases, defeating the purpose.
    ///
    /// For internal plumbing (keychain, persistence), structured errors ARE used — see
    /// `KeychainError` in `keychain_ffi.rs` for the established pattern.
    pub async fn use_mcp_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
        bypass_permission_check: bool,
        profile_id: Option<String>,
    ) -> Result<UnboundedReceiver<Result<CallToolResult, String>>, String> {
        // Intercept local on-demand meta-tools before normal dispatch
        if server_name == HOBBES_META_SERVER || tool_name == "MCP_LOAD_SERVER_TOOLS" || tool_name == "MCP_UNLOAD_SERVER_TOOLS" {
            let (tx, rx) = mpsc::unbounded_channel();

            if tool_name == "MCP_LOAD_SERVER_TOOLS" {
                let target_server = args.get("server_name").and_then(|v| v.as_str()).unwrap_or("");
                if target_server.is_empty() {
                    let _ = tx.send(Err("Missing 'server_name' argument".to_string()));
                    return Ok(rx);
                }

                // Check if the server is in on-demand mode
                let is_on_demand = {
                    let on_demand = self.on_demand_servers.lock().await;
                    on_demand.contains(target_server)
                };

                if !is_on_demand {
                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Server '{}' is not in on-demand mode. Its tools are already available.",
                            target_server
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                    return Ok(rx);
                }

                // Copy tools from the server into dynamic_local_tools cache
                let servers = self.servers.lock().await;
                if let Some(client) = servers.get(target_server) {
                    let tool_count = client.tools.len();
                    let tool_names: Vec<String> = client.tools.iter().map(|t| t.name.to_string()).collect();

                    // Add to dynamic cache (deduplicating)
                    let mut dynamic = self.dynamic_local_tools.lock().await;
                    let mut sources = self.dynamic_local_tool_sources.lock().await;
                    let existing_names: HashSet<String> = dynamic.iter().map(|t| t.name.to_string()).collect();
                    let new_tools: Vec<Tool> = client.tools.iter()
                        .filter(|t| !existing_names.contains(t.name.as_ref()))
                        .cloned()
                        .collect();
                    let injected = new_tools.len();
                    dynamic.extend(new_tools);

                    // Record origin server for each tool so use_mcp_tool can
                    // resolve "local-on-demand" back to the real server.
                    for name in &tool_names {
                        sources.insert(name.clone(), target_server.to_string());
                    }

                    tracing::info!(
                        "MCP_LOAD_SERVER_TOOLS: Loaded {} tools from '{}' (total dynamic: {}, sources: {})",
                        injected, target_server, dynamic.len(), sources.len()
                    );

                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Loaded {} tools from '{}'. They are now available as native function calls on the next turn.\n\nTools: {}",
                            tool_count, target_server, tool_names.join(", ")
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                } else {
                    let _ = tx.send(Err(format!("Server '{}' not found", target_server)));
                }
                return Ok(rx);
            } else if tool_name == "MCP_UNLOAD_SERVER_TOOLS" {
                let target_server = args.get("server_name").and_then(|v| v.as_str()).unwrap_or("");
                if target_server.is_empty() {
                    let _ = tx.send(Err("Missing 'server_name' argument".to_string()));
                    return Ok(rx);
                }

                // Remove this server's tools from dynamic cache
                let servers = self.servers.lock().await;
                if let Some(client) = servers.get(target_server) {
                    let server_tool_names: HashSet<String> =
                        client.tools.iter().map(|t| t.name.to_string()).collect();
                    let mut dynamic = self.dynamic_local_tools.lock().await;
                    let mut sources = self.dynamic_local_tool_sources.lock().await;
                    let before = dynamic.len();
                    dynamic.retain(|t| !server_tool_names.contains(t.name.as_ref()));
                    let removed = before - dynamic.len();
                    // Clean up source map
                    for name in &server_tool_names {
                        sources.remove(name);
                    }

                    tracing::info!(
                        "MCP_UNLOAD_SERVER_TOOLS: Removed {} tools from '{}' (remaining dynamic: {})",
                        removed, target_server, dynamic.len()
                    );

                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Unloaded {} tools from '{}'. Context space freed.",
                            removed, target_server
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                } else {
                    let _ = tx.send(Err(format!("Server '{}' not found", target_server)));
                }
                return Ok(rx);
            }
        }

        // Resolve "local-on-demand" virtual server → real origin server via source map.
        // The AI sends server_name="local-on-demand" for dynamically loaded tools because
        // get_mcp_context() presents them under that virtual name.
        let resolved_server_name: Option<String> = if server_name == "local-on-demand" {
            let sources = self.dynamic_local_tool_sources.lock().await;
            sources.get(tool_name).cloned()
        } else {
            None
        };
        let effective_server_name = resolved_server_name.as_deref().unwrap_or(server_name);

        let mut servers_guard = self.servers.lock().await;

        // Resolve composio-native to the actual profile-suffixed key.
        // The LLM may send:
        //   "composio-native"          — bare name (needs profile resolution)
        //   "composio-native:SomeName" — config.name (profile NAME suffix, NOT the storage key)
        //   "composio-native:SomeUUID" — storage key (profile ID suffix)
        //   "other-server"             — non-composio server (pass through)
        let actual_server_name = if is_composio_native(effective_server_name) {
            // Extract the suffix (profile name or ID) if present
            let suffix = server_name.strip_prefix("composio-native:").unwrap_or("");

            // 1. Try direct key lookup (works if suffix is already the profile ID)
            if !suffix.is_empty() && servers_guard.contains_key(server_name) {
                server_name.to_string()
            }
            // 2. Try with explicit profile_id param (from stream_manager)
            else if let Some(ref p) = profile_id {
                let scoped_key = composio_server_key(p);
                if servers_guard.contains_key(&scoped_key) {
                    scoped_key
                } else {
                    // Fallback: search by profile_id field (handles ID/name mismatch)
                    servers_guard
                        .iter()
                        .find(|(k, c)| is_composio_native(k) && c.profile_id.as_deref() == Some(p))
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| {
                            if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                                COMPOSIO_NATIVE_PREFIX.to_string()
                            } else {
                                String::new()
                            }
                        })
                }
            }
            // 3. If suffix is a profile NAME (from config.name), search by config.name match
            else if !suffix.is_empty() {
                servers_guard
                    .iter()
                    .find(|(_, c)| c.config.name == server_name)
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| {
                        // Also try matching suffix against profile_id
                        servers_guard
                            .iter()
                            .find(|(k, c)| {
                                is_composio_native(k) && c.profile_id.as_deref() == Some(suffix)
                            })
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| {
                                if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                                    COMPOSIO_NATIVE_PREFIX.to_string()
                                } else {
                                    String::new()
                                }
                            })
                    })
            }
            // 4. Bare "composio-native" with no profile — try singleton
            else if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                COMPOSIO_NATIVE_PREFIX.to_string()
            } else {
                return Err("Composio client not found and no profile specified".to_string());
            }
        } else {
            effective_server_name.to_string()
        };

        let client = match servers_guard.get(&actual_server_name) {
            Some(c) => c,
            None => {
                // If this was a local-on-demand tool that couldn't be resolved,
                // give a more specific error message.
                if server_name == "local-on-demand" {
                    return Err(format!(
                        "Tool '{}' not found in on-demand cache. The server may need to be loaded first via MCP_LOAD_SERVER_TOOLS.",
                        tool_name
                    ));
                }
                return Err(format!("Server not found: {}", server_name));
            }
        };

        // PRE-EMPTIVE HEALTH CHECK: If the stdio transport is already dead (child process
        // exited, pipe broken, etc.), attempt to reconnect before dispatching the call.
        // This avoids a wasted call_tool round-trip that would just return TransportClosed.
        if !client.service.is_healthy() {
            let config_for_reconnect = client.config.clone();
            tracing::warn!(
                "[RECONNECT] Transport for stdio server '{}' is dead — attempting auto-reconnect before tool call",
                actual_server_name
            );
            // Drop the lock before reconnecting (reconnect_stdio_server needs it)
            drop(servers_guard);

            match self
                .reconnect_stdio_server(&actual_server_name, config_for_reconnect)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "[RECONNECT] Successfully reconnected stdio server '{}'",
                        actual_server_name
                    );
                }
                Err(e) => {
                    return Err(format!(
                        "Server '{}' transport is dead and reconnect failed: {}",
                        actual_server_name, e
                    ));
                }
            }

            // Re-acquire the lock with the fresh client
            servers_guard = self.servers.lock().await;
        }

        // Re-resolve client (in case reconnect replaced it, or on first pass)
        let client = match servers_guard.get(&actual_server_name) {
            Some(c) => c,
            None => {
                return Err(format!(
                    "Server '{}' disappeared after reconnect attempt",
                    actual_server_name
                ));
            }
        };

        let permission_check_name = if is_composio_native(server_name) {
            COMPOSIO_NATIVE_PREFIX
        } else {
            server_name
        };

        // Permission Check
        if !bypass_permission_check && !client.config.always_allow.contains(&tool_name.to_string())
        {
            let pm = self.permission_manager.read();
            match pm.check_mcp_permission(permission_check_name) {
                PermissionStatus::Allowed => {}
                PermissionStatus::RequiresPrompt => {
                    let tool_call = crate::components::shared::ToolCall::new(
                        server_name.to_string(),
                        tool_name.to_string(),
                        args,
                        None,
                        None, // thought_summary not available in permission check context
                    );
                    return Err(serde_json::to_string(&tool_call).unwrap_or_default());
                }
                PermissionStatus::Denied(reason) => {
                    return Err(format!("Tool use denied: {}", reason));
                }
            }
        }

        let tool = match client.tools.iter().find(|t| t.name == tool_name) {
            Some(t) => t.clone(),
            None => {
                // Fallback 1: check the dynamic local tools cache.
                // Dynamically loaded on-demand server tools live here.
                let dynamic_local_cache = self.dynamic_local_tools.lock().await;
                if let Some(t) = dynamic_local_cache.iter().find(|t| t.name == tool_name) {
                    t.clone()
                } else {
                    // Fallback 2: check the dynamic Composio tools cache.
                    // Dynamically discovered tools (via COMPOSIO_GET_APP_TOOLS) live here,
                    // not in any ActiveMcpClient.tools list. Prefer this client's
                    // profile bucket; scan the rest only defensively (the Tool is
                    // metadata — execution still routes through the resolved client).
                    let dynamic_cache = self.dynamic_composio_tools.lock().await;
                    // Key by the turn's profile_id — the same key get_mcp_context
                    // used when it surfaced this tool to the AI.
                    let key = dyn_composio_key(profile_id.as_deref());
                    let found = dynamic_cache
                        .get(&key)
                        .and_then(|v| v.iter().find(|t| t.name == tool_name))
                        .or_else(|| dynamic_cache.values().flatten().find(|t| t.name == tool_name));
                    match found {
                        Some(t) => t.clone(),
                        None => return Err(format!("Tool not found: {}", tool_name)),
                    }
                }
            }
        };

        // BYOA FIX: Re-inject custom credentials before tool execution (Pattern 25)
        // This ensures any credentials updated via the UI are used immediately.
        if let McpClientType::NativeComposio(ref composio_client) = client.service {
            // Get profile name for scoped injection
            // Pattern 123: Use resolved profile name strictly
            let resolved_profile = if actual_server_name.starts_with("composio-native:") {
                actual_server_name
                    .strip_prefix("composio-native:")
                    .map(|s| s.to_string())
            } else {
                profile_id.clone()
            };

            if let Some(p_name) = resolved_profile {
                Self::inject_custom_credentials(composio_client, &p_name, &self.secret_manager);
            } else {
                // Fallback to all if no profile known
                let all_creds = self.secret_manager.peek().get_all_custom_tool_credentials();
                composio_client.set_custom_creds(all_creds);
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let service = client.service.clone();
        let dynamic_tools_cache = self.dynamic_composio_tools.clone();
        // Profile bucket for on-demand tools discovered/cleared in this turn.
        // Keyed by the turn's profile_id so it matches get_mcp_context's lookup.
        let dyn_profile_key = dyn_composio_key(profile_id.as_deref());
        // Capture the set of tool names currently loaded on this server.
        // Used to filter dynamic injection: only tools the proxy will accept
        // for tools/call should be injected as FunctionDeclarations.
        let loaded_tool_names: std::collections::HashSet<String> =
            client.tools.iter().map(|t| t.name.to_string()).collect();

        // Note: composio_meta synthetic server removed - Tool Router handles on-demand tools

        // Pattern 30: Pre-read API key for native clients before the spawn boundary.
        // This avoids borrowing `self` inside the async move block while still
        // reading the latest key value at call time (not at launch time).
        let native_image_api_key = self.secret_manager.peek().get("gemini_api_key").cloned();
        // Pattern 30: Also read image model fresh from settings at call time
        let native_image_model = self.settings.peek().image_generation_config.model.clone();

        // Capture reconnect info for potential retry inside the spawn block.
        // We need the server name and config in case call_tool returns TransportClosed.
        let reconnect_server_name = actual_server_name.clone();
        let reconnect_config = client.config.clone();
        let reconnect_manager = self.clone();

        spawn(async move {
            let result = match service {
                McpClientType::Service(service_arc) => {
                    let arguments = if let serde_json::Value::Object(map) = args {
                        map
                    } else {
                        return if tx
                            .send(Err("Tool arguments must be a JSON object".to_string()))
                            .is_err()
                        {
                            tracing::error!("StreamManager receiver dropped");
                        };
                    };
                    let request = CallToolRequestParam {
                        name: tool.name.clone(),
                        arguments: Some(arguments.clone()),
                    };
                    match service_arc.call_tool(request).await {
                        Ok(result) => Ok(result),
                        Err(e) => {
                            let error_str = format!("{}", e);
                            // AUTO-RECONNECT: If the transport closed (child process died),
                            // attempt to restart the server and retry the tool call once.
                            if error_str.contains("Transport closed") {
                                tracing::warn!(
                                    "[RECONNECT] call_tool returned TransportClosed for '{}' — attempting reconnect + retry",
                                    reconnect_server_name
                                );
                                match reconnect_manager
                                    .reconnect_stdio_server(
                                        &reconnect_server_name,
                                        reconnect_config,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        // Retry once with the fresh service
                                        let servers = reconnect_manager.servers.lock().await;
                                        if let Some(fresh_client) =
                                            servers.get(&reconnect_server_name)
                                        {
                                            if let McpClientType::Service(ref fresh_svc) =
                                                fresh_client.service
                                            {
                                                let retry_request = CallToolRequestParam {
                                                    name: tool.name.clone(),
                                                    arguments: Some(arguments),
                                                };
                                                match fresh_svc.call_tool(retry_request).await {
                                                    Ok(result) => {
                                                        tracing::info!(
                                                            "[RECONNECT] Retry succeeded for '{}' on server '{}'",
                                                            tool.name,
                                                            reconnect_server_name
                                                        );
                                                        Ok(result)
                                                    }
                                                    Err(retry_e) => Err(format!(
                                                        "Failed to use tool after reconnect: {}",
                                                        retry_e
                                                    )),
                                                }
                                            } else {
                                                Err(format!(
                                                    "Server '{}' is not a stdio service after reconnect",
                                                    reconnect_server_name
                                                ))
                                            }
                                        } else {
                                            Err(format!(
                                                "Server '{}' not found after reconnect",
                                                reconnect_server_name
                                            ))
                                        }
                                    }
                                    Err(reconnect_err) => Err(format!(
                                        "Failed to use tool (transport closed) and reconnect failed: {}",
                                        reconnect_err
                                    )),
                                }
                            } else {
                                Err(format!("Failed to use tool: {}", e))
                            }
                        }
                    }
                }
                McpClientType::NativeImage(image_client) => {
                    // Pattern 30: Both model and key are read fresh at call time
                    match image_client.execute_tool(&tool.name, args.clone(), &native_image_model, native_image_api_key.as_deref()).await {
                        Ok(result) => Ok(result),
                        Err(e) => Err(e),
                    }
                }
                McpClientType::NativeCore => {
                    // HOBBES_PAGE_RESULT and HOBBES_UPDATE_SCRATCHPAD are intercepted
                    // upstream in stream_manager.rs before use_mcp_tool() is called.
                    // If we land here it means a dispatch path was missed.
                    Err(format!(
                        "Built-in core tool '{}' was not intercepted before MCP dispatch. \
                        This is a Hobbes bug — please report it.",
                        tool.name
                    ))
                }
                McpClientType::NativeMeta => {
                    // MCP_LOAD_SERVER_TOOLS and MCP_UNLOAD_SERVER_TOOLS are intercepted
                    // at the top of use_mcp_tool() before reaching this match arm.
                    // If we land here it means a dispatch path was missed.
                    Err(format!(
                        "Built-in meta tool '{}' was not intercepted before MCP dispatch. \
                        This is a Hobbes bug — please report it.",
                        tool.name
                    ))
                }
                McpClientType::NativeComposio(composio_client) => {
                    // Tool Router: Handle meta-tools for on-demand discovery
                    if tool.name == "COMPOSIO_DISCOVER_APPS" {
                        // Extract query from args
                        let query = args.get("query").and_then(|v| v.as_str());
                        tracing::info!("COMPOSIO_DISCOVER_APPS: query='{:?}'", query);

                        // Operational Authority: Use list_connected_toolkits (MCP-first,
                        // cross-referenced with user-scoped toolkit_account_map) rather than
                        // the unfiltered REST marketplace catalog. Only shows apps the user
                        // has connected accounts for.
                        match composio_client.list_connected_toolkits().await {
                            Ok(toolkit_infos) => {
                                // Apply optional query filter client-side
                                let filtered: Vec<_> = if let Some(q) = query {
                                    let q_lower = q.to_lowercase();
                                    toolkit_infos
                                        .into_iter()
                                        .filter(|tk| {
                                            tk.slug.to_lowercase().contains(&q_lower)
                                                || tk.display_name.to_lowercase().contains(&q_lower)
                                        })
                                        .collect()
                                } else {
                                    toolkit_infos
                                };

                                let results: Vec<serde_json::Value> = filtered
                                    .iter()
                                    .map(|tk| {
                                        serde_json::json!({
                                            "name": tk.display_name,
                                            "slug": tk.slug,
                                            "tool_count": tk.tool_count,
                                            "is_connected": tk.is_connected
                                        })
                                    })
                                    .collect();

                                let content_text = serde_json::to_string_pretty(&serde_json::json!({
                                    "apps_found": results.len(),
                                    "apps": results,
                                    "hint": "To use tools from an app, call COMPOSIO_GET_APP_TOOLS with the app's name or slug."
                                })).unwrap_or_default();

                                Ok(CallToolResult {
                                    content: vec![rmcp::model::Content::text(content_text)],
                                    is_error: Some(false),
                                    structured_content: None,
                                    meta: None,
                                })
                            }
                            Err(e) => Err(format!("App discovery failed: {}", e)),
                        }
                    } else if tool.name == "COMPOSIO_GET_APP_TOOLS" {
                        let app_name = args.get("app_name").and_then(|v| v.as_str()).unwrap_or("");
                        tracing::info!("COMPOSIO_GET_APP_TOOLS: app_name='{}'", app_name);

                        if app_name.is_empty() {
                            Err("Missing 'app_name' argument".to_string())
                        } else {
                            // We use list_tools_filtered with the app name/slug
                            match composio_client
                                .list_tools_filtered(Some(&[app_name.to_string()]))
                                .await
                            {
                                Ok(discovery_result) => {
                                    let total_available = discovery_result.tools.len();

                                    // Identify tools already loaded on the server (force-loaded)
                                    let already_loaded: Vec<_> = discovery_result
                                        .tools
                                        .iter()
                                        .filter(|t| loaded_tool_names.contains(&t.name))
                                        .collect();

                                    // Deduplicate: skip tools already loaded on the server
                                    let new_tools: Vec<_> = discovery_result
                                        .tools
                                        .iter()
                                        .filter(|t| !loaded_tool_names.contains(&t.name))
                                        .collect();

                                    // Budget-aware selection: only inject as many as Gemini can handle
                                    let budget =
                                        GEMINI_TOOL_LIMIT.saturating_sub(loaded_tool_names.len());
                                    let budget_limited = new_tools.len() > budget;

                                    let selected: Vec<_> = if budget_limited {
                                        // Score and sort by relevance, take top N within budget
                                        let mut scored: Vec<_> = new_tools
                                            .iter()
                                            .map(|t| (score_tool_relevance(&t.name), *t))
                                            .collect();
                                        scored.sort_by(|a, b| b.0.cmp(&a.0));
                                        scored.into_iter().take(budget).map(|(_, t)| t).collect()
                                    } else {
                                        new_tools
                                    };

                                    // Inject selected tools into the dynamic cache
                                    let rmcp_tools: Vec<rmcp::model::Tool> =
                                        selected.iter().map(|t| composio_to_rmcp_tool(t)).collect();
                                    let injected_count = rmcp_tools.len();
                                    {
                                        let mut cache = dynamic_tools_cache.lock().await;
                                        let bucket = cache.entry(dyn_profile_key.clone()).or_default();
                                        let new_names: std::collections::HashSet<String> =
                                            rmcp_tools.iter().map(|t| t.name.to_string()).collect();
                                        bucket.retain(|t| !new_names.contains(&t.name.to_string()));
                                        bucket.extend(rmcp_tools);
                                        tracing::info!(
                                            "Injected {} dynamic tools for '{}' under profile '{}' (from {} available, budget: {}, bucket: {})",
                                            injected_count, app_name, dyn_profile_key, total_available, budget, bucket.len()
                                        );
                                    }

                                    // Populate tool→toolkit mapping for ALL discovered tools
                                    // (even non-injected ones, so COMPOSIO_EXECUTE_TOOL can resolve context)
                                    if let Ok(mut map) = composio_client.tool_toolkit_map.write() {
                                        for t in &discovery_result.tools {
                                            map.insert(t.name.clone(), app_name.to_string());
                                        }
                                    }

                                    // Build response: include BOTH newly injected AND already-loaded tools
                                    // so the AI knows what it can call directly.
                                    let tool_results: Vec<serde_json::Value> = selected
                                        .iter()
                                        .map(|t| {
                                            serde_json::json!({
                                                "name": t.name,
                                                "description": t.description,
                                            })
                                        })
                                        .collect();

                                    let already_loaded_results: Vec<serde_json::Value> = already_loaded
                                        .iter()
                                        .map(|t| {
                                            serde_json::json!({
                                                "name": t.name,
                                                "description": t.description,
                                            })
                                        })
                                        .collect();
                                    let already_loaded_count = already_loaded_results.len();

                                    let hint = if !already_loaded.is_empty() && selected.is_empty() {
                                        // All tools already force-loaded — tell the AI exactly what it has
                                        format!(
                                            "All {} tools are already loaded as native function calls. Call them directly by name — they are listed in 'already_loaded' below.",
                                            already_loaded_count
                                        )
                                    } else if budget_limited {
                                        format!("{} most relevant tools injected (budget: {} max). Use COMPOSIO_EXECUTE_TOOL for any tool not listed.", injected_count, GEMINI_TOOL_LIMIT)
                                    } else {
                                        "All tools are now available as native function calls. Call them directly by name.".to_string()
                                    };

                                    let mut response_json = serde_json::json!({
                                        "app": app_name,
                                        "injected_count": injected_count,
                                        "total_available": total_available,
                                        "budget_limited": budget_limited,
                                        "tools": tool_results,
                                        "hint": hint,
                                    });

                                    // Include already-loaded tool names so the AI knows what it can call
                                    if !already_loaded_results.is_empty() {
                                        if let Some(obj) = response_json.as_object_mut() {
                                            obj.insert("already_loaded_count".to_string(), serde_json::json!(already_loaded_count));
                                            obj.insert("already_loaded".to_string(), serde_json::json!(already_loaded_results));
                                        }
                                    }

                                    let content_text = serde_json::to_string_pretty(&response_json).unwrap_or_else(|e| {
                                        tracing::error!("Failed to serialize tool discovery response: {}", e);
                                        // Hand-crafted JSON is intentional here: the primary serde_json
                                        // serializer just failed, and this output is consumed as Content::text
                                        // by the AI — not parsed as JSON by any downstream consumer.
                                        format!("{{\"error\": \"Failed to format tool list for '{}'\"}}", app_name)
                                    });

                                    Ok(CallToolResult {
                                        content: vec![rmcp::model::Content::text(content_text)],
                                        is_error: Some(false),
                                        structured_content: None,
                                        meta: None,
                                    })
                                }
                                Err(e) => Err(format!(
                                    "Failed to get tools for app '{}': {}",
                                    app_name, e
                                )),
                            }
                        }
                    } else if tool.name == "COMPOSIO_CLEAR_TOOLS" {
                        // Clear only this profile's dynamically discovered tools,
                        // so one tab clearing its toolset doesn't wipe another
                        // profile's discovered tools.
                        let mut cache = dynamic_tools_cache.lock().await;
                        let cleared_count = cache
                            .remove(&dyn_profile_key)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        tracing::info!(
                            "COMPOSIO_CLEAR_TOOLS: cleared {} dynamic tools for profile '{}'",
                            cleared_count,
                            dyn_profile_key
                        );

                        Ok(CallToolResult {
                            content: vec![rmcp::model::Content::text(format!(
                                "Cleared {} dynamically discovered tools from the session.",
                                cleared_count
                            ))],
                            is_error: Some(false),
                            structured_content: None,
                            meta: None,
                        })
                    } else if tool.name == "COMPOSIO_EXECUTE_TOOL" {
                        // Extract tool_name and arguments - handle missing tool_name as error
                        match args.get("tool_name").and_then(|v| v.as_str()) {
                            Some(target_tool_name) => {
                                let tool_args = args
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                tracing::info!(
                                    "COMPOSIO_EXECUTE_TOOL: executing '{}'",
                                    target_tool_name
                                );

                                // Execute the target tool
                                match composio_client
                                    .execute_tool(target_tool_name, tool_args)
                                    .await
                                {
                                    Ok(response) => {
                                        // Check for auth failure (401/403) and attempt auto-reconnection + retry
                                        if let Some(auth_result) = try_auth_recovery(
                                            &response,
                                            target_tool_name,
                                            &composio_client,
                                        )
                                        .await
                                        {
                                            let _ = tx.send(Ok(auth_result));
                                            return;
                                        }

                                        let content_text = if response.successful {
                                            serde_json::to_string_pretty(&response.data)
                                                .unwrap_or_else(|_| {
                                                    "{\"error\": \"Failed to serialize response\"}"
                                                        .to_string()
                                                })
                                        } else {
                                            response
                                                .error
                                                .unwrap_or_else(|| "Unknown error".to_string())
                                        };

                                        Ok(CallToolResult {
                                            content: vec![rmcp::model::Content::text(content_text)],
                                            is_error: Some(!response.successful),
                                            structured_content: Some(response.data),
                                            meta: None,
                                        })
                                    }
                                    Err(e) => Err(format!("Tool execution failed: {}", e)),
                                }
                            }
                            None => Err("Missing 'tool_name' argument".to_string()),
                        }
                    } else {
                        // Regular Composio tool execution
                        match composio_client.execute_tool(&tool.name, args).await {
                            Ok(response) => {
                                // Check for auth failure (401/403) and attempt auto-reconnection + retry
                                if let Some(auth_result) =
                                    try_auth_recovery(&response, &tool.name, &composio_client).await
                                {
                                    let _ = tx.send(Ok(auth_result));
                                    return;
                                }

                                // Convert the response to a proper CallToolResult
                                let content_text = if response.successful {
                                    serde_json::to_string_pretty(&response.data).unwrap_or_else(
                                        |_| {
                                            "{\"error\": \"Failed to serialize response data\"}"
                                                .to_string()
                                        },
                                    )
                                } else {
                                    response
                                        .error
                                        .unwrap_or_else(|| "Unknown error".to_string())
                                };

                                let content = rmcp::model::Content::text(content_text);

                                // Create metadata with log_id and session_info if available
                                let mut meta_map = serde_json::Map::new();
                                if let Some(log_id) = response.log_id {
                                    meta_map.insert(
                                        "log_id".to_string(),
                                        serde_json::Value::String(log_id),
                                    );
                                }
                                if let Some(session_info) = response.session_info {
                                    meta_map.insert("session_info".to_string(), session_info);
                                }

                                // Create metadata with log_id and session_info if available
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

                                Ok(CallToolResult {
                                    content: vec![content],
                                    is_error: Some(!response.successful),
                                    structured_content: Some(response.data),
                                    meta,
                                })
                            }
                            Err(e) => Err(format!("Composio tool execution failed: {}", e)),
                        }
                    }
                }
            };

            if tx.send(result).is_err() {
                tracing::error!("StreamManager receiver dropped for tool result.");
            }
        });

        Ok(rx)
    }

    /// Start OAuth flow for a server that requires authentication
    /// This calls the generate_oauth_url tool and opens the browser
    #[allow(dead_code)]
    pub async fn start_oauth_flow(&self, server_name: &str) -> Result<String, String> {
        // First, check if the server has a generate_oauth_url tool
        let servers = self.servers.lock().await;

        if let Some(client) = servers.get(server_name) {
            // Check if server has OAuth tools
            let has_oauth_url_tool = client.tools.iter().any(|t| {
                let name = t.name.as_ref();
                name.contains("oauth_url")
                    || name.contains("generate_oauth")
                    || name.contains("auth_url")
            });

            if !has_oauth_url_tool {
                return Err(format!(
                    "Server '{}' does not have OAuth tools",
                    server_name
                ));
            }

            // Find the callback port
            let port = crate::mcp::oauth_flow::find_available_port()
                .ok_or("Could not find available port for OAuth callback")?;

            let redirect_uri = format!("http://localhost:{}/callback", port);

            // Call the generate_oauth_url tool
            let service = client.service.clone();
            drop(servers); // Release lock before async call

            // Try different tool name patterns
            let tool_names = [
                "generate_oauth_url",
                "GENERATE_OAUTH_URL",
                "generate_auth_url",
            ];
            let mut oauth_url = None;

            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(
                        serde_json::json!({
                            "redirect_uri": redirect_uri
                        })
                        .as_object()
                        .cloned()
                        .expect("static JSON is always an object"),
                    ),
                };

                if let McpClientType::Service(service_arc) = &service {
                    if let Ok(result) = service_arc.call_tool(request).await {
                        // Extract URL from result
                        if let Some(content) = result.content.first() {
                            if let Some(text) = content.raw.as_text() {
                                // The result might be a URL directly or JSON containing a URL
                                if text.text.starts_with("http") {
                                    oauth_url = Some(text.text.clone());
                                } else if let Ok(json) =
                                    serde_json::from_str::<serde_json::Value>(&text.text)
                                {
                                    if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
                                        oauth_url = Some(url.to_string());
                                    } else if let Some(url) =
                                        json.get("oauth_url").and_then(|v| v.as_str())
                                    {
                                        oauth_url = Some(url.to_string());
                                    }
                                }
                            }
                        }
                        if oauth_url.is_some() {
                            break;
                        }
                    }
                }
            }

            if let Some(url) = oauth_url {
                // Start callback server and open browser
                let _callback_rx = crate::mcp::oauth_flow::start_callback_server(port);
                crate::mcp::oauth_flow::open_browser(&url, None).await?;

                Ok(format!(
                    "OAuth flow started. Callback server on port {}",
                    port
                ))
            } else {
                Err("Failed to get OAuth URL from server".to_string())
            }
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }

    /// Complete OAuth flow by exchanging auth code for tokens
    #[allow(dead_code)]
    pub async fn complete_oauth_flow(
        &self,
        server_name: &str,
        auth_code: &str,
    ) -> Result<String, String> {
        let servers = self.servers.lock().await;

        if let Some(client) = servers.get(server_name) {
            let service = client.service.clone();
            drop(servers);

            // Try different tool name patterns
            let tool_names = ["exchange_auth_code", "EXCHANGE_AUTH_CODE", "exchange_code"];

            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(
                        serde_json::json!({
                            "code": auth_code
                        })
                        .as_object()
                        .cloned()
                        .expect("static JSON is always an object"),
                    ),
                };

                if let McpClientType::Service(service_arc) = &service {
                    if let Ok(result) = service_arc.call_tool(request).await {
                        // Check if exchange was successful
                        if result.is_error.unwrap_or(false) {
                            if let Some(content) = result.content.first() {
                                if let Some(text) = content.raw.as_text() {
                                    return Err(format!("Token exchange failed: {}", text.text));
                                }
                            }
                            return Err("Token exchange failed".to_string());
                        }

                        // Remove from auth_required_servers
                        self.auth_required_servers.lock().await.remove(server_name);

                        return Ok("OAuth completed successfully".to_string());
                    }
                }
            }

            Err("Failed to exchange auth code".to_string())
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }

    pub async fn get_composio_toolkits(
        &self,
    ) -> Result<Vec<crate::mcp::composio_client::ToolkitInfo>, String> {
        let servers = self.servers.lock().await;
        // Select the ACTIVE profile's composio-native client (several accumulate
        // once multiple profiles connect; a bare find returns an arbitrary one).
        let active_profile_id = self
            .settings
            .peek()
            .get_active_profile()
            .map(|p| p.id.clone());
        let composio_client = active_composio_key(&servers, active_profile_id.as_deref())
            .and_then(|k| servers.get(&k));

        if let Some(client) = composio_client {
            if let McpClientType::NativeComposio(composio_client) = &client.service {
                // Use list_connected_toolkits which:
                // 1. Calls list_connected_accounts() to find connected toolkit slugs
                // 2. Calls list_tools() (MCP) to count tools per toolkit
                // This shows ALL connected toolkits, not just force-loaded ones
                return composio_client.list_connected_toolkits().await;
            }
        }
        Err("Composio client not initialized or not connected".to_string())
    }

    /// Find the active native Composio client, cloning its Arc so the servers
    /// lock is released before any network call.
    async fn find_composio_client(
        &self,
    ) -> Result<Arc<crate::mcp::composio_client::ComposioClient>, String> {
        let servers = self.servers.lock().await;
        // Scope to the active profile's client so operations don't hit a
        // different profile's Composio account.
        let active_profile_id = self
            .settings
            .peek()
            .get_active_profile()
            .map(|p| p.id.clone());
        active_composio_key(&servers, active_profile_id.as_deref())
            .and_then(|k| servers.get(&k))
            .and_then(|v| match &v.service {
                McpClientType::NativeComposio(c) => Some(c.clone()),
                _ => None,
            })
            .ok_or_else(|| "Composio client not initialized or not connected".to_string())
    }

    /// Data for the toolkit tool editor: every tool the toolkit offers
    /// (name + description) and the currently enabled subset. An empty enabled
    /// list means no whitelist entries exist yet — all tools are enabled.
    pub async fn get_composio_toolkit_tool_state(
        &self,
        toolkit_slug: &str,
    ) -> Result<(Vec<(String, Option<String>)>, Vec<String>), String> {
        let composio_client = self.find_composio_client().await?;
        let all_tools = composio_client
            .get_toolkit_tools_detailed(toolkit_slug)
            .await?;
        let enabled = composio_client
            .get_toolkit_enabled_tools(toolkit_slug)
            .await?;
        Ok((all_tools, enabled))
    }

    /// Persist a user-curated tool whitelist for a toolkit, then bust caches and
    /// reload the Composio tool set so the change takes effect immediately.
    ///
    /// Cache busting is thorough because the edited whitelist affects tools in
    /// several places: the client's cached toolkit-info counts, any on-demand
    /// tools already discovered into `dynamic_composio_tools`, and the loaded
    /// tool set on the active client.
    pub async fn set_composio_toolkit_tools(
        &self,
        toolkit_slug: &str,
        enabled_tools: Vec<String>,
        settings: &Settings,
    ) -> Result<(), String> {
        let composio_client = self.find_composio_client().await?;
        composio_client
            .set_toolkit_enabled_tools(toolkit_slug, enabled_tools)
            .await?;

        // Bust the toolkit-info cache so tool counts re-fetch fresh from MCP.
        composio_client.clear_cached_toolkit_info();

        // Drop any on-demand tools already discovered for this toolkit — they
        // were resolved against the OLD whitelist and must be re-discovered.
        // Scoped to the active profile's bucket (the edit is profile-scoped).
        {
            let prefix = format!("{}_", toolkit_slug.to_uppercase().replace('-', "_"));
            let active_profile_id = self
                .settings
                .peek()
                .get_active_profile()
                .map(|p| p.id.clone());
            let key = dyn_composio_key(active_profile_id.as_deref());
            let mut dynamic = self.dynamic_composio_tools.lock().await;
            if let Some(bucket) = dynamic.get_mut(&key) {
                let before = bucket.len();
                bucket.retain(|t| !t.name.to_uppercase().starts_with(&prefix));
                let removed = before - bucket.len();
                if removed > 0 {
                    tracing::info!(
                        "Cleared {} stale dynamic tools for edited toolkit '{}' (profile '{}')",
                        removed,
                        toolkit_slug,
                        key
                    );
                }
            }
        }

        // Reload the loaded/force-loaded tool set from the server.
        self.reload_composio_tools(settings).await?;
        self.invalidate_status_cache();
        Ok(())
    }

    pub async fn get_mcp_context(&self, profile_id: Option<String>) -> McpContext {
        let servers = self.servers.lock().await;
        let unloaded = self.unloaded_servers.lock().await;
        let on_demand = self.on_demand_servers.lock().await;
        let mut server_contexts = Vec::new();
        let mut on_demand_server_info: Vec<(String, String, usize)> = Vec::new(); // (name, description, tool_count)

        for (name, client) in servers.iter() {
            // Skip servers that are unloaded (tools hidden from AI)
            if unloaded.contains(&client.config.name) {
                continue;
            }

            // FILTER: If this is a native client, only include it if it matches the profile
            // Use client.profile_id for comparison (not the HashMap key, which may differ from the name)
            if is_composio_native(name) {
                if let Some(ref target_id) = profile_id {
                    match &client.profile_id {
                        Some(pid) if pid == target_id => { /* include — profile matches */ }
                        None => { /* legacy singleton — include for any profile */ }
                        _ => continue, // different profile — skip
                    }
                }
            }

            // On-demand servers: include name/description but NOT tools
            if on_demand.contains(&client.config.name) && !is_composio_native(name) {
                on_demand_server_info.push((
                    client.config.name.clone(),
                    client.config.description.clone(),
                    client.tools.len(),
                ));
                // Include an empty-tools entry so the system prompt lists the server
                server_contexts.push(McpServerContext {
                    name: client.config.name.clone(),
                    description: format!(
                        "{} [ON-DEMAND: {} tools available — call MCP_LOAD_SERVER_TOOLS with server_name='{}' to load]",
                        client.config.description, client.tools.len(), client.config.name
                    ),
                    tools: Vec::new(), // No tools in prompt
                });
                continue;
            }

            let server_context = McpServerContext {
                name: client.config.name.clone(),
                description: client.config.description.clone(),
                tools: client.tools.clone(),
            };
            server_contexts.push(server_context);
        }

        // hobbes-core and hobbes-meta are now registered in McpManager::servers
        // and included in the main server loop above — no manual injection needed.

        // Include dynamically loaded local MCP tools (from MCP_LOAD_SERVER_TOOLS)
        let dynamic_local = self.dynamic_local_tools.lock().await;
        if !dynamic_local.is_empty() {
            let existing_tool_names: HashSet<String> = server_contexts
                .iter()
                .flat_map(|sc| sc.tools.iter().map(|t| t.name.to_string()))
                .collect();

            let deduped_tools: Vec<Tool> = dynamic_local
                .iter()
                .filter(|t| !existing_tool_names.contains(t.name.as_ref()))
                .cloned()
                .collect();

            if !deduped_tools.is_empty() {
                server_contexts.push(McpServerContext {
                    name: "local-on-demand".to_string(),
                    description: "Dynamically loaded local MCP tools (via MCP_LOAD_SERVER_TOOLS)"
                        .to_string(),
                    tools: deduped_tools,
                });
            }
        }

        // Include dynamically discovered Composio tools as a virtual server entry.
        // These are tools fetched by COMPOSIO_GET_APP_TOOLS and cached for injection
        // into the prompt as real FunctionDeclarations.
        // Use the same display name as the real Composio server so the UX shows
        // a friendly profile name (e.g. "composio-native:Puget Systems") instead of a UUID.
        //
        // DEDUPLICATION (March 2026): Build a set of all tool names already present
        // across all server contexts. Dynamic tools that collide with force-loaded
        // tools are excluded to prevent Gemini "Duplicate function declaration" 400 errors.
        let dynamic_tools = self.dynamic_composio_tools.lock().await;
        // Only this profile's on-demand tools — never another profile's bucket.
        let profile_dynamic_tools: &[Tool] = dynamic_tools
            .get(&dyn_composio_key(profile_id.as_deref()))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if !profile_dynamic_tools.is_empty() {
            let existing_tool_names: HashSet<String> = server_contexts
                .iter()
                .flat_map(|sc| sc.tools.iter().map(|t| t.name.to_string()))
                .collect();

            let deduped_tools: Vec<Tool> = profile_dynamic_tools
                .iter()
                .filter(|t| !existing_tool_names.contains(t.name.as_ref()))
                .cloned()
                .collect();

            let skipped = profile_dynamic_tools.len() - deduped_tools.len();
            if skipped > 0 {
                tracing::info!(
                    "Dynamic tools: {} total, {} skipped (already in force-loaded set), {} injected",
                    profile_dynamic_tools.len(), skipped, deduped_tools.len()
                );
            }

            if !deduped_tools.is_empty() {
                // Find the matching Composio server's display name
                let server_name = servers
                    .iter()
                    .find(|(k, c)| {
                        is_composio_native(k)
                            && match (&profile_id, &c.profile_id) {
                                (Some(target), Some(cid)) => target == cid,
                                (None, _) => true,
                                _ => false,
                            }
                    })
                    .map(|(_, c)| c.config.name.clone())
                    .unwrap_or_else(|| match &profile_id {
                        Some(pid) => composio_server_key(pid),
                        None => COMPOSIO_NATIVE_PREFIX.to_string(),
                    });
                server_contexts.push(McpServerContext {
                    name: server_name,
                    description: "Dynamically discovered Composio tools".to_string(),
                    tools: deduped_tools,
                });
            }
        }

        // Populate connected toolkit slugs from cached Composio toolkit info (MCP-First, Section 6).
        // This is a pure cache read — no network calls. The cache is hydrated by
        // list_connected_toolkits() which uses the MCP `tools/list` endpoint.
        let connected_toolkit_slugs = active_composio_key(&servers, profile_id.as_deref())
            .and_then(|k| servers.get(&k))
            .and_then(|client| {
                if let McpClientType::NativeComposio(ref composio_client) = client.service {
                    composio_client.get_cached_toolkit_info()
                } else {
                    None
                }
            })
            .map(|infos| infos.into_iter().map(|ti| ti.slug).collect::<Vec<_>>())
            .unwrap_or_default();

        McpContext {
            servers: server_contexts,
            connected_toolkit_slugs,
        }
    }

    /// Unload a server's tools from the AI context (runtime only)
    pub async fn unload_server(&self, server_name: &str) {
        let mut unloaded = self.unloaded_servers.lock().await;
        unloaded.insert(server_name.to_string());
        drop(unloaded); // Release lock before sync try_lock in invalidate
                        // Pattern 150.8.1: Authoritative invalidation prevents UI state gaps
        self.invalidate_status_cache();
        tracing::info!(
            "Unloaded server '{}' - tools hidden from AI, cache invalidated",
            server_name
        );
    }

    /// Load a server's tools back into the AI context
    /// NOTE: Superseded by `set_server_loaded()` for the 3-way mode system,
    /// but retained as a simpler API for basic unload/reload use cases.
    #[allow(dead_code)]
    pub async fn load_server(&self, server_name: &str) {
        let mut unloaded = self.unloaded_servers.lock().await;
        unloaded.remove(server_name);
        drop(unloaded); // Release lock before sync try_lock in invalidate
                        // Pattern 150.8.1: Authoritative invalidation prevents UI state gaps
        self.invalidate_status_cache();
        tracing::info!(
            "Loaded server '{}' - tools visible to AI, cache invalidated",
            server_name
        );
    }

    /// Check if a server's tools are currently visible to the AI
    #[allow(dead_code)]
    pub async fn is_server_loaded(&self, server_name: &str) -> bool {
        let unloaded = self.unloaded_servers.lock().await;
        !unloaded.contains(server_name)
    }

    /// Reload Composio tools based on current force_load settings
    /// Call this after changing force_load on any toolkit
    pub async fn reload_composio_tools(
        &self,
        settings: &crate::settings::Settings,
    ) -> Result<(), String> {
        let mut servers = self.servers.lock().await;

        // Select the ACTIVE profile's composio-native client. A bare
        // find(is_composio_native) can return a different profile's client when
        // several profiles have connected, reloading the wrong profile's tools.
        let active_profile_id = settings.get_active_profile().map(|p| p.id.clone());
        let composio_key = active_composio_key(&servers, active_profile_id.as_deref());

        if let Some(key) = composio_key {
            if let Some(active_client) = servers.get_mut(&key) {
                if let McpClientType::NativeComposio(composio_client) = &active_client.service {
                    // Get current force_load slugs from settings
                    let force_load_slugs = settings
                        .get_active_profile()
                        .map(|p| p.get_force_load_toolkit_slugs())
                        .unwrap_or_default();

                    // Reload tools
                    match composio_client
                        .list_tools_for_session(&force_load_slugs)
                        .await
                    {
                        Ok(discovery_result) => {
                            let tools = discovery_result
                                .tools
                                .iter()
                                .map(composio_to_rmcp_tool)
                                .collect();
                            active_client.tools = tools;
                            active_client.warning_message = discovery_result.warning;
                            tracing::info!(
                                "Reloaded Composio tools with force_load_slugs: {:?} (key: {})",
                                force_load_slugs,
                                key
                            );
                            Ok(())
                        }
                        Err(e) => {
                            let error_msg = format!("Failed to reload Composio tools: {}", e);
                            tracing::error!("{}", error_msg);
                            Err(error_msg)
                        }
                    }
                } else {
                    Err("Composio client is not a NativeComposio type".to_string())
                }
            } else {
                Err("Composio client not found (inner)".to_string())
            }
        } else {
            Err("Composio client not found".to_string())
        }
    }

    /// Connect a toolkit to the natively managed Composio server.
    /// Encapsulates the 5-step lifecycle: AuthConfig, Registry (PATCH/Create),
    /// OAuth, and User Binding.
    // All params are Dioxus signals needed for the 5-step toolkit connection lifecycle.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_toolkit(
        &self,
        toolkit_slug: String,
        auth_scheme: Option<String>,
        use_managed_auth: bool,
        no_auth: bool,
        mut mcp_context_signal: Signal<McpContext>,
        mut settings_signal: Signal<Settings>,
        settings_manager: SettingsManager,
        mut is_connecting: Signal<bool>,
        mut connection_status: Signal<String>,
        mut connection_error: Signal<Option<String>>,
        mut trigger_search: Signal<i32>,
        mut connected_slugs: Signal<HashSet<String>>,
    ) -> Result<(), String> {
        tracing::info!(
            "Consolidated 6-Point Connection: toolkit_slug={} (auth_scheme: {:?}, managed: {}, no_auth: {})",
            toolkit_slug,
            auth_scheme,
            use_managed_auth,
            no_auth
        );

        is_connecting.set(true);
        connection_status.set("Connecting...".to_string());
        connection_error.set(None);

        let settings_snapshot = settings_signal.peek().clone();
        let Some(profile) = settings_snapshot.get_active_profile() else {
            let err = "No active Composio profile found".to_string();
            connection_error.set(Some(err.clone()));
            is_connecting.set(false);
            return Err(err);
        };

        let Some(api_key) = &profile.api_key else {
            let err = "No Composio API key configured".to_string();
            connection_error.set(Some(err.clone()));
            is_connecting.set(false);
            return Err(err);
        };

        let base_url = profile
            .base_url
            .clone()
            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
        let user_id = profile
            .user_id
            .clone()
            .or(profile.entity_id.clone())
            .unwrap_or_else(|| "default".to_string());
        let profile_id = profile.id.clone();

        let mut client = ComposioClient::new(
            api_key.clone(),
            base_url,
            profile.entity_id.clone(),
            Some(user_id.clone()),
            profile_id.clone(),
            profile.chrome_profile_directory.clone(),
        );

        // Authoritative no-auth check. The UI hint (no_auth param, from the
        // catalog) is unreliable — Composio often signals no-auth only via
        // auth_schemes=["NO_AUTH"], and get_auth_config_id can find/create a
        // config without ever surfacing the 303 rejection. So when the hint is
        // unset, confirm against the toolkit metadata before touching auth.
        let requires_no_auth = if no_auth {
            true
        } else {
            match client.get_toolkit_metadata(&toolkit_slug).await {
                Ok(meta) => meta.requires_no_auth(),
                Err(e) => {
                    tracing::debug!(
                        "Toolkit metadata lookup for '{}' failed ({}); assuming auth required",
                        toolkit_slug,
                        e
                    );
                    false
                }
            }
        };

        // Step 1: Get or create auth config (reuse existing if available).
        // No-auth toolkits (e.g. hackernews) skip auth entirely: Composio
        // rejects auth config creation for them with error code 303, and their
        // tools work without a connected account.
        let auth_config_id: Option<String> = if requires_no_auth {
            tracing::info!(
                "[Step 1/5] Toolkit '{}' requires no authentication — skipping auth config",
                toolkit_slug
            );
            None
        } else {
            tracing::info!("[Step 1/5] Resolving Auth Config...");
            match client.get_auth_config_id(&toolkit_slug).await {
                Ok(id) => {
                    tracing::info!(
                        "Resolved auth config '{}' for toolkit '{}'",
                        id,
                        toolkit_slug
                    );
                    Some(id)
                }
                // Self-heal: registry listings sometimes omit the no_auth flag;
                // Composio's 303 rejection is authoritative, so fall back to the
                // no-auth path instead of failing the connection.
                Err(e) if crate::mcp::composio_client::auth::is_no_auth_toolkit_error(&e) => {
                    tracing::info!(
                        "Toolkit '{}' reported as no-auth by Composio (code 303) — continuing without auth config",
                        toolkit_slug
                    );
                    None
                }
                Err(e) => {
                    let msg = format!("Failed to resolve auth config: {}", e);
                    tracing::error!("{}", msg);
                    connection_error.set(Some(msg.clone()));
                    is_connecting.set(false);
                    return Err(msg);
                }
            }
        };

        // Step 2: Initiate OAuth (Proxy Link) - AUTH FIRST
        // Skipped for no-auth toolkits: there is no account to connect.
        if auth_config_id.is_some() {
            tracing::info!("[Step 2/5] Initiating OAuth...");
            connection_status.set("Authenticating...".to_string());

            match client
                .initiate_connection(&toolkit_slug, &user_id, false)
                .await
            {
                Ok(result_msg) => {
                    tracing::info!("Connection result for {}: {}", toolkit_slug, result_msg);
                    // Wait implied by await
                }
                Err(e) => {
                    let msg = format!("Authentication failed: {}", e);
                    tracing::error!("{}", msg);
                    connection_error.set(Some(msg.clone()));
                    is_connecting.set(false);
                    return Err(msg);
                }
            }
        } else {
            tracing::info!("[Step 2/5] Skipping OAuth — no authentication required");
        }

        // Step 3: Add Toolkit to Server (PATCH Registry)
        tracing::info!("[Step 3/5] Patching MCP Server...");
        connection_status.set("Configuring Server...".to_string());

        // Pass None so the vacuum fix handles non-standard slugs (e.g. spaces).
        // Step 4 will trigger LLM selection if tools exceed TOOL_SELECTION_THRESHOLD,
        // UNLESS admin has pre-configured allowed_tools (security override).
        let patch_result = client
            .add_toolkit_to_server(&toolkit_slug, auth_config_id.as_deref(), None)
            .await;

        // Track admin-curated tools detected during PATCH for Step 4 skip logic
        let admin_tools_detected;

        match patch_result {
            Ok(result) => {
                admin_tools_detected = !result.existing_toolkit_tools.is_empty();
                if admin_tools_detected {
                    tracing::info!(
                        "[Step 3/5] Admin has pre-configured {} tools for '{}'. Will skip re-selection.",
                        result.existing_toolkit_tools.len(),
                        toolkit_slug
                    );
                }

                if let Some(new_server_url) = result.new_server_url {
                    tracing::info!(
                        "New MCP server created/updated: {}. Syncing client base URL.",
                        new_server_url
                    );

                    // SYNC FIX: Update client internal URL
                    client.base_url = new_server_url.clone();

                    // GLOBAL SYNC: Update the global servers map so Status and other calls
                    // use the correct, newly-provisioned server URL immediately.
                    // Key format MUST be ID-based for stability: "composio-native:{profile_id}"
                    {
                        let server_key = composio_server_key(&profile.id);
                        let mut s = self.servers.lock().await;
                        if let Some(existing) = s.get_mut(&server_key) {
                            existing.service =
                                McpClientType::NativeComposio(std::sync::Arc::new(client.clone()));
                        } else {
                            let active_client = ActiveMcpClient {
                                config: McpServerConfig::composio_stub(composio_server_key(
                                    &profile.id,
                                )),
                                service: McpClientType::NativeComposio(std::sync::Arc::new(
                                    client.clone(),
                                )),
                                tools: Vec::new(),
                                warning_message: None,
                                profile_id: Some(profile.id.clone()),
                            };
                            s.insert(server_key.clone(), active_client);
                        }
                    }

                    {
                        let mut s = settings_signal.write();
                        if let Some(p) = s.composio_profiles.iter_mut().find(|p| p.id == profile_id)
                        {
                            p.base_url = Some(new_server_url.clone());
                        }
                    }
                    let updated = settings_signal.peek().clone();
                    let sm = settings_manager.clone();
                    spawn(async move {
                        let _ = tokio::task::spawn_blocking(move || sm.save(&updated)).await;
                    });
                } else {
                    tracing::debug!("Used existing MCP server for toolkit '{}'", toolkit_slug);
                }
            }
            Err(e) => {
                let msg = format!("Failed to configure server: {}", e);
                tracing::error!("{}", msg);
                connection_error.set(Some(msg.clone()));
                is_connecting.set(false);
                return Err(msg);
            }
        }

        // Step 4: Smart Selection & Binding
        // SECURITY: Skip re-selection if admin has pre-configured allowed_tools for this toolkit.
        // This prevents Hobbes from overwriting admin curation (e.g., disabled SEND actions).
        if admin_tools_detected {
            tracing::info!(
                "[Step 4/5] Skipping tool selection — admin-configured tools are authoritative for '{}'",
                toolkit_slug
            );
            connection_status.set("Admin tools preserved".to_string());
        } else {
            tracing::info!("[Step 4/5] optimizing Tool Selection...");
            connection_status.set("Optimizing Tools...".to_string());

            let selected_tools: Option<Vec<String>> =
                match client.get_toolkit_tools_detailed(&toolkit_slug).await {
                    Ok(tools) if tools.len() > TOOL_SELECTION_THRESHOLD => {
                        connection_status.set(format!("Selecting from {} tools...", tools.len()));
                        let candidates: Vec<ToolCandidate> = tools
                            .into_iter()
                            .map(|(name, desc)| ToolCandidate {
                                name,
                                description: desc,
                            })
                            .collect();

                        let request =
                            ToolSelectionRequest::new(toolkit_slug.clone(), None, candidates);

                        // Use the actively selected LLM connector for smart selection.
                        // Fall back to any configured connector so selection still
                        // works when the active one has no credentials.
                        let instance = settings_snapshot
                            .active_connector()
                            .filter(|c| settings_snapshot.is_connector_configured(c))
                            .or_else(|| {
                                settings_snapshot
                                    .llm_connectors
                                    .iter()
                                    .find(|c| settings_snapshot.is_connector_configured(c))
                            })
                            .cloned();

                        let selection = match instance {
                            Some(instance) => {
                                tracing::info!(
                                    "Tool selection using connector '{}' ({:?}) for toolkit '{}'",
                                    instance.name,
                                    instance.provider(),
                                    toolkit_slug
                                );
                                let connector =
                                    crate::llm::build_connector_for_instance(&instance, None);
                                connector.select_tools_for_toolkit(&request).await
                            }
                            None => {
                                Err("No LLM provider is configured for tool selection".to_string())
                            }
                        };

                        match selection {
                            Ok(selection) => Some(selection.selected_tools),
                            Err(e) => {
                                tracing::warn!("LLM tool selection failed: {}. Using default.", e);
                                None
                            }
                        }
                    }
                    Ok(tools) if !tools.is_empty() => {
                        // Small toolkit: use all tools explicitly
                        tracing::info!(
                            "Toolkit '{}' has {} tools (under threshold), using all.",
                            toolkit_slug,
                            tools.len()
                        );
                        Some(tools.into_iter().map(|(name, _)| name).collect())
                    }
                    _ => None,
                };

            // If selected_tools is present, re-patch to apply filter
            if let Some(tools) = selected_tools.clone() {
                tracing::info!("Applying smart selection of {} tools", tools.len());
                if let Err(e) = client
                    .add_toolkit_to_server(&toolkit_slug, auth_config_id.as_deref(), Some(tools))
                    .await
                {
                    tracing::warn!("Failed to apply smart tool selection: {}", e);
                }
            }
        } // end of !admin_tools_detected branch

        // Step 5: Final Reload
        tracing::info!("[Step 5/5] Finalizing Configuration...");
        {
            // Authoritative no-auth signal: auth resolution ended without an
            // auth config (covers both the caller's hint and the 303 self-heal).
            let effective_no_auth = auth_config_id.is_none();
            let mut s = settings_signal.write();
            if let Some(profile) = s.get_active_profile_mut() {
                if let Some(existing) = profile
                    .toolkit_configs
                    .iter_mut()
                    .find(|c| c.slug == toolkit_slug)
                {
                    // Keep the no-auth flag current (a no-auth toolkit has no
                    // connected account, so this flag is what marks it connected).
                    existing.no_auth = effective_no_auth;
                } else {
                    profile
                        .toolkit_configs
                        .push(crate::settings::ComposioToolkitConfig {
                            slug: toolkit_slug.clone(),
                            display_name: toolkit_slug.clone(),
                            tool_count: 0,
                            force_load: false,
                            load_mode: crate::settings::ToolkitLoadMode::OnDemand,
                            no_auth: effective_no_auth,
                        });
                }
            }
            let updated = s.clone();
            let sm = settings_manager.clone();
            spawn(async move {
                let _ = tokio::task::spawn_blocking(move || sm.save(&updated)).await;
            });
        }

        // Reload tools
        let updated_settings = settings_signal.peek().clone();
        if let Err(e) = self.reload_composio_tools(&updated_settings).await {
            tracing::warn!("Failed to reload tools: {}", e);
        }

        // Update Context — scope to the active profile so we don't surface a
        // different profile's Composio tools right after connecting.
        mcp_context_signal.set(self.get_mcp_context(Some(profile_id.clone())).await);
        self.invalidate_status_cache();

        let current_trigger = *trigger_search.peek();
        trigger_search.set(current_trigger + 1);
        connected_slugs.write().insert(toolkit_slug.to_lowercase());

        is_connecting.set(false);
        connection_status.set("Connected".to_string());
        tracing::info!("6-Point Connection Flow Complete for '{}'", toolkit_slug);

        Ok(())
    }
    pub async fn get_client(&self, server_name: &str) -> Result<ActiveMcpClient, String> {
        let servers = self.servers.lock().await; // Lock held only for this lookup

        // 1. Direct lookup
        if let Some(client) = servers.get(server_name) {
            return Ok(client.clone());
        }

        // 2. Fallback: Search by config name (Case: we have ID key but requested by Name or vice versa)
        for client in servers.values() {
            if client.config.name == server_name {
                return Ok(client.clone());
            }
        }

        Err(format!("Server '{}' not found", server_name))
    }

    pub async fn initiate_composio_auth(
        &self,
        server_name: &str,
        tool_name: &str,
        profile_id: Option<String>,
    ) -> Result<String, String> {
        let actual_server_name = if let Some(ref pid) = profile_id {
            composio_server_key(pid)
        } else {
            server_name.to_string()
        };

        let client_wrapper = self.get_client(&actual_server_name).await?;

        if let McpClientType::NativeComposio(client) = client_wrapper.service {
            // Heuristic: Extract toolkit slug from tool name.
            // Composio tool names are typically UPPERCASE_ACTION, e.g. CLICKUP_GET_SPACES
            // We need to map this to "clickup".
            // A simple heuristic is to take the first part before the first underscore.
            // If there's no underscore, assume the whole name is the slug (unlikely but safe fallback).
            let toolkit_slug = tool_name
                .split('_')
                .next()
                .unwrap_or(tool_name)
                .to_lowercase();

            // We need a user_id. The client has one internally, but initiate_connection takes one optionally override.
            // We'll pass "" and let the client use its internal one, or we can fetch the profile.
            // The client.initiate_connection implementation uses self.user_id if available.
            // We passed it in 'new', so it should be there.

            client.initiate_connection(&toolkit_slug, "", false).await
        } else {
            Err(format!("Server '{}' is not a Composio client", server_name))
        }
    }

    pub async fn get_all_server_statuses(&self) -> Vec<McpServerStatus> {
        // Return cached data if available
        if let Some(cached) = self.cached_server_statuses.lock().await.clone() {
            tracing::debug!("Returning {} cached server statuses", cached.len());
            return cached;
        }

        let mut statuses = Vec::new();

        // Get all configs
        let configs = if let Some(config_path) = &self.config_path {
            self.load_configs(config_path.clone()).await
        } else {
            Vec::new()
        };

        let servers = self.servers.lock().await;
        let failed = self.failed_servers.lock().await;
        let auth_required = self.auth_required_servers.lock().await;
        let unloaded = self.unloaded_servers.lock().await;
        let on_demand = self.on_demand_servers.lock().await;
        let dynamic_tool_sources = self.dynamic_local_tool_sources.lock().await;

        // First, process all configs from the JSON file
        for config in configs {
            let is_on_demand = on_demand.contains(&config.name);
            let is_loaded = !unloaded.contains(&config.name);
            let status = if !is_loaded {
                // User has manually unloaded/disabled this server
                McpServerStatus {
                    uri: config.uri.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::Disabled)
                }
            } else if config.disabled {
                McpServerStatus {
                    uri: config.uri.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::Disabled)
                }
            } else if let Some(client) = servers.get(&config.name) {
                // For on-demand servers, count only tools that have been explicitly
                // loaded via MCP_LOAD_SERVER_TOOLS into dynamic_local_tools.
                // For normal (always-on) servers, all tools are always loaded.
                let total_tools = client.tools.len();
                let loaded_tools = if is_on_demand {
                    dynamic_tool_sources
                        .values()
                        .filter(|src| src.as_str() == config.name.as_str())
                        .count()
                } else {
                    total_tools
                };
                McpServerStatus {
                    tools: total_tools,
                    loaded_tools,
                    is_loaded: true,
                    is_on_demand,
                    uri: config.uri.clone(),
                    warning_message: client.warning_message.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::Loaded)
                }
            } else if let Some(auth_info) = auth_required.get(&config.name) {
                // Server requires OAuth authentication
                McpServerStatus {
                    error_message: Some(auth_info.error_message.clone()),
                    auth_url: auth_info.auth_url.clone(),
                    uri: config.uri.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::NeedsAuth)
                }
            } else if let Some((_, error)) = failed.get(&config.name) {
                McpServerStatus {
                    error_message: Some(error.clone()),
                    uri: config.uri.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::Error)
                }
            } else {
                // Server is still initializing or hasn't been attempted yet
                McpServerStatus {
                    error_message: Some("Initializing...".to_string()),
                    uri: config.uri.clone(),
                    ..McpServerStatus::new(config.name.clone(), config.description.clone(), ServerStatus::Error)
                }
            };
            statuses.push(status);
        }

        // Special handling for the native Composio client - it's not in configs
        // but could still be active or failed. Now uses "composio-native:{profile}" format.
        {
            // Select the ACTIVE profile's composio-native client so the status
            // card reports the current profile's tools, not an arbitrary
            // profile's (several accumulate once multiple profiles connect).
            let active_profile_id = self
                .settings
                .peek()
                .get_active_profile()
                .map(|p| p.id.clone());
            let active_composio: Option<(&String, &ActiveMcpClient)> =
                active_composio_key(&servers, active_profile_id.as_deref())
                    .and_then(|k| servers.get_key_value(&k));

            // Find any failed composio-native client
            let failed_composio: Option<(&String, &(McpServerConfig, String))> =
                failed.iter().find(|(k, _)| is_composio_native(k));

            let display_name = COMPOSIO_NATIVE_PREFIX.to_string();

            if let Some((name, client)) = active_composio {
                let composio_is_loaded = !unloaded.contains(name);
                statuses.push(McpServerStatus {
                    name: display_name.clone(),
                    tools: client.tools.len(),
                    loaded_tools: client.tools.len(),
                    is_loaded: composio_is_loaded,
                    warning_message: client.warning_message.clone(),
                    ..McpServerStatus::new(display_name.clone(), "Native Composio API client".to_string(), ServerStatus::Loaded)
                });
            }
            // Check if native Composio failed to initialize
            else if let Some((_, (_, error))) = failed_composio {
                statuses.push(McpServerStatus {
                    name: display_name.clone(),
                    error_message: Some(error.clone()),
                    ..McpServerStatus::new(display_name.clone(), "Native Composio API client".to_string(), ServerStatus::Error)
                });
            }
            // Not loaded, not failed = API key not configured (NotConfigured state)
            else {
                statuses.push(McpServerStatus {
                    name: display_name.clone(),
                    error_message: Some("Use the Marketplace to connect your first tool, or add your API key in Settings".to_string()),
                    ..McpServerStatus::new(display_name.clone(), "Connect external tools like Gmail, GitHub, Slack".to_string(), ServerStatus::NotConfigured)
                });
            }
        }

        // Status reporting for native built-in clients
        for (server_key, display_name) in [
            ("hobbes-native-image", "Image Generation"),
            (HOBBES_CORE_SERVER, "Hobbes Core Tools"),
            (HOBBES_META_SERVER, "Hobbes Meta Tools"),
        ] {
            if let Some((_k, client)) = servers.iter().find(|(k, _)| k.as_str() == server_key) {
                statuses.push(McpServerStatus {
                    display_name: display_name.to_string(),
                    tools: client.tools.len(),
                    loaded_tools: client.tools.len(),
                    is_loaded: true,
                    ..McpServerStatus::new(server_key.to_string(), client.config.description.clone(), ServerStatus::Loaded)
                });
            }
        }

        // Cache the result before returning
        *self.cached_server_statuses.lock().await = Some(statuses.clone());
        tracing::debug!("Cached {} server statuses", statuses.len());

        statuses
    }

    /// Invalidate the server status cache (call on profile change or server state change)
    pub fn invalidate_status_cache(&self) {
        // Use try_lock to avoid deadlocks in sync context
        if let Ok(mut cache) = self.cached_server_statuses.try_lock() {
            *cache = None;
            tracing::debug!("Invalidated server status cache");
        } else {
            tracing::warn!(
                "invalidate_status_cache: try_lock failed — mutex contended, cache NOT cleared"
            );
        }
    }

    /// Async version of invalidate_status_cache for use in async contexts
    pub async fn invalidate_status_cache_async(&self) {
        let mut cache = self.cached_server_statuses.lock().await;
        *cache = None;
        tracing::debug!("Invalidated server status cache (async)");
    }

    pub async fn retry_server(
        &self,
        server_name: &str,
        mcp_context_signal: dioxus::prelude::Signal<McpContext>,
        settings: crate::settings::Settings,
        access_token: Option<String>,
    ) -> Result<(), String> {
        // Handle virtual Composio-native servers first — they don't exist in mcp_servers.json
        if is_composio_native(server_name) {
            // Remove from failed servers
            self.failed_servers.lock().await.remove(server_name);

            let server_name_owned = server_name.to_string();
            let settings_clone = settings.clone();
            let servers_clone = self.servers.clone();
            let failed_servers_clone = self.failed_servers.clone();
            let self_clone = self.clone();
            let mut mcp_context_signal_clone = mcp_context_signal;

            spawn(async move {
                tracing::info!("Retrying virtual Composio server: {}", server_name_owned);
                let Some(profile) = settings_clone.get_active_profile() else {
                    tracing::error!("No active Composio profile for retry");
                    return;
                };

                match self_clone
                    .initialize_native_composio_for_profile(profile)
                    .await
                {
                    Ok(active_client) => {
                        servers_clone
                            .lock()
                            .await
                            .insert(server_name_owned.clone(), active_client);
                        self_clone.invalidate_status_cache_async().await;
                        let new_context = self_clone.get_mcp_context(None).await;
                        mcp_context_signal_clone.set(new_context);
                        tracing::info!(
                            "Successfully retried virtual Composio client: {}",
                            server_name_owned
                        );
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to retry Composio: {}", e);
                        if !McpManager::is_needs_setup_error(&error_msg) {
                            tracing::error!("{}", error_msg);
                            let stub = McpServerConfig::composio_stub(server_name_owned.clone());
                            failed_servers_clone
                                .lock()
                                .await
                                .insert(server_name_owned, (stub, error_msg));
                            self_clone.invalidate_status_cache_async().await;
                            let new_context = self_clone.get_mcp_context(None).await;
                            mcp_context_signal_clone.set(new_context);
                        }
                    }
                }
            });
            return Ok(());
        }

        // Load config for the specific server
        let configs = if let Some(config_path) = &self.config_path {
            self.load_configs(config_path.clone()).await
        } else {
            return Err("No config path available".to_string());
        };

        let mut server_config = configs
            .into_iter()
            .find(|c| c.name == server_name)
            .ok_or_else(|| format!("Server '{}' not found in config", server_name))?;

        if let Some(key) = &settings.smithery_api_key {
            server_config
                .env
                .insert("SMITHERY_API_KEY".to_string(), key.clone());
        }
        if let Some(key) = &settings.composio_api_key {
            server_config
                .env
                .insert("COMPOSIO_API_KEY".to_string(), key.clone());
        }

        if server_config.disabled {
            return Err("Server is disabled".to_string());
        }

        // Remove from failed servers and auth_required_servers
        self.failed_servers.lock().await.remove(server_name);
        self.auth_required_servers.lock().await.remove(server_name);

        // Launch the server (reusing the same logic from launch_servers)
        let server_config_clone = server_config.clone();
        let settings_clone = settings.clone();
        let failed_servers_clone = self.failed_servers.clone();
        let auth_required_servers_clone = self.auth_required_servers.clone();
        let servers_clone = self.servers.clone();
        let self_clone = self.clone();
        let mut mcp_context_signal_clone = mcp_context_signal;
        let access_token_clone = access_token.clone();

        spawn(async move {
            let server_name = server_config_clone.name.clone();
            tracing::info!("Retrying MCP server: {}", server_name);

            // Determine effective configuration (handle local -> remote upgrade with token)
            let mut effective_config = server_config_clone.clone();
            if effective_config.uri.is_none() && access_token_clone.is_some() {
                tracing::info!(
                    "Upgrading local server '{}' to remote Smithery endpoint using OAuth token",
                    server_name
                );
                effective_config.uri =
                    Some(format!("https://server.smithery.ai/{}/mcp", server_name));
                // Don't run the local command since we are connecting remotely
                effective_config.command = None;
            }

            if server_name == "composio" {
                if let Some(profile) = settings_clone.get_active_profile() {
                    if let Some(api_key) = &profile.api_key {
                        let base_url = profile
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
                        let has_server_uuid = profile.base_url.as_ref()
                            .is_some_and(|u| u.contains("/v3/mcp/"));

                        let entity_id = profile.entity_id.clone();
                        let user_id = profile.user_id.clone();

                        tracing::info!(
                            "Initializing Composio Client (Retry). UserID: {:?}, EntityID: {:?}, has_server_uuid: {}",
                            user_id,
                            entity_id,
                            has_server_uuid
                        );

                        let composio_client = Arc::new(ComposioClient::new(
                            api_key.clone(),
                            base_url,
                            entity_id,
                            user_id,
                            profile.id.clone(),
                            profile.chrome_profile_directory.clone(),
                        ));
                        let client_for_tools = composio_client.clone();

                        // Guard: skip MCP list_tools when no server UUID (prevents 10401)
                        if !has_server_uuid {
                            tracing::warn!("No MCP server UUID in base_url (retry) — skipping retry");
                            return;
                        }
                        match client_for_tools.list_tools().await {
                            Ok(discovery_result) => {
                                let tools = discovery_result
                                    .tools
                                    .iter()
                                    .map(composio_to_rmcp_tool)
                                    .collect();
                                let active_client = ActiveMcpClient {
                                    config: server_config_clone,
                                    service: McpClientType::NativeComposio(composio_client),
                                    tools,
                                    warning_message: discovery_result.warning,
                                    profile_id: Some(profile.id.clone()),
                                };
                                servers_clone
                                    .lock()
                                    .await
                                    .insert(server_name.clone(), active_client);
                                // Invalidate status cache after client mutation (Pattern 150.8)
                                self_clone.invalidate_status_cache_async().await;
                                let new_context = self_clone.get_mcp_context(None).await;
                                mcp_context_signal_clone.set(new_context);
                                tracing::info!("Successfully reconnected Composio client");
                            }
                            Err(e) => {
                                let error_msg =
                                    format!("Failed to list Composio tools on retry: {}", e);
                                // Check if this is a "needs setup" error (405, no tools, etc.)
                                if McpManager::is_needs_setup_error(&error_msg) {
                                    tracing::debug!("Composio needs initial setup (connect first tool via Marketplace): {}", e);
                                } else {
                                    tracing::error!("{}", error_msg);
                                    failed_servers_clone
                                        .lock()
                                        .await
                                        .insert(server_name, (server_config_clone, error_msg));
                                    self_clone.invalidate_status_cache_async().await;
                                }
                            }
                        }
                    } else {
                        let error_msg =
                            "Composio API key not configured for active profile".to_string();
                        tracing::error!("{}", error_msg);
                        failed_servers_clone
                            .lock()
                            .await
                            .insert(server_name, (server_config_clone, error_msg));
                        self_clone.invalidate_status_cache_async().await;
                    }
                } else {
                    let error_msg = "No active Composio profile found".to_string();
                    tracing::error!("{}", error_msg);
                    failed_servers_clone
                        .lock()
                        .await
                        .insert(server_name, (server_config_clone, error_msg));
                    self_clone.invalidate_status_cache_async().await;
                }
                return;
            }

            let service_result = if let Some(uri) = effective_config.uri.clone() {
                // Network-based server (SSE)
                if let Some(command_string) = effective_config.command.clone() {
                    let server_name_clone = server_name.clone();
                    let server_config_clone_for_spawn = effective_config.clone();
                    spawn(async move {
                        let mut cmd = Command::new(&command_string);
                        if let Some(args) = server_config_clone_for_spawn.args {
                            for arg in args {
                                cmd.arg(arg);
                            }
                        }
                        // Inject critical env vars (HOME, USER, etc.) + sane PATH
                        let mut envs = server_config_clone_for_spawn.env.clone();
                        for (key, value) in Self::get_critical_env_vars() {
                            envs.entry(key).or_insert(value);
                        }
                        let current_path = std::env::var("PATH").unwrap_or_default();
                        let sane_path = Self::get_sane_path();
                        let final_path = if current_path.is_empty() {
                            sane_path
                        } else {
                            format!("{}:{}", sane_path, current_path)
                        };
                        envs.insert("PATH".to_string(), final_path);
                        cmd.envs(&envs);
                        if let Err(e) = cmd.status().await {
                            tracing::error!(
                                "Failed to launch command for MCP server '{}': {}",
                                server_name_clone,
                                e
                            );
                        }
                    });
                }
                tracing::info!(
                    "Connecting to network MCP server '{}' at {}",
                    server_name,
                    uri
                );
                // For SSE servers, auth tokens should be provided via env vars
                // or directly in the server config as needed

                // Use authenticated transport for Bearer token support (API keys, etc.)
                // Priority: Explicit access_token > COMPOSIO_API_KEY from env
                let token_to_use = access_token_clone
                    .clone()
                    .or_else(|| effective_config.env.get("COMPOSIO_API_KEY").cloned());

                // If connecting to Composio, ensure transport=sse param is present
                // Using POST as confirmed by manual curl test
                let mut final_uri = uri.clone();
                let use_post = if uri.contains("composio.dev") {
                    if !final_uri.contains("transport=sse") {
                        let separator = if final_uri.contains('?') { "&" } else { "?" };
                        final_uri = format!("{}{}transport=sse", final_uri, separator);
                    }
                    true // Use POST for Composio
                } else {
                    false
                };

                // Debug log for URI
                tracing::info!("Final Composio URI (use_post={}): {}", use_post, final_uri);

                // For Composio POST requests, we need both POST method AND correct headers/body
                // authenticated_sse handles the headers/body logic when use_post is true
                let auth_header = if uri.contains("composio.dev") {
                    Some("x-api-key".to_string())
                } else {
                    None
                };
                let auth_prefix = if uri.contains("composio.dev") {
                    Some("".to_string())
                } else {
                    None
                };
                let transport = match crate::mcp::authenticated_sse::create_authenticated_transport(
                    &final_uri,
                    token_to_use,
                    use_post,
                    auth_header,
                    auth_prefix,
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        let mut auth_url = None;
                        let mut is_auth_error = false;

                        // Check specific error type
                        if let SseTransportError::Client(AuthenticatedClientError::AuthRequired(
                            url,
                        )) = &e
                        {
                            auth_url = Some(url.clone());

                            is_auth_error = true;
                        }

                        let error_msg = format!("Failed to start SSE transport: {}", e);
                        if !is_auth_error {
                            is_auth_error = Self::is_auth_error(&error_msg);
                        }

                        tracing::error!("{}", error_msg);

                        if is_auth_error {
                            auth_required_servers_clone.lock().await.insert(
                                server_name.clone(),
                                AuthRequiredInfo {
                                    config: server_config_clone,
                                    auth_url,
                                    error_message: error_msg,
                                    profile: None,
                                },
                            );
                        } else {
                            failed_servers_clone
                                .lock()
                                .await
                                .insert(server_name, (server_config_clone, error_msg));
                        }
                        self_clone.invalidate_status_cache_async().await;
                        return;
                    }
                };
                ().serve(transport).await
            } else {
                // Stdio-based server
                tracing::trace!("Launching stdio MCP server: {}", server_name);
                let mut cmd = Command::new("sh");
                let mut command_string = server_config_clone.command.clone().unwrap_or_default();

                if let Some(ref args) = server_config_clone.args {
                    command_string.push(' ');
                    command_string.push_str(&args.join(" "));
                }

                if server_name == "filesystem" {
                    if let Some(project_folder) = &settings_clone.project_folder {
                        command_string.push_str(&format!(" \"{}\"", project_folder));
                        tracing::trace!(
                            "Appending project folder to filesystem MCP command: {}",
                            command_string
                        );
                    }
                }

                // Inject sane PATH and critical environment variables (HOME, USER, SHELL, TMPDIR)
                // Without HOME, tools like Playwright fail trying to create dirs at '/'
                let mut envs = server_config_clone.env.clone();
                for (key, value) in Self::get_critical_env_vars() {
                    envs.entry(key).or_insert(value);
                }

                let current_path = std::env::var("PATH").unwrap_or_default();
                let sane_path = Self::get_sane_path();
                let final_path = if current_path.is_empty() {
                    sane_path
                } else {
                    format!("{}:{}", sane_path, current_path)
                };
                envs.insert("PATH".to_string(), final_path);

                cmd.arg("-c")
                    .arg(&command_string)
                    .envs(&envs)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                // Set a sane working directory (see note at line ~1395)
                if let Some(home) = dirs::home_dir() {
                    cmd.current_dir(home);
                }

                match TokioChildProcess::new(cmd) {
                    Ok(transport) => ().serve(transport).await,
                    Err(e) => {
                        tracing::error!(
                            "Failed to launch stdio MCP server '{}': {}",
                            server_name,
                            e
                        );
                        return;
                    }
                }
            };

            match service_result {
                Ok(service) => {
                    tracing::trace!("Connected to MCP server: {}", server_name);

                    // Fetch all pages of tools
                    let mut all_tools = Vec::new();
                    let mut next_cursor: Option<String> = None;

                    loop {
                        let cursor = next_cursor.clone();
                        // Same here for the retry_server function
                        let request_param =
                            cursor.map(|c| PaginatedRequestParam { cursor: Some(c) });

                        match service.list_tools(request_param).await {
                            Ok(result) => {
                                all_tools.extend(result.tools);

                                if let Some(cursor) = result.next_cursor {
                                    if !cursor.is_empty() {
                                        next_cursor = Some(cursor);
                                        continue;
                                    }
                                }
                                break;
                            }
                            Err(e) => {
                                let error_msg = format!("Failed to list tools: {}", e);
                                tracing::error!(
                                    "Failed to list tools for '{}': {}",
                                    server_name,
                                    e
                                );
                                // Check if this is an auth error
                                if Self::is_auth_error(&error_msg) {
                                    tracing::info!(
                                        "Server '{}' requires authentication",
                                        server_name
                                    );
                                    auth_required_servers_clone.lock().await.insert(
                                        server_name.clone(),
                                        AuthRequiredInfo {
                                            config: server_config_clone,
                                            auth_url: None, // TODO: Extract from error if available
                                            error_message: error_msg,
                                            profile: None,
                                        },
                                    );
                                } else {
                                    failed_servers_clone.lock().await.insert(
                                        server_name.clone(),
                                        (server_config_clone, error_msg),
                                    );
                                }
                                self_clone.invalidate_status_cache_async().await;
                                return;
                            }
                        }
                    }

                    tracing::trace!(
                        "Discovered {} capabilities for MCP server: {}",
                        all_tools.len(),
                        server_name
                    );
                    let active_client = ActiveMcpClient {
                        config: server_config_clone.clone(),
                        service: McpClientType::Service(Arc::new(service)),
                        tools: all_tools,
                        warning_message: None,
                        profile_id: None,
                    };
                    servers_clone
                        .lock()
                        .await
                        .insert(server_name.clone(), active_client);

                    // Invalidate status cache after client mutation (Pattern 150.8)
                    self_clone.invalidate_status_cache_async().await;

                    // Update context
                    let new_context = self_clone.get_mcp_context(None).await;
                    mcp_context_signal_clone.set(new_context);
                    tracing::info!("Successfully reconnected '{}'", server_name);
                }
                Err(e) => {
                    let error_msg = format!("Failed to serve: {}", e);
                    tracing::error!("Failed to serve MCP server '{}': {}", server_name, e);
                    if Self::is_auth_error(&error_msg) {
                        tracing::info!("Server '{}' requires authentication", server_name);
                        auth_required_servers_clone.lock().await.insert(
                            server_name,
                            AuthRequiredInfo {
                                config: server_config_clone,
                                auth_url: None,
                                error_message: error_msg,
                                profile: None,
                            },
                        );
                    } else {
                        failed_servers_clone
                            .lock()
                            .await
                            .insert(server_name, (server_config_clone, error_msg));
                    }
                    self_clone.invalidate_status_cache_async().await;
                }
            }
        });

        Ok(())
    }

    /// Attempt to restart a dead stdio MCP server inline (blocking the caller).
    /// This is used by `use_mcp_tool` when it detects a TransportClosed error.
    /// Unlike `retry_server` (which spawns and returns immediately), this method
    /// awaits the full reconnect lifecycle so the caller can retry the tool call.
    async fn reconnect_stdio_server(
        &self,
        server_name: &str,
        config: McpServerConfig,
    ) -> Result<(), String> {
        tracing::info!(
            "[RECONNECT] Restarting stdio MCP server '{}'",
            server_name
        );

        // 1. Remove the dead server entry
        {
            let mut servers = self.servers.lock().await;
            servers.remove(server_name);
        }

        // 2. Rebuild the Command (same logic as launch_servers for stdio)
        let command_base = config
            .command
            .as_deref()
            .ok_or_else(|| {
                format!(
                    "Cannot reconnect '{}': no command configured (SSE servers cannot be reconnected this way)",
                    server_name
                )
            })?;

        let mut cmd = Command::new(command_base);

        if let Some(ref args) = config.args {
            for arg in args {
                cmd.arg(arg);
            }
        }

        // Special handling for filesystem server
        if server_name == "filesystem" {
            let settings = self.settings.peek().clone();
            if let Some(project_folder) = &settings.project_folder {
                cmd.arg(project_folder);
            }
        }

        // Inject sane PATH and critical environment variables
        let mut envs = config.env.clone();
        for (key, value) in Self::get_critical_env_vars() {
            envs.entry(key).or_insert(value);
        }

        let current_path = std::env::var("PATH").unwrap_or_default();
        let sane_path = Self::get_sane_path();
        let final_path = if current_path.is_empty() {
            sane_path
        } else {
            format!("{}:{}", sane_path, current_path)
        };
        envs.insert("PATH".to_string(), final_path);

        cmd.envs(&envs)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Set a sane working directory (see note at line ~1395)
        if let Some(home) = dirs::home_dir() {
            cmd.current_dir(home);
        }

        // 3. Spawn the child process and initialize the rmcp service
        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            format!(
                "Failed to launch stdio MCP server '{}': {}",
                server_name, e
            )
        })?;

        let service = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            ().serve(transport),
        )
        .await
        .map_err(|_| {
            format!(
                "Timeout waiting for stdio MCP server '{}' to initialize",
                server_name
            )
        })?
        .map_err(|e| format!("Failed to serve MCP server '{}': {}", server_name, e))?;

        // 4. Discover tools from the new server
        let mut all_tools = Vec::new();
        let mut next_cursor: Option<String> = None;

        loop {
            let cursor = next_cursor.clone();
            let request_param = cursor.map(|c| PaginatedRequestParam { cursor: Some(c) });

            match service.list_tools(request_param).await {
                Ok(result) => {
                    all_tools.extend(result.tools);
                    if let Some(cursor) = result.next_cursor {
                        if !cursor.is_empty() {
                            next_cursor = Some(cursor);
                            continue;
                        }
                    }
                    break;
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to list tools after reconnecting '{}': {}",
                        server_name, e
                    ));
                }
            }
        }

        tracing::info!(
            "[RECONNECT] Discovered {} tools for reconnected server '{}'",
            all_tools.len(),
            server_name
        );

        // 5. Insert the fresh client into the servers map
        let active_client = ActiveMcpClient {
            config: config.clone(),
            service: McpClientType::Service(Arc::new(service)),
            tools: all_tools,
            warning_message: None,
            profile_id: None,
        };

        {
            let mut servers = self.servers.lock().await;
            servers.insert(server_name.to_string(), active_client);
        }

        // 6. Invalidate the status cache
        self.invalidate_status_cache();

        tracing::info!(
            "[RECONNECT] Successfully restarted stdio MCP server '{}'",
            server_name
        );

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn install_mcp_server(
        &self,
        server_config: &SmitheryServerDetail,
    ) -> Result<(), String> {
        let config_path = self
            .config_path
            .as_ref()
            .ok_or("Config path not set")?
            .clone();

        // Find the correct config for the current platform
        let platform = crate::components::smithery_registry::get_platform();
        let mcp_config = server_config
            .configs
            .as_ref()
            .and_then(|configs| configs.iter().find(|c| c.platform == platform))
            .map(|c| {
                let mut env = HashMap::new();
                // Check for API key in args and move it to env var
                let mut args = c.args.clone();
                if let Some(key_index) = args.iter().position(|arg| arg.starts_with("--key")) {
                    // This assumes the key is the next argument
                    if key_index + 1 < args.len() {
                        let key_value = args.remove(key_index + 1);
                        args.remove(key_index);
                        env.insert("SMITHERY_API_KEY".to_string(), key_value);
                    }
                }

                let mut config =
                    McpServerConfig::composio_stub(server_config.qualified_name.clone());
                config.command = Some(c.command.clone());
                config.args = Some(args);
                config.env = env;
                config
            });

        if let Some(new_config) = mcp_config {
            self.add_or_update_mcp_server(&config_path, new_config)
                .await
        } else {
            Err(format!(
                "No compatible configuration found for platform '{}'",
                platform
            ))
        }
    }

    /// Helper to process the output stream from a tool call into a final status and response string.
    /// Returns (Status, ResponseString, IsPermissionRequest)
    pub async fn process_tool_output(
        mut receiver: UnboundedReceiver<Result<CallToolResult, String>>,
    ) -> (ToolCallStatus, String, bool) {
        let mut aggregated_content: Vec<rmcp::model::Content> = Vec::new();
        let mut final_status = ToolCallStatus::Completed;
        let mut error_string = None;

        while let Some(result) = receiver.recv().await {
            match result {
                Ok(call_tool_result) => {
                    aggregated_content.extend(call_tool_result.content);
                }
                Err(e) => {
                    final_status = ToolCallStatus::Error;
                    error_string = Some(e);
                    break;
                }
            }
        }

        if final_status == ToolCallStatus::Error {
            let err = error_string.unwrap_or_default();
            // Check if this error is actually a serialized ToolCall indicating a permission request
            if let Ok(_tc) = serde_json::from_str::<crate::components::shared::ToolCall>(&err) {
                (ToolCallStatus::Error, err, true)
            } else {
                (final_status, err, false)
            }
        } else {
            // Check for auth requirement
            let mut auth_url = None;
            for content in &aggregated_content {
                let json_content = serde_json::to_value(content).unwrap_or(serde_json::Value::Null);
                if let Some(text) = json_content.get("text").and_then(|t| t.as_str()) {
                    if text.contains("Authentication required")
                        && text.contains("connect your account")
                    {
                        if let Some(start) = text.find("http") {
                            auth_url = Some(text[start..].trim().to_string());
                        }
                    }
                }
            }

            if let Some(url) = auth_url {
                (ToolCallStatus::AuthRequired, url, false)
            } else {
                let final_json =
                    serde_json::to_value(aggregated_content).unwrap_or(serde_json::Value::Null);
                (
                    final_status,
                    serde_json::to_string_pretty(&final_json).unwrap_or_default(),
                    false,
                )
            }
        }
    }
}
