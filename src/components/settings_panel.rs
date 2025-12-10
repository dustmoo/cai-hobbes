use dioxus::prelude::*;
use rfd;
use crate::settings::{Settings, SettingsManager};
use crate::{context::permissions::ToolCategory, secure_storage, session::SessionState};
use std::io::Write;
use crate::components::conflict_modal::ConflictModal;
use crate::components::confirm_save_modal::ConfirmSaveModal;
use zip::write::{FileOptions, ZipWriter};

#[component]
pub fn SettingsPanel() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut session_state = use_context::<Signal<SessionState>>();
    let _permission_manager = use_context::<Signal<crate::context::permissions::PermissionManager>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();

    // Create a local copy of the settings for editing.
    let mut local_settings = use_signal(|| settings.read().clone());

    // This signal will track if the local state differs from the global state.
    let mut has_unsaved_changes = use_signal(|| false);

    // Signals for model fetching
    let mut available_models = use_signal(|| Vec::<crate::services::gemini_models::GeminiModel>::new());
    let mut models_loading = use_signal(|| false);
    let mut models_error = use_signal(|| Option::<String>::None);
    let mut models_fetch_trigger = use_signal(|| 0u32);

    // Effect to fetch models when API key is available or refresh is triggered
    use_effect(move || {
        let api_key = local_settings.read().gemini_config.api_key.clone();
        let _trigger = models_fetch_trigger.read(); // Subscribe to trigger changes
        
        if api_key.is_some() {
            models_loading.set(true);
            models_error.set(None);
            
            spawn(async move {
                match crate::services::gemini_models::fetch_gemini_models(api_key.as_deref()).await {
                    Ok(models) => {
                        available_models.set(models);
                        models_loading.set(false);
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch models: {}", e);
                        models_error.set(Some(format!("Failed to load models: {}", e)));
                        models_loading.set(false);
                    }
                }
            });
        }
    });

    // This effect hook reactively checks for differences between the local and global settings.
    use_effect(move || {
        let global_settings = settings.read();
        let local = local_settings.read();
        has_unsaved_changes.set(*global_settings != *local);
    });

    // This effect synchronizes local_settings with global settings when the global state changes.
    // This is crucial for reflecting changes made elsewhere in the app (e.g., 'remember my choice' in a confirmation modal).
    // It uses .peek() to avoid creating a dependency on local_settings, preventing an infinite loop.
    // NOTE: This will discard any unsaved changes in the settings panel when external changes occur.
    use_effect(move || {
        let global_settings = settings.read();
        if *global_settings != *local_settings.peek() {
            local_settings.set(global_settings.clone());
        }
    });

    let mut llm_config_collapsed = use_signal(|| false);
    let mut app_behavior_collapsed = use_signal(|| false);
    let mut data_management_collapsed = use_signal(|| false);
    let mut permissions_collapsed = use_signal(|| false);
    let mut show_conflict_modal = use_signal(|| false);
    let mut show_confirm_save_modal = use_signal(|| false);
    let mut conflicting_sessions = use_signal(|| Vec::<(String, crate::session::Session)>::new());

    rsx! {
        div {
            class: "flex flex-col h-full p-4 bg-dark-bg text-white",
            h2 {
                class: "text-lg font-bold mb-4",
                "Settings"
            }
            if show_conflict_modal() {
                if let Some((id, _)) = conflicting_sessions.read().first() {
                    ConflictModal {
                        session_id: id.clone(),
                        on_resolve: move |(overwrite, apply_to_all)| {
                            let mut conflicts = conflicting_sessions.write();
                            if apply_to_all {
                                for (id, session) in conflicts.drain(..) {
                                    if overwrite {
                                        session_state.write().sessions.insert(id, session);
                                    }
                                }
                            } else {
                                let (id, session) = conflicts.remove(0);
                                if overwrite {
                                    session_state.write().sessions.insert(id, session);
                                }
                            }

                            if conflicts.is_empty() {
                                show_conflict_modal.set(false);
                                if let Err(e) = session_state.write().save() {
                                    tracing::error!("Failed to save session state after conflict resolution: {}", e);
                                }
                            }
                        }
                    }
                }
            }
            if show_confirm_save_modal() {
                ConfirmSaveModal {
                    is_visible: show_confirm_save_modal,
                    title: "Save Settings".to_string(),
                    message: "You have unsaved changes. Are you sure you want to save?".to_string(),
                    on_confirm: move |remember| {
                        if remember {
                            local_settings.write().confirm_on_save = false;
                        }
                        // 1. Commit the local changes to the global state
                        let mut global_settings = settings.write();
                        *global_settings = local_settings.read().clone();

                        // 2. Perform the save operations
                        let mut settings_to_save = global_settings.clone();
                        if let Some(api_key) = settings_to_save.gemini_config.api_key.take() {
                            if let Err(e) = secure_storage::save_secret("api_key", &api_key) {
                                tracing::error!("Failed to save API key: {}", e);
                            }
                        }
                        if let Some(smithery_api_key) = settings_to_save.smithery_api_key.take() {
                            let trimmed_key = smithery_api_key.trim().to_string();
                            if let Err(e) = secure_storage::save_secret("smithery_api_key", &trimmed_key) {
                                tracing::error!("Failed to save Smithery API key: {}", e);
                            }
                            // Put the trimmed key back in case we continue using settings_to_save (though we don't here)
                            // But cleaner to ensure we save the trimmed version if settings_to_save was used later.
                        }

                        if let Err(e) = settings_manager.read().save(&settings_to_save) {
                            tracing::error!("Failed to save settings: {}", e);
                        }
                        show_confirm_save_modal.set(false);
                    },
                    on_cancel: move |_| {
                        show_confirm_save_modal.set(false);
                    }
                }
            }
            div {
                class: "flex-grow overflow-y-auto pr-2",
                // LLM Configuration Section
                div {
                    class: "border border-primary-700 rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-dark-section rounded-t-lg",
                        onclick: move |_| llm_config_collapsed.set(!llm_config_collapsed()),
                        h3 { class: "text-md font-semibold", "LLM Configuration" }
                        span { if *llm_config_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !llm_config_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "LLM Provider" }
                                select {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    option { value: "Gemini", "Gemini" }
                                }
                            }
                            if local_settings.read().active_llm == crate::settings::LlmProvider::Gemini {
                                div {
                                    class: "pl-4 border-l-2 border-primary-700",
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-gray-300", "API Key" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Using environment variable",
                                            value: "{local_settings.read().gemini_config.api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| local_settings.write().gemini_config.api_key = Some(event.value())
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-gray-300", "Chat Model" }
                                            if local_settings.read().gemini_config.api_key.is_some() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-gray-500 disabled:cursor-not-allowed",
                                                    disabled: *models_loading.read(),
                                                    onclick: move |_| {
                                                        crate::services::gemini_models::clear_models_cache();
                                                        models_fetch_trigger.set(models_fetch_trigger() + 1);
                                                    },
                                                    if *models_loading.read() { "Loading..." } else { "↻ Refresh" }
                                                }
                                            }
                                        }
                                        if local_settings.read().gemini_config.api_key.is_none() {
                                            p {
                                                class: "mt-1 text-sm text-gray-400 italic",
                                                "Please configure your API key above to load available models"
                                            }
                                        } else if *models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-gray-400 italic",
                                                "Loading available models..."
                                            }
                                        } else if let Some(error) = models_error.read().as_ref() {
                                            p {
                                                class: "mt-1 text-sm text-red-400",
                                                "{error}"
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                onchange: move |event| {
                                                    local_settings.write().gemini_config.chat_model = event.value();
                                                },
                                                for model in available_models.read().iter() {
                                                    option {
                                                        value: "{model.name}",
                                                        selected: local_settings.read().gemini_config.chat_model == model.name,
                                                        "{model.display_name}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-gray-300", "Summary Model" }
                                            if local_settings.read().gemini_config.api_key.is_some() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-gray-500 disabled:cursor-not-allowed",
                                                    disabled: *models_loading.read(),
                                                    onclick: move |_| {
                                                        crate::services::gemini_models::clear_models_cache();
                                                        models_fetch_trigger.set(models_fetch_trigger() + 1);
                                                    },
                                                    if *models_loading.read() { "Loading..." } else { "↻ Refresh" }
                                                }
                                            }
                                        }
                                        if local_settings.read().gemini_config.api_key.is_none() {
                                            p {
                                                class: "mt-1 text-sm text-gray-400 italic",
                                                "Please configure your API key above to load available models"
                                            }
                                        } else if *models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-gray-400 italic",
                                                "Loading available models..."
                                            }
                                        } else if let Some(error) = models_error.read().as_ref() {
                                            p {
                                                class: "mt-1 text-sm text-red-400",
                                                "{error}"
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                onchange: move |event| {
                                                    local_settings.write().gemini_config.summary_model = event.value();
                                                },
                                                for model in available_models.read().iter() {
                                                    option {
                                                        value: "{model.name}",
                                                        selected: local_settings.read().gemini_config.summary_model == model.name,
                                                        "{model.display_name}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    
                                    // Thinking Mode Section
                                    div {
                                        class: "mb-4 pt-4 border-t border-primary-700",
                                        div {
                                            class: "flex items-center justify-between mb-2",
                                            label { class: "block text-sm font-medium text-gray-300", "Thinking Mode" }
                                            label {
                                                class: "relative inline-flex items-center cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "sr-only peer",
                                                    checked: local_settings.read().gemini_config.thinking_enabled,
                                                    onchange: move |event| {
                                                        local_settings.write().gemini_config.thinking_enabled = event.checked();
                                                    }
                                                }
                                                div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-gray-400 mb-3",
                                            "Enable extended reasoning for complex tasks. Gemini 3 Pro uses thinking level, Gemini 2.5 uses thinking budget."
                                        }
                                        
                                        if local_settings.read().gemini_config.thinking_enabled {
                                            div {
                                                class: "mb-3",
                                                label { class: "block text-sm font-medium text-gray-300 mb-1", "Thinking Level (Gemini 3 Pro)" }
                                                select {
                                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                    onchange: move |event| {
                                                        local_settings.write().gemini_config.thinking_level = event.value();
                                                    },
                                                    option { value: "low", selected: local_settings.read().gemini_config.thinking_level == "low", "Low" }
                                                    option { value: "high", selected: local_settings.read().gemini_config.thinking_level == "high", "High (Default)" }
                                                }
                                            }
                                            
                                            div {
                                                class: "mb-3",
                                                label { class: "block text-sm font-medium text-gray-300 mb-1", "Thinking Budget (Gemini 2.5)" }
                                                input {
                                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                    r#type: "number",
                                                    placeholder: "Leave empty for model default",
                                                    value: "{local_settings.read().gemini_config.thinking_budget.map(|v| v.to_string()).unwrap_or_default()}",
                                                    oninput: move |event| {
                                                        let val = event.value();
                                                        local_settings.write().gemini_config.thinking_budget = if val.is_empty() {
                                                            None
                                                        } else {
                                                            val.parse::<i32>().ok()
                                                        };
                                                    }
                                                }
                                                p {
                                                    class: "text-xs text-gray-400 mt-1",
                                                    "Higher values allow more reasoning tokens (increases cost)"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Smithery API Key Section
                div {
                    class: "border border-primary-700 rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-dark-section rounded-t-lg",
                        onclick: move |_| llm_config_collapsed.set(!llm_config_collapsed()),
                        h3 { class: "text-md font-semibold", "Smithery.ai Configuration" }
                        span { if *llm_config_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !llm_config_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Smithery API Key" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "password",
                                    placeholder: "Enter your Smithery.ai API key",
                                    value: "{local_settings.read().smithery_api_key.as_deref().unwrap_or(\"\")}",
                                    oninput: move |event| local_settings.write().smithery_api_key = Some(event.value().trim().to_string())
                                }
                            }
                            div {
                                class: "mb-4 pt-4 border-t border-primary-700",
                                label { class: "block text-sm font-medium text-gray-300 mb-2", "Preferred MCP Source" }
                                div {
                                    class: "flex space-x-4",
                                    button {
                                        class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                            "flex-1 px-4 py-2 rounded-md bg-primary-600 text-white font-medium shadow-sm ring-2 ring-primary-400"
                                        } else {
                                            "flex-1 px-4 py-2 rounded-md bg-dark-input text-gray-400 font-medium hover:bg-gray-700 hover:text-white transition-colors"
                                        },
                                        onclick: move |_| {
                                            local_settings.write().preferred_mcp_source = crate::settings::McpSource::Smithery;
                                        },
                                        "Smithery.ai"
                                    }
                                    button {
                                        class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                            "flex-1 px-4 py-2 rounded-md bg-primary-600 text-white font-medium shadow-sm ring-2 ring-primary-400"
                                        } else {
                                            "flex-1 px-4 py-2 rounded-md bg-dark-input text-gray-400 font-medium hover:bg-gray-700 hover:text-white transition-colors"
                                        },
                                        onclick: move |_| {
                                            local_settings.write().preferred_mcp_source = crate::settings::McpSource::Composio;
                                        },
                                        "Composio"
                                    }
                                }
                                    p {
                                    class: "text-xs text-gray-400 mt-2",
                                    "Choose which registry to use when installing new MCP servers. Smithery uses a hosted proxy (requires API key), while Composio runs locally."
                                }
                                
                                if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                    div {
                                        class: "mt-4",
                                        label { class: "block text-sm font-medium text-gray-300", "Composio Server URL" }
                                        div {
                                            class: "mt-1 flex items-center",
                                            input {
                                                class: "flex-grow px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().composio_base_url.clone().unwrap_or_default()}",
                                                placeholder: "e.g., https://backend.composio.dev/v3/mcp/...",
                                                oninput: move |event| {
                                                    let val = event.value();
                                                    local_settings.write().composio_base_url = if val.is_empty() { None } else { Some(val) };
                                                }
                                            }
                                            button {
                                                class: "ml-2 px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600",
                                                onclick: move |_| {
                                                    let url = local_settings.read().composio_base_url.clone();
                                                    if let Some(url) = url {
                                                        if !url.is_empty() {
                                                            let mcp_manager = mcp_manager.clone();
                                                            let mcp_context = mcp_context.clone();
                                                            let settings_val = settings.read().clone();
                                                            spawn(async move {
                                                                let config = crate::mcp::manager::McpServerConfig {
                                                                    name: "composio-server".to_string(),
                                                                    command: None,
                                                                    uri: Some(url),
                                                                    args: None,
                                                                    description: "Composio Master MCP Server".to_string(),
                                                                    env: std::collections::HashMap::new(),
                                                                    disabled: false,
                                                                    always_allow: vec![],
                                                                };
                                                                // Use the same path resolution logic as main.rs
                                                                let config_path = dirs::config_dir().unwrap_or_default().join("com.hobbes.app").join("mcp_servers.json");
                                                                
                                                                if let Err(e) = mcp_manager.read().add_or_update_mcp_server(&config_path, config).await {
                                                                    tracing::error!("Failed to save Composio server config: {}", e);
                                                                }
                                                                
                                                                // Trigger connection
                                                                if let Err(e) = mcp_manager.read().retry_server("composio-server", mcp_context, settings_val, None).await {
                                                                     tracing::error!("Failed to connect to Composio server: {}", e);
                                                                } else {
                                                                    tracing::info!("Successfully connected to Composio server");
                                                                }
                                                            });
                                                        }
                                                    }
                                                },
                                                "Connect"
                                            }
                                        }
                                        p {
                                            class: "text-xs text-gray-400 mt-1",
                                            "Enter the SSE URL from your Composio Dashboard and click Connect."
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Application Behavior Section
                div {
                    class: "border border-primary-700 rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-dark-section rounded-t-lg",
                        onclick: move |_| app_behavior_collapsed.set(!app_behavior_collapsed()),
                        h3 { class: "text-md font-semibold", "Application Behavior" }
                        span { if *app_behavior_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !app_behavior_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Chat History Length" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().chat_history_length}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<usize>() {
                                            local_settings.write().chat_history_length = val;
                                        }
                                    }
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Max Tool Output Length" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().max_tool_output_length}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<usize>() {
                                            local_settings.write().max_tool_output_length = val;
                                        }
                                    }
                                }
                                p {
                                    class: "text-xs text-gray-400 mt-1",
                                    "Limits the size of tool outputs in history to save tokens. Default is 2000."
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Max Active Tool Output Length" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().max_active_tool_output_length}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<usize>() {
                                            local_settings.write().max_active_tool_output_length = val;
                                        }
                                    }
                                }
                                p {
                                    class: "text-xs text-gray-400 mt-1",
                                    "Safety limit for the CURRENT tool response to prevent API errors. Default is 500,000."
                                }
                            }
                            div {
                                class: "mt-4 mb-4 flex items-center justify-between",
                                label { class: "block text-sm font-medium text-gray-300", "Show Tray Icon" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().show_tray_icon,
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().show_tray_icon = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            div {
                                class: "mt-4 mb-4 flex items-center justify-between",
                                label { class: "block text-sm font-medium text-gray-300", "Confirm Before Deleting Sessions" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().confirm_on_delete,
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().confirm_on_delete = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            div {
                                class: "mt-4 mb-4 flex items-center justify-between",
                                label { class: "block text-sm font-medium text-gray-300", "Confirm Before Saving Settings" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().confirm_on_save,
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().confirm_on_save = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            div {
                                class: "mt-4 mb-4 flex items-center justify-between",
                                label { class: "block text-sm font-medium text-gray-300", "Confirm Before Deleting Messages" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().confirm_on_message_delete,
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().confirm_on_message_delete = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            div {
                                class: "mt-4 mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Global Hotkey" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "text",
                                    value: "{local_settings.read().global_hotkey}",
                                    oninput: move |event| local_settings.write().global_hotkey = event.value()
                                }
                            }
                            div {
                                class: "mt-4 mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Max AI Turns" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().permission_settings.max_ai_turns}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<u32>() {
                                            local_settings.write().permission_settings.max_ai_turns = val;
                                        }
                                    }
                                }
                            }
                            div {
                                class: "mt-4 mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "What should Hobbes call you?" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    r#type: "text",
                                    placeholder: "e.g., Dustin",
                                    value: "{local_settings.read().user_name.as_deref().unwrap_or(\"\")}",
                                    oninput: move |event| {
                                        let value = event.value();
                                        if value.is_empty() {
                                            local_settings.write().user_name = None;
                                        } else {
                                            local_settings.write().user_name = Some(value);
                                        }
                                    }
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Persona" }
                                textarea {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    rows: "4",
                                    value: "{local_settings.read().persona}",
                                    oninput: move |event| local_settings.write().persona = event.value()
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Force Tool Use Instruction" }
                                textarea {
                                    class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    rows: "4",
                                    value: "{local_settings.read().force_tool_use_instruction.as_deref().unwrap_or(\"\")}",
                                    oninput: move |event| local_settings.write().force_tool_use_instruction = Some(event.value())
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Project Folder" }
                                div {
                                    class: "mt-1 flex items-center",
                                    p {
                                        class: "flex-grow px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                        "{local_settings.read().project_folder.clone().unwrap_or(\"None\".to_string())}"
                                    }
                                    button {
                                        class: "ml-2 px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600",
                                        onclick: move |_| {
                                            spawn(async move {
                                                if let Some(folder_path) = rfd::AsyncFileDialog::new().pick_folder().await {
                                                    local_settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                                }
                                            });
                                        },
                                        "Select Folder"
                                    }
                                }
                            }
                        }
                    }
                }

                // Data Management Section
                div {
                    class: "border border-primary-700 rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-dark-section rounded-t-lg",
                        onclick: move |_| data_management_collapsed.set(!data_management_collapsed()),
                        h3 { class: "text-md font-semibold", "Data Management" }
                        span { if *data_management_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !data_management_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex space-x-2",
                                button {
                                    class: "px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_settings.zip").save_file().await {
                                                let settings_json = serde_json::to_string_pretty(&*settings.read()).unwrap();
                                                let mut zip_buffer = Vec::new();
                                                {
                                                    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
                                                    zip.start_file("settings.json", FileOptions::default()).unwrap();
                                                    zip.write_all(settings_json.as_bytes()).unwrap();
                                                    zip.finish().unwrap();
                                                }
                                                if let Err(e) = std::fs::write(path.path(), &zip_buffer) {
                                                    tracing::error!("Failed to save settings export: {}", e);
                                                }
                                            }
                                        });
                                    },
                                    "Export Settings"
                                }
                                button {
                                    class: "px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_settings.zip").pick_file().await {
                                                let file = match std::fs::File::open(path.path()) {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("Failed to open file: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut archive = match zip::ZipArchive::new(file) {
                                                    Ok(a) => a,
                                                    Err(e) => {
                                                        tracing::error!("Failed to read zip archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut settings_file = match archive.by_name("settings.json") {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("'settings.json' not found in archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut contents = String::new();
                                                if let Err(e) = std::io::Read::read_to_string(&mut settings_file, &mut contents) {
                                                    tracing::error!("Failed to read settings.json from archive: {}", e);
                                                    return;
                                                }
                                                match serde_json::from_str::<Settings>(&contents) {
                                                    Ok(imported_settings) => {
                                                        local_settings.set(imported_settings);
                                                        tracing::info!("Successfully imported settings. Review and save.");
                                                    },
                                                    Err(e) => {
                                                        tracing::error!("Failed to parse imported settings.json: {}", e);
                                                    }
                                                }
                                            }
                                        });
                                    },
                                    "Import Settings"
                                }
                            }
                            div {
                                class: "flex space-x-2 mt-2",
                                button {
                                    class: "px-4 py-2 bg-secondary-500 rounded-md text-white font-semibold hover:bg-secondary-600",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_history.zip").save_file().await {
                                                let history_json = serde_json::to_string_pretty(&*session_state.read()).unwrap();
                                                let mut zip_buffer = Vec::new();
                                                {
                                                    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
                                                    zip.start_file("history.json", FileOptions::default()).unwrap();
                                                    zip.write_all(history_json.as_bytes()).unwrap();
                                                    zip.finish().unwrap();
                                                }
                                                if let Err(e) = std::fs::write(path.path(), &zip_buffer) {
                                                    tracing::error!("Failed to save history export: {}", e);
                                                }
                                            }
                                        });
                                    },
                                    "Export History"
                                }
                                button {
                                    class: "px-4 py-2 bg-secondary-500 rounded-md text-white font-semibold hover:bg-secondary-600",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_history.zip").pick_file().await {
                                                let file = match std::fs::File::open(path.path()) {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("Failed to open file: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut archive = match zip::ZipArchive::new(file) {
                                                    Ok(a) => a,
                                                    Err(e) => {
                                                        tracing::error!("Failed to read zip archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut history_file = match archive.by_name("history.json") {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("'history.json' not found in archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut contents = String::new();
                                                if let Err(e) = std::io::Read::read_to_string(&mut history_file, &mut contents) {
                                                    tracing::error!("Failed to read history.json from archive: {}", e);
                                                    return;
                                                }
                                                match serde_json::from_str::<SessionState>(&contents) {
                                                    Ok(imported_state) => {
                                                        let mut current_state = session_state.write();
                                                        for (id, session) in imported_state.sessions {
                                                            if current_state.sessions.contains_key(&id) {
                                                                conflicting_sessions.write().push((id, session));
                                                                // TODO: Implement conflict resolution modal
                                                            } else {
                                                                current_state.sessions.insert(id, session);
                                                            }
                                                        }
                                        
                                                        if !conflicting_sessions.read().is_empty() {
                                                            show_conflict_modal.set(true);
                                                        } else {
                                                            if let Err(e) = current_state.save() {
                                                                tracing::error!("Failed to save updated session state: {}", e);
                                                            } else {
                                                                tracing::info!("Successfully imported history with no conflicts.");
                                                            }
                                                        }
                                                    },
                                                    Err(e) => {
                                                        tracing::error!("Failed to parse imported history.json: {}", e);
                                                    }
                                                }
                                            }
                                        });
                                    },
                                    "Import History"
                                }
                            }
                        }
                    }
                }

                // Permissions Section
                div {
                    class: "border border-primary-700 rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-dark-section rounded-t-lg",
                        onclick: move |_| permissions_collapsed.set(!permissions_collapsed()),
                        h3 { class: "text-md font-semibold", "Permissions" }
                        span { if *permissions_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !permissions_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex items-center justify-between mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Enable Auto-Approval" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().permission_settings.auto_approval_enabled,
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().permission_settings.auto_approval_enabled = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            if local_settings.read().permission_settings.auto_approval_enabled {
                                div {
                                    class: "mb-2 pl-4 border-l-2 border-primary-700",
                                    div {
                                        class: "flex items-center justify-between mb-4",
                                        label { "MCP Tools" }
                                        label {
                                            class: "relative inline-flex items-center cursor-pointer",
                                            input {
                                                r#type: "checkbox",
                                                class: "sr-only peer",
                                                checked: local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false),
                                                oninput: move |event| {
                                                    if let Some(checked) = event.value().parse().ok() {
                                                        local_settings.write().permission_settings.granular_permissions.insert(ToolCategory::Mcp, checked);
                                                    }
                                                }
                                            }
                                            div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

            }
            button {
                class: if has_unsaved_changes() {
                    "mt-4 px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600 focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-opacity-50 transition-colors"
                } else {
                    "mt-4 px-4 py-2 bg-gray-600 rounded-md text-white font-semibold cursor-not-allowed"
                },
                disabled: !has_unsaved_changes(),
                onclick: move |_| {
                    if has_unsaved_changes() {
                        if local_settings.read().confirm_on_save {
                            show_confirm_save_modal.set(true);
                        } else {
                            // 1. Commit the local changes to the global state
                            let mut global_settings = settings.write();
                            *global_settings = local_settings.read().clone();
    
                            // 2. Perform the save operations
                            let mut settings_to_save = global_settings.clone();
                            if let Some(api_key) = settings_to_save.gemini_config.api_key.take() {
                                if let Err(e) = secure_storage::save_secret("api_key", &api_key) {
                                    tracing::error!("Failed to save API key: {}", e);
                                }
                            }
                           if let Some(smithery_api_key) = settings_to_save.smithery_api_key.take() {
                               if let Err(e) = secure_storage::save_secret("smithery_api_key", &smithery_api_key) {
                                   tracing::error!("Failed to save Smithery API key: {}", e);
                               }
                           }
                            if let Err(e) = settings_manager.read().save(&settings_to_save) {
                                tracing::error!("Failed to save settings: {}", e);
                            }
                        }
                    }
                },
                "Save Settings"
            }
        }
    }
}