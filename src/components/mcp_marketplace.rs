use dioxus::prelude::*;
use futures_util::StreamExt;
use crate::mcp::manager::{McpManager, McpServerStatus, ServerStatus};
use crate::settings::Settings;
use crate::components::mcp_search_form::McpSearchForm;
use crate::components::smithery_client::{SmitheryClient, SmitheryServer};
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use lazy_static::lazy_static;
use syntect::easy::HighlightLines;
use syntect::highlighting::{ThemeSet, Theme};
use syntect::parsing::SyntaxSet;
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};



lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}


#[derive(Clone, PartialEq)]
enum ActiveTab {
    Marketplace,
    Installed,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FeaturedMcp {
    name: String,
    display_name: String,
    description: String,
    command: String,
    args: Vec<String>,
    env_vars: Vec<String>,
    homepage: String,
}

impl From<SmitheryServer> for FeaturedMcp {
    fn from(server: SmitheryServer) -> Self {
        FeaturedMcp {
            name: server.qualified_name.clone(),
            display_name: server.display_name,
            description: server.description,
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@smithery/cli@latest".to_string(), "run".to_string(), server.qualified_name],
            env_vars: vec![], // This will be populated by the detailed fetch
            homepage: server.homepage,
        }
    }
}

fn get_featured_mcps() -> Vec<FeaturedMcp> {
    vec![
        FeaturedMcp {
            name: "filesystem".to_string(),
            display_name: "Filesystem".to_string(),
            description: "Give the AI access to read and write files in a specific directory.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string(), "/path/to/allowed/dir".to_string()],
            env_vars: vec![],
            homepage: "".to_string(),
        },
        FeaturedMcp {
            name: "brave-search".to_string(),
            display_name: "Brave Search".to_string(),
            description: "Allow the AI to search the web using Brave Search.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-brave-search".to_string()],
            env_vars: vec!["BRAVE_API_KEY".to_string()],
            homepage: "".to_string(),
        },
        FeaturedMcp {
            name: "github".to_string(),
            display_name: "GitHub".to_string(),
            description: "Interact with GitHub repositories, issues, and pull requests.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-github".to_string()],
            env_vars: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            homepage: "".to_string(),
        },
        FeaturedMcp {
            name: "postgres".to_string(),
            display_name: "PostgreSQL".to_string(),
            description: "Read and write data to a PostgreSQL database.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-postgres".to_string(), "postgresql://user:password@localhost/dbname".to_string()],
            env_vars: vec![],
            homepage: "".to_string(),
        },
        FeaturedMcp {
            name: "google-maps".to_string(),
            display_name: "Google Maps".to_string(),
            description: "Access location data and directions via Google Maps.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-google-maps".to_string()],
            env_vars: vec!["GOOGLE_MAPS_API_KEY".to_string()],
            homepage: "".to_string(),
        },
    ]
}

