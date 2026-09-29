use crate::context::permissions::{PermissionManager};
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use crate::settings::{Settings, SettingsManager};
use crate::SecretManagerTrait;
use dioxus::prelude::spawn;
use dioxus::prelude::Signal;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{Tool};
use rmcp::service::{RoleClient, RunningService};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod composio_toolkits;
mod config;
mod lifecycle;
mod oauth;
mod status;
mod tool_call;

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
