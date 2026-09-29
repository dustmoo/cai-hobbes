use super::{is_composio_native, McpManager, McpServerConfig, McpServersWrapper};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

impl McpManager {
    pub(super) async fn load_configs(&self, config_path: PathBuf) -> Vec<McpServerConfig> {
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
}