#[component]
pub fn McpMarketplace() -> Element {
    let mut active_tab = use_signal(|| ActiveTab::Marketplace);
    let search_query = use_signal(|| "".to_string());
    let mut trigger_search = use_signal(|| 0);
    let mut config_content = use_signal(|| "".to_string());
    let mut error_message = use_signal(|| Option::<String>::None);
    let mut success_message = use_signal(|| Option::<String>::None);
    let _mcp_manager = use_context::<Signal<McpManager>>();
    let settings = use_context::<Signal<Settings>>();
    let filter_verified = use_signal(|| true);
    let filter_deployed = use_signal(|| false);
    let sort_by = use_signal(|| "relevance".to_string());
    let _refresh_signal = use_signal(|| 0);
    
    // State for server list
    // State for server list
    let mut current_page = use_signal(|| 1);
    let mut total_pages = use_signal(|| 1);

    // Load config on mount
    use_effect(move || {
        let path = get_mcp_config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => config_content.set(content),
            Err(_) => config_content.set(r#"{ "mcpServers": {} }"#.to_string()),
        }
    });

    let server_resource: Resource<Result<(Vec<FeaturedMcp>, String), String>> = use_resource(move || {
        // This resource will re-run whenever any of these signals change.
        let page = *current_page.read();
        let verified = *filter_verified.read();
        let deployed = *filter_deployed.read();
        let query = search_query.read().clone();
        let sort = sort_by.read().clone();
        // explicit subscription to trigger_search is preserved but redundant if we subscribe to the query itself
        let _ = trigger_search.read();  
            
        async move {
            let api_key = settings.peek().smithery_api_key.clone();

            if let Some(key) = api_key {
                let client = SmitheryClient::new(key);
                let mut filters = Vec::new();
                if verified { filters.push("is:verified"); }
                if deployed { filters.push("is:deployed"); }
                if !query.is_empty() { filters.push(&query); }
                let final_query = filters.join(" ");
                let search_param = if final_query.is_empty() { None } else { Some(final_query.as_str()) };

                match client.fetch_servers(search_param, Some(page), Some(&sort)).await {
                    Ok(response) => {
                        total_pages.set(response.pagination.total_pages);
                        let mcps: Vec<FeaturedMcp> = response.servers.into_iter().map(Into::into).collect();
                        Ok((mcps, "Smithery".to_string()))
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch from Smithery: {}", e);
                        Ok((get_featured_mcps(), "Hardcoded (Smithery error)".to_string()))
                    }
                }
            } else {
                Ok((get_featured_mcps(), "Hardcoded".to_string()))
            }
        }
    });

    let (filtered_mcps, data_source, is_loading) = match &*server_resource.read() {
        Some(Ok((mcps, source))) => (mcps.clone(), source.clone(), false),
        Some(Err(e)) => (vec![], format!("Error: {}", e), false),
        None => (vec![], "Loading...".to_string(), true),
    };

    let save_config_coroutine = use_coroutine(move |mut rx: UnboundedReceiver<String>| {
        let mut config_content = config_content.to_owned();
        let mut success_message = success_message.to_owned();
        let mut error_message = error_message.to_owned();

        async move {
            while let Some(new_content) = rx.next().await {
                let path = get_mcp_config_path();
                
                tracing::info!("Attempting to save MCP config to: {:?}", path);
                
                // Clone new_content for the blocking task
                let content_for_write = new_content.clone();
                let write_result = tokio::task::spawn_blocking(move || {
                    std::fs::write(&path, &content_for_write)
                }).await.unwrap();

                match write_result {
                    Ok(_) => {
                        tracing::info!("Successfully saved MCP config.");
                        config_content.set(new_content);
                        success_message.set(Some("Configuration saved. Restart app to apply changes.".to_string()));
                        error_message.set(None);
                        // TODO: Trigger reload on McpManager if possible
                    }
                    Err(e) => {
                        error_message.set(Some(format!("Failed to save config: {}", e)));
                        tracing::error!("Failed to write MCP config: {}", e);
                    }
                }
            }
        }
    });

    let add_mcp = move |mcp: FeaturedMcp| {
        let current_settings = settings.read().clone();
        let mut error_msg = error_message.clone();
        spawn(async move {
            tracing::info!("Attempting to add MCP server: {}", mcp.name);
            
            // Since we're not an official Smithery client yet, we don't get configs from the API
            // Instead, we construct the config based on the standard Smithery CLI pattern
            // Pattern from smithery.ai website: npx -y @smithery/cli@latest run {server} --key {apikey}
            
            if let Some(api_key) = current_settings.smithery_api_key.clone() {
                let current_config_str = config_content.read().clone();
                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&current_config_str) {
                    if let Some(servers) = json.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
                        // Extract short server name (e.g., "@smithery/googlecalendar" -> "googlecalendar")
                        let server_name = mcp.name.split('/').last().unwrap_or(&mcp.name).replace("-mcp", "");
                        
                        // Use standard Smithery CLI pattern
                        let new_server = serde_json::json!({
                            "command": "npx",
                            "args": [
                                "-y",
                                "@smithery/cli@latest",
                                "run",
                                server_name,
                                "--key",
                                api_key
                            ],
                            "description": mcp.description
                        });
                        
                        servers.insert(server_name.clone(), new_server);
                        if let Ok(new_content) = serde_json::to_string_pretty(&json) {
                            save_config_coroutine.send(new_content);
                        }
                        active_tab.set(ActiveTab::Installed);
                    } else {
                        tracing::error!("'mcpServers' key missing or invalid in config");
                        error_msg.set(Some("Invalid MCP config structure.".to_string()));
                    }
                } else {
                    tracing::error!("Failed to parse current config JSON");
                    error_msg.set(Some("Failed to parse current configuration.".to_string()));
                }
            } else {
                tracing::warn!("No Smithery API key configured");
                error_msg.set(Some("Please set your Smithery API key in Settings first".to_string()));
            }
        });
    };

    rsx! {
        div {
            class: "flex flex-col h-full bg-dark-bg text-white",
            div {
                class: "p-4 border-b border-gray-700",
                div {
                    class: "flex items-center justify-between mb-2",
                    h2 { class: "text-xl font-bold", "MCP Servers" }
                    div {
                        class: "text-xs px-2 py-1 rounded bg-gray-700 text-gray-300",
                        if is_loading {
                            "Loading..."
                        } else {
                            "Source: {data_source}"
                        }
                    }
                }
                div {
                    class: "flex space-x-4",
                    button {
                        class: if *active_tab.read() == ActiveTab::Marketplace {
                            "px-3 py-1 text-sm font-medium text-primary-400 border-b-2 border-primary-400"
                        } else {
                            "px-3 py-1 text-sm font-medium text-gray-400 hover:text-white"
                        },
                        onclick: move |_| active_tab.set(ActiveTab::Marketplace),
                        "Marketplace"
                    }
                    button {
                        class: if *active_tab.read() == ActiveTab::Installed {
                            "px-3 py-1 text-sm font-medium text-primary-400 border-b-2 border-primary-400"
                        } else {
                            "px-3 py-1 text-sm font-medium text-gray-400 hover:text-white"
                        },
                        onclick: move |_| active_tab.set(ActiveTab::Installed),
                        "Installed / Config"
                    }
                    button {
                        class: if *active_tab.read() == ActiveTab::Status {
                            "px-3 py-1 text-sm font-medium text-primary-400 border-b-2 border-primary-400"
                        } else {
                            "px-3 py-1 text-sm font-medium text-gray-400 hover:text-white"
                        },
                        onclick: move |_| active_tab.set(ActiveTab::Status),
                        "Status"
                    }
                }
            }

            div {
                class: "flex-1 overflow-y-auto p-4",
                if let Some(msg) = success_message.read().as_ref() {
                    div { class: "mb-4 p-2 bg-green-900 text-green-200 rounded text-sm", "{msg}" }
                }
                if let Some(msg) = error_message.read().as_ref() {
                    div { class: "mb-4 p-2 bg-red-900 text-red-200 rounded text-sm", "{msg}" }
                }

                match *active_tab.read() {
                    ActiveTab::Marketplace => rsx! {
                        McpSearchForm {
                            search_query: search_query,
                            trigger_search: trigger_search,
                            filter_verified: filter_verified,
                            filter_deployed: filter_deployed,
                            sort_by: sort_by
                        }
                        div {
                            class: "grid grid-cols-1 gap-4",
                            for mcp in filtered_mcps {
                                McpServerCard {
                                    mcp: mcp.clone(),
                                    add_mcp: move |m| add_mcp(m),
                                    trigger_search: trigger_search.clone()
                                }
                            }
                        }
                        div {
                            class: "flex justify-between items-center mt-4",
                            button {
                                class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors disabled:bg-gray-600 disabled:cursor-not-allowed",
                                disabled: *current_page.read() <= 1,
                                onclick: move |_| {
                                    if *current_page.read() > 1 {
                                        let page = *current_page.read();
                                        current_page.set(page - 1);
                                        let current_trigger = *trigger_search.read();
                                        trigger_search.set(current_trigger + 1);
                                    }
                                },
                                "Previous"
                            }
                            span {
                                class: "text-sm text-gray-400",
                                "Page {current_page} of {total_pages}"
                            }
                            button {
                                class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors disabled:bg-gray-600 disabled:cursor-not-allowed",
                                disabled: *current_page.read() >= *total_pages.read(),
                                onclick: move |_| {
                                    if *current_page.read() < *total_pages.read() {
                                        let page = *current_page.read();
                                        current_page.set(page + 1);
                                        let current_trigger = *trigger_search.read();
                                        trigger_search.set(current_trigger + 1);
                                    }
                                },
                                "Next"
                            }
                        }
                    },
                    ActiveTab::Installed => rsx! {
                        div {
                            class: "h-full flex flex-col",
                            p { class: "text-sm text-gray-400 mb-2", "Directly edit the JSON configuration for your MCP servers." }
                            
                            // Syntax highlighted editor
                            div {
                                class: "flex-1 relative bg-dark-section rounded-md border border-gray-700",
                                id: "json-editor-container",
                                
                                // Highlighted background layer
                                pre {
                                    class: "absolute inset-0 p-4 text-sm font-mono pointer-events-none whitespace-pre-wrap break-words overflow-auto",
                                    id: "json-highlight",
                                    code {
                                        dangerous_inner_html: "{highlight_json(config_content.read().clone())}"
                                    }
                                }
                                
                                // Editable overlay with scroll
                                textarea {
                                    class: "absolute inset-0 w-full h-full p-4 bg-transparent font-mono text-sm text-transparent caret-white border-0 focus:outline-none resize-none overflow-auto whitespace-pre-wrap break-words",
                                    id: "json-editor",
                                    style: "color: transparent;",
                                    value: "{config_content}",
                                    spellcheck: false,
                                    oninput: move |e| {
                                        config_content.set(e.value());
                                        success_message.set(None);
                                        error_message.set(None);
                                    },
                                    onscroll: move |_| {
                                        let _ = document::eval(r#"
                                            const editor = document.getElementById('json-editor');
                                            const highlight = document.getElementById('json-highlight');
                                            if (editor && highlight) {
                                                highlight.scrollTop = editor.scrollTop;
                                                highlight.scrollLeft = editor.scrollLeft;
                                            }
                                        "#);
                                    },
                                }
                            }
                            
                            div {
                                class: "mt-4 flex justify-end",
                                button {
                                    class: "px-4 py-2 bg-primary-600 hover:bg-primary-500 rounded font-medium transition-colors",
                                    onclick: move |_| {
                                        let content_to_save = config_content.read().clone();
                                        // Validate JSON before sending to coroutine
                                        match serde_json::from_str::<serde_json::Value>(&content_to_save) {
                                            Ok(_) => {
                                                save_config_coroutine.send(content_to_save);
                                                error_message.set(None);
                                            }
                                            Err(e) => {
                                                error_message.set(Some(format!("Invalid JSON: {}", e)));
                                            }
                                        }
                                    },
                                    "Save Configuration"
                                }
                            }
                        }
                    },
                    ActiveTab::Status => rsx! {
                        StatusView {}
                    }
                }
            }
        }
    }
}

