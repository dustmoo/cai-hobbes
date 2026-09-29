use crate::context::permissions::PermissionManager;
use crate::mcp::composio_client::ComposioClient;
use crate::SecretManagerTrait;
use dioxus::prelude::Signal;
use dioxus_signals::Readable;
use rmcp::model::Tool;
use rmcp::service::{RoleClient, RunningService};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod composio;
mod composio_toolkits;
mod config;
mod lifecycle;
mod oauth;
mod status;
mod tool_call;

// Keep the pre-split `crate::mcp::manager::X` paths resolving.
#[allow(unused_imports)]
pub(crate) use composio::{purge_composio_profile_state, resolve_composio_key, ComposioKeyLookup};

/// The bare server name for Composio native virtual clients.
pub const COMPOSIO_NATIVE_PREFIX: &str = "composio-native";

/// Check if a server name refers to a Composio native client
/// (bare `"composio-native"` or profiled `"composio-native:{id}"`).
pub fn is_composio_native(name: &str) -> bool {
    name == COMPOSIO_NATIVE_PREFIX || name.starts_with("composio-native:")
}

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

/// Canonical server name for the built-in planner tools
/// (`HOBBES_TODO_*`, `HOBBES_PLAN_DAY`, `HOBBES_TIME_BLOCK`,
/// `HOBBES_PROJECT_UPSERT`). Registered in `McpManager::servers` so the AI can
/// accurately introspect all built-in tools and their origins.
pub const HOBBES_PLANNER_SERVER: &str = "hobbes-planner";

/// The native terminal (persistent per-chat pty shells). Deliberately NOT a
/// builtin virtual server: its tools must ride the normal permission flow,
/// and it participates in load/unload like any other server.
pub const HOBBES_TERMINAL_SERVER: &str = crate::mcp::terminal_client::HOBBES_TERMINAL_SERVER;

/// Built-in virtual servers must always stay fully loaded. `hobbes-meta`
/// provides `MCP_LOAD_SERVER_TOOLS` itself — putting it on-demand (or
/// unloading it) locks the loader behind the loader and makes every
/// on-demand server unreachable.
pub fn is_builtin_virtual_server(name: &str) -> bool {
    name == HOBBES_CORE_SERVER || name == HOBBES_META_SERVER || name == HOBBES_PLANNER_SERVER
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
    /// Built-in planner tools (HOBBES_TODO_*, HOBBES_PLAN_DAY, HOBBES_TIME_BLOCK,
    /// HOBBES_PROJECT_UPSERT). Dispatch is intercepted upstream in
    /// components::builtin_tools (requires the PlannerState signal).
    /// This variant exists so the server is visible / introspectable in McpManager.
    NativePlanner,
    /// The native terminal (HOBBES_TERMINAL_*): a REAL executor like
    /// NativeImage — commands run here after the standard permission gate.
    NativeTerminal(Arc<crate::mcp::terminal_client::TerminalClient>),
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
            | McpClientType::NativeMeta
            | McpClientType::NativePlanner
            | McpClientType::NativeTerminal(_) => true,
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

    /// Initialize unloaded servers from persisted state
    pub async fn set_initial_unloaded_servers(&self, servers: Vec<String>) {
        let mut unloaded = self.unloaded_servers.lock().await;
        for server in servers {
            if is_builtin_virtual_server(&server) {
                tracing::warn!(
                    "Ignoring persisted unloaded state for built-in server '{}'",
                    server
                );
                continue;
            }
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
            if is_builtin_virtual_server(&server) {
                tracing::warn!(
                    "Ignoring persisted on-demand state for built-in server '{}'",
                    server
                );
                continue;
            }
            on_demand.insert(server);
        }
        tracing::debug!(
            "Restored {} on-demand servers from persisted state",
            on_demand.len()
        );
    }

    /// Set a server to on-demand mode (tools hidden, discoverable via meta-tool)
    pub async fn set_server_on_demand(&self, server_name: &str) {
        if is_builtin_virtual_server(server_name) {
            tracing::warn!(
                "Refusing to set built-in server '{}' to on-demand mode",
                server_name
            );
            return;
        }
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

    /// Check if a server's tools are currently visible to the AI
    #[allow(dead_code)]
    pub async fn is_server_loaded(&self, server_name: &str) -> bool {
        let unloaded = self.unloaded_servers.lock().await;
        !unloaded.contains(server_name)
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
}
