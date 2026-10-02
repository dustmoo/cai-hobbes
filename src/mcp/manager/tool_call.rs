use super::composio::{composio_server_key, dyn_composio_key};
use super::{
    is_composio_native, McpClientType, McpManager, COMPOSIO_NATIVE_PREFIX, HOBBES_META_SERVER,
};
use crate::components::shared::ToolCallStatus;
use crate::context::permissions::PermissionStatus;
use crate::mcp::composio_client::{composio_to_rmcp_tool, ComposioClient};
use crate::SecretManagerTrait;
use dioxus::prelude::spawn;
use dioxus_signals::Readable;
use rmcp::model::{CallToolRequestParam, CallToolResult, Tool};
use std::collections::HashSet;
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// Gemini's practical tool limit for FunctionDeclarations.
const GEMINI_TOOL_LIMIT: usize = 128;

/// Get meta-tools for local MCP server on-demand loading.
/// These allow the AI to load/unload tools from servers set to "On-demand" mode.
pub(super) fn get_local_meta_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "MCP_LOAD_SERVER_TOOLS".into(),
            description: Some(
                "Load all tools from a local MCP server that is in on-demand mode. \
                After calling this, the server's tools become available as native function calls \
                on the next turn. Use this when you need to use tools from a server marked [ON-DEMAND]."
                    .into(),
            ),
            input_schema: serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "The exact name of the on-demand server to load tools from (e.g., 'playwright', 'graphiti')"
                    }
                },
                "required": ["server_name"]
            }))
            .unwrap_or_default(),
            title: None,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "MCP_UNLOAD_SERVER_TOOLS".into(),
            description: Some(
                "Unload dynamically loaded tools from a local MCP server to free up context space. \
                Use this when you no longer need tools from a server you previously loaded."
                    .into(),
            ),
            input_schema: serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "The exact name of the server whose tools should be unloaded"
                    }
                },
                "required": ["server_name"]
            }))
            .unwrap_or_default(),
            title: None,
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
    ]
}

/// Score a tool's relevance by name keywords for budget-aware selection.
/// Higher score = more likely to be included when over budget.
///
/// MAYBE: Consider exposing these priority weights in the Settings UI in the future.
/// For now, these defaults work well and exposing them risks user misconfiguration.
fn score_tool_relevance(name: &str) -> u32 {
    let upper = name.to_uppercase();
    // Root traversal / auth tools — critical for hierarchy navigation
    if upper.contains("_TEAM")
        || upper.contains("_WORKSPACE")
        || upper.contains("_ORGANIZATION")
        || upper.contains("_AUTH")
        || upper.contains("_SPACE")
    {
        return 100;
    }
    // Core read operations
    if upper.contains("_GET_")
        || upper.contains("_LIST_")
        || upper.contains("_SEARCH_")
        || upper.contains("_FIND_")
    {
        return 80;
    }
    // Core write operations
    if upper.contains("_CREATE_") || upper.contains("_ADD_") || upper.contains("_POST_") {
        return 60;
    }
    // Core update operations
    if upper.contains("_UPDATE_")
        || upper.contains("_SET_")
        || upper.contains("_EDIT_")
        || upper.contains("_MODIFY_")
    {
        return 40;
    }
    // Delete operations
    if upper.contains("_DELETE_") || upper.contains("_REMOVE_") {
        return 20;
    }
    // Everything else (specialized, admin, etc.)
    10
}

/// Check a failed Composio `ToolExecuteResponse` for 401/403 auth errors and attempt
/// auto-reconnection via OAuth. Returns `Some(CallToolResult)` if the auth flow
/// handled the error (success or auth-required URL), or `None` to fall through
/// to the normal error response path.
///
/// Pattern 150.8.1: On auth failure, busts the stale `toolkit_account_map` cache entry,
/// re-authenticates, and retries the tool call with the original arguments.
async fn try_auth_recovery(
    response: &crate::mcp::composio_client::models::ToolExecuteResponse,
    tool_name: &str,
    composio_client: &ComposioClient,
) -> Option<CallToolResult> {
    if response.successful {
        return None;
    }

    // Delegate auth detection to the single-authority method on ToolExecuteResponse.
    // This covers status_code, statusCode, ECODE, http_error, nested data, and error string.
    if !response.is_auth_error() {
        return None;
    }

    // CYCLE GUARD: If the response already contains a redirectUrl, it means execute_tool's
    // proactive auth check already triggered reconnect_toolkit and returned an OAuth URL.
    // We must NOT fire recovery again, or we'll enter a double-reconnect loop where
    // is_auth_error() matches our own "Authentication required" error message.
    if response.data.get("redirectUrl").is_some() {
        tracing::debug!(
            "[AUTH RECOVERY] Skipping — response is already an auth redirect from proactive check"
        );
        return None;
    }

    // Extract toolkit slug from tool name (first segment before _)
    let toolkit_slug =
        ComposioClient::normalize_toolkit_key(tool_name.split('_').next().unwrap_or(tool_name));

    tracing::info!("[AUTH RECOVERY] Auth error detected for '{}' (toolkit: '{}'), triggering 6-point reconnect", tool_name, toolkit_slug);

    // Use the full 6-point reconnect lifecycle:
    // 1. Hydrate auth_config_cache (finds existing configs)
    // 2. Resolve auth_config_id (cache → API → create)
    // 3. Delete stale ACTIVE connections + bust cache
    // 4. Initiate OAuth with force=true (opens browser)
    // 5. Re-patch MCP server
    // 6. Re-hydrate toolkit_account_map
    match composio_client.reconnect_toolkit(&toolkit_slug).await {
        Ok(result_msg) => {
            if result_msg.contains("Authentication successful") {
                // ANTI-PATTERN: Do NOT retry the tool call internally!
                // Because reconnect_toolkit uses force=true (which prunes connections),
                // an immediate internal retry that fails (e.g. due to proxy sync delay)
                // would return a 401 -> is_error: Some(true) -> causing the LLM to autonomously
                // retry -> triggering ANOTHER prune and ANOTHER browser popup in an infinite loop!
                // Returning `is_error: Some(false)` breaks the LLM's panic loop and allows token propagation.
                Some(CallToolResult {
                    content: vec![rmcp::model::Content::text(
                        "Authentication successful! Please try the tool again.".to_string(),
                    )],
                    is_error: Some(false),
                    structured_content: None,
                    meta: None,
                })
            } else {
                let url = result_msg
                    .split_whitespace()
                    .last()
                    .unwrap_or(&result_msg)
                    .to_string();
                let auth_msg = format!(
                    "Authentication required. Please connect your account: {}",
                    url
                );
                Some(CallToolResult {
                    content: vec![rmcp::model::Content::text(auth_msg)],
                    is_error: Some(true),
                    structured_content: None,
                    meta: None,
                })
            }
        }
        Err(e) => {
            tracing::error!("[AUTH RECOVERY] 6-point reconnect failed: {}", e);
            None // Fall through to return original error
        }
    }
}

