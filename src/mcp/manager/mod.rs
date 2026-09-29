use crate::components::smithery_registry::SmitheryServerDetail;
use crate::context::permissions::{PermissionManager};
use crate::mcp::authenticated_sse::AuthenticatedClientError;
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use crate::mcp::tool_selection::{ToolCandidate, ToolSelectionRequest, TOOL_SELECTION_THRESHOLD};
use crate::settings::{Settings, SettingsManager};
use crate::SecretManagerTrait;
use dioxus::prelude::spawn;
use dioxus::prelude::Signal;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{PaginatedRequestParam, Tool};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::sse_client::SseTransportError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc::{self};
use tokio::sync::Mutex;

mod config;
mod oauth;
mod status;
mod tool_call;
use tool_call::get_local_meta_tools;

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
/// Result of resolving which servers-map key holds the Composio-native client
/// for a given profile. Distinguishes "no client at all" from "several
/// clients exist but none matches" — callers that only care whether they have
/// a usable key can collapse both into `None` via a match on the result, but
/// UI/error-reporting call sites should surface the distinction: the second
/// case means Composio *is* connected, just not unambiguously for this
/// profile, which is a materially different message than "not configured".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ComposioKeyLookup {
    /// Exactly one client resolves for this profile (or is the unambiguous
    /// last resort). Carries the servers-map key.
    Found(String),
    /// No Composio-native client exists in the servers map at all.
    NotConnected,
    /// Several Composio-native clients exist and none matches the requested
    /// profile (or none was requested and more than one exists). Carries the
    /// servers-map keys of every candidate, for diagnostics.
    Ambiguous { candidates: Vec<String> },
}

impl ComposioKeyLookup {
    /// Human-readable description of an unresolved lookup, naming the actual
    /// problem instead of the generic "not initialized" callers used to emit
    /// unconditionally.
    pub(crate) fn describe(&self, profile_id: Option<&str>) -> String {
        match self {
            ComposioKeyLookup::Found(k) => format!("resolved to '{}'", k),
            ComposioKeyLookup::NotConnected => {
                "Composio client not initialized or not connected".to_string()
            }
            ComposioKeyLookup::Ambiguous { candidates } => match profile_id {
                Some(pid) => format!(
                    "multiple Composio profiles connected, none matches profile {}: {}",
                    pid,
                    candidates.join(", ")
                ),
                None => format!(
                    "multiple Composio profiles connected and no profile was specified: {}",
                    candidates.join(", ")
                ),
            },
        }
    }
}

/// Resolve the servers-map key for a specific profile's Composio-native
/// client, generic over the client value type so it can be exercised in
/// tests without constructing a real `ActiveMcpClient`. `profile_id_of`
/// extracts the profile id a given client value claims, if any.
///
/// Prefers the profile-scoped key, then a `profile_id` field match, then the
/// bare legacy prefix, then any Composio client as a last resort *only* when
/// that resort is unambiguous.
///
/// Profile-scoping is mandatory: each connected profile has its own
/// `composio-native:{id}` entry, so a bare `find(is_composio_native)` returns an
/// arbitrary client and can operate on the wrong profile (e.g. reloading tools
/// for "Clearmirror" but binding them to "Puget").
pub(crate) fn resolve_composio_key<V>(
    servers: &std::collections::HashMap<String, V>,
    profile_id: Option<&str>,
    profile_id_of: impl Fn(&V) -> Option<&str>,
) -> ComposioKeyLookup {
    if let Some(pid) = profile_id {
        let scoped = composio_server_key(pid);
        if servers.contains_key(&scoped) {
            return ComposioKeyLookup::Found(scoped);
        }
        if let Some((k, _)) = servers
            .iter()
            .find(|(k, c)| is_composio_native(k) && profile_id_of(c) == Some(pid))
        {
            return ComposioKeyLookup::Found(k.clone());
        }
    }
    if servers.contains_key(COMPOSIO_NATIVE_PREFIX) {
        return ComposioKeyLookup::Found(COMPOSIO_NATIVE_PREFIX.to_string());
    }
    // Last resort: only safe when the answer is unambiguous. Other profiles'
    // native clients now survive a reinit, so with several of them present
    // `find` would bind to an arbitrary profile — exactly the mis-binding
    // profile scoping exists to prevent. Refuse instead, loudly.
    let mut composio_keys = servers.keys().filter(|k| is_composio_native(k));
    let Some(first) = composio_keys.next() else {
        return ComposioKeyLookup::NotConnected;
    };
    if composio_keys.next().is_some() {
        let candidates: Vec<String> = servers
            .keys()
            .filter(|k| is_composio_native(k))
            .cloned()
            .collect();
        tracing::warn!(
            profile_id = ?profile_id,
            candidates = ?candidates,
            "resolve_composio_key: several Composio native clients and no profile match; refusing to guess which profile to operate on."
        );
        return ComposioKeyLookup::Ambiguous { candidates };
    }
    ComposioKeyLookup::Found(first.clone())
}

