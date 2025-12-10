use dioxus::prelude::spawn;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{CallToolRequestParam, Tool, CallToolResult, PaginatedRequestParam};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use serde::{Deserialize, Serialize};
use crate::components::smithery_client::SmitheryServerDetail;
use crate::mcp::authenticated_sse::AuthenticatedClientError;
use rmcp::transport::sse_client::SseTransportError;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use crate::context::permissions::{PermissionManager, PermissionStatus, ToolCategory};
use dioxus::prelude::Signal;
use tokio::sync::Mutex;

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

#[derive(Serialize, Deserialize, Debug, Clone)]
struct McpServersWrapper {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct McpContext {
    pub servers: Vec<McpServerContext>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct McpServerContext {
    pub name: String,
    pub description: String,
    pub tools: Vec<Tool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ServerStatus {
    Loaded,      // Green - fully working
    Error,       // Red - configured but failed to load
    Disabled,    // Gray - configured but disabled
    NeedsAuth,   // Yellow - server requires OAuth authentication
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub status: ServerStatus,
    pub error_message: Option<String>,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

pub struct ActiveMcpClient {
    pub config: McpServerConfig,
    pub service: Arc<RunningService<RoleClient, ()>>,
    pub tools: Vec<Tool>,
}

/// Information about a server that requires authentication
#[derive(Clone)]
pub struct AuthRequiredInfo {
    pub config: McpServerConfig,
    pub auth_url: Option<String>,
    pub error_message: String,
}

#[derive(Clone)]
pub struct McpManager {
    config_path: Option<PathBuf>,
    pub servers: Arc<Mutex<HashMap<String, ActiveMcpClient>>>,
    pub failed_servers: Arc<Mutex<HashMap<String, (McpServerConfig, String)>>>,
    /// Servers that require OAuth authentication
    pub auth_required_servers: Arc<Mutex<HashMap<String, AuthRequiredInfo>>>,
    permission_manager: Signal<PermissionManager>,
}


impl McpManager {
    pub fn new(config_path: PathBuf, permission_manager: Signal<PermissionManager>) -> Self {
        Self {
            servers: Arc::new(Mutex::new(HashMap::new())),
            failed_servers: Arc::new(Mutex::new(HashMap::new())),
            auth_required_servers: Arc::new(Mutex::new(HashMap::new())),
            permission_manager,
            config_path: Some(config_path),
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

    pub async fn launch_servers(&self, mcp_context_signal: dioxus::prelude::Signal<McpContext>, settings: crate::settings::Settings) {
        let configs = if let Some(config_path) = &self.config_path {
            self.load_configs(config_path.clone()).await
        } else {
            Vec::new()
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ActiveMcpClient>();
        let servers_map_clone = self.servers.clone();
        let self_clone_for_receiver = self.clone();
        let mut mcp_context_signal_clone_for_receiver = mcp_context_signal.clone();

        // Spawn a dedicated receiver task to serialize context updates
        spawn(async move {
            while let Some(active_client) = rx.recv().await {
                let server_name = active_client.config.name.clone();
                tracing::info!("Received initialized client for: {}", server_name);
                
                // Lock and insert the new client
                servers_map_clone.lock().await.insert(server_name.clone(), active_client);

                // Get the full, updated context and set the signal
                let new_context = self_clone_for_receiver.get_mcp_context().await;
                mcp_context_signal_clone_for_receiver.set(new_context);
                tracing::info!("Successfully added '{}' and updated MCP context atomically.", server_name);
            }
            tracing::info!("MCP context update receiver task finished.");
        });

        for server_config in configs.iter().filter(|sc| !sc.disabled) {
            let mut server_config_clone = server_config.clone();
            if let Some(key) = &settings.smithery_api_key {
                server_config_clone.env.insert("SMITHERY_API_KEY".to_string(), key.trim().to_string());
            }
            let tx_clone = tx.clone();
            let settings_clone = settings.clone();
            let failed_servers_clone = self.failed_servers.clone();
            let auth_required_servers_clone = self.auth_required_servers.clone();

            spawn(async move {
                let server_name = server_config_clone.name.clone();
                tracing::info!("Initializing MCP server: {}", server_name);

                let service_result = if let Some(uri) = server_config_clone.uri.clone() {
                    // If a command is provided for a network server, launch it as a background process.
                    if let Some(command_string) = server_config_clone.command.clone() {
                        let server_name_clone = server_name.clone();
                        let server_config_clone_for_spawn = server_config_clone.clone();
                        spawn(async move {
                            let mut cmd = Command::new("sh");
                            let mut full_command = command_string;
                            if let Some(args) = server_config_clone_for_spawn.args {
                                full_command.push(' ');
                                full_command.push_str(&args.join(" "));
                            }
                            cmd.arg("-c").arg(&full_command);
                            cmd.envs(&server_config_clone_for_spawn.env);
                            // We run this as a detached process. We don't care if it fails,
                            // as the connection logic will handle that.
                            if let Err(e) = cmd.status().await {
                                tracing::error!("Failed to launch command for MCP server '{}': {}", server_name_clone, e);
                            }
                        });
                    }
                    // Network-based server (SSE)
                    tracing::info!("Connecting to network MCP server '{}' at {}", server_name, uri);
                    // For SSE servers, auth tokens should be provided via env vars by the CLI
                    // or directly in the server config as needed
                    
                    // Use authenticated transport for Bearer token support (API keys, etc.)
                    let transport = match crate::mcp::authenticated_sse::create_authenticated_transport(&uri, None).await {
                        Ok(t) => t,
                        Err(e) => {
                            let mut auth_url = None;
                            let mut is_auth_error = false;
                            
                            // Check specific error type
                            if let SseTransportError::Client(AuthenticatedClientError::AuthRequired(url)) = &e {
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
                                    }
                                );
                            } else {
                                failed_servers_clone.lock().await.insert(server_name, (server_config_clone, error_msg));
                            }
                            return;
                        }
                    };
                    ().serve(transport).await
                } else {
                    // Stdio-based server
                    tracing::info!("Launching stdio MCP server: {}", server_name);
                    let mut cmd = Command::new("sh");
                    let mut command_string = server_config_clone.command.clone().unwrap_or_default();

                    if let Some(ref args) = server_config_clone.args {
                        command_string.push(' ');
                        command_string.push_str(&args.join(" "));
                    }

                    if server_name == "filesystem" {
                        if let Some(project_folder) = &settings_clone.project_folder {
                            command_string.push_str(&format!(" \"{}\"", project_folder));
                            tracing::info!("Appending project folder to filesystem MCP command: {}", command_string);
                        }
                    }

                    cmd.arg("-c")
                        .arg(&command_string)
                        .envs(&server_config_clone.env)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped());

                    match TokioChildProcess::new(cmd) {
                        Ok(transport) => ().serve(transport).await,
                        Err(e) => {
                            tracing::error!("Failed to launch stdio MCP server '{}': {}", server_name, e);
                            return;
                        }
                    }
                };

                match service_result {
                    Ok(service) => {
                        tracing::info!("Connected to MCP server: {}", server_name);
                        
                        // Fetch all pages of tools
                        let mut all_tools = Vec::new();
                        let mut next_cursor: Option<String> = None;
                        
                        loop {
                            let cursor = next_cursor.clone();
                            // We need to use ListToolsRequest, but construct it correctly for the rmcp crate version
                            // The service.list_tools expects Option<PaginatedRequestParam>
                            let request_param = cursor.map(|c| PaginatedRequestParam {
                                cursor: Some(c),
                            });

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
                                            }
                                        );
                                    } else {
                                        failed_servers_clone.lock().await.insert(server_name.clone(), (server_config_clone, error_msg));
                                    }
                                    return;
                                }
                            }
                        }

                        tracing::info!("Discovered {} capabilities for MCP server: {}", all_tools.len(), server_name);
                        let active_client = ActiveMcpClient {
                            config: server_config_clone.clone(),
                            service: Arc::new(service),
                            tools: all_tools,
                        };
                        if tx_clone.send(active_client).is_err() {
                            tracing::error!("Failed to send initialized MCP client for '{}' to receiver task.", server_name);
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
                                }
                            );
                        } else {
                            failed_servers_clone.lock().await.insert(server_name, (server_config_clone, error_msg));
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

        match fs::read_to_string(config_path) {
            Ok(content) => {
                let wrapper: McpServersWrapper = serde_json::from_str(&content).unwrap_or_else(|e| {
                    tracing::error!("Failed to parse mcp_servers.json: {}", e);
                    McpServersWrapper {
                        mcp_servers: HashMap::new(),
                    }
                });

                let configs_vec: Vec<McpServerConfig> =
                    wrapper.mcp_servers.into_iter().map(|(name, mut config)| {
                        config.name = name;
                        config
                    }).collect();

                tracing::info!(
                    "Successfully parsed {} MCP server configs.",
                    configs_vec.len()
                );
                configs_vec
            }
            Err(e) => {
                tracing::error!("Failed to read mcp_servers.json: {}", e);
                Vec::new()
            }
        }
    }
    pub async fn add_or_update_mcp_server(&self, config_path: &PathBuf, new_config: McpServerConfig) -> Result<(), String> {
        let mut configs = self.load_configs(config_path.clone()).await;

        if let Some(existing_config) = configs.iter_mut().find(|c| c.name == new_config.name) {
            *existing_config = new_config;
        } else {
            configs.push(new_config);
        }

        self.save_configs(config_path, configs).await
    }

    async fn save_configs(&self, config_path: &PathBuf, configs: Vec<McpServerConfig>) -> Result<(), String> {
        let mcp_servers_map: HashMap<String, McpServerConfig> = configs.into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();

        let wrapper = McpServersWrapper {
            mcp_servers: mcp_servers_map,
        };

        let content = serde_json::to_string_pretty(&wrapper)
            .map_err(|e| format!("Failed to serialize MCP servers: {}", e))?;

        fs::write(config_path, content)
            .map_err(|e| format!("Failed to write to mcp_servers.json: {}", e))
    }

    fn map_tool_to_category(_tool_name: &str) -> ToolCategory {
        ToolCategory::Mcp
    }

    pub async fn use_mcp_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
        bypass_permission_check: bool,
    ) -> Result<UnboundedReceiver<Result<CallToolResult, String>>, String> {
        let servers_guard = self.servers.lock().await;

        let client_service_and_tool = if let Some(client) = servers_guard.get(server_name) {
            if !bypass_permission_check && !client.config.always_allow.contains(&tool_name.to_string())
            {
                let category = Self::map_tool_to_category(tool_name);
                let pm = self.permission_manager.read();
                match pm.check_permission(&category) {
                    PermissionStatus::Allowed => {}
                    PermissionStatus::RequiresPrompt => {
                        let tool_call = crate::components::shared::ToolCall::new(
                            server_name.to_string(),
                            tool_name.to_string(),
                            args,
                            None,
                        );
                        return Err(serde_json::to_string(&tool_call).unwrap_or_default());
                    }
                    PermissionStatus::Denied(reason) => {
                        return Err(format!("Tool use denied: {}", reason));
                    }
                }
            }

            if let Some(tool) = client.tools.iter().find(|t| t.name == tool_name) {
                Some((client.service.clone(), tool.clone()))
            } else {
                None
            }
        } else {
            return Err(format!("Server not found: {}", server_name));
        };

        drop(servers_guard);

        if let Some((service, tool)) = client_service_and_tool {
            let arguments = if let serde_json::Value::Object(map) = args {
                map
            } else {
                return Err("Tool arguments must be a JSON object".to_string());
            };
            let request = CallToolRequestParam {
                name: tool.name.clone(),
                arguments: Some(arguments),
            };

            let (tx, rx) = mpsc::unbounded_channel();

            spawn(async move {
                match service.call_tool(request).await {
                    Ok(result) => {
                        if tx.send(Ok(result)).is_err() {
                            tracing::error!("StreamManager receiver dropped for tool result.");
                        }
                    }
                    Err(e) => {
                        if tx.send(Err(format!("Failed to use tool: {}", e))).is_err() {
                            tracing::error!("StreamManager receiver dropped for tool error.");
                        }
                    }
                }
            });

            Ok(rx)
        } else {
            Err(format!("Tool not found: {}", tool_name))
        }
    }

    /// Start OAuth flow for a server that requires authentication
    /// This calls the generate_oauth_url tool and opens the browser
    pub async fn start_oauth_flow(&self, server_name: &str) -> Result<String, String> {
        // First, check if the server has a generate_oauth_url tool
        let servers = self.servers.lock().await;
        
        if let Some(client) = servers.get(server_name) {
            // Check if server has OAuth tools
            let has_oauth_url_tool = client.tools.iter().any(|t| {
                let name = t.name.as_ref();
                name.contains("oauth_url") || 
                name.contains("generate_oauth") ||
                name.contains("auth_url")
            });
            
            if !has_oauth_url_tool {
                return Err(format!("Server '{}' does not have OAuth tools", server_name));
            }
            
            // Find the callback port
            let port = crate::mcp::oauth_flow::find_available_port()
                .ok_or("Could not find available port for OAuth callback")?;
            
            let redirect_uri = format!("http://localhost:{}/callback", port);
            
            // Call the generate_oauth_url tool
            let service = client.service.clone();
            drop(servers); // Release lock before async call
            
            // Try different tool name patterns
            let tool_names = ["generate_oauth_url", "GENERATE_OAUTH_URL", "generate_auth_url"];
            let mut oauth_url = None;
            
            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(serde_json::json!({
                        "redirect_uri": redirect_uri
                    }).as_object().cloned().unwrap()),
                };
                
                if let Ok(result) = service.call_tool(request).await {
                    // Extract URL from result
                    if let Some(content) = result.content.first() {
                        if let Some(text) = content.raw.as_text() {
                            // The result might be a URL directly or JSON containing a URL
                            if text.text.starts_with("http") {
                                oauth_url = Some(text.text.clone());
                            } else if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text.text) {
                                if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
                                    oauth_url = Some(url.to_string());
                                } else if let Some(url) = json.get("oauth_url").and_then(|v| v.as_str()) {
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
            
            if let Some(url) = oauth_url {
                // Start callback server and open browser
                let _callback_rx = crate::mcp::oauth_flow::start_callback_server(port);
                crate::mcp::oauth_flow::open_browser(&url)?;
                
                Ok(format!("OAuth flow started. Callback server on port {}", port))
            } else {
                Err("Failed to get OAuth URL from server".to_string())
            }
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }

    /// Complete OAuth flow by exchanging auth code for tokens
    pub async fn complete_oauth_flow(&self, server_name: &str, auth_code: &str) -> Result<String, String> {
        let servers = self.servers.lock().await;
        
        if let Some(client) = servers.get(server_name) {
            let service = client.service.clone();
            drop(servers);
            
            // Try different tool name patterns
            let tool_names = ["exchange_auth_code", "EXCHANGE_AUTH_CODE", "exchange_code"];
            
            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(serde_json::json!({
                        "code": auth_code
                    }).as_object().cloned().unwrap()),
                };
                
                if let Ok(result) = service.call_tool(request).await {
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
            
            Err("Failed to exchange auth code".to_string())
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }

    pub async fn get_mcp_context(&self) -> McpContext {
        let servers = self.servers.lock().await;
        let mut server_contexts = Vec::new();

        for (_, client) in servers.iter() {
            let server_context = McpServerContext {
                name: client.config.name.clone(),
                description: client.config.description.clone(),
                tools: client.tools.clone(),
            };
            server_contexts.push(server_context);
        }

        McpContext {
            servers: server_contexts,
        }
    }

    pub async fn get_all_server_statuses(&self) -> Vec<McpServerStatus> {
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
        
        for config in configs {
            let status = if config.disabled {
                McpServerStatus {
                    name: config.name.clone(),
                    display_name: config.name.clone(),
                    description: config.description.clone(),
                    status: ServerStatus::Disabled,
                    error_message: None,
                    tools: 0,
                    resources: 0,
                    prompts: 0,
                    auth_url: None,
                    uri: config.uri.clone(),
                }
            } else if servers.contains_key(&config.name) {
                let client = servers.get(&config.name).unwrap();
                McpServerStatus {
                    name: config.name.clone(),
                    display_name: config.name.clone(),
                    description: config.description.clone(),
                    status: ServerStatus::Loaded,
                    error_message: None,
                    tools: client.tools.len(),
                    resources: 0, // TODO: Implement resources tracking
                    prompts: 0,   // TODO: Implement prompts tracking
                    auth_url: None,
                    uri: config.uri.clone(),
                }
            } else if let Some(auth_info) = auth_required.get(&config.name) {
                // Server requires OAuth authentication
                McpServerStatus {
                    name: config.name.clone(),
                    display_name: config.name.clone(),
                    description: config.description.clone(),
                    status: ServerStatus::NeedsAuth,
                    error_message: Some(auth_info.error_message.clone()),
                    tools: 0,
                    resources: 0,
                    prompts: 0,
                    auth_url: auth_info.auth_url.clone(),
                    uri: config.uri.clone(),
                }
            } else if let Some((_, error)) = failed.get(&config.name) {
                McpServerStatus {
                    name: config.name.clone(),
                    display_name: config.name.clone(),
                    description: config.description.clone(),
                    status: ServerStatus::Error,
                    error_message: Some(error.clone()),
                    tools: 0,
                    resources: 0,
                    prompts: 0,
                    auth_url: None,
                    uri: config.uri.clone(),
                }
            } else {
                // Server is still initializing or hasn't been attempted yet
                McpServerStatus {
                    name: config.name.clone(),
                    display_name: config.name.clone(),
                    description: config.description.clone(),
                    status: ServerStatus::Error,
                    error_message: Some("Initializing...".to_string()),
                    tools: 0,
                    resources: 0,
                    prompts: 0,
                    auth_url: None,
                    uri: config.uri.clone(),
                }
            };
            statuses.push(status);
        }
        
        statuses
    }

    pub async fn retry_server(&self, server_name: &str, mcp_context_signal: dioxus::prelude::Signal<McpContext>, settings: crate::settings::Settings, access_token: Option<String>) -> Result<(), String> {
        // Load config for the specific server
        let configs = if let Some(config_path) = &self.config_path {
            self.load_configs(config_path.clone()).await
        } else {
            return Err("No config path available".to_string());
        };

        let mut server_config = configs.into_iter()
            .find(|c| c.name == server_name)
            .ok_or_else(|| format!("Server '{}' not found in config", server_name))?;

        if let Some(key) = &settings.smithery_api_key {
            server_config.env.insert("SMITHERY_API_KEY".to_string(), key.clone());
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
        let mut mcp_context_signal_clone = mcp_context_signal.clone();
        let access_token_clone = access_token.clone();

        spawn(async move {
            let server_name = server_config_clone.name.clone();
            tracing::info!("Retrying MCP server: {}", server_name);

            // Determine effective configuration (handle local -> remote upgrade with token)
            let mut effective_config = server_config_clone.clone();
            if effective_config.uri.is_none() && access_token_clone.is_some() {
                 tracing::info!("Upgrading local server '{}' to remote Smithery endpoint using OAuth token", server_name);
                 effective_config.uri = Some(format!("https://server.smithery.ai/{}/mcp", server_name));
                 // Don't run the local command since we are connecting remotely
                 effective_config.command = None;
            }

            let service_result = if let Some(uri) = effective_config.uri.clone() {
                // Network-based server (SSE)
                if let Some(command_string) = effective_config.command.clone() {
                    let server_name_clone = server_name.clone();
                    let server_config_clone_for_spawn = effective_config.clone();
                    spawn(async move {
                        let mut cmd = Command::new("sh");
                        let mut full_command = command_string;
                        if let Some(args) = server_config_clone_for_spawn.args {
                            full_command.push(' ');
                            full_command.push_str(&args.join(" "));
                        }
                        cmd.arg("-c").arg(&full_command);
                        cmd.envs(&server_config_clone_for_spawn.env);
                        if let Err(e) = cmd.status().await {
                            tracing::error!("Failed to launch command for MCP server '{}': {}", server_name_clone, e);
                        }
                    });
                }
                tracing::info!("Connecting to network MCP server '{}' at {}", server_name, uri);
                // For SSE servers, auth tokens should be provided via env vars
                // or directly in the server config as needed
                
                // Use authenticated transport for Bearer token support (API keys, etc.)
                let transport = match crate::mcp::authenticated_sse::create_authenticated_transport(&uri, access_token_clone).await {
                    Ok(t) => t,
                    Err(e) => {
                        let mut auth_url = None;
                        let mut is_auth_error = false;
                        
                        // Check specific error type
                        if let SseTransportError::Client(AuthenticatedClientError::AuthRequired(url)) = &e {
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
                                }
                            );
                        } else {
                            failed_servers_clone.lock().await.insert(server_name, (server_config_clone, error_msg));
                        }
                        return;
                    }
                };
                ().serve(transport).await
            } else {
                // Stdio-based server
                tracing::info!("Launching stdio MCP server: {}", server_name);
                let mut cmd = Command::new("sh");
                let mut command_string = server_config_clone.command.clone().unwrap_or_default();

                if let Some(ref args) = server_config_clone.args {
                    command_string.push(' ');
                    command_string.push_str(&args.join(" "));
                }

                if server_name == "filesystem" {
                    if let Some(project_folder) = &settings_clone.project_folder {
                        command_string.push_str(&format!(" \"{}\"", project_folder));
                        tracing::info!("Appending project folder to filesystem MCP command: {}", command_string);
                    }
                }

                cmd.arg("-c")
                    .arg(&command_string)
                    .envs(&server_config_clone.env)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                match TokioChildProcess::new(cmd) {
                    Ok(transport) => ().serve(transport).await,
                    Err(e) => {
                        tracing::error!("Failed to launch stdio MCP server '{}': {}", server_name, e);
                        return;
                    }
                }
            };

            match service_result {
                Ok(service) => {
                    tracing::info!("Connected to MCP server: {}", server_name);
                    
                    // Fetch all pages of tools
                    let mut all_tools = Vec::new();
                    let mut next_cursor: Option<String> = None;
                    
                    loop {
                        let cursor = next_cursor.clone();
                        // Same here for the retry_server function
                        let request_param = cursor.map(|c| PaginatedRequestParam {
                            cursor: Some(c),
                        });

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
                                        }
                                    );
                                } else {
                                    failed_servers_clone.lock().await.insert(server_name.clone(), (server_config_clone, error_msg));
                                }
                                return;
                            }
                        }
                    }

                    tracing::info!("Discovered {} capabilities for MCP server: {}", all_tools.len(), server_name);
                    let active_client = ActiveMcpClient {
                        config: server_config_clone.clone(),
                        service: Arc::new(service),
                        tools: all_tools,
                    };
                    servers_clone.lock().await.insert(server_name.clone(), active_client);
                    
                    // Update context
                    let new_context = self_clone.get_mcp_context().await;
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
                            }
                        );
                    } else {
                        failed_servers_clone.lock().await.insert(server_name, (server_config_clone, error_msg));
                    }
                }
            }
        });

        Ok(())
    }
    pub async fn install_mcp_server(&self, server_config: &SmitheryServerDetail) -> Result<(), String> {
        let config_path = self.config_path.as_ref().ok_or("Config path not set")?.clone();

        // Find the correct config for the current platform
        let platform = crate::components::smithery_client::get_platform();
        let mcp_config = server_config.configs.as_ref()
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

                McpServerConfig {
                    name: server_config.qualified_name.clone(),
                    command: Some(c.command.clone()),
                    uri: None, // Assuming stdio-based for now
                    args: Some(args),
                    description: "".to_string(), // Can be enriched later
                    env,
                    disabled: false,
                    always_allow: Vec::new(),
                }
            });

        if let Some(new_config) = mcp_config {
            self.add_or_update_mcp_server(&config_path, new_config).await
        } else {
            Err(format!("No compatible configuration found for platform '{}'", platform))
        }
    }
}