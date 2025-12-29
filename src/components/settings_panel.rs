use dioxus::prelude::*;
use rfd;
use crate::settings::{Settings, SettingsManager};
use crate::{context::permissions::ToolCategory, session::SessionState};
use crate::mcp::composio_client::validate_composio_api_key;
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
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();

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

                        // 2. Prepare data for background saving (Clone cheap stuff or take ownership)
                        let mut settings_to_save = global_settings.clone();
                        let smithery_key_opt = settings_to_save.smithery_api_key.clone();
                        let gemini_key_opt = settings_to_save.gemini_config.api_key.clone();
                        // Extract Composio keys to save
                        let composio_keys: Vec<(String, String)> = settings_to_save.composio_profiles.iter()
                            .filter_map(|p| p.api_key.as_ref().map(|k| (p.name.clone(), k.clone())))
                            .collect();
                        
                        // We also need to update the settings to store "trimmed" keys if we modify them logic-wise, 
                        // but for now let's just save what we have.
                        // Actually, the original logic trimmed the smithery key. Let's replicate that.
                        let smithery_key_to_save = smithery_key_opt.map(|k| k.trim().to_string());
                        if let Some(ref trimmed) = smithery_key_to_save {
                             settings_to_save.smithery_api_key = Some(trimmed.clone());
                        }

                        // Clone managers for async use
                        // We can use the signal directly inside spawn as they are Copy handles

                        // Wait, SettingsManager matches: pub struct SettingsManager { settings_path: PathBuf } -> derives?
                        // settings.rs:299: pub struct SettingsManager ... doesn't verify Clone. It was used as signal.
                        // Let's check settings.rs. It doesn't derive Clone!
                        // We can reconstruct it or just pass the path?
                        // Or we can just use the signal reader inside the spawn_blocking? No, signal is not Send.
                        
                        // Workaround: We only need `save` which writes to a path.
                        // Let's just capture the path if possible, or assume we can't easily clone SettingsManager.
                        // Actually, looking at code, `SettingsManager` is `Clone`? No, looking at `settings.rs` line 299: `pub struct SettingsManager { settings_path: PathBuf, }`.
                        // It does NOT derive Clone.
                        // However, we can construct a new one if we know the path. 
                        // Or better: `settings_manager` signal held by the component. 
                        // We can't pass the signal.
                        // We just need to write the file. `Settings` is serializable.
                        // We can use `std::fs::write` in the blocking task manually or implement a helper.
                        // The `save` method just does: `fs::write(&self.settings_path, content)`.
                        
                        // Strategy: We can't easily call `settings_manager.save` in background without cloning it. 
                        // Let's assume for now we keep `settings_manager.save` on main thread (file IO is fast-ish) 
                        // BUT definitely move Keychain IO (Composio/Smithery keys) to background.
                        
                        let mut secret_updates = Vec::new();
                        if let Some(k) = gemini_key_opt { secret_updates.push(("api_key".to_string(), k)); }
                        if let Some(k) = smithery_key_to_save { secret_updates.push(("smithery_api_key".to_string(), k)); }
                        tracing::debug!("Composio keys to save: {:?}", composio_keys.iter().map(|(n, _)| n).collect::<Vec<_>>());

                        spawn(async move {
                            // Validate Composio API keys before saving
                            let mut validated_composio_keys = Vec::new();
                            for (profile_name, key) in composio_keys {
                                if key.trim().is_empty() {
                                    continue; // Skip empty keys
                                }
                                match validate_composio_api_key(&key).await {
                                    Ok(()) => {
                                        tracing::info!("Composio API key for profile '{}' validated successfully", profile_name);
                                        validated_composio_keys.push((profile_name, key));
                                    }
                                    Err(e) => {
                                        tracing::error!("Invalid Composio API key for profile '{}': {}", profile_name, e);
                                        // Don't save invalid keys - they'll remain unchanged in keychain
                                    }
                                }
                            }
                            
                            // Build final secret updates with validated Composio keys
                            let mut final_secret_updates = secret_updates;
                            for (profile_name, key) in validated_composio_keys {
                                final_secret_updates.push((format!("{}{}", crate::secret_manager::COMPOSIO_KEY_PREFIX, profile_name), key));
                            }
                            tracing::debug!("Total validated secret updates: {}", final_secret_updates.len());

                            // Run Keychain operations in blocking task
                            let results = tokio::task::spawn_blocking(move || {
                                let mut saved = Vec::new();
                                for (key_name, key_value) in final_secret_updates {
                                    let save_result = crate::keychain_ffi::set_generic_password_with_biometric_protection(&key_name, &key_value)
                                        .or_else(|e| {
                                            if let crate::keychain_ffi::KeychainError::SecurityError(-34018) = e {
                                                crate::keychain_ffi::set_generic_password(&key_name, &key_value)
                                            } else {
                                                Err(e)
                                            }
                                        });
                                    if let Err(e) = save_result {
                                        tracing::error!("Failed to save secret {}: {}", key_name, e);
                                    } else {
                                        saved.push((key_name, key_value));
                                    }
                                }
                                saved
                            }).await;

                            // Back on main thread
                            match results {
                                Ok(saved_secrets) => {
                                    // Update SecretManager cache
                                    let mut sm = secret_manager.write();
                                    for (k, v) in saved_secrets {
                                        sm.update_cache(k, v);
                                    }
                                }
                                Err(e) => tracing::error!("Keychain task failed: {}", e),
                            }
                            
                            // Save settings.json (fast enough for main thread usually, or we could spawn another blocking task if we could clone path)
                            // Since we didn't solve the SettingsManager Clone issue easily without editing settings.rs, 
                            // we'll run this here. It's just a file write.
                            if let Err(e) = settings_manager.read().save(&settings_to_save) {
                                tracing::error!("Failed to save settings: {}", e);
                            }
                        });

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
                        h3 { class: "text-md font-semibold", "MCP Configuration" }
                        span { if *llm_config_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !llm_config_collapsed() {
                        div {
                            class: "p-4",
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
                                
                                // Smithery API Key - shown when Smithery is selected
                                if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                    div {
                                        class: "mt-4 pt-4 border-t border-primary-700",
                                        label { class: "block text-sm font-medium text-gray-300 mb-1", "Smithery API Key" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Enter your Smithery.ai API key",
                                            value: "{local_settings.read().smithery_api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| local_settings.write().smithery_api_key = Some(event.value().trim().to_string())
                                        }
                                        p {
                                            class: "text-xs text-gray-400 mt-1",
                                            "Required for Smithery.ai marketplace access"
                                        }
                                    }
                                }
                                
                                if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                    div {
                                        class: "mt-4 pt-4 border-t border-primary-700",
                                        // Profile list header
                                        div {
                                            class: "flex justify-between items-center mb-3",
                                            label { class: "block text-sm font-medium text-gray-300", "Composio Profiles" }
                                            button {
                                                class: "px-3 py-1 bg-primary-500 rounded-md text-white text-sm font-medium hover:bg-primary-600",
                                                onclick: move |_| {
                                                    let new_profile = crate::settings::ComposioProfile::default();
                                                    local_settings.write().add_profile(new_profile);
                                                },
                                                "+ Add Profile"
                                            }
                                        }
                                        
                                        // Profile list
                                        div {
                                            class: "space-y-2 mb-4",
                                            for profile in local_settings.read().composio_profiles.iter() {
                                                {
                                                    let profile_name = profile.name.clone();
                                                    let active_name = local_settings.read().active_composio_profile.clone();
                                                    let is_active = active_name.as_ref() == Some(&profile_name);
                                                    rsx! {
                                                        div {
                                                            class: format!("flex items-center justify-between p-2 rounded-md transition-all {}", 
                                                                if is_active { "bg-dark-input border border-primary-500 ring-1 ring-primary-500/20" } else { "bg-dark-input/50 border border-transparent hover:bg-dark-input" }),
                                                            div {
                                                                class: "flex items-center gap-3",
                                                                input {
                                                                    r#type: "radio",
                                                                    class: "w-4 h-4 text-primary-500 focus:ring-primary-500 bg-transparent border-gray-600",
                                                                    name: "active_profile",
                                                                    checked: is_active,
                                                                    onchange: {
                                                                        let name = profile_name.clone();
                                                                        move |_| {
                                                                            local_settings.write().active_composio_profile = Some(name.clone());
                                                                        }
                                                                    }
                                                                }
                                                                span { 
                                                                    class: format!("text-sm font-medium {}", if is_active { "text-white" } else { "text-gray-300" }),
                                                                    "{profile_name}" 
                                                                }
                                                                if is_active {
                                                                    span {
                                                                        class: "px-2 py-0.5 bg-blue-600 text-white rounded text-[10px] font-bold uppercase tracking-wider",
                                                                        "Active"
                                                                    }
                                                                }
                                                            }
                                                            button {
                                                                class: "text-xs font-medium text-red-500 hover:text-red-400 transition-colors uppercase tracking-tight",
                                                                onclick: {
                                                                    let name = profile_name.clone();
                                                                    move |_| {
                                                                        local_settings.write().remove_profile(&name);
                                                                    }
                                                                },
                                                                "Remove"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        
                                        // Edit active profile
                                        if let Some(active_name) = local_settings.read().active_composio_profile.clone() {
                                            div {
                                                class: "border border-primary-700 rounded-lg p-3",
                                                h4 { class: "text-sm font-medium text-gray-300 mb-3", "Edit Profile: {active_name}" }
                                                
                                                // Profile Name
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-gray-400 mb-1", "Profile Name" }
                                                    input {
                                                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                                                        value: "{active_name}",
                                                        oninput: {
                                                            let old_name = active_name.clone();
                                                            move |event: Event<FormData>| {
                                                                let new_name = event.value();
                                                                let mut settings = local_settings.write();
                                                                if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == old_name) {
                                                                    profile.name = new_name.clone();
                                                                }
                                                                if settings.active_composio_profile.as_ref() == Some(&old_name) {
                                                                    settings.active_composio_profile = Some(new_name);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                
                                                // Server URL
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-gray-400 mb-1", "Server URL" }
                                                    input {
                                                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                                                        placeholder: "https://backend.composio.dev/v3/mcp/0a4474b3-d8...",
                                                        value: "{local_settings.read().get_active_profile().and_then(|p| p.base_url.clone()).unwrap_or_default()}",
                                                        oninput: {
                                                            let name = active_name.clone();
                                                            move |event: Event<FormData>| {
                                                                let val = event.value();
                                                                let mut settings = local_settings.write();
                                                                if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                    profile.base_url = if val.is_empty() { None } else { Some(val) };
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                
                                                // API Key
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-gray-400 mb-1", "API Key" }
                                                    input {
                                                        r#type: "password",
                                                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                                                        placeholder: "Enter Composio API key",
                                                        value: "{local_settings.read().get_active_profile().and_then(|p| p.api_key.clone()).unwrap_or_default()}",
                                                        oninput: {
                                                            let name = active_name.clone();
                                                            move |event: Event<FormData>| {
                                                                let val = event.value();
                                                                let mut settings = local_settings.write();
                                                                if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                    profile.api_key = if val.is_empty() { None } else { Some(val) };
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                
                                                // User ID
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-gray-400 mb-1", "User ID" }
                                                    input {
                                                        class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm",
                                                        placeholder: "bb98696d-d833-4953-8857-...",
                                                        value: "{local_settings.read().get_active_profile().and_then(|p| p.user_id.clone()).unwrap_or_default()}",
                                                        oninput: {
                                                            let name = active_name.clone();
                                                            move |event: Event<FormData>| {
                                                                let val = event.value();
                                                                let mut settings = local_settings.write();
                                                                if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                    profile.user_id = if val.is_empty() { None } else { Some(val) };
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                
                                                // Connection status
                                                div {
                                                    class: "flex items-center justify-end gap-2 mt-2",
                                                    span { class: "h-2 w-2 rounded-full bg-green-500" }
                                                    span { class: "text-sm text-green-400", "Connected" }
                                                }
                                                p {
                                                    class: "text-xs text-gray-400 mt-2",
                                                    "Connection happens automatically when you select a profile."
                                                }
                                            }
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
                            // Directly perform the save (same logic as ConfirmSaveModal on_confirm)
                            // 1. Commit the local changes to the global state
                            let mut global_settings = settings.write();
                            *global_settings = local_settings.read().clone();

                            // 2. Prepare data for background saving
                            let mut settings_to_save = global_settings.clone();
                            let smithery_key_opt = settings_to_save.smithery_api_key.clone();
                            let gemini_key_opt = settings_to_save.gemini_config.api_key.clone();
                            let composio_keys: Vec<(String, String)> = settings_to_save.composio_profiles.iter()
                                .filter_map(|p| p.api_key.as_ref().map(|k| (p.name.clone(), k.clone())))
                                .collect();
                            
                            let smithery_key_to_save = smithery_key_opt.map(|k| k.trim().to_string());
                            if let Some(ref trimmed) = smithery_key_to_save {
                                 settings_to_save.smithery_api_key = Some(trimmed.clone());
                            }

                            let mut secret_updates = Vec::new();
                            if let Some(k) = gemini_key_opt { secret_updates.push(("api_key".to_string(), k)); }
                            if let Some(k) = smithery_key_to_save { secret_updates.push(("smithery_api_key".to_string(), k)); }
                            tracing::debug!("Composio keys to save: {:?}", composio_keys.iter().map(|(n, _)| n).collect::<Vec<_>>());

                            spawn(async move {
                                // Validate Composio API keys before saving
                                let mut validated_composio_keys = Vec::new();
                                for (profile_name, key) in composio_keys {
                                    if key.trim().is_empty() {
                                        continue; // Skip empty keys
                                    }
                                    match validate_composio_api_key(&key).await {
                                        Ok(()) => {
                                            tracing::info!("Composio API key for profile '{}' validated successfully", profile_name);
                                            validated_composio_keys.push((profile_name, key));
                                        }
                                        Err(e) => {
                                            tracing::error!("Invalid Composio API key for profile '{}': {}", profile_name, e);
                                            // Don't save invalid keys - they'll remain unchanged in keychain
                                        }
                                    }
                                }
                                
                                // Build final secret updates with validated Composio keys
                                let mut final_secret_updates = secret_updates;
                                for (profile_name, key) in validated_composio_keys {
                                    final_secret_updates.push((format!("{}{}", crate::secret_manager::COMPOSIO_KEY_PREFIX, profile_name), key));
                                }
                                tracing::debug!("Total validated secret updates: {}", final_secret_updates.len());

                                let results = tokio::task::spawn_blocking(move || {
                                    let mut saved = Vec::new();
                                    for (key_name, key_value) in final_secret_updates {
                                        let save_result = crate::keychain_ffi::set_generic_password_with_biometric_protection(&key_name, &key_value)
                                            .or_else(|e| {
                                                if let crate::keychain_ffi::KeychainError::SecurityError(-34018) = e {
                                                    crate::keychain_ffi::set_generic_password(&key_name, &key_value)
                                                } else {
                                                    Err(e)
                                                }
                                            });
                                        if let Err(e) = save_result {
                                            tracing::error!("Failed to save secret {}: {}", key_name, e);
                                        } else {
                                            saved.push((key_name, key_value));
                                        }
                                    }
                                    saved
                                }).await;

                                match results {
                                    Ok(saved_secrets) => {
                                        let mut sm = secret_manager.write();
                                        for (k, v) in saved_secrets {
                                            sm.update_cache(k, v);
                                        }
                                    }
                                    Err(e) => tracing::error!("Keychain task failed: {}", e),
                                }
                                
                                if let Err(e) = settings_manager.read().save(&settings_to_save) {
                                    tracing::error!("Failed to save settings: {}", e);
                                }
                            });
                        }
                    }
                },
                "Save Settings"
            }
        }
    }
}