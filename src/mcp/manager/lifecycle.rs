use super::composio::composio_server_key;
use super::tool_call::get_local_meta_tools;
use super::{
    is_builtin_virtual_server, is_composio_native, ActiveMcpClient, AuthRequiredInfo,
    McpClientType, McpContext, McpManager, McpServerConfig, COMPOSIO_NATIVE_PREFIX,
    HOBBES_CORE_SERVER, HOBBES_META_SERVER, HOBBES_PLANNER_SERVER, HOBBES_TERMINAL_SERVER,
};
use crate::components::smithery_registry::SmitheryServerDetail;
use crate::mcp::client_transport::{self, McpService, MCP_INIT_TIMEOUT};
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use dioxus::prelude::spawn;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{PaginatedRequestParams, Tool};
use rmcp::service::ServiceExt;
use rmcp::transport::child_process::TokioChildProcess;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc::{self};

/// Fetch every page of a server's tool list.
///
/// Stops when `nextCursor` is missing OR empty: some servers send
/// `"nextCursor": ""` on the last page, and treating that as "more pages"
/// would re-request the first page forever (which is why rmcp's
/// `list_all_tools()` is not used here — it only stops on a missing cursor).
async fn fetch_all_tools(service: &McpService) -> Result<Vec<Tool>, rmcp::ServiceError> {
    let mut all_tools = Vec::new();
    let mut params: Option<PaginatedRequestParams> = None;
    loop {
        let result = service.list_tools(params.take()).await?;
        all_tools.extend(result.tools);
        match result.next_cursor {
            Some(cursor) if !cursor.is_empty() => {
                params = Some(PaginatedRequestParams::default().with_cursor(Some(cursor)));
            }
            _ => break,
        }
    }
    Ok(all_tools)
}

