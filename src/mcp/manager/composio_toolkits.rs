use super::composio::{
    composio_server_key, dyn_composio_key, resolve_composio_key, ComposioKeyLookup,
};
use super::{ActiveMcpClient, McpClientType, McpContext, McpManager, McpServerConfig};
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use crate::mcp::tool_selection::{ToolCandidate, ToolSelectionRequest, TOOL_SELECTION_THRESHOLD};
use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::spawn;
use dioxus::prelude::Signal;
use dioxus_signals::{Readable, Writable};
use std::collections::HashSet;

impl McpManager {
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
                        crate::log_utils::redact_url(&new_server_url)
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
                    .or_else(|| {
                        p.retained_curation
                            .get(&toolkit_slug.to_lowercase())
                            .cloned()
                    })
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
                .add_toolkit_to_server(
                    &toolkit_slug,
                    auth_config_id.as_deref(),
                    Some(saved.clone()),
                )
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

            let selected_tools: Option<Vec<String>> = match client
                .get_toolkit_tools_detailed(&toolkit_slug)
                .await
            {
                Ok(tools) if tools.len() > TOOL_SELECTION_THRESHOLD => {
                    connection_status.set(format!("Selecting from {} tools...", tools.len()));
                    let candidates: Vec<ToolCandidate> = tools
                        .into_iter()
                        .map(|(name, desc)| ToolCandidate {
                            name,
                            description: desc,
                        })
                        .collect();

                    let request = ToolSelectionRequest::new(toolkit_slug.clone(), None, candidates);

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
                        None => Err("No LLM provider is configured for tool selection".to_string()),
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
                    .add_toolkit_to_server(
                        &toolkit_slug,
                        auth_config_id.as_deref(),
                        Some(tools.clone()),
                    )
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
}