#[component]
fn StatusView() -> Element {
    let mcp_manager = use_context::<Signal<McpManager>>();
    
    let mut server_statuses = use_resource(move || {
        let mcp_manager = mcp_manager.clone();
        async move {
            mcp_manager.read().get_all_server_statuses().await
        }
    });

    let refresh_statuses = move |_| {
        server_statuses.restart();
    };

    rsx! {
        div {
            class: "space-y-4",
            div {
                class: "flex items-center justify-between mb-4",
                p { class: "text-sm text-gray-400", "Status of all configured MCP servers." }
                button {
                    class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors",
                    onclick: refresh_statuses,
                    "Refresh"
                }
            }
            
            match server_statuses.read().as_ref() {
                Some(statuses) => rsx! {
                    if statuses.is_empty() {
                        div {
                            class: "text-center text-gray-500 py-8",
                            "No MCP servers configured. Add servers from the Marketplace tab."
                        }
                    } else {
                        div {
                            class: "grid grid-cols-1 gap-3",
                            for status in statuses {
                                StatusCard { status: status.clone() }
                            }
                        }
                    }
                },
                None => rsx! {
                    div { class: "text-center text-gray-500 py-8", "Loading server status..." }
                }
            }
        }
    }
}

#[component]
fn StatusCard(status: McpServerStatus) -> Element {
    let mcp_manager = use_context::<Signal<McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let settings = use_context::<Signal<Settings>>();
    let mut is_retrying = use_signal(|| false);
    
    // Log errors to console for debugging
    let status_for_logging = status.clone();
    use_effect(move || {
        if let Some(ref error) = status_for_logging.error_message {
            tracing::error!("[MCP {}] {}", status_for_logging.name, error);
        }
    });
    
    let (status_color, status_text, status_bg) = match status.status {
        ServerStatus::Loaded => ("bg-green-500", "Loaded", "bg-green-900/20"),
        ServerStatus::Error => ("bg-red-500", "Error", "bg-red-900/20"),
        ServerStatus::Disabled => ("bg-gray-500", "Disabled", "bg-gray-900/20"),
        ServerStatus::NeedsAuth => ("bg-yellow-500", "Needs Auth", "bg-yellow-900/20"),
    };

    let status_clone = status.clone();
    let retry_server = move |_| {
        let server_name = status_clone.name.clone();
        let mcp_manager = mcp_manager.clone();
        let mcp_context = mcp_context.clone();
        let settings = settings.clone();
        is_retrying.set(true);
        
        spawn(async move {
            tracing::info!("Retrying server: {}", server_name);
            let result = mcp_manager.read().retry_server(&server_name, mcp_context, settings.read().clone(), None).await;
            match result {
                Ok(_) => tracing::info!("Retry initiated for {}", server_name),
                Err(e) => tracing::error!("Failed to retry {}: {}", server_name, e),
            }
        });
    };

    // OAuth authentication is now handled by Smithery CLI automatically
    // No manual authentication needed - just ensure SMITHERY_API_KEY is set in env
    
    // Determine if this server needs OAuth authorization
    // More deterministic checks:
    // 1. NeedsAuth status (set explicitly via is_auth_error detection)
    // 2. Error message contains 401/invalid_token (HTTP status based)
    // 3. Server is using Smithery CLI (indicates Smithery-hosted server)
    let is_smithery_hosted = status.uri.as_ref().map(|u| u.contains("smithery.ai")).unwrap_or(false) ||
        status.name.contains("google") || status.name.contains("calendar") || 
        status.name.contains("drive") || status.name.contains("gmail");
    
    let has_auth_error = status.error_message.as_ref().map(|e| {
        // Check for HTTP 401 status or known auth error patterns
        e.contains("401") || e.contains("invalid_token") || e.contains("unauthorized")
    }).unwrap_or(false);
    
    let needs_oauth = status.status == ServerStatus::NeedsAuth || 
       (status.status == ServerStatus::Error && (has_auth_error || is_smithery_hosted));

    rsx! {
        div {
            class: "bg-dark-section p-4 rounded-lg border border-gray-700 {status_bg}",
            div {
                class: "flex items-start justify-between",
                div {
                    class: "flex items-center gap-x-3 flex-1",
                    span { class: "h-3 w-3 rounded-full {status_color}" },
                    div {
                        class: "flex-1",
                        div {
                            class: "flex items-center gap-x-2",
                            h3 { class: "font-bold text-base", "{status.name}" }
                            span {
                                class: "text-xs px-2 py-0.5 rounded {status_color} bg-opacity-20 text-white",
                                "{status_text}"
                            }
                        }
                        if !status.description.is_empty() {
                            p { class: "text-sm text-gray-400 mt-1", "{status.description}" }
                        }
                        if let Some(ref error) = status.error_message {
                            div {
                                class: "mt-2 p-2 bg-red-900/20 rounded border border-red-800",
                                p { 
                                    class: "text-sm text-red-300 font-mono whitespace-pre-wrap break-all", 
                                    "Error: {error}" 
                                }
                            }
                        }
                    }
                }

                // Show Retry button for Error status
                if status.status == ServerStatus::Error {
                    div {
                        class: "mt-4 flex justify-end gap-2",
                        button {
                            class: if *is_retrying.read() {
                                "px-3 py-1 bg-gray-600 rounded text-sm font-medium cursor-not-allowed"
                            } else {
                                "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors"
                            },
                            disabled: *is_retrying.read(),
                            onclick: retry_server,
                            if *is_retrying.read() {
                                "Retrying..."
                            } else {
                                "Retry"
                            }
                        }
                    }
                }

                // Show Authorize button if OAuth is needed
                if needs_oauth {
                    div {
                        class: "mt-4 flex justify-end gap-2",
                        button {
                            class: "px-3 py-1 bg-yellow-600 hover:bg-yellow-500 rounded text-sm font-medium transition-colors flex items-center gap-2",
                            onclick: {
                                let server_name = status.name.clone();
                                let mcp_manager = mcp_manager.clone();
                                let mcp_context = mcp_context.clone();
                                let settings = settings.clone();
                                move |_| {
                                    let server_name = server_name.clone();
                                    let mcp_manager = mcp_manager.clone();
                                    let mcp_context = mcp_context.clone();
                                    let settings = settings.clone();
                                    
                                    spawn(async move {
                                        use crate::mcp::smithery_client::{SmitheryOAuthClient, SmitheryOAuthConfig, SmitheryOAuthError};
                                        
                                        tracing::info!("Starting OAuth flow for: {}", server_name);
                                        
                                        // Create OAuth client
                                        let server_url = format!("https://server.smithery.ai/{}/mcp", server_name);
                                        let config = SmitheryOAuthConfig::new(&server_url);
                                        let client = SmitheryOAuthClient::new(config);
                                        
                                        // Start callback server to receive auth code
                                        let mut callback_rx = client.start_callback_server();
                                        
                                        // Try to connect - this triggers OAuth discovery
                                        match client.connect().await {
                                            Ok(()) => {
                                                tracing::info!("Connected without needing auth!");
                                            }
                                            Err(SmitheryOAuthError::AuthRequired(auth_url)) => {
                                                tracing::info!("Opening OAuth authorization URL: {}", auth_url);
                                                
                                                // Open browser for user to authorize
                                                if let Err(e) = open::that(&auth_url) {
                                                    tracing::error!("Failed to open browser: {}", e);
                                                    return;
                                                }
                                                
                                                // Wait for callback with auth code
                                                tracing::info!("Waiting for OAuth callback...");
                                                if let Some(result) = callback_rx.recv().await {
                                                    if result.success {
                                                        if let Some(code) = result.auth_code {
                                                            tracing::info!("Received auth code, completing OAuth flow...");
                                                            
                                                            // Exchange code for tokens
                                                            match client.finish_auth(&code).await {
                                                                Ok(()) => {
                                                                    tracing::info!("OAuth complete! Retrying server connection...");
                                                                    
                                                                    // Retry the server now that we have auth
                                                                    let _ = mcp_manager.read().retry_server(
                                                                        &server_name, 
                                                                        mcp_context, 
                                                                        settings.read().clone(),
                                                                        client.access_token().await
                                                                    ).await;
                                                                }
                                                                Err(e) => {
                                                                    tracing::error!("Token exchange failed: {}", e);
                                                                }
                                                            }
                                                        }
                                                    } else if let Some(error) = result.error {
                                                        tracing::error!("OAuth failed: {}", error);
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("OAuth connection failed: {}", e);
                                            }
                                        }
                                    });
                                }
                            },
                            "🔐 Authorize"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn McpServerCard(
    mcp: FeaturedMcp,
    add_mcp: EventHandler<FeaturedMcp>,
    trigger_search: Signal<i32>,
) -> Element {
    let mcp_clone_for_add = mcp.clone();
    let mcp_manager = use_context::<Signal<McpManager>>();

    // Check if installed and get status
    let install_status = use_resource({
        let mcp = mcp.clone();
        move || {
            // Depend on the search trigger to force re-evaluation
            let _ = trigger_search.read();
            let mcp_manager = mcp_manager.clone();
            let mcp_name = mcp.name.clone(); // This is the qualified_name
            async move {
                let guard = mcp_manager.read();
                let servers = guard.servers.lock().await;
                let failed = guard.failed_servers.lock().await;

                // Check if any loaded server matches the qualified name.
                // The keys in the McpManager are the simple names (e.g., "google-calendar"),
                // so we compare against the end of the qualified name.
                let server_key = mcp_name.split('/').last().unwrap_or(&mcp_name);

                let is_loaded = servers.keys().any(|key| key == server_key || key == &mcp_name);

                if is_loaded {
                    return (true, false); // (installed, has_error)
                }

                // Check if any failed server matches this qualified name.
                let has_error = failed.keys().any(|key| key == server_key || key == &mcp_name);

                (has_error, has_error) // (installed, has_error)
            }
        }
    });

    let (status_class, _status_text) = match install_status.read().as_ref() {
        Some((true, false)) => ("h-2 w-2 rounded-full bg-green-500", "Loaded"),
        Some((true, true)) => ("h-2 w-2 rounded-full bg-red-500", "Error"),
        _ => ("h-2 w-2 rounded-full bg-gray-500", "Not Installed"),
    };

    rsx! {
        div {
            class: "bg-dark-section p-4 rounded-lg border border-gray-700 hover:border-gray-600 transition-colors flex flex-col",
            div {
                class: "flex justify-between items-start",
                div {
                    class: "flex items-center gap-x-3",
                    span { class: "{status_class}" },
                    h3 { class: "font-bold text-lg", "{mcp.display_name}" }
                }
                if let Some((installed, _)) = install_status.read().as_ref() {
                    if *installed {
                        button {
                            class: "px-3 py-1 bg-gray-600 rounded text-sm font-medium cursor-not-allowed",
                            disabled: true,
                            "Installed"
                        }
                    } else {
                        button {
                            class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors",
                            onclick: move |_| add_mcp.call(mcp_clone_for_add.clone()),
                            "Add"
                        }
                    }
                } else {
                    button {
                        class: "px-3 py-1 bg-gray-600 rounded text-sm font-medium cursor-not-allowed",
                        disabled: true,
                        "Loading..."
                    }
                }
            }
            div {
                class: "flex-grow mt-2",
                p { class: "text-sm text-gray-400", "{mcp.description}" }
            }
            if !mcp.homepage.is_empty() {
                div {
                    class: "flex justify-end mt-2",
                    a {
                        class: "text-sm text-primary-400 hover:text-primary-300",
                        href: "{mcp.homepage}",
                        target: "_blank",
                        title: "View on Smithery",
                        dioxus_free_icons::Icon {
                            icon: dioxus_free_icons::icons::fi_icons::FiExternalLink
                        }
                    }
                }
            }
        }
    }
}

fn highlight_json(json: String) -> String {
    let syntax = SYNTAX_SET.find_syntax_by_extension("json")
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, &THEME);
    let mut html = String::new();
    
    for line in json.lines() {
        let regions = h.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
        let html_line = styled_line_to_highlighted_html(&regions, IncludeBackground::No)
            .unwrap_or_else(|_| line.to_string());
        html.push_str(&html_line);
        html.push('\n');
    }
    
    if html.ends_with('\n') {
        html.pop();
    }
    html
}

fn get_mcp_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("mcp_servers.json")
}
