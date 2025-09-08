use rmcp::model::{CallToolRequestParam, Tool};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Deserialize, Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
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
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<Tool>,
}

#[derive(Clone)]
pub struct McpManager {
    configs: Vec<McpServerConfig>,
    pub servers: Arc<Mutex<HashMap<String, ActiveMcpClient>>>,
}

impl McpManager {
    pub fn new(config_path: PathBuf) -> Self {
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
                let configs_vec: Vec<McpServerConfig> = serde_json::from_str(&content).unwrap_or_else(|e| {
                    tracing::error!("Failed to parse mcp_servers.json: {}", e);
                    Vec::new()
                });
                tracing::info!("Successfully parsed {} MCP server configs.", configs_vec.len());
                configs_vec
            },
            Err(e) => {
                tracing::error!("Failed to read mcp_servers.json: {}", e);
                Vec::new()
            }
        };

        Self {
            configs,
            servers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn launch_servers(&self) {
        for server_config in self.configs.iter() {
            if server_config.disabled {
                tracing::info!("Skipping disabled MCP server: {}", server_config.name);
                continue;
            }
            let server_name = server_config.name.clone();
            tracing::info!("Launching MCP server: {}", server_name);
            let mut parts = server_config.command.split_whitespace();
            let program = if let Some(p) = parts.next() {
                p
            } else {
                tracing::error!("Empty command for server: {}", server_name);
                continue;
            };
            let args = parts;

            let mut cmd = Command::new(program);
            cmd.args(args)
                .envs(&server_config.env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let servers_map = self.servers.clone();
            let server_config_clone = server_config.clone();

            // This is now a blocking async operation within the function's scope
            match TokioChildProcess::new(cmd) {
                Ok(transport) => match ().serve(transport).await {
                    Ok(service) => {
                        tracing::info!("Connected to MCP server: {}", server_name);
                        match service.list_tools(Default::default()).await {
                            Ok(result) => {
                                tracing::info!(
                                    "Discovered capabilities for MCP server: {}",
                                    server_name
                                );
                                let active_client = ActiveMcpClient {
                                    config: server_config_clone,
                                    service,
                                    tools: result.tools,
                                };
                                let mut servers = servers_map.lock().await;
                                servers.insert(server_name.clone(), active_client);
                                tracing::info!("Successfully added '{}' to active MCP clients.", server_name);
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to discover capabilities for MCP server '{}': {}",
                                    server_name,
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to connect to MCP server '{}': {}", server_name, e);
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to launch MCP server '{}': {}", server_name, e);
                }
            }
        }
        tracing::info!("All MCP server launch tasks completed.");
    }
    pub async fn use_mcp_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let servers = self.servers.lock().await;
        if let Some(client) = servers.get(server_name) {
            if let Some(tool) = client.tools.iter().find(|t| t.name == tool_name) {
                let arguments = if let serde_json::Value::Object(map) = args {
                    map
                } else {
                    return Err("Tool arguments must be a JSON object".to_string());
                };
                let request = CallToolRequestParam {
                    name: tool.name.clone(),
                    arguments: Some(arguments),
                };
                match client.service.call_tool(request).await {
                    Ok(result) => Ok(serde_json::to_value(result.content).unwrap()),
                    Err(e) => Err(format!("Failed to use tool: {}", e)),
                }
            } else {
                Err(format!("Tool not found: {}", tool_name))
            }
        } else {
            Err(format!("Server not found: {}", server_name))
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