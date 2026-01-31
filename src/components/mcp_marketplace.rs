#![allow(clippy::await_holding_invalid_type)]

use crate::components::mcp_search_form::McpSearchForm;
use crate::components::smithery_registry::{SmitheryClient, SmitheryServer};
use crate::components::syntax_highlighter::highlight_json;
use crate::mcp::composio_client::{ComposioCategory, ComposioClient, ComposioToolkitListing};
use crate::mcp::manager::{McpManager, McpServerStatus, ServerStatus};
use crate::settings::{McpSource, Settings, SettingsManager};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use dioxus_free_icons::{icons::fi_icons, Icon};

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
    /// Primary auth scheme for this toolkit (e.g., "OAUTH2", "API_KEY")
    #[serde(default)]
    auth_scheme: Option<String>,
    /// Whether Composio managed auth is available (true for OAuth apps Composio supports)
    #[serde(default)]
    use_managed_auth: bool,
}

impl From<SmitheryServer> for FeaturedMcp {
    fn from(server: SmitheryServer) -> Self {
        FeaturedMcp {
            name: server.qualified_name.clone(),
            display_name: server.display_name,
            description: server.description,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@smithery/cli@latest".to_string(),
                "run".to_string(),
                server.qualified_name,
            ],
            env_vars: vec![], // This will be populated by the detailed fetch
            homepage: server.homepage,
            auth_scheme: None, // Not applicable for Smithery
            use_managed_auth: false,
        }
    }
}

impl From<ComposioToolkitListing> for FeaturedMcp {
    fn from(toolkit: ComposioToolkitListing) -> Self {
        let description = toolkit.description().unwrap_or_default();
        let homepage = toolkit
            .app_url()
            .unwrap_or_else(|| format!("https://app.composio.dev/app/{}", toolkit.slug));
        let use_managed_auth = toolkit.supports_managed_auth();
        let auth_scheme = toolkit.primary_auth_scheme();
        FeaturedMcp {
            name: toolkit.slug,
            display_name: toolkit.name,
            description,
            command: String::new(), // Not used for Composio
            args: vec![],
            env_vars: vec![],
            homepage,
            auth_scheme,
            use_managed_auth,
        }
    }
}

fn get_featured_mcps() -> Vec<FeaturedMcp> {
    vec![
        FeaturedMcp {
            name: "filesystem".to_string(),
            display_name: "Filesystem".to_string(),
            description: "Give the AI access to read and write files in a specific directory."
                .to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
                "/path/to/allowed/dir".to_string(),
            ],
            env_vars: vec![],
            homepage: "".to_string(),
            auth_scheme: None,
            use_managed_auth: false,
        },
        FeaturedMcp {
            name: "brave-search".to_string(),
            display_name: "Brave Search".to_string(),
            description: "Allow the AI to search the web using Brave Search.".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-brave-search".to_string(),
            ],
            env_vars: vec!["BRAVE_API_KEY".to_string()],
            homepage: "".to_string(),
            auth_scheme: None,
            use_managed_auth: false,
        },
        FeaturedMcp {
            name: "github".to_string(),
            display_name: "GitHub".to_string(),
            description: "Interact with GitHub repositories, issues, and pull requests."
                .to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            env_vars: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            homepage: "".to_string(),
            auth_scheme: None,
            use_managed_auth: false,
        },
        FeaturedMcp {
            name: "postgres".to_string(),
            display_name: "PostgreSQL".to_string(),
            description: "Read and write data to a PostgreSQL database.".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-postgres".to_string(),
                "<YOUR_POSTGRES_CONNECTION_STRING>".to_string(),
            ],
            env_vars: vec![],
            homepage: "".to_string(),
            auth_scheme: None,
            use_managed_auth: false,
        },
        FeaturedMcp {
            name: "google-maps".to_string(),
            display_name: "Google Maps".to_string(),
            description: "Access location data and directions via Google Maps.".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-google-maps".to_string(),
            ],
            env_vars: vec!["GOOGLE_MAPS_API_KEY".to_string()],
            homepage: "".to_string(),
            auth_scheme: None,
            use_managed_auth: false,
        },
    ]
}