/// Whether the bare legacy `composio-native` singleton is orphaned: its client
/// claims no profile, or claims one that is no longer configured. Such a
/// singleton can never be claimed by a future profile reinit, so it would
/// otherwise linger forever as a stale `resolve_composio_key` fallback.
fn legacy_singleton_orphaned<'a>(
    singleton_profile_id: Option<&str>,
    mut configured_profile_ids: impl Iterator<Item = &'a str>,
) -> bool {
    match singleton_profile_id {
        None => true,
        Some(pid) => !configured_profile_ids.any(|id| id == pid),
    }
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

/// Every servers-map key that could hold state for one Composio profile:
/// the canonical `composio-native:{id}`, the legacy `composio-native:{name}`,
/// and — only when this profile owns it — the bare legacy singleton
/// `composio-native`. Shared by `purge_composio_profile_state` (removing
/// stale state before a reinit) and `reinitialize_composio_client`'s failure
/// branch (removing a half-initialized client after a failed reinit), so the
/// candidate list can't drift between the two.
fn composio_profile_keys(
    profile_id: &str,
    profile_name: &str,
    owns_legacy_singleton: bool,
) -> Vec<String> {
    let mut keys = vec![
        composio_server_key(profile_id),
        format!("{}:{}", COMPOSIO_NATIVE_PREFIX, profile_name),
    ];
    if owns_legacy_singleton {
        keys.push(COMPOSIO_NATIVE_PREFIX.to_string());
    }
    keys
}

/// Remove the state belonging to a single Composio profile ahead of a
/// (re)initialization, leaving every other profile's state intact.
///
/// Scope is deliberately narrow, because a profile switch in one tab must not
/// disturb another tab that is mid-stream:
/// * `servers` — only this profile's native client keys: the canonical
///   `composio-native:{id}`, the legacy `composio-native:{name}`, and the bare
///   legacy singleton `composio-native` when this profile is the one it stands
///   for (`owns_legacy_singleton`). Other profiles' native clients survive.
/// * `dynamic_composio` — only this profile's `dyn_composio_key` bucket.
/// * `dynamic_local` — never touched. On-demand local tools loaded via
///   `MCP_LOAD_SERVER_TOOLS` are profile-independent, so they are borrowed
///   immutably here: the type makes their survival impossible to regress.
///
/// Returns the servers-map keys that were actually removed.
pub(crate) fn purge_composio_profile_state<V, T, L>(
    servers: &mut HashMap<String, V>,
    dynamic_composio: &mut HashMap<String, Vec<T>>,
    dynamic_local: &[L],
    profile_id: &str,
    profile_name: &str,
    owns_legacy_singleton: bool,
) -> Vec<String> {
    let mut removed_server_keys = Vec::new();
    let candidates = composio_profile_keys(profile_id, profile_name, owns_legacy_singleton);
    for key in candidates {
        if servers.remove(&key).is_some() {
            removed_server_keys.push(key);
        }
    }

    let removed_composio_tools = dynamic_composio
        .remove(&dyn_composio_key(Some(profile_id)))
        .map(|tools| tools.len())
        .unwrap_or(0);

    tracing::info!(
        "Purged Composio state for profile '{}' ({}): removed clients {:?}, {} cached tools; \
         preserved {} on-demand local tools and all other profiles' clients",
        profile_name,
        profile_id,
        removed_server_keys,
        removed_composio_tools,
        dynamic_local.len()
    );

    removed_server_keys
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

        // EAGER CLEANUP (Pattern 151): drop the state of *this* profile only, so
        // that during the initialization window (which may involve network calls)
        // we cannot pick up a stale client for it. Other profiles' native clients
        // and every profile's on-demand local tools are left alone — another tab
        // may be mid-stream on them.
        //
        // LOCK ORDERING (P-010): servers → dynamic_local_tools → dynamic_composio_tools.
        let active_owns_legacy = settings
            .get_active_profile()
            .map(|p| p.id == profile.id)
            .unwrap_or(false);
        let owns_legacy_singleton;
        {
            let mut servers = self.servers.lock().await;
            // The bare `composio-native` singleton predates profile scoping, so
            // nothing in the key says whose it is. Purge it when this profile is
            // the active one (the profile it stands for), or when it is orphaned
            // — its client names no configured profile, so no reinit will ever
            // claim it, and it would otherwise linger as a stale last-resort
            // fallback forever.
            let singleton_orphaned = servers
                .get(COMPOSIO_NATIVE_PREFIX)
                .is_some_and(|c: &ActiveMcpClient| {
                    legacy_singleton_orphaned(
                        c.profile_id.as_deref(),
                        settings.composio_profiles.iter().map(|p| p.id.as_str()),
                    )
                });
            owns_legacy_singleton = active_owns_legacy || singleton_orphaned;
            let dynamic_local = self.dynamic_local_tools.lock().await;
            let mut dynamic_composio = self.dynamic_composio_tools.lock().await;
            purge_composio_profile_state(
                &mut servers,
                &mut dynamic_composio,
                &dynamic_local,
                &profile.id,
                &profile.name,
                owns_legacy_singleton,
            );
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
                    // Remove only this profile's native client keys — a failure
                    // here says nothing about other profiles, whose clients may
                    // be serving other tabs.
                    let stale =
                        composio_profile_keys(&profile.id, &profile.name, owns_legacy_singleton);
                    for key in stale {
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
    pub(crate) fn get_sane_path() -> String {
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
    pub(crate) fn get_critical_env_vars() -> HashMap<String, String> {
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

        // Initialize the Native Terminal (opt-in; a shell is the highest-risk
        // tool in the app). NOT a builtin virtual server: its tools ride the
        // normal permission flow and the load/unload machinery.
        if self.settings.peek().terminal_enabled {
            let tx_clone = tx.clone();
            spawn(async move {
                tracing::info!("Initializing native terminal server");
                let terminal_client =
                    Arc::new(crate::mcp::terminal_client::TerminalClient::new());
                let tools = terminal_client.list_tools();
                let config = McpServerConfig::native_stub(
                    HOBBES_TERMINAL_SERVER.to_string(),
                    "Native terminal: run shell commands in a persistent per-chat zsh \
                     session (always behind tool approval)."
                        .to_string(),
                );
                let active_client = ActiveMcpClient {
                    config,
                    service: McpClientType::NativeTerminal(terminal_client),
                    tools,
                    warning_message: None,
                    profile_id: None,
                };
                if tx_clone.send(active_client).is_err() {
                    tracing::error!("Failed to send initialized terminal client");
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

        // Initialize Native Planner Client (HOBBES_TODO_*, HOBBES_PLAN_DAY, …)
        {
            let tx_clone = tx.clone();
            spawn(async move {
                tracing::info!("Initializing Hobbes Planner built-in tools server");
                let planner_client = crate::mcp::planner_client::PlannerClient::new();
                let tools = planner_client.list_tools();
                let config = McpServerConfig::native_stub(
                    HOBBES_PLANNER_SERVER.to_string(),
                    "Built-in planner: to-dos, daily planning, time blocks.".to_string(),
                );
                let active_client = ActiveMcpClient {
                    config,
                    service: McpClientType::NativePlanner,
                    tools,
                    warning_message: None,
                    profile_id: None,
                };
                if tx_clone.send(active_client).is_err() {
                    tracing::error!("Failed to send initialized Hobbes Planner client");
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
        let lookup = resolve_composio_key(&servers, active_profile_id.as_deref(), |c| {
            c.profile_id.as_deref()
        });
        if let ComposioKeyLookup::Found(ref key) = lookup {
            if let Some(client) = servers.get(key) {
                if let McpClientType::NativeComposio(composio_client) = &client.service {
                    // Use list_connected_toolkits which:
                    // 1. Calls list_connected_accounts() to find connected toolkit slugs
                    // 2. Calls list_tools() (MCP) to count tools per toolkit
                    // This shows ALL connected toolkits, not just force-loaded ones
                    return composio_client.list_connected_toolkits().await;
                }
            }
        }
        Err(lookup.describe(active_profile_id.as_deref()))
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
        let lookup = resolve_composio_key(&servers, active_profile_id.as_deref(), |c| {
            c.profile_id.as_deref()
        });
        let found = match &lookup {
            ComposioKeyLookup::Found(key) => servers.get(key).and_then(|v| match &v.service {
                McpClientType::NativeComposio(c) => Some(c.clone()),
                _ => None,
            }),
            _ => None,
        };
        found.ok_or_else(|| lookup.describe(active_profile_id.as_deref()))
    }

    /// Resolve the native Composio client for a **specific profile**, cloning
    /// its Arc so the servers lock is released before any network call.
    ///
    /// Strict, unlike `find_composio_client`: never falls back to the bare
    /// prefix or an arbitrary Composio client — callers like the calendar
    /// sync pin a subscription to a profile, and silently reading a different
    /// profile's Google account would be worse than an honest error.
    pub async fn composio_client_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Arc<crate::mcp::composio_client::ComposioClient>, String> {
        let servers = self.servers.lock().await;
        let scoped = composio_server_key(profile_id);
        servers
            .get(&scoped)
            .or_else(|| {
                servers
                    .iter()
                    .find(|(k, c)| {
                        is_composio_native(k) && c.profile_id.as_deref() == Some(profile_id)
                    })
                    .map(|(_, v)| v)
            })
            .and_then(|v| match &v.service {
                McpClientType::NativeComposio(c) => Some(c.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "No connected Composio client for profile '{}'",
                    profile_id
                )
            })
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
    /// Drop the active profile's on-demand dynamic tools for a toolkit
    /// (matched by `TOOLKIT_` name prefix). Used whenever a toolkit's
    /// whitelist changes or the toolkit is removed — already-discovered
    /// tools were resolved against stale state.
    /// P-010: takes only the `dynamic_composio_tools` lock.
    async fn clear_dynamic_tools_for_toolkit(&self, toolkit_slug: &str) {
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
                    "Cleared {} stale dynamic tools for toolkit '{}' (profile '{}')",
                    removed,
                    toolkit_slug,
                    key
                );
            }
        }
    }

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
        self.clear_dynamic_tools_for_toolkit(toolkit_slug).await;

        // Reload the loaded/force-loaded tool set from the server.
        self.reload_composio_tools(settings).await?;
        self.invalidate_status_cache();
        Ok(())
    }

    /// Fully remove a toolkit from the active profile's Composio server: unbind
    /// it (PATCH), delete its connected account(s) and auth config(s), drop its
    /// on-demand tools, then reload. Settings (`toolkit_configs`) and
    /// `connected_slugs` are updated UI-side, mirroring how connect splits work.
    ///
    /// Step 1 (the PATCH) is the fail point: on an already-wedged server it will
    /// error, which is expected — recovery is `recreate_composio_server`.
    pub async fn remove_toolkit(
        &self,
        toolkit_slug: &str,
        settings: &Settings,
    ) -> Result<(), String> {
        let composio_client = self.find_composio_client().await?;

        // 1. Unbind from the MCP server (PATCH + generate); returns the auth
        //    config id(s) that were dropped from the server's array.
        let dropped_auth_ids = composio_client
            .remove_toolkit_from_server(toolkit_slug)
            .await?;

        // 2. Delete the toolkit's connected account(s) (best-effort).
        if let Err(e) = composio_client
            .delete_toolkit_connections(toolkit_slug)
            .await
        {
            tracing::warn!(
                "[REMOVE] delete_toolkit_connections('{}') failed: {}",
                toolkit_slug,
                e
            );
        }

        // 3. Delete the dropped auth config(s) so they can't accumulate
        //    (best-effort — only runs after the PATCH already removed the
        //    references, so a failure here never leaves a dangling reference).
        for id in &dropped_auth_ids {
            if let Err(e) = composio_client.delete_auth_config(id).await {
                tracing::warn!("[REMOVE] delete_auth_config('{}') failed: {}", id, e);
            }
        }

        // 4. Bust caches + drop this profile's on-demand tools for the toolkit.
        composio_client.clear_cached_toolkit_info();
        self.clear_dynamic_tools_for_toolkit(toolkit_slug).await;

        // 5. Reload the active client's tool set and refresh status.
        self.reload_composio_tools(settings).await?;
        self.invalidate_status_cache();
        tracing::info!("[REMOVE] Toolkit '{}' fully removed", toolkit_slug);
        Ok(())
    }

    /// Recovery for an already-wedged Composio server: its poisoned
    /// `auth_config_ids` reject every PATCH, so removal can't help. Provisions a
    /// FRESH empty server, repoints the active profile at it, and best-effort
    /// re-binds the toolkits the user already had (existing accounts/configs
    /// survive, so most rebind without new OAuth). Per-toolkit tool curation is
    /// carried over — locally persisted `enabled_tools` wins, falling back to
    /// the old server's `allowed_tools` entries — and re-applied to the new
    /// server. On success the old server is soft-deleted (`delete_mcp_server`)
    /// so abandoned servers don't accumulate. Returns
    /// `(rebound, needs_reconnect)` slug lists for a UI summary.
    pub async fn recreate_composio_server(
        &self,
        mut settings_signal: Signal<Settings>,
        settings_manager: SettingsManager,
        mut mcp_context_signal: Signal<McpContext>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let settings_snapshot = settings_signal.peek().clone();
        let Some(profile) = settings_snapshot.get_active_profile() else {
            return Err("No active Composio profile found".to_string());
        };
        let Some(api_key) = profile.api_key.clone() else {
            return Err("No Composio API key configured".to_string());
        };
        let profile_id = profile.id.clone();
        let base_url = profile
            .base_url
            .clone()
            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
        let user_id = profile
            .user_id
            .clone()
            .or(profile.entity_id.clone())
            .unwrap_or_else(|| "default".to_string());
        let entity_id = profile.entity_id.clone();
        let chrome_dir = profile.chrome_profile_directory.clone();
        let kept: Vec<(String, bool, Option<Vec<String>>)> = profile
            .toolkit_configs
            .iter()
            .map(|c| (c.slug.clone(), c.no_auth, c.enabled_tools.clone()))
            .collect();

        // The old server's id, so it can be retired after a successful migration.
        let old_server_id: Option<String> = base_url
            .split("/mcp/")
            .nth(1)
            .map(|s| s.split('?').next().unwrap_or(s))
            .map(|s| s.trim_end_matches("/mcp").to_string())
            .filter(|s| !s.is_empty());

        // Owned, mutable client (mirrors connect_toolkit) so we can repoint its
        // base_url and clone it into the servers map.
        let mut client = ComposioClient::new(
            api_key,
            base_url,
            entity_id,
            Some(user_id),
            profile_id.clone(),
            chrome_dir,
        );

        // 0. Snapshot the old server's allowed_tools while the client still
        //    points at it (GET works even on a wedged server — only PATCH is
        //    rejected). Fallback source for curation carry-over.
        let old_allowed_tools: Vec<String> = match client.server_allowed_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                tracing::warn!(
                    "[RECREATE] Could not read old server's allowed_tools ({}); \
                     relying on locally persisted curation only",
                    e
                );
                Vec::new()
            }
        };

        // 1. Resolve auth configs for all kept toolkits FIRST. This only hits
        //    /auth_configs, so it works even while the old server is wedged.
        //    Reconcile curation per toolkit: local enabled_tools wins, else the
        //    old server's whitelist entries for the toolkit's prefix.
        //    A toolkit whose auth config has no ACTIVE connected account binds
        //    fine but fails every call with AUTH — flag it honestly.
        let active_slugs = client.active_account_slugs().await;
        let mut resolved: Vec<(String, Option<String>, Option<Vec<String>>)> = Vec::new();
        let mut needs_reconnect: Vec<String> = Vec::new();
        let mut auth_missing: Vec<String> = Vec::new();
        for (slug, no_auth, local_curation) in kept {
            let curation = local_curation.or_else(|| {
                let prefix = format!("{}_", slug.to_uppercase().replace('-', "_"));
                let from_server: Vec<String> = old_allowed_tools
                    .iter()
                    .filter(|t| t.to_uppercase().starts_with(&prefix))
                    .cloned()
                    .collect();
                if from_server.is_empty() {
                    None
                } else {
                    Some(from_server)
                }
            });
            if no_auth {
                resolved.push((slug, None, curation));
                continue;
            }
            match client.get_auth_config_id(&slug).await {
                Ok(id) => {
                    if !active_slugs.contains(&slug.to_lowercase()) {
                        tracing::warn!(
                            "[RECREATE] '{}' has an auth config but no ACTIVE connected \
                             account — binding it, but it needs a reconnect (OAuth)",
                            slug
                        );
                        auth_missing.push(slug.clone());
                    }
                    resolved.push((slug, Some(id), curation));
                }
                Err(e) if crate::mcp::composio_client::auth::is_no_auth_toolkit_error(&e) => {
                    resolved.push((slug, None, curation));
                }
                Err(e) => {
                    tracing::warn!("[RECREATE] '{}' auth resolve failed: {}", slug, e);
                    needs_reconnect.push(slug);
                }
            }
        }

        // Composio rejects a server with no applications (error 1153
        // MCP_MissingAuthConfigsOrToolkits), so the fresh server must be seeded
        // with at least one toolkit.
        let Some((seed_slug, seed_auth, _)) = resolved.first().cloned() else {
            return Err(
                "No toolkits available to provision a fresh server — connect a toolkit first."
                    .to_string(),
            );
        };

        // 2. Provision the fresh server, seeded with the first toolkit.
        let (_new_id, new_url) = client
            .create_fresh_mcp_server(&seed_slug, seed_auth.as_deref())
            .await?;

        // 3. Repoint client + servers map + persisted base_url (mirror connect Step 3).
        client.base_url = new_url.clone();
        {
            let server_key = composio_server_key(&profile_id);
            let mut s = self.servers.lock().await;
            if let Some(existing) = s.get_mut(&server_key) {
                existing.service =
                    McpClientType::NativeComposio(std::sync::Arc::new(client.clone()));
                existing.tools = Vec::new();
            } else {
                let active_client = ActiveMcpClient {
                    config: McpServerConfig::composio_stub(composio_server_key(&profile_id)),
                    service: McpClientType::NativeComposio(std::sync::Arc::new(client.clone())),
                    tools: Vec::new(),
                    warning_message: None,
                    profile_id: Some(profile_id.clone()),
                };
                s.insert(server_key, active_client);
            }
        }
        {
            let mut s = settings_signal.write();
            if let Some(p) = s.composio_profiles.iter_mut().find(|p| p.id == profile_id) {
                p.base_url = Some(new_url.clone());
            }
        }
        let updated = settings_signal.peek().clone();
        {
            let sm = settings_manager.clone();
            let updated_for_save = updated.clone();
            spawn(async move {
                let _ = tokio::task::spawn_blocking(move || sm.save(&updated_for_save)).await;
            });
        }

        // 4. Bind the remaining toolkits (the seed is already bound by creation).
        //    Tools are bound without a whitelist first; curation is re-applied
        //    in one pass afterwards so the all-mode guard only expands once.
        let mut rebound = vec![seed_slug.clone()];
        tracing::info!(
            "[RECREATE] Seeded new server with '{}' (auth={:?}); binding {} more",
            seed_slug,
            seed_auth,
            resolved.len().saturating_sub(1)
        );
        for (slug, auth, _) in resolved.iter().skip(1) {
            match client.add_toolkit_to_server(slug, auth.as_deref(), None).await {
                Ok(_) => {
                    tracing::info!("[RECREATE] Bound '{}' (auth={:?})", slug, auth);
                    rebound.push(slug.clone());
                }
                Err(e) => {
                    tracing::warn!("[RECREATE] Failed to re-bind '{}': {}", slug, e);
                    needs_reconnect.push(slug.clone());
                }
            }
        }

        // Surface bound-but-account-less toolkits in the reconnect list so the
        // UI reports them honestly instead of counting them as restored.
        for slug in auth_missing {
            if !needs_reconnect.contains(&slug) {
                needs_reconnect.push(slug);
            }
        }

        // 4b. Re-apply the carried-over curation to the new server. Best-effort:
        //     a failure here leaves the toolkit serving all tools (never zero).
        let mut recovered_curation: Vec<(String, Vec<String>)> = Vec::new();
        for (slug, _, curation) in &resolved {
            let Some(tools) = curation else { continue };
            if tools.is_empty() || !rebound.contains(slug) {
                continue;
            }
            match client
                .set_toolkit_enabled_tools(slug, tools.clone())
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "[RECREATE] Re-applied curation for '{}' ({} tools)",
                        slug,
                        tools.len()
                    );
                    recovered_curation.push((slug.clone(), tools.clone()));
                }
                Err(e) => tracing::warn!(
                    "[RECREATE] Could not re-apply curation for '{}' ({} tools): {} — \
                     toolkit will serve all tools until re-curated",
                    slug,
                    tools.len(),
                    e
                ),
            }
        }

        // Mirror the applied curation into local settings so it stays the
        // durable source of truth (covers curation recovered from the old
        // server that was never persisted locally).
        if !recovered_curation.is_empty() {
            {
                let mut s = settings_signal.write();
                if let Some(p) = s.composio_profiles.iter_mut().find(|p| p.id == profile_id) {
                    for (slug, tools) in &recovered_curation {
                        if let Some(config) = p
                            .toolkit_configs
                            .iter_mut()
                            .find(|c| c.slug.eq_ignore_ascii_case(slug))
                        {
                            config.enabled_tools = Some(tools.clone());
                        }
                    }
                }
            }
            let updated_curation = settings_signal.peek().clone();
            let sm = settings_manager.clone();
            spawn(async move {
                let _ = tokio::task::spawn_blocking(move || sm.save(&updated_curation)).await;
            });
        }

        // Verify what actually persisted on the new server (config-level state,
        // independent of what the MCP proxy exposes via tools/list).
        match client.server_config_summary().await {
            Ok((sid, toolkits, auth_ids)) => tracing::info!(
                "[RECREATE] New server {} now has toolkits={:?} auth_config_ids={:?}",
                sid,
                toolkits,
                auth_ids
            ),
            Err(e) => tracing::warn!("[RECREATE] Could not read new server config: {}", e),
        }

        // 4c. Retire the old server (soft delete) now that the profile points at
        //     the new one. Best-effort: a leftover server is only clutter, but
        //     deleting it prevents any future write from mis-targeting it.
        if let Some(old_id) = old_server_id {
            if new_url.contains(&old_id) {
                tracing::warn!(
                    "[RECREATE] New URL unexpectedly matches old server id {}; skipping delete",
                    old_id
                );
            } else {
                match client.delete_mcp_server(&old_id).await {
                    Ok(()) => tracing::info!("[RECREATE] Retired old server {}", old_id),
                    Err(e) => tracing::warn!(
                        "[RECREATE] Could not delete old server {}: {}",
                        old_id,
                        e
                    ),
                }
            }
        }

        // 5. Reload tools + refresh context/status.
        let _ = self.reload_composio_tools(&updated).await;
        mcp_context_signal.set(self.get_mcp_context(Some(profile_id)).await);
        self.invalidate_status_cache();

        tracing::info!(
            "[RECREATE] Done: {} bound to server, {} could not resolve auth. NOTE: auth toolkits \
             may still need a reconnect (OAuth) before their tools appear.",
            rebound.len(),
            needs_reconnect.len()
        );
        Ok((rebound, needs_reconnect))
    }

    /// Unload a server's tools from the AI context (runtime only)
    pub async fn unload_server(&self, server_name: &str) {
        if is_builtin_virtual_server(server_name) {
            tracing::warn!("Refusing to unload built-in server '{}'", server_name);
            return;
        }
        let mut unloaded = self.unloaded_servers.lock().await;
        unloaded.insert(server_name.to_string());
        drop(unloaded); // Release lock before sync try_lock in invalidate
                        // Pattern 150.8.1: Authoritative invalidation prevents UI state gaps
        self.invalidate_status_cache();
        // Unloading the terminal kills every shell — hiding the tools while
        // leaving live ptys running would be a lie.
        self.reap_terminal_if(server_name).await;
        tracing::info!(
            "Unloaded server '{}' - tools hidden from AI, cache invalidated",
            server_name
        );
    }

    /// Kill all pty shells when `server_name` is the terminal (no-op
    /// otherwise). Takes the `servers` lock alone (P-010).
    async fn reap_terminal_if(&self, server_name: &str) {
        if server_name != HOBBES_TERMINAL_SERVER {
            return;
        }
        let client = {
            let servers = self.servers.lock().await;
            servers.get(HOBBES_TERMINAL_SERVER).and_then(|c| {
                if let McpClientType::NativeTerminal(t) = &c.service {
                    Some(t.clone())
                } else {
                    None
                }
            })
        };
        if let Some(t) = client {
            t.shutdown_all().await;
        }
    }

    /// Kill every terminal shell (settings toggle off).
    pub async fn shutdown_terminal(&self) {
        self.reap_terminal_if(HOBBES_TERMINAL_SERVER).await;
    }

    /// Kill one chat session's terminal shell (tab close).
    pub async fn kill_terminal_session(&self, session_id: &str) {
        let client = {
            let servers = self.servers.lock().await;
            servers.get(HOBBES_TERMINAL_SERVER).and_then(|c| {
                if let McpClientType::NativeTerminal(t) = &c.service {
                    Some(t.clone())
                } else {
                    None
                }
            })
        };
        if let Some(t) = client {
            t.kill_session(session_id).await;
        }
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
        let lookup = resolve_composio_key(&servers, active_profile_id.as_deref(), |c| {
            c.profile_id.as_deref()
        });
        let composio_key = match lookup {
            ComposioKeyLookup::Found(ref key) => Some(key.clone()),
            ComposioKeyLookup::NotConnected | ComposioKeyLookup::Ambiguous { .. } => None,
        };

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
            Err(lookup.describe(active_profile_id.as_deref()))
        }
    }

    /// Undo the auth config / connected account created by a failed connect
    /// attempt — but only when the toolkit had **no** pre-existing auth config,
    /// so we never delete state an already-connected toolkit depends on.
    /// Best-effort; deletes the account first (it references the config).
    async fn rollback_partial_connect(
        client: &crate::mcp::composio_client::ComposioClient,
        toolkit_slug: &str,
        auth_config_id: Option<&str>,
        had_auth_config_before: bool,
    ) {
        if had_auth_config_before {
            return;
        }
        let Some(acid) = auth_config_id else {
            return;
        };
        tracing::warn!(
            "[ROLLBACK] '{}' connect failed; removing the auth config/account created this attempt",
            toolkit_slug
        );
        let _ = client.delete_toolkit_connections(toolkit_slug).await;
        let _ = client.delete_auth_config(acid).await;
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
            tracing::info!(
                "[no-auth] '{}': UI hint no_auth=true — treating as no-auth",
                toolkit_slug
            );
            true
        } else {
            match client.get_toolkit_metadata(&toolkit_slug).await {
                Ok(meta) => {
                    let decided = meta.requires_no_auth();
                    tracing::info!(
                        "[no-auth] '{}': metadata no_auth={:?} auth_schemes={:?} managed_schemes={:?} -> requires_no_auth={}",
                        toolkit_slug,
                        meta.no_auth,
                        meta.auth_schemes,
                        meta.composio_managed_auth_schemes,
                        decided
                    );
                    decided
                }
                Err(e) => {
                    tracing::warn!(
                        "[no-auth] '{}': metadata lookup FAILED ({}); assuming auth required",
                        toolkit_slug,
                        e
                    );
                    false
                }
            }
        };

        // For rollback scoping: did an auth config already exist before this
        // attempt? If not and we fail mid-connect, we delete the config/account
        // this attempt created so a failed connect leaves no orphan behind.
        let had_auth_config_before = if requires_no_auth {
            true // nothing to roll back for no-auth toolkits
        } else {
            client.has_existing_auth_config(&toolkit_slug).await
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
                    Self::rollback_partial_connect(
                        &client,
                        &toolkit_slug,
                        auth_config_id.as_deref(),
                        had_auth_config_before,
                    )
                    .await;
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
                Self::rollback_partial_connect(
                    &client,
                    &toolkit_slug,
                    auth_config_id.as_deref(),
                    had_auth_config_before,
                )
                .await;
                connection_error.set(Some(msg.clone()));
                is_connecting.set(false);
                return Err(msg);
            }
        }

        // Step 4: Smart Selection & Binding
        // SECURITY: Skip re-selection if admin has pre-configured allowed_tools for this toolkit.
        // This prevents Hobbes from overwriting admin curation (e.g., disabled SEND actions).
        // Locally saved curation (Edit Tools) is checked next — it survives server wipes and
        // delete/re-add cycles, unlike the on-server state the admin guard reads.
        let saved_curation: Option<Vec<String>> = settings_signal
            .peek()
            .get_active_profile()
            .and_then(|p| {
                p.toolkit_configs
                    .iter()
                    .find(|c| c.slug.eq_ignore_ascii_case(&toolkit_slug))
                    .and_then(|c| c.enabled_tools.clone())
                    // Curation retained across a delete → re-add cycle.
                    .or_else(|| p.retained_curation.get(&toolkit_slug.to_lowercase()).cloned())
            })
            .filter(|t| !t.is_empty());
        // LLM selection applied this connect, to persist as local curation in Step 5.
        let mut applied_selection: Option<Vec<String>> = None;
        if admin_tools_detected {
            tracing::info!(
                "[Step 4/5] Skipping tool selection — admin-configured tools are authoritative for '{}'",
                toolkit_slug
            );
            connection_status.set("Admin tools preserved".to_string());
        } else if let Some(saved) = saved_curation.clone() {
            tracing::info!(
                "[Step 4/5] Re-applying saved curation for '{}' ({} tools) — skipping LLM selection",
                toolkit_slug,
                saved.len()
            );
            connection_status.set("Restoring your tool selection...".to_string());
            match client
                .add_toolkit_to_server(&toolkit_slug, auth_config_id.as_deref(), Some(saved.clone()))
                .await
            {
                Ok(_) => applied_selection = Some(saved),
                Err(e) => tracing::warn!(
                    "Failed to re-apply saved curation for '{}': {} — toolkit will serve all tools",
                    toolkit_slug,
                    e
                ),
            }
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
                        // Small toolkit: leave the backend default (all tools).
                        // Writing an explicit whitelist here used to flip the
                        // shared server into whitelist mode for no benefit.
                        tracing::info!(
                            "Toolkit '{}' has {} tools (under threshold), keeping all enabled.",
                            toolkit_slug,
                            tools.len()
                        );
                        None
                    }
                    _ => None,
                };

            // If selected_tools is present, re-patch to apply filter
            if let Some(tools) = selected_tools.clone() {
                tracing::info!("Applying smart selection of {} tools", tools.len());
                match client
                    .add_toolkit_to_server(&toolkit_slug, auth_config_id.as_deref(), Some(tools.clone()))
                    .await
                {
                    Ok(_) => applied_selection = Some(tools),
                    Err(e) => tracing::warn!("Failed to apply smart tool selection: {}", e),
                }
            }
        } // end of !admin_tools_detected branch

        // Step 5: Final Reload
        tracing::info!("[Step 5/5] Finalizing Configuration...");
        {
            // Authoritative no-auth signal: auth resolution ended without an
            // auth config (covers both the caller's hint and the 303 self-heal).
            let effective_no_auth = auth_config_id.is_none();
            tracing::info!(
                "[no-auth] '{}': persisting no_auth={} (auth_config_id={:?})",
                toolkit_slug,
                effective_no_auth,
                auth_config_id
            );
            let mut s = settings_signal.write();
            if let Some(profile) = s.get_active_profile_mut() {
                // The curation (if any) is applied and about to be recorded on
                // the live config entry; drop the delete-retained copy.
                profile
                    .retained_curation
                    .remove(&toolkit_slug.to_lowercase());
                if let Some(existing) = profile
                    .toolkit_configs
                    .iter_mut()
                    .find(|c| c.slug == toolkit_slug)
                {
                    // Keep the no-auth flag current (a no-auth toolkit has no
                    // connected account, so this flag is what marks it connected).
                    existing.no_auth = effective_no_auth;
                    // Persist the LLM's selection as local curation so the user
                    // can review/edit it and it survives server recreation.
                    if let Some(sel) = applied_selection.clone() {
                        existing.enabled_tools = Some(sel);
                    }
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
                            enabled_tools: applied_selection.clone(),
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

}

#[cfg(test)]
mod purge_tests {
    use super::*;

    /// Stand-in for `ActiveMcpClient` / `Tool`: the purge helper is generic over
    /// the map's value type, so tests need no real MCP client or tool.
    #[derive(Debug, PartialEq, Clone)]
    struct Stub(&'static str);

    fn servers_with(keys: &[&str]) -> HashMap<String, Stub> {
        keys.iter().map(|k| (k.to_string(), Stub("client"))).collect()
    }

    #[test]
    fn reinit_of_one_profile_leaves_other_profiles_native_clients() {
        // Profile A is streaming in another tab; profile B is being reinitialized.
        let mut servers = servers_with(&[
            "composio-native:profile-a",
            "composio-native:profile-b",
            "composio-native:Profile B Name",
            "filesystem",
        ]);
        let mut dynamic_composio: HashMap<String, Vec<Stub>> = HashMap::new();
        dynamic_composio.insert("profile-a".to_string(), vec![Stub("a-tool")]);
        dynamic_composio.insert("profile-b".to_string(), vec![Stub("b-tool")]);
        let local: Vec<Stub> = vec![];

        let removed = purge_composio_profile_state(
            &mut servers,
            &mut dynamic_composio,
            &local,
            "profile-b",
            "Profile B Name",
            false,
        );

        assert!(removed.contains(&"composio-native:profile-b".to_string()));
        assert!(removed.contains(&"composio-native:Profile B Name".to_string()));
        // Profile A's native client — and unrelated servers — must survive.
        assert!(servers.contains_key("composio-native:profile-a"));
        assert!(servers.contains_key("filesystem"));
        assert!(!servers.contains_key("composio-native:profile-b"));
        assert!(!servers.contains_key("composio-native:Profile B Name"));
        // Only profile B's dynamic Composio bucket is dropped.
        assert!(dynamic_composio.contains_key("profile-a"));
        assert!(!dynamic_composio.contains_key("profile-b"));
    }

    #[test]
    fn legacy_singleton_removed_only_when_profile_owns_it() {
        let mut servers = servers_with(&[COMPOSIO_NATIVE_PREFIX, "composio-native:profile-a"]);
        let mut dynamic_composio: HashMap<String, Vec<Stub>> = HashMap::new();
        let local: Vec<Stub> = vec![];

        // Not the active profile: bare legacy key stays put.
        purge_composio_profile_state(
            &mut servers,
            &mut dynamic_composio,
            &local,
            "profile-b",
            "B",
            false,
        );
        assert!(servers.contains_key(COMPOSIO_NATIVE_PREFIX));

        // Owns the legacy singleton: it is replaced along with the scoped key.
        purge_composio_profile_state(
            &mut servers,
            &mut dynamic_composio,
            &local,
            "profile-a",
            "A",
            true,
        );
        assert!(!servers.contains_key(COMPOSIO_NATIVE_PREFIX));
        assert!(!servers.contains_key("composio-native:profile-a"));
    }

    #[test]
    fn orphaned_legacy_singleton_is_recognised() {
        let configured = ["profile-a".to_string(), "profile-b".to_string()];
        let ids = || configured.iter().map(|s| s.as_str());

        // Claims a configured profile — still owned, leave it alone.
        assert!(!legacy_singleton_orphaned(Some("profile-a"), ids()));
        // Claims a profile that has been removed from settings.
        assert!(legacy_singleton_orphaned(Some("profile-gone"), ids()));
        // Pre-profile client: claims nothing, so nothing will ever reclaim it.
        assert!(legacy_singleton_orphaned(None, ids()));
    }

    #[test]
    fn purge_never_clears_on_demand_local_tools() {
        // Local on-demand tools (MCP_LOAD_SERVER_TOOLS) are profile-independent:
        // a Composio profile switch in one tab must not destroy them.
        let mut servers = servers_with(&["composio-native:profile-b"]);
        let mut dynamic_composio: HashMap<String, Vec<Stub>> = HashMap::new();
        let local = vec![Stub("read_file"), Stub("write_file")];

        purge_composio_profile_state(
            &mut servers,
            &mut dynamic_composio,
            &local,
            "profile-b",
            "B",
            true,
        );

        assert_eq!(local, vec![Stub("read_file"), Stub("write_file")]);
    }

    /// Stand-in for `ActiveMcpClient` carrying a profile id, for exercising
    /// `resolve_composio_key` without a real MCP client.
    #[derive(Debug, PartialEq, Clone)]
    struct ProfiledStub(Option<&'static str>);

    fn profiled_servers(entries: &[(&'static str, Option<&'static str>)]) -> HashMap<String, ProfiledStub> {
        entries
            .iter()
            .map(|(k, pid)| (k.to_string(), ProfiledStub(*pid)))
            .collect()
    }

    #[test]
    fn resolve_composio_key_ambiguous_when_several_and_no_profile_matches() {
        let servers = profiled_servers(&[
            ("composio-native:profile-a", Some("profile-a")),
            ("composio-native:profile-b", Some("profile-b")),
        ]);
        match resolve_composio_key(&servers, Some("profile-c"), |c| c.0) {
            ComposioKeyLookup::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates.contains(&"composio-native:profile-a".to_string()));
                assert!(candidates.contains(&"composio-native:profile-b".to_string()));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }

        // No profile requested at all, with several clients present: still
        // ambiguous, never an arbitrary guess.
        match resolve_composio_key(&servers, None, |c| c.0) {
            ComposioKeyLookup::Ambiguous { .. } => {}
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn resolve_composio_key_found_when_profile_matches() {
        let servers = profiled_servers(&[
            ("composio-native:profile-a", Some("profile-a")),
            ("composio-native:profile-b", Some("profile-b")),
        ]);
        assert_eq!(
            resolve_composio_key(&servers, Some("profile-b"), |c| c.0),
            ComposioKeyLookup::Found("composio-native:profile-b".to_string())
        );
    }

    #[test]
    fn resolve_composio_key_found_for_single_client_last_resort() {
        let servers = profiled_servers(&[("composio-native:profile-a", Some("profile-a"))]);
        // No profile requested, but only one Composio client exists — safe to
        // use it as the unambiguous last resort.
        assert_eq!(
            resolve_composio_key(&servers, None, |c| c.0),
            ComposioKeyLookup::Found("composio-native:profile-a".to_string())
        );
    }

    #[test]
    fn resolve_composio_key_not_connected_when_none_exist() {
        let servers: HashMap<String, ProfiledStub> = profiled_servers(&[("filesystem", None)]);
        assert_eq!(
            resolve_composio_key(&servers, Some("profile-a"), |c| c.0),
            ComposioKeyLookup::NotConnected
        );
    }

    #[test]
    fn composio_profile_keys_includes_legacy_singleton_only_when_owned() {
        let not_owned = composio_profile_keys("profile-a", "Profile A", false);
        assert!(!not_owned.contains(&COMPOSIO_NATIVE_PREFIX.to_string()));
        assert!(not_owned.contains(&"composio-native:profile-a".to_string()));
        assert!(not_owned.contains(&"composio-native:Profile A".to_string()));

        let owned = composio_profile_keys("profile-a", "Profile A", true);
        assert!(owned.contains(&COMPOSIO_NATIVE_PREFIX.to_string()));
        assert!(owned.contains(&"composio-native:profile-a".to_string()));
        assert!(owned.contains(&"composio-native:Profile A".to_string()));
    }
}