impl McpManager {
    /// Execute a tool on an MCP server.
    ///
    /// # Error Type: `Result<..., String>` — Architectural Decision
    ///
    /// This function (and the MCP boundary in general) deliberately uses `String`
    /// for error types rather than a structured `thiserror` enum. This is intentional:
    ///
    /// 1. **MCP is cross-language**: Tool responses come from servers written in Python,
    ///    Node.js, Go, etc. Errors are free-form strings with no guaranteed schema.
    /// 2. **AI consumption**: Errors are forwarded to the LLM for self-correction.
    ///    The AI needs human-readable context, not Rust enum variants.
    /// 3. **Display-final**: Most errors are shown directly to the user or AI — `String`
    ///    is the terminal format regardless.
    /// 4. **Normalization is impractical**: Each MCP server produces unique error formats.
    ///    A catch-all `Other(String)` variant would contain 90%+ of cases, defeating the purpose.
    ///
    /// For internal plumbing (keychain, persistence), structured errors ARE used — see
    /// `KeychainError` in `keychain_ffi.rs` for the established pattern.
    pub async fn use_mcp_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
        bypass_permission_check: bool,
        profile_id: Option<String>,
        // Chat-session context for tools that are per-session (the native
        // terminal). A typed parameter rather than arg-injection: args
        // serialize into the permission bubble, and the compiler enumerates
        // every call site. None for everything else.
        session_ctx: Option<crate::mcp::terminal_client::TerminalCtx>,
    ) -> Result<UnboundedReceiver<Result<CallToolResult, String>>, String> {
        // Intercept local on-demand meta-tools before normal dispatch
        if server_name == HOBBES_META_SERVER
            || tool_name == "MCP_LOAD_SERVER_TOOLS"
            || tool_name == "MCP_UNLOAD_SERVER_TOOLS"
        {
            let (tx, rx) = mpsc::unbounded_channel();

            if tool_name == "MCP_LOAD_SERVER_TOOLS" {
                let target_server = args
                    .get("server_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if target_server.is_empty() {
                    let _ = tx.send(Err("Missing 'server_name' argument".to_string()));
                    return Ok(rx);
                }

                // Check if the server is in on-demand mode
                let is_on_demand = {
                    let on_demand = self.on_demand_servers.lock().await;
                    on_demand.contains(target_server)
                };

                if !is_on_demand {
                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Server '{}' is not in on-demand mode. Its tools are already available.",
                            target_server
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                    return Ok(rx);
                }

                // Copy tools from the server into dynamic_local_tools cache
                let servers = self.servers.lock().await;
                if let Some(client) = servers.get(target_server) {
                    let tool_count = client.tools.len();
                    let tool_names: Vec<String> =
                        client.tools.iter().map(|t| t.name.to_string()).collect();

                    // Add to dynamic cache (deduplicating)
                    let mut dynamic = self.dynamic_local_tools.lock().await;
                    let mut sources = self.dynamic_local_tool_sources.lock().await;
                    let existing_names: HashSet<String> =
                        dynamic.iter().map(|t| t.name.to_string()).collect();
                    let new_tools: Vec<Tool> = client
                        .tools
                        .iter()
                        .filter(|t| !existing_names.contains(t.name.as_ref()))
                        .cloned()
                        .collect();
                    let injected = new_tools.len();
                    dynamic.extend(new_tools);

                    // Record origin server for each tool so use_mcp_tool can
                    // resolve "local-on-demand" back to the real server.
                    for name in &tool_names {
                        sources.insert(name.clone(), target_server.to_string());
                    }

                    tracing::info!(
                        "MCP_LOAD_SERVER_TOOLS: Loaded {} tools from '{}' (total dynamic: {}, sources: {})",
                        injected, target_server, dynamic.len(), sources.len()
                    );

                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Loaded {} tools from '{}'. They are now available as native function calls on the next turn.\n\nTools: {}",
                            tool_count, target_server, tool_names.join(", ")
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                } else {
                    let _ = tx.send(Err(format!("Server '{}' not found", target_server)));
                }
                return Ok(rx);
            } else if tool_name == "MCP_UNLOAD_SERVER_TOOLS" {
                let target_server = args
                    .get("server_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if target_server.is_empty() {
                    let _ = tx.send(Err("Missing 'server_name' argument".to_string()));
                    return Ok(rx);
                }

                // Remove this server's tools from dynamic cache
                let servers = self.servers.lock().await;
                if let Some(client) = servers.get(target_server) {
                    let server_tool_names: HashSet<String> =
                        client.tools.iter().map(|t| t.name.to_string()).collect();
                    let mut dynamic = self.dynamic_local_tools.lock().await;
                    let mut sources = self.dynamic_local_tool_sources.lock().await;
                    let before = dynamic.len();
                    dynamic.retain(|t| !server_tool_names.contains(t.name.as_ref()));
                    let removed = before - dynamic.len();
                    // Clean up source map
                    for name in &server_tool_names {
                        sources.remove(name);
                    }

                    tracing::info!(
                        "MCP_UNLOAD_SERVER_TOOLS: Removed {} tools from '{}' (remaining dynamic: {})",
                        removed, target_server, dynamic.len()
                    );

                    let _ = tx.send(Ok(CallToolResult {
                        content: vec![rmcp::model::Content::text(format!(
                            "Unloaded {} tools from '{}'. Context space freed.",
                            removed, target_server
                        ))],
                        is_error: Some(false),
                        structured_content: None,
                        meta: None,
                    }));
                } else {
                    let _ = tx.send(Err(format!("Server '{}' not found", target_server)));
                }
                return Ok(rx);
            }
        }

        // Resolve "local-on-demand" virtual server → real origin server via source map.
        // The AI sends server_name="local-on-demand" for dynamically loaded tools because
        // get_mcp_context() presents them under that virtual name.
        let resolved_server_name: Option<String> = if server_name == "local-on-demand" {
            let sources = self.dynamic_local_tool_sources.lock().await;
            sources.get(tool_name).cloned()
        } else {
            None
        };
        let effective_server_name = resolved_server_name.as_deref().unwrap_or(server_name);

        let mut servers_guard = self.servers.lock().await;

        // Resolve composio-native to the actual profile-suffixed key.
        // The LLM may send:
        //   "composio-native"          — bare name (needs profile resolution)
        //   "composio-native:SomeName" — config.name (profile NAME suffix, NOT the storage key)
        //   "composio-native:SomeUUID" — storage key (profile ID suffix)
        //   "other-server"             — non-composio server (pass through)
        let actual_server_name = if is_composio_native(effective_server_name) {
            // Extract the suffix (profile name or ID) if present
            let suffix = server_name.strip_prefix("composio-native:").unwrap_or("");

            // 1. Try direct key lookup (works if suffix is already the profile ID)
            if !suffix.is_empty() && servers_guard.contains_key(server_name) {
                server_name.to_string()
            }
            // 2. Try with explicit profile_id param (from stream_manager)
            else if let Some(ref p) = profile_id {
                let scoped_key = composio_server_key(p);
                if servers_guard.contains_key(&scoped_key) {
                    scoped_key
                } else {
                    // Fallback: search by profile_id field (handles ID/name mismatch)
                    servers_guard
                        .iter()
                        .find(|(k, c)| is_composio_native(k) && c.profile_id.as_deref() == Some(p))
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| {
                            if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                                COMPOSIO_NATIVE_PREFIX.to_string()
                            } else {
                                String::new()
                            }
                        })
                }
            }
            // 3. If suffix is a profile NAME (from config.name), search by config.name match
            else if !suffix.is_empty() {
                servers_guard
                    .iter()
                    .find(|(_, c)| c.config.name == server_name)
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| {
                        // Also try matching suffix against profile_id
                        servers_guard
                            .iter()
                            .find(|(k, c)| {
                                is_composio_native(k) && c.profile_id.as_deref() == Some(suffix)
                            })
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| {
                                if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                                    COMPOSIO_NATIVE_PREFIX.to_string()
                                } else {
                                    String::new()
                                }
                            })
                    })
            }
            // 4. Bare "composio-native" with no profile — try singleton
            else if servers_guard.contains_key(COMPOSIO_NATIVE_PREFIX) {
                COMPOSIO_NATIVE_PREFIX.to_string()
            } else {
                return Err("Composio client not found and no profile specified".to_string());
            }
        } else {
            effective_server_name.to_string()
        };

        let client = match servers_guard.get(&actual_server_name) {
            Some(c) => c,
            None => {
                // If this was a local-on-demand tool that couldn't be resolved,
                // give a more specific error message.
                if server_name == "local-on-demand" {
                    return Err(format!(
                        "Tool '{}' not found in on-demand cache. The server may need to be loaded first via MCP_LOAD_SERVER_TOOLS.",
                        tool_name
                    ));
                }
                return Err(format!("Server not found: {}", server_name));
            }
        };

        // PRE-EMPTIVE HEALTH CHECK: If the stdio transport is already dead (child process
        // exited, pipe broken, etc.), attempt to reconnect before dispatching the call.
        // This avoids a wasted call_tool round-trip that would just return TransportClosed.
        if !client.service.is_healthy() {
            let config_for_reconnect = client.config.clone();
            tracing::warn!(
                "[RECONNECT] Transport for stdio server '{}' is dead — attempting auto-reconnect before tool call",
                actual_server_name
            );
            // Drop the lock before reconnecting (reconnect_stdio_server needs it)
            drop(servers_guard);

            match self
                .reconnect_stdio_server(&actual_server_name, config_for_reconnect)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "[RECONNECT] Successfully reconnected stdio server '{}'",
                        actual_server_name
                    );
                }
                Err(e) => {
                    return Err(format!(
                        "Server '{}' transport is dead and reconnect failed: {}",
                        actual_server_name, e
                    ));
                }
            }

            // Re-acquire the lock with the fresh client
            servers_guard = self.servers.lock().await;
        }

        // Re-resolve client (in case reconnect replaced it, or on first pass)
        let client = match servers_guard.get(&actual_server_name) {
            Some(c) => c,
            None => {
                return Err(format!(
                    "Server '{}' disappeared after reconnect attempt",
                    actual_server_name
                ));
            }
        };

        let permission_check_name = if is_composio_native(server_name) {
            COMPOSIO_NATIVE_PREFIX
        } else {
            server_name
        };

        // Permission Check
        if !bypass_permission_check && !client.config.always_allow.contains(&tool_name.to_string())
        {
            // Trust rules run BEFORE the permission check: a user-authored
            // rule (matched against the RAW server name the bubble saw)
            // skips the prompt this branch would otherwise raise — and only
            // that. Nothing here touches always_allow or bypass paths.
            let trust_hit: Option<String> = {
                let settings = self.settings.peek();
                crate::context::trust::evaluate(
                    &settings.permission_settings.trust_rules,
                    server_name,
                    tool_name,
                    &args,
                    &crate::context::trust::TrustCtx {
                        project_id: session_ctx.as_ref().and_then(|c| c.project_id.as_deref()),
                    },
                )
                .map(|rule| rule.id.clone())
            };
            if let Some(rule_id) = trust_hit {
                crate::context::trust_store::log_decision(
                    crate::context::trust_store::TrustDecision::new(
                        crate::context::trust_store::DecisionKind::RuleHit,
                        server_name,
                        tool_name,
                        crate::context::trust_store::arg_summary(&args),
                        session_ctx.as_ref().map(|c| c.session_id.clone()),
                        session_ctx.as_ref().and_then(|c| c.project_id.clone()),
                        Some(rule_id),
                    ),
                );
                // Fall through as allowed.
            } else {
                let pm = self.permission_manager.read();
                match pm.check_mcp_permission(permission_check_name) {
                    PermissionStatus::Allowed => {}
                    PermissionStatus::RequiresPrompt => {
                        let tool_call = crate::components::shared::ToolCall::new(
                            server_name.to_string(),
                            tool_name.to_string(),
                            args,
                            None,
                            None, // thought_summary not available in permission check context
                        );
                        return Err(serde_json::to_string(&tool_call).unwrap_or_default());
                    }
                    PermissionStatus::Denied(reason) => {
                        return Err(format!("Tool use denied: {}", reason));
                    }
                }
            }
        }

        let tool = match client.tools.iter().find(|t| t.name == tool_name) {
            Some(t) => t.clone(),
            None => {
                // Fallback 1: check the dynamic local tools cache.
                // Dynamically loaded on-demand server tools live here.
                let dynamic_local_cache = self.dynamic_local_tools.lock().await;
                if let Some(t) = dynamic_local_cache.iter().find(|t| t.name == tool_name) {
                    t.clone()
                } else {
                    // Fallback 2: check the dynamic Composio tools cache.
                    // Dynamically discovered tools (via COMPOSIO_GET_APP_TOOLS) live here,
                    // not in any ActiveMcpClient.tools list. Prefer this client's
                    // profile bucket; scan the rest only defensively (the Tool is
                    // metadata — execution still routes through the resolved client).
                    let dynamic_cache = self.dynamic_composio_tools.lock().await;
                    // Key by the turn's profile_id — the same key get_mcp_context
                    // used when it surfaced this tool to the AI.
                    let key = dyn_composio_key(profile_id.as_deref());
                    let found = dynamic_cache
                        .get(&key)
                        .and_then(|v| v.iter().find(|t| t.name == tool_name))
                        .or_else(|| {
                            dynamic_cache
                                .values()
                                .flatten()
                                .find(|t| t.name == tool_name)
                        });
                    match found {
                        Some(t) => t.clone(),
                        None => return Err(format!("Tool not found: {}", tool_name)),
                    }
                }
            }
        };

        // BYOA FIX: Re-inject custom credentials before tool execution (Pattern 25)
        // This ensures any credentials updated via the UI are used immediately.
        if let McpClientType::NativeComposio(ref composio_client) = client.service {
            // Get profile name for scoped injection
            // Pattern 123: Use resolved profile name strictly
            let resolved_profile = if actual_server_name.starts_with("composio-native:") {
                actual_server_name
                    .strip_prefix("composio-native:")
                    .map(|s| s.to_string())
            } else {
                profile_id.clone()
            };

            if let Some(p_name) = resolved_profile {
                Self::inject_custom_credentials(composio_client, &p_name, &self.secret_manager);
            } else {
                // Fallback to all if no profile known
                let all_creds = self.secret_manager.peek().get_all_custom_tool_credentials();
                composio_client.set_custom_creds(all_creds);
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let service = client.service.clone();
        let dynamic_tools_cache = self.dynamic_composio_tools.clone();
        // Profile bucket for on-demand tools discovered/cleared in this turn.
        // Keyed by the turn's profile_id so it matches get_mcp_context's lookup.
        let dyn_profile_key = dyn_composio_key(profile_id.as_deref());
        // Capture the set of tool names currently loaded on this server.
        // Used to filter dynamic injection: only tools the proxy will accept
        // for tools/call should be injected as FunctionDeclarations.
        let loaded_tool_names: std::collections::HashSet<String> =
            client.tools.iter().map(|t| t.name.to_string()).collect();

        // Note: composio_meta synthetic server removed - Tool Router handles on-demand tools

        // Pattern 30: Pre-read API key for native clients before the spawn boundary.
        // This avoids borrowing `self` inside the async move block while still
        // reading the latest key value at call time (not at launch time).
        let native_image_api_key = self.secret_manager.peek().get("gemini_api_key").cloned();
        // Pattern 30: captured before the executor spawn (no self inside).
        let terminal_ctx = session_ctx.clone();
        // Pattern 30: Also read image model fresh from settings at call time
        let native_image_model = self.settings.peek().image_generation_config.model.clone();

        // Capture reconnect info for potential retry inside the spawn block.
        // We need the server name and config in case call_tool returns TransportClosed.
        let reconnect_server_name = actual_server_name.clone();
        let reconnect_config = client.config.clone();
        let reconnect_manager = self.clone();

        spawn(async move {
            let result = match service {
                McpClientType::Service(service_arc) => {
                    let arguments = if let serde_json::Value::Object(map) = args {
                        map
                    } else {
                        return if tx
                            .send(Err("Tool arguments must be a JSON object".to_string()))
                            .is_err()
                        {
                            tracing::error!("StreamManager receiver dropped");
                        };
                    };
                    let request = CallToolRequestParam {
                        name: tool.name.clone(),
                        arguments: Some(arguments.clone()),
                    };
                    match service_arc.call_tool(request).await {
                        Ok(result) => Ok(result),
                        Err(e) => {
                            let error_str = format!("{}", e);
                            // AUTO-RECONNECT: If the transport closed (child process died),
                            // attempt to restart the server and retry the tool call once.
                            if error_str.contains("Transport closed") {
                                tracing::warn!(
                                    "[RECONNECT] call_tool returned TransportClosed for '{}' — attempting reconnect + retry",
                                    reconnect_server_name
                                );
                                match reconnect_manager
                                    .reconnect_stdio_server(
                                        &reconnect_server_name,
                                        reconnect_config,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        // Retry once with the fresh service
                                        let servers = reconnect_manager.servers.lock().await;
                                        if let Some(fresh_client) =
                                            servers.get(&reconnect_server_name)
                                        {
                                            if let McpClientType::Service(ref fresh_svc) =
                                                fresh_client.service
                                            {
                                                let retry_request = CallToolRequestParam {
                                                    name: tool.name.clone(),
                                                    arguments: Some(arguments),
                                                };
                                                match fresh_svc.call_tool(retry_request).await {
                                                    Ok(result) => {
                                                        tracing::info!(
                                                            "[RECONNECT] Retry succeeded for '{}' on server '{}'",
                                                            tool.name,
                                                            reconnect_server_name
                                                        );
                                                        Ok(result)
                                                    }
                                                    Err(retry_e) => Err(format!(
                                                        "Failed to use tool after reconnect: {}",
                                                        retry_e
                                                    )),
                                                }
                                            } else {
                                                Err(format!(
                                                    "Server '{}' is not a stdio service after reconnect",
                                                    reconnect_server_name
                                                ))
                                            }
                                        } else {
                                            Err(format!(
                                                "Server '{}' not found after reconnect",
                                                reconnect_server_name
                                            ))
                                        }
                                    }
                                    Err(reconnect_err) => Err(format!(
                                        "Failed to use tool (transport closed) and reconnect failed: {}",
                                        reconnect_err
                                    )),
                                }
                            } else {
                                Err(format!("Failed to use tool: {}", e))
                            }
                        }
                    }
                }
                McpClientType::NativeImage(image_client) => {
                    // Pattern 30: Both model and key are read fresh at call time
                    match image_client
                        .execute_tool(
                            &tool.name,
                            args.clone(),
                            &native_image_model,
                            native_image_api_key.as_deref(),
                        )
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(e) => Err(e),
                    }
                }
                McpClientType::NativeTerminal(terminal_client) => {
                    // A real executor: the permission gate already ran above.
                    terminal_client
                        .execute_tool(&tool.name, args.clone(), terminal_ctx.clone())
                        .await
                }
                McpClientType::NativeCore => {
                    // HOBBES_PAGE_RESULT and HOBBES_UPDATE_SCRATCHPAD are intercepted
                    // upstream in stream_manager.rs before use_mcp_tool() is called.
                    // If we land here it means a dispatch path was missed.
                    Err(format!(
                        "Built-in core tool '{}' was not intercepted before MCP dispatch. \
                        This is a Hobbes bug — please report it.",
                        tool.name
                    ))
                }
                McpClientType::NativePlanner => {
                    // Planner tools are intercepted upstream in
                    // components::builtin_tools before use_mcp_tool() is called.
                    // If we land here it means a dispatch path was missed.
                    Err(format!(
                        "Built-in planner tool '{}' was not intercepted before MCP dispatch. \
                        This is a Hobbes bug — please report it.",
                        tool.name
                    ))
                }
                McpClientType::NativeMeta => {
                    // MCP_LOAD_SERVER_TOOLS and MCP_UNLOAD_SERVER_TOOLS are intercepted
                    // at the top of use_mcp_tool() before reaching this match arm.
                    // If we land here it means a dispatch path was missed.
                    Err(format!(
                        "Built-in meta tool '{}' was not intercepted before MCP dispatch. \
                        This is a Hobbes bug — please report it.",
                        tool.name
                    ))
                }
                McpClientType::NativeComposio(composio_client) => {
                    // Tool Router: Handle meta-tools for on-demand discovery
                    if tool.name == "COMPOSIO_DISCOVER_APPS" {
                        // Extract query from args
                        let query = args.get("query").and_then(|v| v.as_str());
                        tracing::debug!("COMPOSIO_DISCOVER_APPS: query='{:?}'", query);

                        // Operational Authority: Use list_connected_toolkits (MCP-first,
                        // cross-referenced with user-scoped toolkit_account_map) rather than
                        // the unfiltered REST marketplace catalog. Only shows apps the user
                        // has connected accounts for.
                        match composio_client.list_connected_toolkits().await {
                            Ok(toolkit_infos) => {
                                // Apply optional query filter client-side
                                let filtered: Vec<_> = if let Some(q) = query {
                                    let q_lower = q.to_lowercase();
                                    toolkit_infos
                                        .into_iter()
                                        .filter(|tk| {
                                            tk.slug.to_lowercase().contains(&q_lower)
                                                || tk.display_name.to_lowercase().contains(&q_lower)
                                        })
                                        .collect()
                                } else {
                                    toolkit_infos
                                };

                                let results: Vec<serde_json::Value> = filtered
                                    .iter()
                                    .map(|tk| {
                                        serde_json::json!({
                                            "name": tk.display_name,
                                            "slug": tk.slug,
                                            "tool_count": tk.tool_count,
                                            "is_connected": tk.is_connected
                                        })
                                    })
                                    .collect();

                                let content_text = serde_json::to_string_pretty(&serde_json::json!({
                                    "apps_found": results.len(),
                                    "apps": results,
                                    "hint": "To use tools from an app, call COMPOSIO_GET_APP_TOOLS with the app's name or slug."
                                })).unwrap_or_default();

                                Ok(CallToolResult {
                                    content: vec![rmcp::model::Content::text(content_text)],
                                    is_error: Some(false),
                                    structured_content: None,
                                    meta: None,
                                })
                            }
                            Err(e) => Err(format!("App discovery failed: {}", e)),
                        }
                    } else if tool.name == "COMPOSIO_GET_APP_TOOLS" {
                        let app_name = args.get("app_name").and_then(|v| v.as_str()).unwrap_or("");
                        tracing::info!("COMPOSIO_GET_APP_TOOLS: app_name='{}'", app_name);

                        if app_name.is_empty() {
                            Err("Missing 'app_name' argument".to_string())
                        } else {
                            // We use list_tools_filtered with the app name/slug
                            match composio_client
                                .list_tools_filtered(Some(&[app_name.to_string()]))
                                .await
                            {
                                Ok(discovery_result) => {
                                    let total_available = discovery_result.tools.len();

                                    // Identify tools already loaded on the server (force-loaded)
                                    let already_loaded: Vec<_> = discovery_result
                                        .tools
                                        .iter()
                                        .filter(|t| loaded_tool_names.contains(&t.name))
                                        .collect();

                                    // Deduplicate: skip tools already loaded on the server
                                    let new_tools: Vec<_> = discovery_result
                                        .tools
                                        .iter()
                                        .filter(|t| !loaded_tool_names.contains(&t.name))
                                        .collect();

                                    // Budget-aware selection: only inject as many as Gemini can handle
                                    let budget =
                                        GEMINI_TOOL_LIMIT.saturating_sub(loaded_tool_names.len());
                                    let budget_limited = new_tools.len() > budget;

                                    let selected: Vec<_> = if budget_limited {
                                        // Score and sort by relevance, take top N within budget
                                        let mut scored: Vec<_> = new_tools
                                            .iter()
                                            .map(|t| (score_tool_relevance(&t.name), *t))
                                            .collect();
                                        scored.sort_by(|a, b| b.0.cmp(&a.0));
                                        scored.into_iter().take(budget).map(|(_, t)| t).collect()
                                    } else {
                                        new_tools
                                    };

                                    // Inject selected tools into the dynamic cache
                                    let rmcp_tools: Vec<rmcp::model::Tool> =
                                        selected.iter().map(|t| composio_to_rmcp_tool(t)).collect();
                                    let injected_count = rmcp_tools.len();
                                    {
                                        let mut cache = dynamic_tools_cache.lock().await;
                                        let bucket =
                                            cache.entry(dyn_profile_key.clone()).or_default();
                                        let new_names: std::collections::HashSet<String> =
                                            rmcp_tools.iter().map(|t| t.name.to_string()).collect();
                                        bucket.retain(|t| !new_names.contains(&t.name.to_string()));
                                        bucket.extend(rmcp_tools);
                                        tracing::info!(
                                            "Injected {} dynamic tools for '{}' under profile '{}' (from {} available, budget: {}, bucket: {})",
                                            injected_count, app_name, dyn_profile_key, total_available, budget, bucket.len()
                                        );
                                    }

                                    // Populate tool→toolkit mapping for ALL discovered tools
                                    // (even non-injected ones, so COMPOSIO_EXECUTE_TOOL can resolve context)
                                    if let Ok(mut map) = composio_client.tool_toolkit_map.write() {
                                        for t in &discovery_result.tools {
                                            map.insert(t.name.clone(), app_name.to_string());
                                        }
                                    }

                                    // Build response: include BOTH newly injected AND already-loaded tools
                                    // so the AI knows what it can call directly.
                                    let tool_results: Vec<serde_json::Value> = selected
                                        .iter()
                                        .map(|t| {
                                            serde_json::json!({
                                                "name": t.name,
                                                "description": t.description,
                                            })
                                        })
                                        .collect();

                                    let already_loaded_results: Vec<serde_json::Value> =
                                        already_loaded
                                            .iter()
                                            .map(|t| {
                                                serde_json::json!({
                                                    "name": t.name,
                                                    "description": t.description,
                                                })
                                            })
                                            .collect();
                                    let already_loaded_count = already_loaded_results.len();

                                    let hint = if !already_loaded.is_empty() && selected.is_empty()
                                    {
                                        // All tools already force-loaded — tell the AI exactly what it has
                                        format!(
                                            "All {} tools are already loaded as native function calls. Call them directly by name — they are listed in 'already_loaded' below.",
                                            already_loaded_count
                                        )
                                    } else if budget_limited {
                                        format!("{} most relevant tools injected (budget: {} max). Use COMPOSIO_EXECUTE_TOOL for any tool not listed.", injected_count, GEMINI_TOOL_LIMIT)
                                    } else {
                                        "All tools are now available as native function calls. Call them directly by name.".to_string()
                                    };

                                    let mut response_json = serde_json::json!({
                                        "app": app_name,
                                        "injected_count": injected_count,
                                        "total_available": total_available,
                                        "budget_limited": budget_limited,
                                        "tools": tool_results,
                                        "hint": hint,
                                    });

                                    // Include already-loaded tool names so the AI knows what it can call
                                    if !already_loaded_results.is_empty() {
                                        if let Some(obj) = response_json.as_object_mut() {
                                            obj.insert(
                                                "already_loaded_count".to_string(),
                                                serde_json::json!(already_loaded_count),
                                            );
                                            obj.insert(
                                                "already_loaded".to_string(),
                                                serde_json::json!(already_loaded_results),
                                            );
                                        }
                                    }

                                    let content_text = serde_json::to_string_pretty(&response_json).unwrap_or_else(|e| {
                                        tracing::error!("Failed to serialize tool discovery response: {}", e);
                                        // Hand-crafted JSON is intentional here: the primary serde_json
                                        // serializer just failed, and this output is consumed as Content::text
                                        // by the AI — not parsed as JSON by any downstream consumer.
                                        format!("{{\"error\": \"Failed to format tool list for '{}'\"}}", app_name)
                                    });

                                    Ok(CallToolResult {
                                        content: vec![rmcp::model::Content::text(content_text)],
                                        is_error: Some(false),
                                        structured_content: None,
                                        meta: None,
                                    })
                                }
                                Err(e) => Err(format!(
                                    "Failed to get tools for app '{}': {}",
                                    app_name, e
                                )),
                            }
                        }
                    } else if tool.name == "COMPOSIO_CLEAR_TOOLS" {
                        // Clear only this profile's dynamically discovered tools,
                        // so one tab clearing its toolset doesn't wipe another
                        // profile's discovered tools.
                        let mut cache = dynamic_tools_cache.lock().await;
                        let cleared_count =
                            cache.remove(&dyn_profile_key).map(|v| v.len()).unwrap_or(0);
                        tracing::info!(
                            "COMPOSIO_CLEAR_TOOLS: cleared {} dynamic tools for profile '{}'",
                            cleared_count,
                            dyn_profile_key
                        );

                        Ok(CallToolResult {
                            content: vec![rmcp::model::Content::text(format!(
                                "Cleared {} dynamically discovered tools from the session.",
                                cleared_count
                            ))],
                            is_error: Some(false),
                            structured_content: None,
                            meta: None,
                        })
                    } else if tool.name == "COMPOSIO_EXECUTE_TOOL" {
                        // Extract tool_name and arguments - handle missing tool_name as error
                        match args.get("tool_name").and_then(|v| v.as_str()) {
                            Some(target_tool_name) => {
                                let tool_args = args
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                tracing::debug!(
                                    "COMPOSIO_EXECUTE_TOOL: executing '{}'",
                                    target_tool_name
                                );

                                // Execute the target tool
                                match composio_client
                                    .execute_tool(target_tool_name, tool_args)
                                    .await
                                {
                                    Ok(response) => {
                                        // Check for auth failure (401/403) and attempt auto-reconnection + retry
                                        if let Some(auth_result) = try_auth_recovery(
                                            &response,
                                            target_tool_name,
                                            &composio_client,
                                        )
                                        .await
                                        {
                                            let _ = tx.send(Ok(auth_result));
                                            return;
                                        }

                                        let content_text = if response.successful {
                                            serde_json::to_string_pretty(&response.data)
                                                .unwrap_or_else(|_| {
                                                    "{\"error\": \"Failed to serialize response\"}"
                                                        .to_string()
                                                })
                                        } else {
                                            response
                                                .error
                                                .unwrap_or_else(|| "Unknown error".to_string())
                                        };

                                        Ok(CallToolResult {
                                            content: vec![rmcp::model::Content::text(content_text)],
                                            is_error: Some(!response.successful),
                                            structured_content: Some(response.data),
                                            meta: None,
                                        })
                                    }
                                    Err(e) => Err(format!("Tool execution failed: {}", e)),
                                }
                            }
                            None => Err("Missing 'tool_name' argument".to_string()),
                        }
                    } else {
                        // Regular Composio tool execution
                        match composio_client.execute_tool(&tool.name, args).await {
                            Ok(response) => {
                                // Check for auth failure (401/403) and attempt auto-reconnection + retry
                                if let Some(auth_result) =
                                    try_auth_recovery(&response, &tool.name, &composio_client).await
                                {
                                    let _ = tx.send(Ok(auth_result));
                                    return;
                                }

                                // Convert the response to a proper CallToolResult
                                let content_text = if response.successful {
                                    serde_json::to_string_pretty(&response.data).unwrap_or_else(
                                        |_| {
                                            "{\"error\": \"Failed to serialize response data\"}"
                                                .to_string()
                                        },
                                    )
                                } else {
                                    response
                                        .error
                                        .unwrap_or_else(|| "Unknown error".to_string())
                                };

                                let content = rmcp::model::Content::text(content_text);

                                // Create metadata with log_id and session_info if available
                                let mut meta_map = serde_json::Map::new();
                                if let Some(log_id) = response.log_id {
                                    meta_map.insert(
                                        "log_id".to_string(),
                                        serde_json::Value::String(log_id),
                                    );
                                }
                                if let Some(session_info) = response.session_info {
                                    meta_map.insert("session_info".to_string(), session_info);
                                }

                                // Create metadata with log_id and session_info if available
                                let meta = if !meta_map.is_empty() {
                                    // Convert our map to a HashMap<String, String> for Meta
                                    let mut string_map = std::collections::HashMap::new();
                                    for (key, value) in meta_map {
                                        let string_value = match value {
                                            serde_json::Value::String(s) => s,
                                            _ => value.to_string(),
                                        };
                                        string_map.insert(key, string_value);
                                    }

                                    // Create a Meta object from our HashMap
                                    let mut meta_obj = rmcp::model::Meta::new();
                                    for (key, value) in string_map {
                                        meta_obj.insert(key, serde_json::Value::String(value));
                                    }
                                    Some(meta_obj)
                                } else {
                                    None
                                };

                                Ok(CallToolResult {
                                    content: vec![content],
                                    is_error: Some(!response.successful),
                                    structured_content: Some(response.data),
                                    meta,
                                })
                            }
                            Err(e) => Err(format!("Composio tool execution failed: {}", e)),
                        }
                    }
                }
            };

            if tx.send(result).is_err() {
                tracing::error!("StreamManager receiver dropped for tool result.");
            }
        });

        Ok(rx)
    }

    /// Helper to process the output stream from a tool call into a final status and response string.
    /// Returns (Status, ResponseString, IsPermissionRequest)
    pub async fn process_tool_output(
        mut receiver: UnboundedReceiver<Result<CallToolResult, String>>,
    ) -> (ToolCallStatus, String, bool) {
        let mut aggregated_content: Vec<rmcp::model::Content> = Vec::new();
        let mut final_status = ToolCallStatus::Completed;
        let mut error_string = None;

        while let Some(result) = receiver.recv().await {
            match result {
                Ok(call_tool_result) => {
                    aggregated_content.extend(call_tool_result.content);
                }
                Err(e) => {
                    final_status = ToolCallStatus::Error;
                    error_string = Some(e);
                    break;
                }
            }
        }

        if final_status == ToolCallStatus::Error {
            let err = error_string.unwrap_or_default();
            // Check if this error is actually a serialized ToolCall indicating a permission request
            if let Ok(_tc) = serde_json::from_str::<crate::components::shared::ToolCall>(&err) {
                (ToolCallStatus::Error, err, true)
            } else {
                (final_status, err, false)
            }
        } else {
            // Check for auth requirement
            let mut auth_url = None;
            for content in &aggregated_content {
                let json_content = serde_json::to_value(content).unwrap_or(serde_json::Value::Null);
                if let Some(text) = json_content.get("text").and_then(|t| t.as_str()) {
                    if text.contains("Authentication required")
                        && text.contains("connect your account")
                    {
                        if let Some(start) = text.find("http") {
                            auth_url = Some(text[start..].trim().to_string());
                        }
                    }
                }
            }

            if let Some(url) = auth_url {
                (ToolCallStatus::AuthRequired, url, false)
            } else {
                let final_json =
                    serde_json::to_value(aggregated_content).unwrap_or(serde_json::Value::Null);
                (
                    final_status,
                    serde_json::to_string_pretty(&final_json).unwrap_or_default(),
                    false,
                )
            }
        }
    }
}
