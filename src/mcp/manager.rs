use dioxus::prelude::spawn;
use rmcp::service::ClientInitializeError;
use dioxus_signals::{Readable, Writable};
use rmcp::model::{CallToolRequestParam, Tool, CallToolResult};
use reqwest::Client;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::sse_client::SseClientTransport;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use crate::context::permissions::{PermissionManager, PermissionStatus, ToolCategory};
use dioxus::prelude::Signal;
use tokio::sync::Mutex;

#[derive(Deserialize, Debug, Clone)]
pub struct McpServerConfig {
    #[serde(default)] // Name will be injected from the map key
    pub name: String,
    pub command: Option<String>,
    pub uri: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub always_allow: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
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

pub struct ActiveMcpClient {
    pub config: McpServerConfig,
    pub service: Arc<RunningService<RoleClient, ()>>,
    pub tools: Vec<Tool>,
}

#[derive(Clone)]
pub struct McpManager {
    configs: Vec<McpServerConfig>,
    pub servers: Arc<Mutex<HashMap<String, ActiveMcpClient>>>,
    permission_manager: Signal<PermissionManager>,
}


impl McpManager {
    pub fn new(config_path: PathBuf, permission_manager: Signal<PermissionManager>) -> Self {
        if !config_path.exists() {
            if let Some(parent) = config_path.parent() {
                if !parent.exists() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        tracing::error!("Failed to create config directory: {}", e);
                    }
                }
            }
            if let Err(e) = fs::write(&config_path, "[]") {
                tracing::error!("Failed to write default mcp_servers.json: {}", e);
            }
        }

        let configs = match fs::read_to_string(config_path) {
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
        };

        Self {
            configs,
            servers: Arc::new(Mutex::new(HashMap::new())),
            permission_manager,
        }
    }

    pub async fn launch_servers(&self, mcp_context_signal: dioxus::prelude::Signal<McpContext>, settings: crate::settings::Settings) {
        for server_config in self.configs.iter().filter(|sc| !sc.disabled) {
            let server_config_clone = server_config.clone();
            let servers_map = self.servers.clone();
            let mut mcp_context_signal_clone = mcp_context_signal.clone();
            let self_clone = self.clone();
            let settings_clone = settings.clone();

            spawn(async move {
                let server_name = server_config_clone.name.clone();
                tracing::info!("Initializing MCP server: {}", server_name);

                // If a command is provided, launch it as a background process.
                if let Some(command_string) = server_config_clone.command.clone() {
                    let server_name_clone = server_name.clone();
                    let server_config_clone_for_spawn = server_config_clone.clone();
                    spawn(async move {
                        let mut cmd = Command::new("sh");
                        cmd.arg("-c").arg(&command_string);
                        cmd.envs(&server_config_clone_for_spawn.env);
                        // We run this as a detached process. We don't care if it fails,
                        // as the connection logic will handle that.
                        if let Err(e) = cmd.status().await {
                            tracing::error!("Failed to launch command for MCP server '{}': {}", server_name_clone, e);
                        }
                    });
                }

                let service_result = if let Some(uri) = server_config_clone.uri.clone() {
                    // Network-based server (SSE)
                    tracing::info!("Connecting to network MCP server '{}' at {}", server_name, uri);
                    match SseClientTransport::start(uri).await {
                        Ok(transport) => ().serve(transport).await,
                        Err(e) => Err(ClientInitializeError::transport::<SseClientTransport<Client>>(
                            e,
                            "Failed to start SSE client transport",
                        )),
                    }
                } else {
                    // Stdio-based server
                    tracing::info!("Launching stdio MCP server: {}", server_name);
                    let mut cmd = Command::new("sh");
                    let mut command_string = server_config_clone.command.clone().unwrap_or_default();

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
                        match service.list_tools(Default::default()).await {
                            Ok(result) => {
                                tracing::info!("Discovered capabilities for MCP server: {}", server_name);
                                let active_client = ActiveMcpClient {
                                    config: server_config_clone,
                                    service: Arc::new(service),
                                    tools: result.tools,
                                };
                                {
                                    let mut servers = servers_map.lock().await;
                                    servers.insert(server_name.clone(), active_client);
                                }

                                let new_context = self_clone.get_mcp_context().await;
                                mcp_context_signal_clone.set(new_context);
                                tracing::info!("Successfully added '{}' and updated MCP context.", server_name);
                            }
                            Err(e) => tracing::error!("Failed to list tools for '{}': {}", server_name, e),
                        }
                    }
                    Err(e) => tracing::error!("Failed to serve MCP server '{}': {}", server_name, e),
                }
            });
        }
        tracing::info!("All MCP server launch tasks initiated.");
    }
    fn map_tool_to_category(_tool_name: &str) -> ToolCategory {
        // All tools loaded via MCP are considered MCP tools for permission purposes.
        // The Browser/Execute categories are commented out as they are not currently used
        // and all tools are dynamically loaded.
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
}