use dioxus_signals::Readable;
use rmcp::model::Tool;
use std::collections::HashSet;
use super::{COMPOSIO_NATIVE_PREFIX, is_composio_native, HOBBES_CORE_SERVER, HOBBES_META_SERVER, HOBBES_PLANNER_SERVER, HOBBES_TERMINAL_SERVER, McpServerConfig, McpContext, McpServerContext, ServerStatus, McpServerStatus, McpClientType, ActiveMcpClient, McpManager, composio_server_key, resolve_composio_key, dyn_composio_key, ComposioKeyLookup};

impl McpManager {
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
        let composio_lookup = resolve_composio_key(&servers, profile_id.as_deref(), |c| {
            c.profile_id.as_deref()
        });
        if let ComposioKeyLookup::Ambiguous { ref candidates } = composio_lookup {
            // Not a warning here: this is the prompt-build path, called on every
            // turn, so a degrade (silently reporting no connected toolkits) must
            // stay traceable without being noisy.
            tracing::info!(
                profile_id = ?profile_id,
                candidates = ?candidates,
                "get_mcp_context: several Composio native clients and no profile match; \
                 reporting no connected toolkits for this turn."
            );
        }
        let connected_toolkit_slugs = match composio_lookup {
            ComposioKeyLookup::Found(key) => servers.get(&key),
            _ => None,
        }
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
            let composio_lookup = resolve_composio_key(&servers, active_profile_id.as_deref(), |c| {
                c.profile_id.as_deref()
            });
            let active_composio: Option<(&String, &ActiveMcpClient)> = match &composio_lookup {
                ComposioKeyLookup::Found(k) => servers.get_key_value(k),
                _ => None,
            };

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
            // Several Composio profiles are connected but none matches the active
            // profile — this is NOT "not configured" (Composio *is* connected),
            // so it must not render as the blue NotConfigured card. Surface it as
            // an Error status naming the actual problem instead.
            else if let ComposioKeyLookup::Ambiguous { candidates } = &composio_lookup {
                statuses.push(McpServerStatus {
                    name: display_name.clone(),
                    error_message: Some(format!(
                        "Several Composio profiles are connected but none is active — pick a profile in Settings. (candidates: {})",
                        candidates.join(", ")
                    )),
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
            (HOBBES_TERMINAL_SERVER, "Terminal"),
            (HOBBES_CORE_SERVER, "Hobbes Core Tools"),
            (HOBBES_META_SERVER, "Hobbes Meta Tools"),
            (HOBBES_PLANNER_SERVER, "Hobbes Planner"),
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
}