#[component]
pub fn McpMarketplace() -> Element {
    let mut active_tab = use_signal(|| ActiveTab::Status);
    let search_query = use_signal(|| "".to_string());
    let mut trigger_search = use_signal(|| 0);
    let mut config_content = use_signal(|| "".to_string());
    let mut error_message = use_signal(|| Option::<String>::None);
    let mut success_message = use_signal(|| Option::<String>::None);
    let mcp_manager = use_context::<Signal<McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let filter_verified = use_signal(|| true);
    let filter_deployed = use_signal(|| false);
    let sort_by = use_signal(|| "usage".to_string());
    let _refresh_signal = use_signal(|| 0);

    // State for server list
    let mut current_page = use_signal(|| 1i32);
    let mut total_pages = use_signal(|| 1i32);

    // Cursor-based pagination for Composio
    // cursor_stack stores previous cursors for "back" navigation
    // next_cursor stores the cursor for the next page
    let mut cursor_stack: Signal<Vec<String>> = use_signal(Vec::new);
    let mut current_cursor: Signal<Option<String>> = use_signal(|| None);
    let mut next_cursor: Signal<Option<String>> = use_signal(|| None);

    // Connected toolkit slugs for Composio (for "Connected" detection in marketplace)
    let connected_slugs: Signal<std::collections::HashSet<String>> =
        use_signal(std::collections::HashSet::new);

    // Composio category filtering state
    let selected_categories: Signal<Vec<String>> = use_signal(Vec::new);
    let available_categories: Signal<Vec<ComposioCategory>> = use_signal(Vec::new);
    let show_category_dropdown = use_signal(|| false);
    let categories_loading = use_signal(|| false);

    // Reset cursor state when search query, categories, or sort order change
    {
        let mut cursor_stack = cursor_stack;
        let mut current_cursor = current_cursor;
        let mut current_page = current_page;
        use_effect(move || {
            // Subscribe to changes in search query, categories, and sort order
            let _ = search_query.read();
            let _ = selected_categories.read();
            let _ = sort_by.read();
            // Reset pagination to first page
            cursor_stack.set(vec![]);
            current_cursor.set(None);
            current_page.set(1);
        });
    }

    // Fetch categories when Composio is selected - use effect with explicit dependency
    {
        use_effect(move || {
            let settings_snapshot = settings.read().clone();
            let mut available_categories = available_categories;
            let mut categories_loading = categories_loading;

            if settings_snapshot.preferred_mcp_source != McpSource::Composio {
                return; // Only fetch for Composio
            }

            // Set loading state
            categories_loading.set(true);

            spawn(async move {
                if let Some(profile) = settings_snapshot.get_active_profile() {
                    if let Some(api_key) = &profile.api_key {
                        let base_url = profile
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
                        let client = ComposioClient::new(
                            api_key.clone(),
                            base_url,
                            profile.entity_id.clone(),
                            profile.user_id.clone(),
                            profile.id.clone(),
                        );

                        tracing::debug!("Fetching toolkit categories...");
                        match client.list_toolkit_categories().await {
                            Ok(cats) => {
                                // Deduplicate categories by slug (id)
                                let mut seen = std::collections::HashSet::new();
                                let unique_cats: Vec<ComposioCategory> = cats
                                    .into_iter()
                                    .filter(|c| seen.insert(c.slug.clone()))
                                    .collect();
                                tracing::debug!("Fetched {} unique categories", unique_cats.len());
                                available_categories.set(unique_cats);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to fetch categories: {}", e);
                            }
                        }
                    } else {
                        tracing::warn!("No API key available for fetching categories");
                    }
                } else {
                    tracing::warn!("No active profile for fetching categories");
                }
                // Clear loading state
                categories_loading.set(false);
            });
        });
    }

    // Fetch connected toolkits when Composio is selected
    let _connected_slugs_resource = use_resource({
        move || {
            let mut connected_slugs = connected_slugs;
            // Track settings changes
                // Track settings changes
                let settings_snapshot = settings.read().clone();

                async move {
                    // Check if we have a profile with an API key
                    if let Some(profile) = settings_snapshot.get_active_profile() {
                        if let Some(api_key) = &profile.api_key {
                        let base_url = profile
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
                        let client = ComposioClient::new(
                            api_key.clone(),
                            base_url,
                            profile.entity_id.clone(),
                            profile.user_id.clone(),
                            profile.id.clone(),
                        );

                        match client.get_connected_toolkit_slugs().await {
                            Ok(slugs) => {
                                tracing::debug!("Fetched {} connected toolkit slugs", slugs.len());
                                connected_slugs.set(slugs);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to fetch connected toolkit slugs: {}", e);
                            }
                        }
                    }
                }
            }
        }
    });

    // Load config on mount
    use_effect(move || {
        let path = get_mcp_config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => config_content.set(content),
            Err(_) => config_content.set(r#"{ "mcpServers": {} }"#.to_string()),
        }
    });

    let server_resource: Resource<Result<(Vec<FeaturedMcp>, String), String>> = use_resource(
        move || {
            // This resource will re-run whenever any of these signals change.
            // IMPORTANT: All signals must be read at the top level of this closure for reactive tracking.
            let page = *current_page.read();
            let verified = *filter_verified.read();
            let deployed = *filter_deployed.read();
            let query = search_query.read().clone();
            let sort = sort_by.read().clone();
            // Track selected categories so changes trigger a refetch
            let cats = selected_categories.read().clone();
            // Explicit trigger counter - must be used (not just discarded) for proper reactivity
            let trigger = *trigger_search.read();

            // Track settings changes (e.g. API key loading)
            let settings_snapshot = settings.read().clone();

            tracing::debug!(
                "Resource triggered - sort={:?}, trigger={}, categories={:?}",
                sort,
                trigger,
                cats
            );

            // Get the preferred source from settings
            let source = settings_snapshot.preferred_mcp_source.clone();

            async move {
                // Ensure trigger is captured for reactive tracking
                let _ = trigger;

                match source {
                    McpSource::Composio => {
                        // Fetch from Composio
                        if let Some(profile) = settings_snapshot.get_active_profile() {
                            if let Some(api_key) = &profile.api_key {
                                let base_url = profile.base_url.clone().unwrap_or_else(|| {
                                    "https://backend.composio.dev/v3/mcp".to_string()
                                });
                                let client = ComposioClient::new(
                                    api_key.clone(),
                                    base_url,
                                    profile.entity_id.clone(),
                                    profile.user_id.clone(),
                                    profile.id.clone(),
                                );

                                // Use search query if provided
                                let search_param = if query.is_empty() {
                                    None
                                } else {
                                    Some(query.as_str())
                                };

                                // Use categories from the captured cats variable (already read in reactive tracking section)
                                let categories_param = if cats.is_empty() {
                                    None
                                } else {
                                    Some(cats.clone())
                                };

                                // Get current cursor for pagination
                                let cursor_param = current_cursor.peek().clone();
                                let cursor_ref = cursor_param.as_deref();

                                // Get sort parameter
                                let sort_param = if sort.is_empty() {
                                    None
                                } else {
                                    Some(sort.as_str())
                                };

                                tracing::debug!("Fetching toolkits with search={:?}, cursor={:?}, categories={:?}, sort={:?}", search_param, cursor_ref, categories_param, sort_param);

                                match client
                                    .list_all_toolkits(
                                        search_param,
                                        cursor_ref,
                                        Some(20),
                                        categories_param,
                                        sort_param,
                                    )
                                    .await
                                {
                                    Ok((toolkits, pages, next_cur)) => {
                                        total_pages.set(pages);
                                        next_cursor.set(next_cur);
                                        let mcps: Vec<FeaturedMcp> =
                                            toolkits.into_iter().map(Into::into).collect();
                                        Ok((mcps, "Composio".to_string()))
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to fetch from Composio: {}", e);
                                        Err(format!("Composio error: {}", e))
                                    }
                                }
                            } else {
                                Err("No Composio API key configured. Set your API key in Settings → MCP → Composio".to_string())
                            }
                        } else {
                            Err(
                                "No Composio profile configured. Add a profile in Settings → MCP"
                                    .to_string(),
                            )
                        }
                    }
                    McpSource::Smithery => {
                        // Existing Smithery logic
                        let api_key = settings_snapshot.smithery_api_key.clone();

                        if let Some(key) = api_key {
                            let client = SmitheryClient::new(key);
                            let mut filters = Vec::new();
                            if verified {
                                filters.push("is:verified");
                            }
                            if deployed {
                                filters.push("is:deployed");
                            }
                            if !query.is_empty() {
                                filters.push(&query);
                            }
                            let final_query = filters.join(" ");
                            let search_param = if final_query.is_empty() {
                                None
                            } else {
                                Some(final_query.as_str())
                            };

                            match client
                                .fetch_servers(search_param, Some(page as u32), Some(&sort))
                                .await
                            {
                                Ok(response) => {
                                    total_pages.set(response.pagination.total_pages as i32);
                                    let mcps: Vec<FeaturedMcp> =
                                        response.servers.into_iter().map(Into::into).collect();
                                    Ok((mcps, "Smithery".to_string()))
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to fetch from Smithery: {}", e);
                                    Ok((
                                        get_featured_mcps(),
                                        "Hardcoded (Smithery error)".to_string(),
                                    ))
                                }
                            }
                        } else {
                            Ok((get_featured_mcps(), "Hardcoded".to_string()))
                        }
                    }
                }
            }
        },
    );

    // Get current search query for client-side filtering (Composio API doesn't support server-side filtering)
    let current_query = search_query.read().to_lowercase();

    let (filtered_mcps, data_source, is_loading) = match &*server_resource.read() {
        Some(Ok((mcps, source))) => {
            // Apply client-side filtering for Composio since API ignores search params
            let filtered = if current_query.is_empty() || source != "Composio" {
                mcps.clone()
            } else {
                mcps.iter()
                    .filter(|mcp| {
                        mcp.display_name.to_lowercase().contains(&current_query)
                            || mcp.name.to_lowercase().contains(&current_query)
                            || mcp.description.to_lowercase().contains(&current_query)
                    })
                    .cloned()
                    .collect()
            };
            (filtered, source.clone(), false)
        }
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

                tracing::debug!("Attempting to save MCP config to: {:?}", path);

                // Clone new_content for the blocking task
                let content_for_write = new_content.clone();
                let write_result =
                    tokio::task::spawn_blocking(move || std::fs::write(&path, &content_for_write))
                        .await
                        .unwrap();

                match write_result {
                    Ok(_) => {
                        tracing::info!("Successfully saved MCP config.");
                        config_content.set(new_content);
                        success_message.set(Some(
                            "Configuration saved. Reloading servers...".to_string(),
                        ));
                        error_message.set(None);

                        // Trigger reload on McpManager
                        let manager = mcp_manager.read().clone();
                        let context_signal = mcp_context;
                        let current_settings = settings.read().clone();

                        spawn(async move {
                            manager
                                .reload_config(context_signal, current_settings)
                                .await;
                        });
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
        let mut error_msg = error_message;
        spawn(async move {
            tracing::debug!("Attempting to add MCP server: {}", mcp.name);

            let current_config_str = config_content.read().clone();

            // Extract short server name (e.g., "@smithery/googlecalendar" -> "googlecalendar")
            // This is a rough heuristic, might need refinement for composio mapping
            let short_name = mcp
                .name
                .split('/')
                .next_back()
                .unwrap_or(&mcp.name)
                .replace("-mcp", "");

            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&current_config_str) {
                if let Some(servers) = json.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
                    match current_settings.preferred_mcp_source {
                        crate::settings::McpSource::Smithery => {
                            if let Some(api_key) = current_settings.smithery_api_key.clone() {
                                // Use standard Smithery CLI pattern
                                let new_server = serde_json::json!({
                                    "command": "npx",
                                    "args": [
                                        "-y",
                                        "@smithery/cli@latest",
                                        "run",
                                        short_name,
                                        "--key",
                                        api_key
                                    ],
                                    "description": mcp.description
                                });
                                servers.insert(short_name.clone(), new_server);
                            } else {
                                tracing::warn!("No Smithery API key configured");
                                error_msg.set(Some(
                                    "Please set your Smithery API key in Settings first"
                                        .to_string(),
                                ));
                                return;
                            }
                        }
                        crate::settings::McpSource::Composio => {
                            // ------------------------------------------------------------------
                            // COMPOSIO INSTALLATION PATH
                            // ------------------------------------------------------------------
                            // Note: Interactive installation of Composio tools is handled directly
                            // in `McpServerCard` via `McpManager::connect_toolkit` because it
                            // requires local UI state signals (spinners, status updates) that
                            // are not available in this top-level closure.
                            //
                            // If this branch is hit, it means we are in a mixed state (e.g. Smithery
                            // source selected but clicking a Composio tool).
                            // ------------------------------------------------------------------
                            
                            tracing::warn!("Delegating Composio install to McpServerCard logic.");
                            error_msg.set(Some(
                                "Please switch to 'Composio' source in settings to install this tool."
                                    .to_string(),
                            ));
                            return;
                        }
                    }

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
                    if *active_tab.read() == ActiveTab::Marketplace {
                        div {
                            class: "text-xs px-2 py-1 rounded bg-gray-700 text-gray-300",
                            if is_loading {
                                "Loading..."
                            } else {
                                "Source: {data_source}"
                            }
                        }
                    }
                }
                div {
                    class: "flex space-x-4",
                    button {
                        class: if *active_tab.read() == ActiveTab::Status {
                            "px-3 py-1 text-sm font-medium text-primary-400 border-b-2 border-primary-400"
                        } else {
                            "px-3 py-1 text-sm font-medium text-gray-400 hover:text-white"
                        },
                        onclick: move |_| active_tab.set(ActiveTab::Status),
                        "Status"
                    }
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
                }
            }

            div {
                class: if *active_tab.read() == ActiveTab::Installed {
                    "flex-1 flex flex-col overflow-hidden p-4"
                } else {
                    "flex-1 overflow-y-auto p-4"
                },
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
                            sort_by: sort_by,
                            mcp_source: settings.peek().preferred_mcp_source.clone(),
                            composio_categories: selected_categories,
                            available_categories: available_categories,
                            show_category_dropdown: show_category_dropdown,
                            categories_loading: *categories_loading.read()
                        }
                        div {
                            class: "grid grid-cols-1 gap-4",
                            for mcp in filtered_mcps {
                                McpServerCard {
                                    key: "{mcp.name}",
                                    mcp: mcp.clone(),
                                    add_mcp: add_mcp,
                                    trigger_search: trigger_search,
                                    connected_slugs: connected_slugs
                                }
                            }
                        }
                        div {
                            class: "flex justify-between items-center mt-4",
                            button {
                                class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors disabled:bg-gray-600 disabled:cursor-not-allowed",
                                // Disable if no previous cursors (we're on first page)
                                disabled: cursor_stack.read().is_empty(),
                                onclick: move |_| {
                                    // Pop the last cursor from stack and use it
                                    let mut stack = cursor_stack.write();
                                    if let Some(prev_cursor) = stack.pop() {
                                        // Set previous cursor (or None if empty string = first page)
                                        if prev_cursor.is_empty() {
                                            current_cursor.set(None);
                                        } else {
                                            current_cursor.set(Some(prev_cursor));
                                        }
                                        // Decrement page counter
                                        let page = *current_page.read();
                                        if page > 1 {
                                            current_page.set(page - 1);
                                        }
                                        // Trigger refresh
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
                                // Disable if no next cursor
                                disabled: next_cursor.read().is_none(),
                                onclick: move |_| {
                                    if let Some(next) = next_cursor.read().clone() {
                                        // Push current cursor to stack (for going back)
                                        let mut stack = cursor_stack.write();
                                        if let Some(cur) = current_cursor.read().clone() {
                                            stack.push(cur);
                                        } else {
                                            // First page has no cursor, push empty string as marker
                                            stack.push(String::new());
                                        }
                                        // Move to next cursor
                                        current_cursor.set(Some(next));
                                        // Increment page counter
                                        let page = *current_page.read();
                                        current_page.set(page + 1);
                                        // Trigger refresh
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
                            class: "flex-1 flex flex-col min-h-0",
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
                                class: "mt-4 flex justify-end gap-2",
                                button {
                                    class: "px-4 py-2 bg-gray-600 hover:bg-gray-500 rounded font-medium transition-colors",
                                    onclick: move |_| {
                                        let content = config_content.read().clone();
                                        match serde_json::from_str::<serde_json::Value>(&content) {
                                            Ok(parsed) => {
                                                if let Ok(formatted) = serde_json::to_string_pretty(&parsed) {
                                                    config_content.set(formatted);
                                                    error_message.set(None);
                                                    success_message.set(Some("JSON formatted.".to_string()));
                                                }
                                            }
                                            Err(e) => {
                                                error_message.set(Some(format!("Cannot format - Invalid JSON: {}", e)));
                                            }
                                        }
                                    },
                                    "Format JSON"
                                }
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
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();

    // Trigger signal for forcing resource refresh
    let mut refresh_trigger = use_signal(|| 0i32);

    let server_statuses = use_resource(move || {
        let _trigger = *refresh_trigger.read(); // Subscribe to manual refresh
        let _context = mcp_context.read(); // Subscribe to context changes (profile switch)
        let mcp_manager = mcp_manager;
        async move {
            let mut statuses = mcp_manager.read().get_all_server_statuses().await;
            statuses.sort_by(|a, b| a.name.cmp(&b.name));
            statuses
        }
    });

    let refresh_statuses = move |_| {
        // Invalidate cache before refresh
        mcp_manager.read().invalidate_status_cache();
        let current = *refresh_trigger.peek();
        refresh_trigger.set(current + 1);
    };

    // Calculate aggregate tool counts
    let (total_tools, loaded_tools) = match server_statuses.read().as_ref() {
        Some(statuses) => {
            let total: usize = statuses.iter().map(|s| s.tools).sum();
            let loaded: usize = statuses
                .iter()
                .filter(|s| s.is_loaded)
                .map(|s| s.tools)
                .sum();
            (total, loaded)
        }
        None => (0, 0),
    };

    // Format for display in rsx
    let tool_count_display = format!("{} / {} tools loaded", loaded_tools, total_tools);

    // Pre-compute warning state to avoid rsx type inference issues
    let exceeds_limit = loaded_tools > 128;
    let above_optimal = loaded_tools > 50;
    let tool_badge_class = if exceeds_limit {
        "text-xs px-2 py-0.5 rounded bg-red-900/50 text-red-300 border border-red-600"
    } else if above_optimal {
        "text-xs px-2 py-0.5 rounded bg-yellow-900/50 text-yellow-300 border border-yellow-600"
    } else {
        "text-xs px-2 py-0.5 rounded bg-gray-700 text-gray-300"
    };

    rsx! {
        div {
            class: "space-y-4",
            div {
                class: "flex items-center justify-between mb-4",
                div {
                    class: "flex flex-col gap-1",
                    p { class: "text-sm text-gray-400", "Status of all configured MCP servers." }
                    // Aggregate tool count with warnings
                    div {
                        class: "flex items-center gap-2 flex-wrap",
                        span {
                            class: "{tool_badge_class}",
                            "{tool_count_display}"
                        }
                        if exceeds_limit {
                            span {
                                class: "text-xs text-red-400",
                                "⚠️ Exceeds Gemini limit (128)"
                            }
                        }
                        if !exceeds_limit && above_optimal {
                            span {
                                class: "text-xs text-yellow-400",
                                "⚠️ Above optimal (50)"
                            }
                        }
                    }
                }
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
                                StatusCard {
                                    status: status.clone(),
                                    refresh_trigger: refresh_trigger
                                }
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
fn StatusCard(status: McpServerStatus, refresh_trigger: Signal<i32>) -> Element {
    let mcp_manager = use_context::<Signal<McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let mut local_settings = use_signal(|| settings.read().clone());
    let mut is_retrying = use_signal(|| false);

    // Toolkit management state (only relevant for Composio)
    let is_composio = status.name == "composio-native";
    // NOTE: show_toolkits is read from ui_state.composio_toolkit_expanded instead of local signal
    let mut toolkits: Signal<Vec<crate::mcp::composio_client::ToolkitInfo>> =
        use_signal(Vec::new);
    let mut toolkits_loading = use_signal(|| false);
    let mut toolkits_error: Signal<Option<String>> = use_signal(|| None);

    // Log errors to console for debugging
    let status_for_logging = status.clone();
    use_effect(move || {
        if let Some(ref error) = status_for_logging.error_message {
            tracing::error!("[MCP {}] {}", status_for_logging.name, error);
        }
    });

    // Reset toolkit state when the ACTIVE PROFILE changes (not on every mount)
    // This clears stale data and allows a fresh fetch with the new client
    let mut last_profile_name: Signal<Option<String>> = use_signal(|| None);
    use_effect(move || {
        let _context = mcp_context.read(); // Subscribe to context changes
        let current_profile = settings.read().active_composio_profile.clone();
        let previous_profile = last_profile_name.peek().clone();

        // Only reset if profile actually changed (not on initial mount with None -> Some)
        if previous_profile.is_some() && previous_profile != current_profile {
            tracing::debug!(
                "Profile changed from {:?} to {:?}, resetting toolkit state",
                previous_profile,
                current_profile
            );
            toolkits.set(Vec::new());
            toolkits_error.set(None);
            toolkits_loading.set(false);
            // ALSO sync local_settings with global settings
            local_settings.set(settings.read().clone());
        }
        last_profile_name.set(current_profile);
    });

    // Fetch toolkits when show_toolkits is expanded for Composio
    // Only fetch if: composio server is loaded, dropdown is expanded, no toolkits yet, not loading, and no prior error
    let status_for_fetch = status.status.clone();
    use_effect(move || {
        let should_fetch = is_composio
            && ui_state.read().composio_toolkit_expanded
            && status_for_fetch == ServerStatus::Loaded;
        // Use peek() to avoid creating a dependency on the signals we only check condition against
        // This prevents infinite loops where we write to these signals within the effect
        let no_error = toolkits_error.peek().is_none();
        if should_fetch && toolkits.peek().is_empty() && !*toolkits_loading.peek() && no_error {
            toolkits_loading.set(true);

            let mcp_manager_clone = mcp_manager;
            spawn(async move {
                match mcp_manager_clone.read().get_composio_toolkits().await {
                    Ok(kits) => {
                        toolkits.set(kits);
                        toolkits_loading.set(false);
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch toolkits: {}", e);
                        toolkits_error.set(Some(e));
                        toolkits_loading.set(false);
                    }
                }
            });
        }
    });

    // Note: Settings persistence is now handled directly in the dropdown onchange handler,
    // which calls settings_manager.save() immediately when the user changes a value.

    let (status_color, status_text, status_bg) = match status.status {
        ServerStatus::Loaded => ("bg-green-500", "Loaded", "bg-green-900/20"),
        ServerStatus::Error => ("bg-red-500", "Error", "bg-red-900/20"),
        ServerStatus::Disabled => ("bg-gray-500", "Disabled", "bg-gray-900/20"),
        ServerStatus::NeedsAuth => ("bg-yellow-500", "Needs Auth", "bg-yellow-900/20"),
        ServerStatus::NotConfigured => ("bg-blue-500", "Not Configured", "bg-blue-900/20"),
    };

    let status_clone = status.clone();
    let retry_server = move |_| {
        let server_name = status_clone.name.clone();
        let mcp_manager = mcp_manager;
        let mcp_context = mcp_context;
        let settings = settings;
        is_retrying.set(true);

        spawn(async move {
            tracing::debug!("Retrying server: {}", server_name);
            let result = mcp_manager
                .read()
                .retry_server(&server_name, mcp_context, settings.read().clone(), None)
                .await;
            match result {
                Ok(_) => tracing::debug!("Retry initiated for {}", server_name),
                Err(e) => tracing::error!("Failed to retry {}: {}", server_name, e),
            }
            // Reset retrying state after operation completes
            is_retrying.set(false);
        });
    };

    // OAuth authentication is now handled by Smithery CLI automatically
    // No manual authentication needed - just ensure SMITHERY_API_KEY is set in env

    // Determine if this server needs OAuth authorization
    // More deterministic checks:
    // 1. NeedsAuth status (set explicitly via is_auth_error detection)
    // 2. Error message contains 401/invalid_token (HTTP status based)
    // 3. Server is using Smithery CLI (indicates Smithery-hosted server)
    let is_smithery_hosted = status
        .uri
        .as_ref()
        .map(|u| u.contains("smithery.ai"))
        .unwrap_or(false)
        || status.name.contains("google")
        || status.name.contains("calendar")
        || status.name.contains("drive")
        || status.name.contains("gmail");

    let has_auth_error = status
        .error_message
        .as_ref()
        .map(|e| {
            // Check for HTTP 401 status or known auth error patterns
            e.contains("401") || e.contains("invalid_token") || e.contains("unauthorized")
        })
        .unwrap_or(false);

    let needs_oauth = status.status == ServerStatus::NeedsAuth
        || (status.status == ServerStatus::Error && (has_auth_error || is_smithery_hosted));

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
                            class: "flex items-center gap-x-2 flex-wrap",
                            h3 { class: "font-bold text-base", "{status.name}" }
                            span {
                                class: "text-xs px-2 py-0.5 rounded {status_color} bg-opacity-20 text-white",
                                "{status_text}"
                            }
                            // Tool count badge for loaded servers
                            if status.status == ServerStatus::Loaded && status.tools > 0 {
                                span {
                                    class: "text-xs px-2 py-0.5 rounded bg-primary-900/30 text-primary-300 border border-primary-700",
                                    "{status.tools} tools"
                                }
                            }
                            // Load/Unload toggle for local MCPs (not Composio, not error (unless retriable? no, error handles retry separately))
                            // We now allow Disabled status because user-unloaded servers are reported as Disabled.
                            if !is_composio && (status.status == ServerStatus::Loaded || status.status == ServerStatus::Disabled) {
                                {
                                    let server_name = status.name.clone();
                                    let current_is_loaded = status.is_loaded;
                                    // Redundant locals removed
                                    // Captured by move closure below
                                    rsx! {
                                        button {
                                            class: if current_is_loaded {
                                                "text-xs px-2 py-0.5 rounded bg-yellow-900/30 text-yellow-300 border border-yellow-700 hover:bg-yellow-900/50 transition-colors"
                                            } else {
                                                "text-xs px-2 py-0.5 rounded bg-green-900/30 text-green-300 border border-green-700 hover:bg-green-900/50 transition-colors"
                                            },
                                            onclick: move |_| {
                                                let server_name = server_name.clone();
                                                let mcp_manager = mcp_manager;
                                                let mut mcp_context = mcp_context;
                                                let mut refresh_trigger = refresh_trigger;
                                                let mut ui_state = ui_state;
                                                let ui_state_manager = ui_state_manager;
                                                let should_load = !current_is_loaded;
                                                spawn(async move {
                                                    if should_load {
                                                        mcp_manager.read().load_server(&server_name).await;
                                                    } else {
                                                        mcp_manager.read().unload_server(&server_name).await;
                                                    }

                                                    // Persist unloaded state to UiState
                                                    {
                                                        let mut state = ui_state.write();
                                                        if should_load {
                                                            state.unloaded_mcp_servers.retain(|s| s != &server_name);
                                                        } else {
                                                            if !state.unloaded_mcp_servers.contains(&server_name) {
                                                                state.unloaded_mcp_servers.push(server_name.clone());
                                                            }
                                                        }
                                                        // Save to disk
                                                        let uism = ui_state_manager.read();
                                                        if let Err(e) = uism.save(&state) {
                                                            tracing::error!("Failed to save UI state after load/unload toggle: {}", e);
                                                        }
                                                    }

                                                    // Update mcp_context with filtered tools
                                                    let new_context = mcp_manager.read().get_mcp_context().await;
                                                    mcp_context.set(new_context);
                                                    // Invalidate cache and trigger refresh
                                                    mcp_manager.read().invalidate_status_cache();
                                                    let current = *refresh_trigger.peek();
                                                    refresh_trigger.set(current + 1);
                                                });
                                            },
                                            if current_is_loaded { "Unload" } else { "Load" }
                                        }
                                    }
                                }
                            }
                        }
                        if !status.description.is_empty() {
                            p { class: "text-sm text-gray-400 mt-1", "{status.description}" }
                        }
                        if let Some(ref warning) = status.warning_message {
                            div {
                                class: "mt-2 p-2 bg-yellow-900/20 rounded border border-yellow-800",
                                p {
                                    class: "text-sm text-yellow-300 font-mono whitespace-pre-wrap break-all",
                                    "⚠️ {warning}"
                                }
                            }
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
                                // Redundant locals removed
                                // mcp_manager, mcp_context, settings captured by move
                                let settings = settings; // settings not listed as redundant in error? 
                                // wait, error 1254, 1255. 1259 was settings.
                                // If settings was redundant, it should have been flagged.
                                // Let's try removing it too to be safe, as it is likely redundant if others are.
                                move |_| {
                                    let server_name = server_name.clone();
                                    let mcp_manager = mcp_manager;
                                    let mcp_context = mcp_context;
                                    let settings = settings;

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
                                                tracing::debug!("Waiting for OAuth callback...");
                                                if let Some(result) = callback_rx.recv().await {
                                                    if result.success {
                                                        if let Some(code) = result.auth_code {
                                                            tracing::debug!("Received auth code, completing OAuth flow...");

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

            // Composio toolkit configuration section
            if is_composio && status.status == ServerStatus::Loaded {
                div {
                    class: "mt-4 border-t border-gray-600 pt-4",
                    div {
                        class: "flex items-center justify-between cursor-pointer hover:bg-gray-700/50 rounded p-2 -m-2",
                        onclick: {
                            let mut ui_state = ui_state;
                            // ui_state_manager captured by move
                            move |_| {
                                let new_state = !ui_state.read().composio_toolkit_expanded;
                                ui_state.write().composio_toolkit_expanded = new_state;
                                if let Err(e) = ui_state_manager.read().save(&ui_state.read()) {
                                    tracing::error!("Failed to save toolkit expanded state: {}", e);
                                }
                            }
                        },
                        h4 {
                            class: "text-sm font-semibold text-gray-300",
                            "Toolkit Loading Configuration"
                        }
                        span {
                            class: "text-gray-400",
                            if ui_state.read().composio_toolkit_expanded { "▼" } else { "▶" }
                        }
                    }

                    if ui_state.read().composio_toolkit_expanded {
                        div {
                            class: "mt-3 space-y-2",
                            if *toolkits_loading.read() {
                                p {
                                    class: "text-sm text-gray-400 italic",
                                    "Loading toolkits..."
                                }
                            } else if let Some(error) = toolkits_error.read().as_ref() {
                                p {
                                    class: "text-sm text-red-400",
                                    "{error}"
                                }
                            } else if toolkits.read().is_empty() {
                                p {
                                    class: "text-sm text-gray-400 italic",
                                    "No toolkits found. Connect toolkits via Composio."
                                }
                            } else {
                                for toolkit in toolkits.read().iter() {
                                    {
                                        let toolkit_slug = toolkit.slug.clone();
                                        let settings_snapshot = local_settings.read().clone();

                                        // Get the current force_load status (default: false = on-demand)
                                        let is_force_load = settings_snapshot.get_active_profile()
                                            .and_then(|p| p.toolkit_configs.iter().find(|c| c.slug == toolkit_slug))
                                            .map(|c| c.force_load)
                                            .unwrap_or(false);

                                        rsx! {
                                            div {
                                                class: "flex items-center justify-between p-2 bg-dark-input rounded border border-gray-600",
                                                div {
                                                    class: "flex items-center gap-2",
                                                    span {
                                                        class: if toolkit.is_connected { "text-green-400" } else { "text-gray-500" },
                                                        if toolkit.is_connected { "✓" } else { "○" }
                                                    }
                                                    span {
                                                        class: "text-sm",
                                                        "{toolkit.display_name}"
                                                    }
                                                    span {
                                                        class: "text-xs text-gray-400",
                                                        "({toolkit.tool_count} tools)"
                                                    }
                                                }
                                                if toolkit.is_connected {
                                                    select {
                                                        class: "px-2 py-1 bg-dark-section border border-gray-600 rounded text-xs",
                                                        value: if is_force_load { "force" } else { "ondemand" },
                                                        onchange: {
                                                            let slug = toolkit_slug.clone();
                                                            let tool_count = toolkit.tool_count;
                                                            let display_name = toolkit.display_name.clone();
                                                            // Redundant locals removed
                                                            move |event: dioxus::events::FormEvent| {
                                                                let new_force_load = event.value() == "force";
                                                                let slug = slug.clone();
                                                                let display_name = display_name.clone();
                                                                let mcp_manager = mcp_manager;
                                                                let mut mcp_context = mcp_context;
                                                                let mut refresh_trigger = refresh_trigger;

                                                                // Update local settings
                                                                let mut s = local_settings.write();
                                                                if let Some(profile) = s.get_active_profile_mut() {
                                                                    if let Some(config) = profile.toolkit_configs.iter_mut().find(|c| c.slug == slug) {
                                                                        config.force_load = new_force_load;
                                                                    } else {
                                                                        profile.toolkit_configs.push(crate::settings::ComposioToolkitConfig {
                                                                            slug: slug.clone(),
                                                                            display_name: display_name.clone(),
                                                                            tool_count,
                                                                            force_load: new_force_load,
                                                                        });
                                                                    }
                                                                }
                                                                let updated_settings = s.clone();
                                                                drop(s); // Release write lock

                                                                // Persist to global settings and disk
                                                                settings.set(updated_settings.clone());
                                                                if let Err(e) = settings_manager.read().save(&updated_settings) {
                                                                    tracing::error!("Failed to save toolkit settings: {}", e);
                                                                } else {
                                                                    tracing::debug!("Saved toolkit config: {} = {}", slug, if new_force_load { "force-load" } else { "on-demand" });
                                                                }

                                                                // Reload Composio tools and update UI
                                                                spawn(async move {
                                                                    if let Err(e) = mcp_manager.read().reload_composio_tools(&updated_settings).await {
                                                                        tracing::error!("Failed to reload Composio tools: {}", e);
                                                                    }
                                                                    // Update mcp_context with new tools
                                                                    let new_context = mcp_manager.read().get_mcp_context().await;
                                                                    mcp_context.set(new_context);
                                                                    // Invalidate cache and trigger refresh
                                                                    mcp_manager.read().invalidate_status_cache();
                                                                    let current = *refresh_trigger.peek();
                                                                    refresh_trigger.set(current + 1);
                                                                });
                                                            }
                                                        },
                                                        option { value: "ondemand", "On-demand" }
                                                        option { value: "force", "Load always" }
                                                    }
                                                } else {
                                                    span {
                                                        class: "text-xs text-gray-500",
                                                        "Not connected"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                p {
                                    class: "text-xs text-gray-400 mt-2",
                                    "On-demand uses Tool Router's search→execute pattern. Force load makes all tools available upfront."
                                }
                            }
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
    connected_slugs: Signal<std::collections::HashSet<String>>,
) -> Element {
    let mcp_clone_for_add = mcp.clone();
    let mcp_manager = use_context::<Signal<McpManager>>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut chat_command = use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();

    // Connection state for Composio Connect button
    let mut is_connecting = use_signal(|| false);
    let mut connection_status: Signal<String> = use_signal(|| "Connect".to_string());
    let mut connection_error: Signal<Option<String>> = use_signal(|| None);
    let connection_task = use_signal(|| Option::<Task>::None);


    // Source-aware install detection:
    // - Composio: check if toolkit slug is in connected_slugs
    // - Smithery: check if MCP server is running
    let install_status = use_resource({
        let mcp = mcp.clone();
        let connected_slugs = connected_slugs;
        move || {
            // Depend on the search trigger to force re-evaluation
            let _ = trigger_search.read();
            let mcp_manager = mcp_manager;
            let mcp_name = mcp.name.clone();
            let connected = connected_slugs.read().clone();
            // Per-tool detection: Composio tools have no command
            let is_composio_tool = mcp.command.is_empty();

            async move {
                if is_composio_tool {
                    // For Composio: check if toolkit is connected
                    let is_connected = connected.contains(&mcp_name.to_lowercase());
                    tracing::debug!(
                        "Connection check for '{}': connected={}, slugs={:?}",
                        mcp_name.to_lowercase(),
                        is_connected,
                        connected
                    );
                    return (is_connected, false); // (installed/connected, has_error)
                }

                // For Smithery: check MCP server status
                let guard = mcp_manager.read();
                let servers = guard.servers.lock().await;
                let failed = guard.failed_servers.lock().await;

                let server_key = mcp_name.split('/').next_back().unwrap_or(&mcp_name);
                let is_loaded = servers
                    .keys()
                    .any(|key| key == server_key || key == &mcp_name);

                if is_loaded {
                    return (true, false);
                }

                let has_error = failed
                    .keys()
                    .any(|key| key == server_key || key == &mcp_name);
                (has_error, has_error)
            }
        }
    });

    // Source-aware status display
    let is_composio_tool = mcp.command.is_empty();
    let (status_class, _status_text) = match install_status.read().as_ref() {
        Some((true, false)) => (
            "h-2 w-2 rounded-full bg-green-500",
            if is_composio_tool { "Connected" } else { "Loaded" },
        ),
        Some((true, true)) => ("h-2 w-2 rounded-full bg-red-500", "Error"),
        _ => (
            "h-2 w-2 rounded-full bg-gray-500",
            if is_composio_tool {
                "Not Connected"
            } else {
                "Not Installed"
            },
        ),
    };

    let byoa_required = is_composio_tool && !mcp.use_managed_auth && mcp.auth_scheme.is_some();
    let has_custom_creds = if is_composio_tool {
        let sm = secret_manager.read();
        sm.has_custom_tool_credentials(&mcp.name)
    } else {
        false
    };
    let show_setup_credentials = byoa_required && !has_custom_creds;

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
                if *is_connecting.read() {
                    // Step 4 Priority: Show "Authenticating..." or "Connecting..." even if registry thinks it's installed
                    button {
                        class: "px-3 py-1 bg-gray-600 rounded text-sm font-medium cursor-wait flex items-center gap-2",
                        disabled: true,
                        span { class: "inline-block animate-spin h-3 w-3 border-2 border-white border-t-transparent rounded-full" }
                        "{connection_status.read()}"
                    }
                } else if let Some((installed, _)) = install_status.read().as_ref() {
                    if *installed {
                        button {
                            class: "px-3 py-1 bg-gray-600 rounded text-sm font-medium cursor-not-allowed",
                            disabled: true,
                            if is_composio_tool { "Connected" } else { "Installed" }
                        }
                    } else {
                        if is_composio_tool {
                            // Composio Connect or Setup button
                            div {
                                class: "flex flex-col items-end gap-1",
                                if show_setup_credentials {
                                    button {
                                        class: "px-3 py-1 bg-amber-600 hover:bg-amber-500 rounded text-sm font-medium transition-colors flex items-center gap-1",
                                        title: "This tool requires your own API Key/Secret. Click to setup in Settings.",
                                        onclick: move |_| {
                                            tracing::info!("Redirecting to BYOA setup for toolkit: {}", mcp.name);
                                            chat_command.set(Some(crate::components::chat_input::ChatCommand::SwitchToSettingsTab(
                                                crate::settings::SettingsTab::Credentials,
                                                Some(mcp.name.clone())
                                            )));
                                        },
                                        Icon { width: 14, height: 14, icon: fi_icons::FiSettings }
                                        "Setup Credentials"
                                    }
                                } else {
                                    button {
                                        class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors",
                                        onclick: {
                                            if mcp.name == "news_api" {
                                                tracing::error!("DEBUG: Rendering Connect button for news_api. Auth: {:?}, Managed: {}", mcp.auth_scheme, mcp.use_managed_auth);
                                            }
                                            let toolkit_slug = mcp.name.clone();
                                            let auth_scheme = mcp.auth_scheme.clone();
                                            let use_managed_auth = mcp.use_managed_auth;
                                            move |_| {
                                                let toolkit_slug = toolkit_slug.clone();
                                                let auth_scheme = auth_scheme.clone();
                                                let trigger_search = trigger_search;
                                                let mut connection_task = connection_task;
                                                is_connecting.set(true);
                                                connection_status.set("Connecting...".to_string());
                                                connection_error.set(None);

                                                let task = spawn(async move {
                                                    tracing::info!("Initiating Composio connection for toolkit: {} (auth_scheme: {:?}, managed: {})", 
                                                        toolkit_slug, auth_scheme, use_managed_auth);

                                                    let settings_snapshot = settings.peek().clone();
                                                    if let Some(profile) = settings_snapshot.get_active_profile() {
                                                        if let Some(_api_key) = &profile.api_key {
                                                            let mcp_manager_val = mcp_manager.read().clone();
                                                            let settings_manager_val = settings_manager.peek().clone();
                                                            
                                                            spawn(async move {
                                                                if let Err(e) = mcp_manager_val.connect_toolkit(
                                                                    toolkit_slug,
                                                                    auth_scheme,
                                                                    use_managed_auth,
                                                                    mcp_context,
                                                                    settings,
                                                                    settings_manager_val,
                                                                    is_connecting,
                                                                    connection_status,
                                                                    connection_error,
                                                                    trigger_search,
                                                                    connected_slugs,
                                                                ).await {
                                                                    tracing::error!("Consolidated connection failed: {}", e);
                                                                }
                                                            });
                                                        } else {
                                                            connection_error.set(Some("No Composio API key configured".to_string()));
                                                            tracing::warn!("No Composio API key configured");
                                                            is_connecting.set(false);
                                                        }
                                                    } else {
                                                        connection_error.set(Some("No Composio profile configured".to_string()));
                                                        tracing::warn!("No Composio profile configured");
                                                        is_connecting.set(false);
                                                    }
                                                    connection_task.set(None);
                                                });
                                                connection_task.set(Some(task));
                                            }
                                        },
                                        if is_connecting() { "{connection_status}" } else { "Connect" }
                                    }
                                } // end if/else show_setup_credentials

                                // Show error message if present
                                if let Some(error) = connection_error.read().as_ref() {
                                    span {
                                        class: "text-xs text-red-400 max-w-48 text-right",
                                        "{error}"
                                    }
                                }
                            } // end div
                        } else {
                            button {
                                class: "px-3 py-1 bg-primary-600 hover:bg-primary-500 rounded text-sm font-medium transition-colors",
                                onclick: move |_| add_mcp.call(mcp_clone_for_add.clone()),
                                "Add"
                            }
                        }
                    } // end if *installed else
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
                        title: if is_composio_tool { "View on Composio" } else { "View on Smithery" },
                        dioxus_free_icons::Icon {
                            icon: dioxus_free_icons::icons::fi_icons::FiExternalLink
                        }
                    }
                }
            }
        }
    }
}

fn get_mcp_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("mcp_servers.json")
}
