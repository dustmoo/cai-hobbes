use crate::mcp::composio_client::ComposioClient;
use crate::secret_manager::SecretManager;
use crate::SecretManagerTrait;
use crate::settings::Settings;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::collections::{HashMap, HashSet};

/// Helper to reload credentials from SecretManager into local state
fn refresh_credentials(
    secret_manager: Signal<SecretManager>,
    profile_name: Option<String>,
    mut credentials_signal: Signal<HashMap<String, Vec<(String, String)>>>,
) {
    let sm = secret_manager.read();
    // Pattern 153.1: Only show credentials for the active profile in the UI
    let raw_map = sm.get_custom_tool_credentials(profile_name.as_deref());
    let mut sorted_map = HashMap::new();
    for (slug, fields) in raw_map {
        let mut field_list: Vec<(String, String)> = fields.into_iter().collect();
        field_list.sort_by(|a, b| a.0.cmp(&b.0));
        sorted_map.insert(slug, field_list);
    }
    credentials_signal.set(sorted_map);
}

/// Component for managing Custom Tool Credentials (BYOA)
#[component]
pub fn ToolCredentials() -> Element {
    let mut secret_manager = use_context::<Signal<SecretManager>>();
    let settings = use_context::<Signal<Settings>>();
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();

    // Local state for the form
    let mut selected_slug = use_signal(String::new);
    let mut new_field = use_signal(String::new);
    let mut new_value = use_signal(String::new);

    // Sync from global UI state (redirection)
    use_effect(move || {
        if let Some(slug) = ui_state.read().selected_byoa_slug.clone() {
            if !slug.is_empty() {
                selected_slug.set(slug);
                // Clear the trigger after consuming
                spawn(async move {
                    ui_state.write().selected_byoa_slug = None;
                });
            }
        }
    });

    // State for the list of credentials
    let credentials = use_signal(HashMap::<String, Vec<(String, String)>>::new);
    let mut show_values = use_signal(HashMap::<String, bool>::new);

    // Toolkit dropdown state
    let available_toolkits = use_signal(Vec::<(String, String, bool)>::new); // (slug, display_name, is_connected)
    let toolkits_loading = use_signal(|| false);
    let toolkits_error = use_signal(|| Option::<String>::None);

    // Filter state for dropdown
    let mut toolkit_filter = use_signal(String::new);
    // Combobox state
    let mut dropdown_open = use_signal(|| false);
    let mut highlighted_index = use_signal(|| 0usize);

    // State for schema discovery
    let mut selected_toolkit_listing = use_signal(|| Option::<crate::mcp::composio_client::models::ComposioToolkitListing>::None);
    let mut listing_loading = use_signal(|| false);

    // Fetch toolkit listing when slug is selected
    use_effect(move || {
        let slug = selected_slug.read().clone();
        if slug.is_empty() {
            selected_toolkit_listing.set(None);
            return;
        }

        let settings_snapshot = settings.peek().clone();
        spawn(async move {
            listing_loading.set(true);
            if let Some(profile) = settings_snapshot.get_active_profile() {
                if let Some(api_key) = &profile.api_key {
                    let base_url = profile.base_url.clone().unwrap_or_else(|| "https://backend.composio.dev/v3/mcp".to_string());
                    let client = ComposioClient::new(api_key.clone(), base_url, profile.entity_id.clone(), profile.user_id.clone(), profile.id.clone());
                    
                    match crate::mcp::composio_client::discovery::get_toolkit_metadata(&client, &slug).await {
                        Ok(listing) => {
                            tracing::info!("Discovered schema for toolkit {}: {:?}", slug, listing.auth_config);
                            selected_toolkit_listing.set(Some(listing));
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch metadata for toolkit {}: {}", slug, e);
                            selected_toolkit_listing.set(None);
                        }
                    }
                }
            }
            listing_loading.set(false);
        });
    });

    // Initial load of existing credentials
    use_effect(move || {
        let p_name = settings.peek().get_active_profile().map(|p| p.name.clone());
        refresh_credentials(secret_manager, p_name, credentials);
    });

    // Fetch available toolkits AND connected slugs from Composio API
    use_effect(move || {
        let settings_snapshot = settings.read().clone();
        let mut available_toolkits = available_toolkits;
        let mut toolkits_loading = toolkits_loading;
        let mut toolkits_error = toolkits_error;

        spawn(async move {
            toolkits_loading.set(true);
            toolkits_error.set(None);

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

                    // Fetch connected toolkit slugs first
                    let connected_slugs: HashSet<String> = client
                        .get_connected_toolkit_slugs()
                        .await
                        .unwrap_or_default();

                    // Fetch all toolkits
                    match client.list_all_toolkits(None, None, Some(100), None, Some("name")).await {
                        Ok((toolkits, _, _)) => {
                            let mut toolkit_list: Vec<(String, String, bool)> = toolkits
                                .into_iter()
                                .map(|t| {
                                    let is_connected = connected_slugs.contains(&t.slug.to_lowercase());
                                    (t.slug.clone(), t.name.clone(), is_connected)
                                })
                                .collect();

                            // Sort: connected first, then alphabetically by name
                            toolkit_list.sort_by(|a, b| {
                                match (a.2, b.2) {
                                    (true, false) => std::cmp::Ordering::Less,
                                    (false, true) => std::cmp::Ordering::Greater,
                                    _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
                                }
                            });

                            available_toolkits.set(toolkit_list);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch toolkits for credentials dropdown: {}", e);
                            toolkits_error.set(Some(format!("Failed to load toolkits: {}", e)));
                        }
                    }
                } else {
                    toolkits_error.set(Some("No Composio API key configured.".to_string()));
                }
            } else {
                toolkits_error.set(Some("No active Composio profile.".to_string()));
            }

            toolkits_loading.set(false);
        });
    });

    // Filter toolkits based on search input
    let filtered_toolkits = {
        let filter = toolkit_filter.read().to_lowercase();
        let all_toolkits = available_toolkits.read();
        if filter.is_empty() {
            all_toolkits.clone()
        } else {
            all_toolkits
                .iter()
                .filter(|(slug, name, _)| {
                    slug.to_lowercase().contains(&filter) || name.to_lowercase().contains(&filter)
                })
                .cloned()
                .collect()
        }
    };

    // Action handlers
    let handle_add = move |_| {
        let slug = selected_slug.read().trim().to_string();
        let field = new_field.read().trim().to_string();
        let value = new_value.read().trim().to_string();

        if slug.is_empty() || field.is_empty() || value.is_empty() {
            return;
        }

        let p_name = settings.peek().get_active_profile().map(|p| p.name.clone());

        spawn(async move {
            let result = {
                let mut sm = secret_manager.write();
                sm.set_custom_tool_credential(p_name.as_deref(), &slug, &field, value)
            };

            match result {
                Ok(_) => {
                    selected_slug.set(String::new());
                    new_field.set(String::new());
                    new_value.set(String::new());
                    toolkit_filter.set(String::new());
                    let p_name = settings.peek().get_active_profile().map(|p| p.name.clone());
                    refresh_credentials(secret_manager, p_name, credentials);
                }
                Err(e) => {
                    tracing::error!("Failed to save credential: {}", e);
                }
            }
        });
    };

    let handle_delete = move |slug: String, field: String| {
        let p_name = settings.peek().get_active_profile().map(|p| p.name.clone());
        spawn(async move {
            let result = {
                let mut sm = secret_manager.write();
                sm.delete_custom_tool_credential(p_name.as_deref(), &slug, &field)
            };
            if let Err(e) = result {
                tracing::error!("Failed to delete credential: {}", e);
            } else {
                let p_name = settings.peek().get_active_profile().map(|p| p.name.clone());
                refresh_credentials(secret_manager, p_name, credentials);
            }
        });
    };

    rsx! {
        div {
            class: "flex flex-col space-y-4",

            div {
                class: "p-4 bg-section border border-subtle rounded-lg",
                h3 { class: "text-md font-semibold text-fg mb-2", "Custom Tool Credentials (BYOA)" }
                p { class: "text-sm text-fg-muted",
                    "Add your own Client IDs and Secrets for tools that require custom authentication (e.g., LinkedIn, Google)."
                }
                
                // LinkedIn-specific guidance (Pattern 25: Security Master)
                if selected_slug.read().to_lowercase().contains("linkedin") {
                    div { class: "mt-3 p-3 bg-blue-900/30 border border-blue-700/50 rounded-md",
                        h5 { class: "text-xs font-semibold text-blue-300 mb-1", "LinkedIn App Limits Detected?" }
                        p { class: "text-xs text-blue-100/70 leading-relaxed",
                            "If you hit 'App Level Rate Limits', you should create your own LinkedIn App at "
                            a { 
                                class: "text-blue-400 hover:underline", 
                                href: "https://www.linkedin.com/developers/apps",
                                "LinkedIn Developers"
                            }
                            ". Then add your "
                            span { class: "font-mono text-blue-300", "client_id" }
                            " and "
                            span { class: "font-mono text-blue-300", "client_secret" }
                            " below to use 'Bring Your Own App' (BYOA) mode."
                        }
                    }
                }
            }

            div {
                class: "p-4 bg-section border border-subtle rounded-lg",
                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Add New Credential" }

                // Error message for toolkit loading
                if let Some(err) = toolkits_error.read().as_ref() {
                    div {
                        class: "mb-3 p-2 bg-red-900/50 text-red-300 text-sm rounded",
                        "{err}"
                    }
                }

                div { class: "grid grid-cols-1 md:grid-cols-3 gap-3 mb-3",
                    div { class: "relative",
                        label { class: "block text-xs font-medium text-fg-muted mb-1", "Toolkit" }
                        if *toolkits_loading.read() {
                            div {
                                class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm text-fg-muted",
                                "Loading toolkits..."
                            }
                        } else {
                            // Combobox input
                            input {
                                class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm text-fg focus:ring-1 focus:ring-primary-500",
                                placeholder: "Select toolkit...",
                                value: if selected_slug.read().is_empty() {
                                    toolkit_filter.read().clone()
                                } else {
                                    // Show the selected slug in the input
                                    selected_slug.read().clone()
                                },
                                onfocus: move |_| {
                                    dropdown_open.set(true);
                                    // If there's a selected slug, clear it to allow re-filtering
                                    if !selected_slug.read().is_empty() {
                                        toolkit_filter.set(selected_slug.read().clone());
                                        selected_slug.set(String::new());
                                    }
                                },
                                oninput: move |e| {
                                    toolkit_filter.set(e.value());
                                    selected_slug.set(String::new());
                                    dropdown_open.set(true);
                                    highlighted_index.set(0);
                                },
                                onkeydown: move |e| {
                                    let key = e.key();
                                    let list_len = filtered_toolkits.len();
                                    match key {
                                        Key::ArrowDown => {
                                            e.prevent_default();
                                            if list_len > 0 {
                                                let current = *highlighted_index.read();
                                                highlighted_index.set((current + 1) % list_len);
                                            }
                                        }
                                        Key::ArrowUp => {
                                            e.prevent_default();
                                            if list_len > 0 {
                                                let current = *highlighted_index.read();
                                                highlighted_index.set(if current == 0 { list_len - 1 } else { current - 1 });
                                            }
                                        }
                                        Key::Enter => {
                                            e.prevent_default();
                                            if list_len > 0 {
                                                let idx = *highlighted_index.read();
                                                if let Some((slug, _, _)) = filtered_toolkits.get(idx) {
                                                    selected_slug.set(slug.clone());
                                                    toolkit_filter.set(String::new());
                                                    dropdown_open.set(false);
                                                }
                                            }
                                        }
                                        Key::Escape => {
                                            dropdown_open.set(false);
                                        }
                                        _ => {}
                                    }
                                },
                                onblur: move |_| {
                                    // Delay closing to allow click events to fire
                                    spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                                        dropdown_open.set(false);
                                    });
                                }
                            }
                            // Floating dropdown menu
                            if *dropdown_open.read() && !filtered_toolkits.is_empty() {
                                div {
                                    class: "absolute z-50 w-full mt-1 bg-card border border-primary-600 rounded-md shadow-lg max-h-48 overflow-y-auto",
                                    for (idx, (slug, name, _is_connected)) in filtered_toolkits.iter().enumerate() {
                                        div {
                                            key: "{slug}",
                                            class: if idx == *highlighted_index.read() {
                                                "px-3 py-2 text-sm cursor-pointer bg-btn-primary text-fg"
                                            } else {
                                                "px-3 py-2 text-sm cursor-pointer hover:bg-primary-700 text-fg-muted"
                                            },
                                            onmousedown: {
                                                let slug_clone = slug.clone();
                                                move |e: Event<MouseData>| {
                                                    e.prevent_default();
                                                    selected_slug.set(slug_clone.clone());
                                                    toolkit_filter.set(String::new());
                                                    dropdown_open.set(false);
                                                }
                                            },
                                            onmouseenter: {
                                                let idx_copy = idx;
                                                move |_| {
                                                    highlighted_index.set(idx_copy);
                                                }
                                            },
                                            "{name} ({slug})"
                                        }
                                    }
                                }
                            }
                            if *dropdown_open.read() && filtered_toolkits.is_empty() && !toolkit_filter.read().is_empty() {
                                div {
                                    class: "absolute z-50 w-full mt-1 bg-card border border-primary-600 rounded-md shadow-lg p-3",
                                    span { class: "text-xs text-fg-muted italic", "No toolkits match filter" }
                                }
                            }
                        }
                    }
                    div {
                        label { class: "block text-xs font-medium text-fg-muted mb-1", "Field Name (e.g. client_id)" }
                        
                        // Dynamic field suggestions based on schema
                        if let Some(listing) = selected_toolkit_listing.read().as_ref() {
                            if let Some(auth_config) = &listing.auth_config {
                                if let Some(fields) = &auth_config.expected_input_fields {
                                    div { class: "flex flex-wrap gap-1 mb-2",
                                        for field in fields {
                                            button {
                                                class: "px-2 py-0.5 bg-primary-900/40 hover:bg-primary-800/60 border border-subtle rounded text-[10px] text-primary-300 transition-colors",
                                                title: field.description.as_deref().unwrap_or("Click to use this field name"),
                                                onclick: {
                                                    let field_name = field.name.clone();
                                                    move |_| {
                                                        new_field.set(field_name.clone());
                                                    }
                                                },
                                                "{field.name}"
                                            }
                                        }
                                    }
                                }
                            }
                        } else if *listing_loading.read() {
                            div { class: "text-[10px] text-fg-muted italic mb-2", "Fetching suggested fields..." }
                        }

                        input {
                            class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm text-fg focus:ring-1 focus:ring-primary-500",
                            placeholder: "client_id",
                            value: "{new_field}",
                            oninput: move |e| new_field.set(e.value())
                        }
                    }
                    div {
                        label { class: "block text-xs font-medium text-fg-muted mb-1", "Value" }
                        input {
                            class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm text-fg focus:ring-1 focus:ring-primary-500",
                            r#type: "password",
                            placeholder: "Enter secret...",
                            value: "{new_value}",
                            oninput: move |e| new_value.set(e.value())
                        }
                    }
                }
                div { class: "flex justify-end",
                    button {
                        class: "px-4 py-2 bg-btn-primary hover:bg-btn-primary-hover text-fg rounded-md text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                        disabled: selected_slug.read().is_empty() || new_field.read().is_empty() || new_value.read().is_empty(),
                        onclick: handle_add,
                        Icon { width: 16, height: 16, icon: fi_icons::FiPlus, class: "mr-2 inline-block" }
                        "Save Credential"
                    }
                }
            }

            div {
                class: "space-y-4",
                for (slug, fields) in credentials.read().iter() {
                    div {
                        key: "{slug}",
                        class: "border border-faint rounded-lg overflow-hidden",
                        div { class: "bg-section/50 px-4 py-2 border-b border-faint flex items-center",
                            Icon { width: 16, height: 16, icon: fi_icons::FiTool, class: "text-primary-400 mr-2" }
                            span { class: "font-medium text-gray-200", "{slug}" }
                        }
                        div { class: "p-0",
                            for (field, value) in fields {
                                div {
                                    key: "{field}",
                                    class: "flex items-center justify-between px-4 py-3 border-b border-gray-800 last:border-0 hover:bg-white/5 transition-colors group",
                                    div { class: "flex items-center space-x-4 flex-1 min-w-0",
                                        span { class: "text-sm text-fg-muted font-mono shrink-0", "{field}" }
                                        div { class: "flex items-center space-x-2 flex-1 min-w-0 bg-app/50 px-2 py-1.5 rounded border border-gray-800",
                                            span {
                                                class: "text-xs font-mono text-fg-muted truncate flex-1",
                                                {
                                                    let key = format!("{}__{}", slug, field);
                                                    let is_visible = *show_values.read().get(&key).unwrap_or(&false);
                                                    if is_visible { value.clone() } else { "••••••••••••••••".to_string() }
                                                }
                                            }
                                            button {
                                                class: "text-fg-muted hover:text-fg-muted p-1 rounded transition-colors",
                                                onclick: {
                                                    let key = format!("{}__{}", slug, field);
                                                    move |_| {
                                                        let mut vis = show_values.write();
                                                        let current = *vis.get(&key).unwrap_or(&false);
                                                        vis.insert(key.clone(), !current);
                                                    }
                                                },
                                                if *show_values.read().get(&format!("{}__{}", slug, field)).unwrap_or(&false) {
                                                    Icon { width: 14, height: 14, icon: fi_icons::FiEyeOff }
                                                } else {
                                                    Icon { width: 14, height: 14, icon: fi_icons::FiEye }
                                                }
                                            }
                                        }
                                    }
                                    button {
                                        class: "ml-4 text-fg-muted hover:text-red-400 p-1.5 rounded transition-colors opacity-0 group-hover:opacity-100",
                                        title: "Delete credential",
                                        onclick: {
                                            let s = slug.clone();
                                            let f = field.clone();
                                            move |_| handle_delete(s.clone(), f.clone())
                                        },
                                        Icon { width: 16, height: 16, icon: fi_icons::FiTrash2 }
                                    }
                                }
                            }
                        }
                    }
                }
                if credentials.read().is_empty() {
                    div { class: "text-center py-8 text-fg-muted italic", "No custom credentials configured." }
                }
            }
        }
    }
}