impl McpManager {
    /// Remove dynamically loaded tools belonging to `server_name` from the
    /// `dynamic_local_tools` cache and `dynamic_local_tool_sources` reverse-lookup.
    ///
    /// LOCK ORDERING INVARIANT (P-010): Always acquire `servers` before
    /// `dynamic_local_tools` to match `call_tool`'s acquisition order
    /// and prevent ABBA deadlocks.
    pub(super) async fn clear_dynamic_tools_for(&self, server_name: &str) {
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
    pub(super) fn is_needs_setup_error(error_msg: &str) -> bool {
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
                let new_context = self_clone_for_receiver
                    .get_active_profile_mcp_context()
                    .await;
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
                let terminal_client = Arc::new(crate::mcp::terminal_client::TerminalClient::new());
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
                            let has_server_uuid = profile
                                .base_url
                                .as_ref()
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
                            match tool_result {
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
                    // Network-based server (Streamable HTTP)
                    tracing::info!(
                        "Connecting to network MCP server '{}' at {}",
                        server_name,
                        crate::log_utils::redact_url(&uri)
                    );
                    // Credentials are host-scoped: the COMPOSIO_API_KEY env value is
                    // only ever sent (as x-api-key) to *.composio.dev hosts.
                    let endpoint = match client_transport::plan_remote_endpoint(
                        &uri,
                        None,
                        server_config_clone
                            .env
                            .get("COMPOSIO_API_KEY")
                            .map(String::as_str),
                    ) {
                        Ok(endpoint) => endpoint,
                        Err(error_msg) => {
                            tracing::error!(
                                "Invalid URI for MCP server '{}': {}",
                                server_name,
                                error_msg
                            );
                            failed_servers_clone
                                .lock()
                                .await
                                .insert(server_name, (server_config_clone, error_msg));
                            return;
                        }
                    };
                    match client_transport::connect_remote(&endpoint, MCP_INIT_TIMEOUT).await {
                        Ok(service) => Ok(service),
                        Err(err) => {
                            tracing::error!(
                                "Failed to connect to MCP server '{}': {}",
                                server_name,
                                err.message
                            );
                            if err.auth_required || Self::is_auth_error(&err.message) {
                                tracing::info!("Server '{}' requires authentication", server_name);
                                auth_required_servers_clone.lock().await.insert(
                                    server_name.clone(),
                                    AuthRequiredInfo {
                                        config: server_config_clone,
                                        auth_url: None,
                                        error_message: err.message,
                                        profile: None,
                                    },
                                );
                            } else {
                                failed_servers_clone
                                    .lock()
                                    .await
                                    .insert(server_name, (server_config_clone, err.message));
                            }
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
                                MCP_INIT_TIMEOUT,
                                client_transport::hobbes_client_config().serve(transport),
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
                        let all_tools = match fetch_all_tools(&service).await {
                            Ok(tools) => tools,
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
                        };

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
                        let is_auth =
                            e.is_authorization_required() || Self::is_auth_error(&error_msg);
                        if is_auth {
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
                        let new_context = self_clone.get_active_profile_mcp_context().await;
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
                            let new_context = self_clone.get_active_profile_mcp_context().await;
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
                        let has_server_uuid = profile
                            .base_url
                            .as_ref()
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
                            tracing::warn!(
                                "No MCP server UUID in base_url (retry) — skipping retry"
                            );
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
                                let new_context = self_clone.get_active_profile_mcp_context().await;
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
                // Network-based server (Streamable HTTP)
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
                    crate::log_utils::redact_url(&uri)
                );
                // Credentials are host-scoped: an explicit access token goes as a
                // Bearer token; the COMPOSIO_API_KEY env value is only ever sent
                // (as x-api-key) to *.composio.dev hosts.
                let endpoint = match client_transport::plan_remote_endpoint(
                    &uri,
                    access_token_clone.as_deref(),
                    effective_config
                        .env
                        .get("COMPOSIO_API_KEY")
                        .map(String::as_str),
                ) {
                    Ok(endpoint) => endpoint,
                    Err(error_msg) => {
                        tracing::error!(
                            "Invalid URI for MCP server '{}': {}",
                            server_name,
                            error_msg
                        );
                        failed_servers_clone
                            .lock()
                            .await
                            .insert(server_name, (server_config_clone, error_msg));
                        self_clone.invalidate_status_cache_async().await;
                        return;
                    }
                };
                match client_transport::connect_remote(&endpoint, MCP_INIT_TIMEOUT).await {
                    Ok(service) => Ok(service),
                    Err(err) => {
                        tracing::error!(
                            "Failed to connect to MCP server '{}': {}",
                            server_name,
                            err.message
                        );
                        if err.auth_required || Self::is_auth_error(&err.message) {
                            tracing::info!("Server '{}' requires authentication", server_name);
                            auth_required_servers_clone.lock().await.insert(
                                server_name.clone(),
                                AuthRequiredInfo {
                                    config: server_config_clone,
                                    auth_url: None,
                                    error_message: err.message,
                                    profile: None,
                                },
                            );
                        } else {
                            failed_servers_clone
                                .lock()
                                .await
                                .insert(server_name, (server_config_clone, err.message));
                        }
                        self_clone.invalidate_status_cache_async().await;
                        return;
                    }
                }
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
                    Ok(transport) => {
                        client_transport::hobbes_client_config()
                            .serve(transport)
                            .await
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
                    let all_tools = match fetch_all_tools(&service).await {
                        Ok(tools) => tools,
                        Err(e) => {
                            let error_msg = format!("Failed to list tools: {}", e);
                            tracing::error!("Failed to list tools for '{}': {}", server_name, e);
                            // Check if this is an auth error
                            if Self::is_auth_error(&error_msg) {
                                tracing::info!("Server '{}' requires authentication", server_name);
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
                                failed_servers_clone
                                    .lock()
                                    .await
                                    .insert(server_name.clone(), (server_config_clone, error_msg));
                            }
                            self_clone.invalidate_status_cache_async().await;
                            return;
                        }
                    };

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
                    let new_context = self_clone.get_active_profile_mcp_context().await;
                    mcp_context_signal_clone.set(new_context);
                    tracing::info!("Successfully reconnected '{}'", server_name);
                }
                Err(e) => {
                    let error_msg = format!("Failed to serve: {}", e);
                    tracing::error!("Failed to serve MCP server '{}': {}", server_name, e);
                    let is_auth = e.is_authorization_required() || Self::is_auth_error(&error_msg);
                    if is_auth {
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
    pub(super) async fn reconnect_stdio_server(
        &self,
        server_name: &str,
        config: McpServerConfig,
    ) -> Result<(), String> {
        tracing::info!("[RECONNECT] Restarting stdio MCP server '{}'", server_name);

        // Remote servers have no child process to restart; refuse before
        // touching the servers map so a live remote entry is never dropped.
        if config.uri.is_some() {
            return Err(format!(
                "Cannot reconnect '{}': remote (HTTP) servers cannot be reconnected this way",
                server_name
            ));
        }

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
                    "Cannot reconnect '{}': no command configured (remote (HTTP) servers cannot be reconnected this way)",
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
        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| format!("Failed to launch stdio MCP server '{}': {}", server_name, e))?;

        let service = tokio::time::timeout(
            MCP_INIT_TIMEOUT,
            client_transport::hobbes_client_config().serve(transport),
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
        let all_tools = fetch_all_tools(&service).await.map_err(|e| {
            format!(
                "Failed to list tools after reconnecting '{}': {}",
                server_name, e
            )
        })?;

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
