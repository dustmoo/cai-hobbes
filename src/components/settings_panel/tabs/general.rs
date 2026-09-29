use super::super::SettingsCtx;
use super::super::shared::load_instance_into_draft;
use super::super::shared::selected_kind;
use crate::llm::GeminiModel;
use crate::settings::get_slot_icon;
use crate::settings::is_sandboxed;
use dioxus::prelude::*;

#[component]
pub(in super::super) fn GeneralTab(
    ctx: SettingsCtx,
    mut available_models: Signal<Vec<crate::services::gemini_models::GeminiModel>>,
    mut models_loading: Signal<bool>,
    mut models_error: Signal<Option<String>>,
    mut models_fetch_trigger: Signal<u32>,
    mut picker_open_for_slot: Signal<Option<usize>>,
    mut oai_available_models: Signal<Vec<String>>,
    mut oai_models_loading: Signal<bool>,
    mut oai_models_error: Signal<Option<String>>,
    mut oai_models_fetch_trigger: Signal<u32>,
    mut show_keychain_mode_confirm: Signal<bool>,
    mut pending_keychain_mode: Signal<Option<crate::settings::KeychainStorageMode>>,
) -> Element {
    let mut ui_state = ctx.ui_state;
    let ui_state_manager = ctx.ui_state_manager;
    let mut selected_connector_id = ctx.selected_connector_id;
    let mut local_settings = ctx.local_settings;

    let toggle_llm_collapsed = move |_| {
        {
            let mut state = ui_state.write();
            state.llm_config_collapsed = !state.llm_config_collapsed;
        }
        let state = (*ui_state.read()).clone();
        let manager = (*ui_state_manager.read()).clone();
        spawn(async move {
            let _ = manager.save(&state);
        });
    };

    rsx! {

                // LLM Configuration Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: toggle_llm_collapsed,
                        h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "LLM Configuration" }
                        span { if ui_state.read().llm_config_collapsed { "▶" } else { "▼" } }
                    }
                    if !ui_state.read().llm_config_collapsed {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-fg-muted mb-2", "Connectors" }
                                // Connector list: one row per configured instance
                                for instance in local_settings.read().llm_connectors.clone() {
                                    {
                                        let is_selected = selected_connector_id.read().as_deref() == Some(instance.id.as_str());
                                        let is_default = local_settings.read().active_connector().map(|c| c.id.clone()) == Some(instance.id.clone());
                                        let is_last = local_settings.read().llm_connectors.len() <= 1;
                                        let provider = instance.provider();
                                        let row_id = instance.id.clone();
                                        let default_id = instance.id.clone();
                                        let delete_id = instance.id.clone();
                                        let rename_id = instance.id.clone();

                                        rsx! {
                                            div {
                                                class: if is_selected {
                                                    "flex items-center gap-2 px-3 py-2 mb-1 rounded-md border border-primary-500 bg-primary-900/30 cursor-pointer"
                                                } else {
                                                    "flex items-center gap-2 px-3 py-2 mb-1 rounded-md border border-subtle hover:border-primary-600 cursor-pointer"
                                                },
                                                onclick: move |_| {
                                                    if selected_connector_id.peek().as_deref() != Some(row_id.as_str()) {
                                                        // Pending edits were already flushed reactively;
                                                        // load the newly selected instance for editing.
                                                        load_instance_into_draft(&mut local_settings.write(), &row_id);
                                                        selected_connector_id.set(Some(row_id.clone()));
                                                    }
                                                },
                                                span {
                                                    class: format!("w-6 h-6 rounded-full {} border border-faint flex items-center justify-center text-[10px] text-fg font-bold shadow-sm shrink-0", provider.color_class()),
                                                    "{provider.initial()}"
                                                }
                                                if is_selected {
                                                    input {
                                                        class: "flex-1 min-w-0 px-2 py-1 bg-input border border-primary-600 rounded text-sm",
                                                        value: "{instance.name}",
                                                        onclick: move |e| e.stop_propagation(),
                                                        oninput: move |event| {
                                                            local_settings.write().rename_connector(&rename_id, &event.value());
                                                        }
                                                    }
                                                } else {
                                                    span { class: "flex-1 truncate text-sm", "{instance.name}" }
                                                }
                                                span { class: "text-xs text-fg-muted shrink-0", "{provider.display_name()}" }
                                                button {
                                                    class: if is_default {
                                                        "text-yellow-400 text-sm shrink-0"
                                                    } else {
                                                        "text-fg-muted/40 hover:text-yellow-400 text-sm shrink-0"
                                                    },
                                                    title: if is_default { "Default connector for new sessions" } else { "Make default for new sessions" },
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        let mut draft = local_settings.write();
                                                        draft.active_connector_id = Some(default_id.clone());
                                                        if let Some(kind) = draft.connector_by_id(&default_id).map(|c| c.provider()) {
                                                            draft.active_llm = kind;
                                                        }
                                                    },
                                                    "★"
                                                }
                                                button {
                                                    class: if is_last {
                                                        "text-fg-muted/30 text-sm shrink-0 cursor-not-allowed"
                                                    } else {
                                                        "text-fg-muted hover:text-red-400 text-sm shrink-0"
                                                    },
                                                    title: if is_last { "At least one connector is required" } else { "Remove connector" },
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        let mut draft = local_settings.write();
                                                        if draft.remove_connector(&delete_id) {
                                                            let new_selected = draft.active_connector().map(|c| c.id.clone());
                                                            if let Some(ref id) = new_selected {
                                                                load_instance_into_draft(&mut draft, id);
                                                            }
                                                            drop(draft);
                                                            selected_connector_id.set(new_selected);
                                                        }
                                                    },
                                                    "🗑"
                                                }
                                            }
                                        }
                                    }
                                }
                                // Add-connector buttons, one per provider kind
                                {
                                    let at_cap = local_settings.read().llm_connectors.len() >= crate::settings::MAX_LLM_CONNECTORS;
                                    rsx! {
                                        div {
                                            class: "flex gap-2 mt-2",
                                            for kind in crate::settings::LlmProvider::all_variants().iter().copied() {
                                                button {
                                                    class: if at_cap {
                                                        "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted/40 cursor-not-allowed"
                                                    } else {
                                                        "px-3 py-1.5 rounded-md text-xs bg-input text-fg-muted hover:text-fg hover:border-primary-500 border border-subtle"
                                                    },
                                                    title: if at_cap {
                                                        format!("Connector limit reached ({})", crate::settings::MAX_LLM_CONNECTORS)
                                                    } else {
                                                        format!("Add a {} connector", kind.display_name())
                                                    },
                                                    onclick: move |_| {
                                                        if at_cap { return; }
                                                        let mut draft = local_settings.write();
                                                        match draft.add_connector(
                                                            kind.display_name(),
                                                            crate::settings::ProviderInstanceConfig::default_for(kind),
                                                        ) {
                                                            Ok(id) => {
                                                                load_instance_into_draft(&mut draft, &id);
                                                                drop(draft);
                                                                selected_connector_id.set(Some(id));
                                                            }
                                                            Err(e) => tracing::warn!("Add connector failed: {}", e),
                                                        }
                                                    },
                                                    "+ {kind.display_name()}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if selected_kind(&local_settings.read(), &selected_connector_id.read()) == Some(crate::settings::LlmProvider::Gemini) {
                                div {
                                    class: "pl-4 border-l-2 border-subtle",
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted", "API Key" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Enter your Gemini API key",
                                            value: "{local_settings.read().gemini_config.api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| local_settings.write().gemini_config.api_key = Some(event.value())
                                        }
                                    }
                                    // Keychain Storage Mode - conditional based on environment
                                    div {
                                        class: "mb-4 p-3 bg-app rounded-lg border border-subtle",
                                        label { class: "block text-sm font-medium text-fg-muted mb-2", "API Key Storage" }

                                        if is_sandboxed() {
                                            // App Store/TestFlight: Show Biometric and iCloud options
                                            div {
                                                class: "flex gap-2",
                                                button {
                                                    class: if local_settings.read().keychain_storage_mode == crate::settings::KeychainStorageMode::Biometric {
                                                        "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg"
                                                    } else {
                                                        "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg"
                                                    },
                                                    onclick: move |_| {
                                                        if local_settings.read().keychain_storage_mode != crate::settings::KeychainStorageMode::Biometric {
                                                            pending_keychain_mode.set(Some(crate::settings::KeychainStorageMode::Biometric));
                                                            show_keychain_mode_confirm.set(true);
                                                        }
                                                    },
                                                    "🔐 Biometric"
                                                }
                                                button {
                                                    class: if local_settings.read().keychain_storage_mode == crate::settings::KeychainStorageMode::ICloudSync {
                                                        "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg"
                                                    } else {
                                                        "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg"
                                                    },
                                                    onclick: move |_| {
                                                        if local_settings.read().keychain_storage_mode != crate::settings::KeychainStorageMode::ICloudSync {
                                                            pending_keychain_mode.set(Some(crate::settings::KeychainStorageMode::ICloudSync));
                                                            show_keychain_mode_confirm.set(true);
                                                        }
                                                    },
                                                    "☁️ iCloud Sync"
                                                }
                                            }
                                            p {
                                                class: "text-xs text-fg-muted mt-2",
                                                if local_settings.read().keychain_storage_mode == crate::settings::KeychainStorageMode::Biometric {
                                                    "Keys require Touch ID/passcode. Device-only, more secure."
                                                } else {
                                                    "Keys sync across your devices via iCloud. No biometric lock."
                                                }
                                            }
                                        } else {
                                            // PRO/Developer ID: Local keychain only (read-only display)
                                            div {
                                                class: "flex gap-2",
                                                div {
                                                    class: "flex-1 bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm text-center",
                                                    "🔑 Local Keychain"
                                                }
                                            }
                                            p {
                                                class: "text-xs text-fg-muted mt-2",
                                                "API keys stored securely in your local keychain. (PRO build)"
                                            }
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-fg-muted", "Active Chat Model" }
                                            if local_settings.read().gemini_config.api_key.is_some() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-fg-muted disabled:cursor-not-allowed",
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
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Please configure your API key above to load available models"
                                            }
                                        } else if *models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Loading available models..."
                                            }
                                        } else if let Some(error) = models_error.read().as_ref() {
                                            p {
                                                class: "mt-1 text-sm text-red-400",
                                                "{error}"
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().gemini_config.chat_model}",
                                                onchange: move |event| {
                                                    local_settings.write().gemini_config.chat_model = event.value();
                                                },
                                                {
                                                    let current = local_settings.read().gemini_config.chat_model.clone();
                                                    let models = available_models.read();
                                                    let in_list = models.iter().any(|m| m.name == current);
                                                    rsx! {
                                                        if !current.is_empty() && !in_list {
                                                            option { value: "{current}", selected: true, "{GeminiModel::from_slug(&current).display_name()}" }
                                                        }
                                                        for model in models.iter() {
                                                            option {
                                                                value: "{model.name}",
                                                                selected: model.name == current,
                                                                "{model.display_name}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-fg-muted", "Active Summary Model" }
                                            if local_settings.read().gemini_config.api_key.is_some() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-fg-muted disabled:cursor-not-allowed",
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
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Please configure your API key above to load available models"
                                            }
                                        } else if *models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Loading available models..."
                                            }
                                        } else if let Some(error) = models_error.read().as_ref() {
                                            p {
                                                class: "mt-1 text-sm text-red-400",
                                                "{error}"
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().gemini_config.summary_model}",
                                                onchange: move |event| {
                                                    local_settings.write().gemini_config.summary_model = event.value();
                                                },
                                                {
                                                    let current = local_settings.read().gemini_config.summary_model.clone();
                                                    let models = available_models.read();
                                                    let in_list = models.iter().any(|m| m.name == current);
                                                    rsx! {
                                                        if !current.is_empty() && !in_list {
                                                            option { value: "{current}", selected: true, "{GeminiModel::from_slug(&current).display_name()}" }
                                                        }
                                                        for model in models.iter() {
                                                            option {
                                                                value: "{model.name}",
                                                                selected: model.name == current,
                                                                "{model.display_name}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Image Generation Model
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted", "Image Generation Model" }
                                        if local_settings.read().gemini_config.api_key.is_none() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Please configure your API key above to load available models"
                                            }
                                        } else if *models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Loading available models..."
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().image_generation_config.model}",
                                                onchange: move |event| {
                                                    local_settings.write().image_generation_config.model = event.value();
                                                },
                                                {
                                                    let current = local_settings.read().image_generation_config.model.clone();
                                                    let models = available_models.read();
                                                    // Filter for image-capable models (name contains "image")
                                                    let image_models: Vec<_> = models.iter()
                                                        .filter(|m| m.name.to_lowercase().contains("image"))
                                                        .collect();
                                                    let in_list = image_models.iter().any(|m| m.name == current);
                                                    rsx! {
                                                        if !current.is_empty() && !in_list {
                                                            option { value: "{current}", selected: true, "{GeminiModel::from_slug(&current).display_name()}" }
                                                        }
                                                        for model in image_models.iter() {
                                                            option {
                                                                value: "{model.name}",
                                                                selected: model.name == current,
                                                                "{model.display_name}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Thinking Mode Section
                                    div {
                                        class: "mb-4 pt-4 border-t border-subtle",
                                        div {
                                            class: "flex items-center justify-between mb-2",
                                            label { class: "block text-sm font-medium text-fg-muted", "Thinking Mode" }
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
                                                div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mb-3",
                                            "Enable extended reasoning for complex tasks. Gemini 3 Pro uses thinking level, Gemini 2.5 uses thinking budget."
                                        }

                                        if local_settings.read().gemini_config.thinking_enabled {
                                            {
                                                let current_model = local_settings.read().gemini_config.chat_model.clone();
                                                let gemini_model = crate::llm::GeminiModel::from_slug(&current_model);

                                                match gemini_model.thinking_config_style() {
                                                    crate::llm::ThinkingConfigStyle::LevelPro |
                                                    crate::llm::ThinkingConfigStyle::LevelFlash => {
                                                        rsx! {
                                                            div {
                                                                class: "mb-3",
                                                                label { class: "block text-sm font-medium text-fg-muted mb-1", "Thinking Level" }
                                                                select {
                                                                    class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                                    onchange: move |event| {
                                                                        local_settings.write().gemini_config.thinking_level = event.value();
                                                                    },
                                                                    for level in gemini_model.valid_thinking_levels() {
                                                                        option {
                                                                            value: "{level}",
                                                                            selected: local_settings.read().gemini_config.thinking_level == *level,
                                                                            "{level}"
                                                                        }
                                                                    }
                                                                }
                                                                p { class: "text-xs text-fg-muted mt-1", "Controls reasoning depth." }
                                                            }
                                                        }
                                                    },
                                                    crate::llm::ThinkingConfigStyle::Budget => {
                                                        rsx! {
                                                            div {
                                                                class: "mb-3",
                                                                label { class: "block text-sm font-medium text-fg-muted mb-1", "Thinking Budget (Web 2.5)" }
                                                                input {
                                                                    class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
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
                                                                    class: "text-xs text-fg-muted mt-1",
                                                                    "Higher values allow more reasoning tokens (increases cost)"
                                                                }
                                                            }
                                                        }
                                                    },
                                                    crate::llm::ThinkingConfigStyle::None => {
                                                        rsx! {
                                                            div {
                                                                class: "p-3 bg-yellow-900/30 border border-yellow-700/50 rounded-lg",
                                                                p { class: "text-sm text-yellow-200", "⚠️ This model does not support thinking mode." }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Context Caching Section
                                    div {
                                        class: "mb-4 pt-4 border-t border-subtle",
                                        div {
                                            class: "flex items-center justify-between mb-2",
                                            label { class: "block text-sm font-medium text-fg-muted", "Context Caching" }
                                            label {
                                                class: "relative inline-flex items-center cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "sr-only peer",
                                                    checked: local_settings.read().gemini_config.context_caching_enabled,
                                                    onchange: move |event: FormEvent| {
                                                        local_settings.write().gemini_config.context_caching_enabled = event.checked();
                                                    }
                                                }
                                                div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mb-3",
                                            "Enable explicit context caching via the cachedContents API for ~75% discount on input tokens. Recommended for long conversation histories or many active tools."
                                        }

                                        if local_settings.read().gemini_config.context_caching_enabled {
                                            div {
                                                class: "mb-3",
                                                label { class: "block text-sm font-medium text-fg-muted mb-1", "Cache TTL (seconds)" }
                                                input {
                                                    r#type: "number",
                                                    class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                    min: "60",
                                                    value: "{local_settings.read().gemini_config.cache_ttl_seconds}",
                                                    onchange: move |event: FormEvent| {
                                                        if let Ok(val) = event.value().trim().parse::<u32>() {
                                                            local_settings.write().gemini_config.cache_ttl_seconds = val.max(60);
                                                        }
                                                    }
                                                }
                                                p { class: "text-xs text-fg-muted mt-1", "Duration the cache is stored server-side. Shorter TTL minimizes storage fees; longer TTL improves hit rate for infrequent turns (minimum 60s)." }
                                            }
                                        }
                                    }

                                    // Model Quick-Switch Slots Section
                                    div {
                                        class: "mb-4 pt-4 border-t border-subtle",
                                        div {
                                            class: "flex justify-between items-center cursor-pointer mb-2 group",
                                            onclick: move |_| {
                                                let mut state = ui_state.write();
                                                state.show_model_slots = !state.show_model_slots;
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            },
                                            label { class: "block text-sm font-medium text-fg-muted group-hover:text-fg transition-colors", "Model Quick-Switch Slots" }
                                            span { class: "text-xs text-fg-muted", if ui_state.read().show_model_slots { "▼" } else { "▶" } }
                                        }
                                        if ui_state.read().show_model_slots {
                                            div {
                                                class: "space-y-3 pl-2 mb-4",
                                                for i in 0..10 {
                                                    div {
                                                        key: "model-slot-{i}",
                                                        class: "flex items-center gap-3",
                                                        {
                                                            let slot_model = local_settings.read().model_slots_for(crate::settings::LlmProvider::Gemini).get(i).cloned().unwrap_or_default();
                                                            let effective_icon = if !slot_model.is_empty() {
                                                                local_settings.read().model_icons.get(&slot_model).cloned()
                                                                    .unwrap_or_else(|| get_slot_icon(i))
                                                            } else {
                                                                get_slot_icon(i)
                                                            };
                                                            let has_model = !slot_model.is_empty();
                                                            rsx! {
                                                                div {
                                                                    class: if has_model {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0 cursor-pointer hover:border-primary-500 transition-all"
                                                                    } else {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0"
                                                                    },
                                                                    title: if has_model { "Click to change icon" } else { "" },
                                                                    onclick: move |_| {
                                                                        if has_model {
                                                                            let current = picker_open_for_slot();
                                                                            if current == Some(i) {
                                                                                picker_open_for_slot.set(None);
                                                                            } else {
                                                                                picker_open_for_slot.set(Some(i));
                                                                            }
                                                                        }
                                                                    },
                                                                    "{effective_icon}"
                                                                }
                                                            }
                                                        }
                                                        div {
                                                            class: "flex-1",
                                                            select {
                                                                class: "block w-full px-2 py-1 bg-input border border-subtle rounded text-xs",
                                                                onchange: move |evt| {
                                                                    local_settings.write().update_model_slot_for(crate::settings::LlmProvider::Gemini, i, evt.value());
                                                                },
                                                                {
                                                                    let current_slot_value = local_settings.read().model_slots_for(crate::settings::LlmProvider::Gemini).get(i).cloned().unwrap_or_default();
                                                                    let models = available_models.read();
                                                                    let current_in_list = models.iter().any(|m| m.name == current_slot_value);

                                                                    rsx! {
                                                                        option { value: "", selected: current_slot_value.is_empty(), "None" }
                                                                        if !current_slot_value.is_empty() && !current_in_list {
                                                                            option {
                                                                                value: "{current_slot_value}",
                                                                                selected: true,
                                                                                "{GeminiModel::from_slug(&current_slot_value).display_name()}"
                                                                            }
                                                                        }
                                                                        for model in models.iter() {
                                                                            option {
                                                                                value: "{model.name}",
                                                                                selected: model.name == current_slot_value,
                                                                                "{model.display_name}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            // Emoji picker popover — only shown when this slot's icon is clicked
                                                            if picker_open_for_slot() == Some(i) {
                                                                {
                                                                    let slot_model_for_picker = local_settings.read().model_slots_for(crate::settings::LlmProvider::Gemini).get(i).cloned().unwrap_or_default();
                                                                    if !slot_model_for_picker.is_empty() {
                                                                        let current_custom = local_settings.read().model_icons.get(&slot_model_for_picker).cloned().unwrap_or_default();
                                                                        rsx! {
                                                                            div {
                                                                                class: "flex gap-1 flex-wrap mt-1 p-1.5 bg-card border border-subtle rounded-lg shadow-lg",
                                                                                for emoji in ["\u{26a1}", "\u{1f9e0}", "\u{1f34c}", "\u{1f916}", "\u{1f48e}", "\u{1f525}", "\u{2728}", "\u{1f680}", "\u{1f319}", "\u{2b50}"] {
                                                                                    button {
                                                                                        class: format!("w-7 h-7 rounded-full bg-section border flex items-center justify-center text-sm transition-all {}",
                                                                                            if current_custom == emoji { "border-primary-500 ring-1 ring-primary-500/30 scale-110" } else { "border-subtle hover:border-primary-500 hover:scale-110" }
                                                                                        ),
                                                                                        onclick: {
                                                                                            let model = slot_model_for_picker.clone();
                                                                                            let icon = emoji.to_string();
                                                                                            move |_| {
                                                                                                local_settings.write().model_icons.insert(model.clone(), icon.clone());
                                                                                                picker_open_for_slot.set(None);
                                                                                            }
                                                                                        },
                                                                                        "{emoji}"
                                                                                    }
                                                                                }
                                                                                button {
                                                                                    class: "w-7 h-7 rounded-full bg-section border border-subtle flex items-center justify-center text-[10px] text-fg-muted hover:border-red-500 hover:text-red-400 transition-all",
                                                                                    title: "Reset to default",
                                                                                    onclick: {
                                                                                        let model = slot_model_for_picker.clone();
                                                                                        move |_| {
                                                                                            local_settings.write().model_icons.remove(&model);
                                                                                            picker_open_for_slot.set(None);
                                                                                        }
                                                                                    },
                                                                                    "✕"
                                                                                }
                                                                            }
                                                                        }
                                                                    } else {
                                                                        rsx! {}
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        span { class: "text-[10px] text-fg-muted font-mono w-6 text-right", "^{i+1}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // === OpenAI-Compatible Config ===
                            if selected_kind(&local_settings.read(), &selected_connector_id.read()) == Some(crate::settings::LlmProvider::OpenAiCompat) {
                                div {
                                    class: "pl-4 border-l-2 border-subtle",
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted", "Endpoint URL" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "text",
                                            placeholder: "http://localhost:11434",
                                            value: "{local_settings.read().openai_compat_config.endpoint}",
                                            oninput: move |event| local_settings.write().openai_compat_config.endpoint = event.value().trim().to_string()
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Ollama, vLLM, LM Studio, or any OpenAI-format API endpoint"
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-fg-muted", "Model" }
                                            if !local_settings.read().openai_compat_config.endpoint.is_empty() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-fg-muted disabled:cursor-not-allowed",
                                                    disabled: *oai_models_loading.read(),
                                                    onclick: move |_| {
                                                        oai_models_fetch_trigger.set(oai_models_fetch_trigger() + 1);
                                                    },
                                                    if *oai_models_loading.read() { "Loading..." } else { "↻ Refresh" }
                                                }
                                            }
                                        }
                                        if local_settings.read().openai_compat_config.endpoint.is_empty() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Please configure your endpoint above to discover models"
                                            }
                                        } else if *oai_models_loading.read() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Discovering available models..."
                                            }
                                        } else if let Some(error) = oai_models_error.read().as_ref() {
                                            div {
                                                p {
                                                    class: "mt-1 text-sm text-red-400 mb-2",
                                                    "{error}"
                                                }
                                                // Fallback text input when discovery fails
                                                input {
                                                    class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                    r#type: "text",
                                                    placeholder: "Enter model name manually",
                                                    value: "{local_settings.read().openai_compat_config.model}",
                                                    oninput: move |event| local_settings.write().openai_compat_config.model = event.value()
                                                }
                                            }
                                        } else if oai_available_models.read().is_empty() {
                                            // No models discovered — fallback to text input
                                            input {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                r#type: "text",
                                                placeholder: "Enter model name",
                                                value: "{local_settings.read().openai_compat_config.model}",
                                                oninput: move |event| local_settings.write().openai_compat_config.model = event.value()
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().openai_compat_config.model}",
                                                onchange: move |event: Event<FormData>| {
                                                    local_settings.write().openai_compat_config.model = event.value();
                                                },
                                                {
                                                    let current = local_settings.read().openai_compat_config.model.clone();
                                                    let models = oai_available_models.read();
                                                    let in_list = models.contains(&current);
                                                    rsx! {
                                                        if current.is_empty() {
                                                            option { value: "", selected: true, "Select a model..." }
                                                        }
                                                        if !current.is_empty() && !in_list {
                                                            option { value: "{current}", selected: true, "{current}" }
                                                        }
                                                        for model in models.iter() {
                                                            option {
                                                                value: "{model}",
                                                                selected: *model == current,
                                                                "{model}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Summary Model Selector
                                    div {
                                        class: "mb-4",
                                        div {
                                            class: "flex justify-between items-center mb-1",
                                            label { class: "block text-sm font-medium text-fg-muted", "Summary Model" }
                                            if !local_settings.read().openai_compat_config.endpoint.is_empty() {
                                                button {
                                                    class: "text-xs text-primary-400 hover:text-primary-300 disabled:text-fg-muted disabled:cursor-not-allowed",
                                                    disabled: *oai_models_loading.read(),
                                                    onclick: move |_| {
                                                        oai_models_fetch_trigger.set(oai_models_fetch_trigger() + 1);
                                                    },
                                                    if *oai_models_loading.read() { "Loading..." } else { "↻ Refresh" }
                                                }
                                            }
                                        }
                                        if local_settings.read().openai_compat_config.endpoint.is_empty() {
                                            p {
                                                class: "mt-1 text-sm text-fg-muted italic",
                                                "Configure endpoint above to discover models"
                                            }
                                        } else if oai_available_models.read().is_empty() && !*oai_models_loading.read() {
                                            // No models discovered — fallback to text input
                                            input {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                r#type: "text",
                                                placeholder: "Enter summary model name (defaults to chat model)",
                                                value: "{local_settings.read().openai_compat_config.summary_model.as_deref().unwrap_or(\"\")}",
                                                oninput: move |event| {
                                                    let val = event.value();
                                                    local_settings.write().openai_compat_config.summary_model = if val.is_empty() { None } else { Some(val) };
                                                }
                                            }
                                        } else {
                                            select {
                                                class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                                value: "{local_settings.read().openai_compat_config.summary_model.as_deref().unwrap_or(\"\")}",
                                                onchange: move |event: Event<FormData>| {
                                                    let val = event.value();
                                                    local_settings.write().openai_compat_config.summary_model = if val.is_empty() { None } else { Some(val) };
                                                },
                                                {
                                                    let current = local_settings.read().openai_compat_config.summary_model.clone().unwrap_or_default();
                                                    let models = oai_available_models.read();
                                                    let in_list = models.contains(&current);
                                                    rsx! {
                                                        option { value: "", selected: current.is_empty(), "Use chat model" }
                                                        if !current.is_empty() && !in_list {
                                                            option { value: "{current}", selected: true, "{current}" }
                                                        }
                                                        for model in models.iter() {
                                                            option {
                                                                value: "{model}",
                                                                selected: *model == current,
                                                                "{model}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Model used for conversation summarization. Defaults to your chat model if not set."
                                        }
                                    }

                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted", "API Key (optional)" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Leave blank for local servers",
                                            value: "{local_settings.read().openai_compat_config.api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| {
                                                let val = event.value();
                                                local_settings.write().openai_compat_config.api_key = if val.is_empty() { None } else { Some(val) };
                                            }
                                        }
                                    }
                                    // Enable Tool Use toggle
                                    div {
                                        class: "mb-4 pt-2",
                                        div {
                                            class: "flex items-center justify-between",
                                            label { class: "block text-sm font-medium text-fg-muted", "Enable Tool Use" }
                                            label {
                                                class: "relative inline-flex items-center cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "sr-only peer",
                                                    checked: local_settings.read().openai_compat_config.tools_enabled,
                                                    onchange: move |event| {
                                                        local_settings.write().openai_compat_config.tools_enabled = event.checked();
                                                    }
                                                }
                                                div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Send MCP tool definitions to the model. Requires server support (e.g. vLLM needs --enable-auto-tool-choice --tool-call-parser)."
                                        }
                                    }

                                    // Thinking Mode toggle
                                    div {
                                        class: "mb-4 pt-2",
                                        div {
                                            class: "flex items-center justify-between",
                                            label { class: "block text-sm font-medium text-fg-muted", "Thinking Mode" }
                                            label {
                                                class: "relative inline-flex items-center cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "sr-only peer",
                                                    checked: local_settings.read().openai_compat_config.thinking_enabled,
                                                    onchange: move |event| {
                                                        local_settings.write().openai_compat_config.thinking_enabled = event.checked();
                                                    }
                                                }
                                                div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Enable reasoning/thinking for models that support it (Gemma 4, Qwen3). Requires vLLM with --enable-reasoning --reasoning-parser."
                                        }
                                    }

                                    // Watch Words (Auto-Recovery) Section
                                    div {
                                        class: "mb-4 pt-2",
                                        div {
                                            class: "flex items-center justify-between",
                                            label { class: "block text-sm font-medium text-fg-muted", "Watch Words (Auto-Recovery)" }
                                            label {
                                                class: "relative inline-flex items-center cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "sr-only peer",
                                                    checked: local_settings.read().openai_compat_config.watch_words_enabled,
                                                    onchange: move |event| {
                                                        local_settings.write().openai_compat_config.watch_words_enabled = event.checked();
                                                    }
                                                }
                                                div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Detect stalled output patterns and automatically continue the AI's turn. Useful for models like Qwen that halt mid-response."
                                        }

                                        if local_settings.read().openai_compat_config.watch_words_enabled {
                                            div {
                                                class: "mt-3 space-y-3 pl-2 border-l-2 border-primary-700/50",
                                                {
                                                    let watch_words_len = local_settings.read().openai_compat_config.watch_words.len();
                                                    rsx! {
                                                        for i in 0..watch_words_len {
                                                            div {
                                                                key: "watch-word-{i}",
                                                                class: "p-3 bg-section rounded-lg border border-subtle space-y-2",
                                                                div {
                                                                    class: "flex items-center justify-between",
                                                                    label { class: "text-xs text-fg-muted font-medium", "Pattern {i + 1}" }
                                                                    button {
                                                                        class: "text-xs text-red-400 hover:text-red-300 transition-colors",
                                                                        onclick: move |_| {
                                                                            local_settings.write().openai_compat_config.watch_words.remove(i);
                                                                        },
                                                                        "✕ Remove"
                                                                    }
                                                                }
                                                                input {
                                                                    class: "block w-full px-3 py-1.5 bg-input border border-primary-600 rounded text-sm",
                                                                    r#type: "text",
                                                                    placeholder: "e.g., Let me ",
                                                                    value: "{local_settings.read().openai_compat_config.watch_words.get(i).map(|w| w.pattern.as_str()).unwrap_or(\"\")}",
                                                                    oninput: move |event| {
                                                                        if let Some(ww) = local_settings.write().openai_compat_config.watch_words.get_mut(i) {
                                                                            ww.pattern = event.value();
                                                                        }
                                                                    }
                                                                }
                                                                label { class: "text-xs text-fg-muted", "Recovery instruction (optional)" }
                                                                textarea {
                                                                    class: "block w-full px-3 py-1.5 bg-input border border-primary-600 rounded text-sm resize-y min-h-[2.5rem]",
                                                                    rows: "2",
                                                                    placeholder: "Leave empty for default: \"Your previous response appears to have been cut off...\"",
                                                                    value: "{local_settings.read().openai_compat_config.watch_words.get(i).map(|w| w.instruction.as_str()).unwrap_or(\"\")}",
                                                                    oninput: move |event| {
                                                                        if let Some(ww) = local_settings.write().openai_compat_config.watch_words.get_mut(i) {
                                                                            ww.instruction = event.value();
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                button {
                                                    class: "w-full py-1.5 text-xs text-primary-400 hover:text-primary-300 border border-dashed border-primary-700/50 rounded-lg hover:border-primary-500/50 transition-all",
                                                    onclick: move |_| {
                                                        local_settings.write().openai_compat_config.watch_words.push(
                                                            crate::llm::config::WatchWord {
                                                                pattern: String::new(),
                                                                instruction: String::new(),
                                                            }
                                                        );
                                                    },
                                                    "+ Add Watch Word"
                                                }
                                                div {
                                                    class: "flex items-center gap-2 min-w-0 pt-1",
                                                    label {
                                                        class: "text-xs text-fg-muted w-40 shrink-0",
                                                        title: "Maximum number of automatic recovery attempts per user turn. Prevents infinite loops if the model keeps stalling.",
                                                        "Max recoveries ⓘ"
                                                    }
                                                    input {
                                                        r#type: "number",
                                                        class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                        min: "1",
                                                        max: "20",
                                                        value: "{local_settings.read().openai_compat_config.max_watch_word_recoveries}",
                                                        onchange: move |event: FormEvent| {
                                                            if let Ok(n) = event.value().trim().parse::<u32>() {
                                                                local_settings.write().openai_compat_config.max_watch_word_recoveries = n.clamp(1, 20);
                                                            }
                                                        }
                                                    }
                                                }
                                                div {
                                                    class: "flex items-center gap-2 min-w-0 pt-1",
                                                    label {
                                                        class: "text-xs text-fg-muted w-40 shrink-0",
                                                        title: "Watch words are only checked when the AI's response is shorter than this many characters. Long responses are likely complete even if they contain a watch word.",
                                                        "Max response length ⓘ"
                                                    }
                                                    input {
                                                        r#type: "number",
                                                        class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                        min: "100",
                                                        max: "5000",
                                                        step: "100",
                                                        value: "{local_settings.read().openai_compat_config.watch_word_max_response_chars}",
                                                        onchange: move |event: FormEvent| {
                                                            if let Ok(n) = event.value().trim().parse::<usize>() {
                                                                local_settings.write().openai_compat_config.watch_word_max_response_chars = n.clamp(100, 5000);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Context window is auto-detected from /v1/models (max_model_len).
                                    // Surface it, and warn when it's too small for reliable
                                    // tool-calling — Hobbes is a tool harness and small windows
                                    // cause the model to loop (see RECOMMENDED_MIN_CONTEXT_TOKENS).
                                    {
                                        let cfg_ctx = local_settings.read().openai_compat_config.max_context_tokens;
                                        let rec_min = crate::llm::config::RECOMMENDED_MIN_CONTEXT_TOKENS;
                                        match cfg_ctx {
                                            Some(tokens) if tokens < rec_min => rsx! {
                                                div {
                                                    class: "mb-4 flex items-start gap-2 p-2 rounded border border-yellow-600/40 bg-yellow-500/5",
                                                    span { class: "text-yellow-500 shrink-0 text-sm", "⚠️" }
                                                    p { class: "text-xs text-yellow-200",
                                                        "Small context window detected ({tokens} tokens). Hobbes is a tool-calling harness — the system prompt and tool definitions alone use roughly 5–7K tokens, so windows under {rec_min} can make the model loop, re-requesting tool results that were truncated out of its context. Raise the server's max context length (e.g. vLLM "
                                                        code { class: "px-1 bg-card rounded", "--max-model-len" }
                                                        ") for reliable tool use."
                                                    }
                                                }
                                            },
                                            Some(tokens) => rsx! {
                                                p { class: "text-xs text-fg-muted mb-4", "Context window: {tokens} tokens (auto-detected)." }
                                            },
                                            None => rsx! {},
                                        }
                                    }

                                    // Per-Provider Context Tuning Overrides
                                    div {
                                        class: "mb-4 border border-faint rounded-lg overflow-hidden",
                                        div {
                                            class: "p-3 cursor-pointer flex justify-between items-center",
                                            onclick: move |_| {
                                                // Toggle inline — no extra state needed, use a simple detail/summary approach via class
                                            },
                                            h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2", "Context Tuning (Override Defaults)" }
                                        }
                                        div {
                                            class: "px-3 pb-3 space-y-2",
                                            p { class: "text-xs text-fg-muted mb-2", "Leave empty to use global defaults from Behavior tab." }
                                            // Chat History Length Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Chat History Length" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    placeholder: format!("{} (global)", local_settings.read().chat_history_length),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.chat_history_length {
                                                        Some(v) => v.to_string(),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.chat_history_length = None;
                                                        } else if let Ok(n) = val.trim().parse::<usize>() {
                                                            local_settings.write().openai_compat_config.context_tuning.chat_history_length = Some(n);
                                                        }
                                                    }
                                                }
                                            }
                                            // Max Tool Output Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Max Tool Output" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    placeholder: format!("{} (global)", local_settings.read().max_tool_output_length),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.max_tool_output_length {
                                                        Some(v) => v.to_string(),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_tool_output_length = None;
                                                        } else if let Ok(n) = val.trim().parse::<usize>() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_tool_output_length = Some(n);
                                                        }
                                                    }
                                                }
                                            }
                                            // Max Active Tool Output Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Max Active Tool Out" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    placeholder: format!("{} (global)", local_settings.read().max_active_tool_output_length),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.max_active_tool_output_length {
                                                        Some(v) => v.to_string(),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_active_tool_output_length = None;
                                                        } else if let Ok(n) = val.trim().parse::<usize>() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_active_tool_output_length = Some(n);
                                                        }
                                                    }
                                                }
                                            }
                                            // Max Summary Chars Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Max Summary Chars" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    placeholder: format!("{} (global)", local_settings.read().max_summary_chars),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.max_summary_chars {
                                                        Some(v) => v.to_string(),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_summary_chars = None;
                                                        } else if let Ok(n) = val.trim().parse::<usize>() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_summary_chars = Some(n);
                                                        }
                                                    }
                                                }
                                            }
                                            // Max Entities Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Max Entities" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    placeholder: format!("{} (global)", local_settings.read().max_entity_count),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.max_entity_count {
                                                        Some(v) => v.to_string(),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_entity_count = None;
                                                        } else if let Ok(n) = val.trim().parse::<usize>() {
                                                            local_settings.write().openai_compat_config.context_tuning.max_entity_count = Some(n);
                                                        }
                                                    }
                                                }
                                            }
                                            // Tool Result Budget % Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Tool Budget %" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    min: "5",
                                                    max: "95",
                                                    step: "5",
                                                    placeholder: format!("{}% (global)", (local_settings.read().tool_result_budget_ratio * 100.0).round() as u32),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.tool_result_budget_ratio {
                                                        Some(v) => format!("{}", (v * 100.0).round() as u32),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.tool_result_budget_ratio = None;
                                                        } else if let Ok(pct) = val.trim().parse::<f64>() {
                                                            local_settings.write().openai_compat_config.context_tuning.tool_result_budget_ratio = Some(crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0));
                                                        }
                                                    }
                                                }
                                            }
                                            // Active Result Share % Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Active Result %" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    min: "5",
                                                    max: "95",
                                                    step: "5",
                                                    placeholder: format!("{}% (global)", (local_settings.read().active_result_budget_ratio * 100.0).round() as u32),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.active_result_budget_ratio {
                                                        Some(v) => format!("{}", (v * 100.0).round() as u32),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.active_result_budget_ratio = None;
                                                        } else if let Ok(pct) = val.trim().parse::<f64>() {
                                                            local_settings.write().openai_compat_config.context_tuning.active_result_budget_ratio = Some(crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0));
                                                        }
                                                    }
                                                }
                                            }
                                            // Context Safety Margin % Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "Safety Margin %" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    min: "5",
                                                    max: "95",
                                                    step: "5",
                                                    placeholder: format!("{}% (global)", (local_settings.read().context_safety_margin * 100.0).round() as u32),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.context_safety_margin {
                                                        Some(v) => format!("{}", (v * 100.0).round() as u32),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.context_safety_margin = None;
                                                        } else if let Ok(pct) = val.trim().parse::<f64>() {
                                                            local_settings.write().openai_compat_config.context_tuning.context_safety_margin = Some(crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0));
                                                        }
                                                    }
                                                }
                                            }
                                            // System Prompt Budget % Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label { class: "text-xs text-fg-muted w-32 shrink-0", "System Prompt %" }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    min: "5",
                                                    max: "95",
                                                    step: "5",
                                                    placeholder: format!("{}% (global)", (local_settings.read().system_prompt_budget_ratio * 100.0).round() as u32),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.system_prompt_budget_ratio {
                                                        Some(v) => format!("{}", (v * 100.0).round() as u32),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.system_prompt_budget_ratio = None;
                                                        } else if let Ok(pct) = val.trim().parse::<f64>() {
                                                            local_settings.write().openai_compat_config.context_tuning.system_prompt_budget_ratio = Some(crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0));
                                                        }
                                                    }
                                                }
                                            }
                                            // Chars/Token Ratio Override
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label {
                                                    class: "text-xs text-fg-muted w-32 shrink-0",
                                                    title: "Characters per token ratio used for context budget calculations. English prose ≈ 4.0, CJK or code-heavy content ≈ 2.0. Lower values allocate more tokens per character, giving the model more room for non-English or dense content.",
                                                    "Chars/Token ⓘ"
                                                }
                                                input {
                                                    r#type: "number",
                                                    class: "flex-1 min-w-0 bg-input border border-primary-600 rounded px-2 py-1 text-sm",
                                                    min: "1.0",
                                                    max: "8.0",
                                                    step: "0.5",
                                                    placeholder: format!("{:.1} (default)", crate::settings::DEFAULT_CHARS_PER_TOKEN),
                                                    value: match local_settings.read().openai_compat_config.context_tuning.chars_per_token {
                                                        Some(v) => format!("{:.1}", v),
                                                        None => String::new(),
                                                    },
                                                    onchange: move |event: FormEvent| {
                                                        let val = event.value();
                                                        if val.trim().is_empty() {
                                                            local_settings.write().openai_compat_config.context_tuning.chars_per_token = None;
                                                        } else if let Ok(v) = val.trim().parse::<f64>() {
                                                            local_settings.write().openai_compat_config.context_tuning.chars_per_token = Some(v.clamp(1.0, 8.0));
                                                        }
                                                    }
                                                }
                                            }
                                            // Compact Tool Results toggle
                                            div {
                                                class: "flex items-center gap-2 min-w-0",
                                                label {
                                                    class: "text-xs text-fg-muted w-32 shrink-0",
                                                    title: "Convert tool results from JSON to TOON (Token-Oriented Object Notation) for compact display. Reduces token usage by 30–50% for structured tool output.",
                                                    "Compact Results ⓘ"
                                                }
                                                label {
                                                    class: "relative inline-flex items-center cursor-pointer",
                                                    input {
                                                        r#type: "checkbox",
                                                        class: "sr-only peer",
                                                        checked: local_settings.read().openai_compat_config.context_tuning.compact_tool_results.unwrap_or(true),
                                                        onchange: move |event| {
                                                            local_settings.write().openai_compat_config.context_tuning.compact_tool_results = Some(event.checked());
                                                        }
                                                    }
                                                    div { class: "w-9 h-5 bg-input peer-focus:outline-none peer-focus:ring-2 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-4 after:w-4 after:transition-all peer-checked:bg-primary-500" }
                                                }
                                            }
                                        }
                                    }

                                    // Model Quick-Switch Slots Section
                                    div {
                                        class: "mb-4 pt-4 border-t border-subtle",
                                        div {
                                            class: "flex justify-between items-center cursor-pointer mb-2 group",
                                            onclick: move |_| {
                                                let mut state = ui_state.write();
                                                state.show_model_slots = !state.show_model_slots;
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            },
                                            label { class: "block text-sm font-medium text-fg-muted group-hover:text-fg transition-colors", "Model Quick-Switch Slots" }
                                            span { class: "text-xs text-fg-muted", if ui_state.read().show_model_slots { "▼" } else { "▶" } }
                                        }
                                        if ui_state.read().show_model_slots {
                                            div {
                                                class: "space-y-3 pl-2 mb-4",
                                                for i in 0..10 {
                                                    div {
                                                        key: "oai-model-slot-{i}",
                                                        class: "flex items-center gap-3",
                                                        {
                                                            let slot_model = local_settings.read().model_slots_for(crate::settings::LlmProvider::OpenAiCompat).get(i).cloned().unwrap_or_default();
                                                            let effective_icon = if !slot_model.is_empty() {
                                                                local_settings.read().model_icons.get(&slot_model).cloned()
                                                                    .unwrap_or_else(|| get_slot_icon(i))
                                                            } else {
                                                                get_slot_icon(i)
                                                            };
                                                            let has_model = !slot_model.is_empty();
                                                            rsx! {
                                                                div {
                                                                    class: if has_model {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0 cursor-pointer hover:border-primary-500 transition-all"
                                                                    } else {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0"
                                                                    },
                                                                    title: if has_model { "Click to change icon" } else { "" },
                                                                    onclick: move |_| {
                                                                        if has_model {
                                                                            let current = picker_open_for_slot();
                                                                            if current == Some(i) {
                                                                                picker_open_for_slot.set(None);
                                                                            } else {
                                                                                picker_open_for_slot.set(Some(i));
                                                                            }
                                                                        }
                                                                    },
                                                                    "{effective_icon}"
                                                                }
                                                            }
                                                        }
                                                        div {
                                                            class: "flex-1",
                                                            if oai_available_models.read().is_empty() {
                                                                // No models discovered — fallback to text input
                                                                input {
                                                                    class: "block w-full px-2 py-1 bg-input border border-subtle rounded text-xs",
                                                                    r#type: "text",
                                                                    placeholder: "Enter model name",
                                                                    value: "{local_settings.read().model_slots_for(crate::settings::LlmProvider::OpenAiCompat).get(i).cloned().unwrap_or_default()}",
                                                                    onchange: move |evt: FormEvent| {
                                                                        local_settings.write().update_model_slot_for(crate::settings::LlmProvider::OpenAiCompat, i, evt.value());
                                                                    }
                                                                }
                                                            } else {
                                                                select {
                                                                    class: "block w-full px-2 py-1 bg-input border border-subtle rounded text-xs",
                                                                    value: "{local_settings.read().model_slots_for(crate::settings::LlmProvider::OpenAiCompat).get(i).cloned().unwrap_or_default()}",
                                                                    onchange: move |evt| {
                                                                        local_settings.write().update_model_slot_for(crate::settings::LlmProvider::OpenAiCompat, i, evt.value());
                                                                    },
                                                                    {
                                                                        let current_slot_value = local_settings.read().model_slots_for(crate::settings::LlmProvider::OpenAiCompat).get(i).cloned().unwrap_or_default();
                                                                        let models = oai_available_models.read();
                                                                        let current_in_list = models.contains(&current_slot_value);

                                                                        rsx! {
                                                                            option { value: "", selected: current_slot_value.is_empty(), "None" }
                                                                            if !current_slot_value.is_empty() && !current_in_list {
                                                                                option {
                                                                                    value: "{current_slot_value}",
                                                                                    selected: true,
                                                                                    "{current_slot_value}"
                                                                                }
                                                                            }
                                                                            for model in models.iter() {
                                                                                option {
                                                                                    value: "{model}",
                                                                                    selected: *model == current_slot_value,
                                                                                    "{model}"
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            // Emoji picker popover
                                                            if picker_open_for_slot() == Some(i) {
                                                                {
                                                                    let slot_model_for_picker = local_settings.read().model_slots_for(crate::settings::LlmProvider::OpenAiCompat).get(i).cloned().unwrap_or_default();
                                                                    if !slot_model_for_picker.is_empty() {
                                                                        let current_custom = local_settings.read().model_icons.get(&slot_model_for_picker).cloned().unwrap_or_default();
                                                                        rsx! {
                                                                            div {
                                                                                class: "flex gap-1 flex-wrap mt-1 p-1.5 bg-card border border-subtle rounded-lg shadow-lg",
                                                                                for emoji in ["\u{26a1}", "\u{1f9e0}", "\u{1f34c}", "\u{1f916}", "\u{1f48e}", "\u{1f525}", "\u{2728}", "\u{1f680}", "\u{1f319}", "\u{2b50}"] {
                                                                                    button {
                                                                                        class: format!("w-7 h-7 rounded-full bg-section border flex items-center justify-center text-sm transition-all {}",
                                                                                            if current_custom == emoji { "border-primary-500 ring-1 ring-primary-500/30 scale-110" } else { "border-subtle hover:border-primary-500 hover:scale-110" }
                                                                                        ),
                                                                                        onclick: {
                                                                                            let model = slot_model_for_picker.clone();
                                                                                            let icon = emoji.to_string();
                                                                                            move |_| {
                                                                                                local_settings.write().model_icons.insert(model.clone(), icon.clone());
                                                                                                picker_open_for_slot.set(None);
                                                                                            }
                                                                                        },
                                                                                        "{emoji}"
                                                                                    }
                                                                                }
                                                                                button {
                                                                                    class: "w-7 h-7 rounded-full bg-section border border-subtle flex items-center justify-center text-[10px] text-fg-muted hover:border-red-500 hover:text-red-400 transition-all",
                                                                                    title: "Reset to default",
                                                                                    onclick: {
                                                                                        let model = slot_model_for_picker.clone();
                                                                                        move |_| {
                                                                                            local_settings.write().model_icons.remove(&model);
                                                                                            picker_open_for_slot.set(None);
                                                                                        }
                                                                                    },
                                                                                    "✕"
                                                                                }
                                                                            }
                                                                        }
                                                                    } else {
                                                                        rsx! {}
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        span { class: "text-[10px] text-fg-muted font-mono w-6 text-right", "^{i+1}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if selected_kind(&local_settings.read(), &selected_connector_id.read()) == Some(crate::settings::LlmProvider::Claude) {
                                div {
                                    class: "pl-4 border-l-2 border-subtle",
                                    // API Key
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted", "API Key" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Enter your Anthropic (Claude) API key",
                                            value: "{local_settings.read().claude_config.api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| local_settings.write().claude_config.api_key = Some(event.value())
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Stored securely in your keychain. Also reads ANTHROPIC_API_KEY if unset."
                                        }
                                    }
                                    // Active Chat Model
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Active Chat Model" }
                                        select {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            value: "{local_settings.read().claude_config.model}",
                                            onchange: move |event| {
                                                local_settings.write().claude_config.model = event.value();
                                            },
                                            {
                                                let current = local_settings.read().claude_config.model.clone();
                                                let models = crate::llm::claude_models::ClaudeModel::all_models();
                                                let in_list = models.iter().any(|m| m.canonical_slug() == current);
                                                rsx! {
                                                    if !current.is_empty() && !in_list {
                                                        option { value: "{current}", selected: true,
                                                            "{crate::llm::claude_models::ClaudeModel::from_slug(&current).display_name()}" }
                                                    }
                                                    for m in models.iter() {
                                                        option {
                                                            value: "{m.canonical_slug()}",
                                                            selected: m.canonical_slug() == current,
                                                            "{m.display_name()}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Summary Model
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Summary Model" }
                                        select {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            value: "{local_settings.read().claude_config.summary_model.as_deref().unwrap_or(\"\")}",
                                            onchange: move |event| {
                                                let v = event.value();
                                                local_settings.write().claude_config.summary_model =
                                                    if v.is_empty() { None } else { Some(v) };
                                            },
                                            option { value: "", "Same as chat model" }
                                            {
                                                let current = local_settings.read().claude_config.summary_model.clone().unwrap_or_default();
                                                rsx! {
                                                    for m in crate::llm::claude_models::ClaudeModel::all_models().iter() {
                                                        option {
                                                            value: "{m.canonical_slug()}",
                                                            selected: m.canonical_slug() == current,
                                                            "{m.display_name()}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Max Output Tokens
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Max Output Tokens" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                            r#type: "number",
                                            min: "1",
                                            placeholder: "32000",
                                            value: "{local_settings.read().claude_config.max_tokens.map(|v| v.to_string()).unwrap_or_default()}",
                                            oninput: move |event| {
                                                let v = event.value();
                                                local_settings.write().claude_config.max_tokens =
                                                    v.trim().parse::<u32>().ok();
                                            }
                                        }
                                        p {
                                            class: "text-xs text-fg-muted mt-1",
                                            "Claude requires an explicit output cap. Clamped to the model's ceiling."
                                        }
                                    }
                                    // Extended Thinking toggle (only for models that support it)
                                    {
                                        let supports = crate::llm::claude_models::ClaudeModel::from_slug(
                                            &local_settings.read().claude_config.model,
                                        ).supports_thinking();
                                        let enabled = local_settings.read().claude_config.extended_thinking;
                                        rsx! {
                                            if supports {
                                                div {
                                                    class: "mb-4 flex items-center justify-between",
                                                    div {
                                                        label { class: "block text-sm font-medium text-fg-muted", "Extended Thinking" }
                                                        p { class: "text-xs text-fg-muted", "Adaptive reasoning before responding." }
                                                    }
                                                    button {
                                                        class: if enabled {
                                                            "px-3 py-1 rounded-md text-sm font-medium bg-btn-primary text-fg"
                                                        } else {
                                                            "px-3 py-1 rounded-md text-sm font-medium bg-input text-fg-muted hover:text-fg"
                                                        },
                                                        onclick: move |_| {
                                                            let cur = local_settings.read().claude_config.extended_thinking;
                                                            local_settings.write().claude_config.extended_thinking = !cur;
                                                        },
                                                        if enabled { "On" } else { "Off" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Model Quick-Switch Slots
                                    div {
                                        class: "mb-4 pt-4 border-t border-subtle",
                                        div {
                                            class: "flex justify-between items-center cursor-pointer mb-2 group",
                                            onclick: move |_| {
                                                let mut state = ui_state.write();
                                                state.show_model_slots = !state.show_model_slots;
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            },
                                            label { class: "block text-sm font-medium text-fg-muted group-hover:text-fg transition-colors", "Model Quick-Switch Slots" }
                                            span { class: "text-xs text-fg-muted", if ui_state.read().show_model_slots { "▼" } else { "▶" } }
                                        }
                                        if ui_state.read().show_model_slots {
                                            p {
                                                class: "text-xs text-fg-muted mb-3",
                                                "Assign models to slots ^1–^9 for fast switching in the chat bar."
                                            }
                                            div {
                                                class: "space-y-3 pl-2 mb-4",
                                                for i in 0..10 {
                                                    div {
                                                        key: "claude-slot-{i}",
                                                        class: "flex items-center gap-3",
                                                        {
                                                            let slot_model = local_settings.read().model_slots_for(crate::settings::LlmProvider::Claude).get(i).cloned().unwrap_or_default();
                                                            let effective_icon = if !slot_model.is_empty() {
                                                                local_settings.read().model_icons.get(&slot_model).cloned()
                                                                    .unwrap_or_else(|| get_slot_icon(i))
                                                            } else {
                                                                get_slot_icon(i)
                                                            };
                                                            let has_model = !slot_model.is_empty();
                                                            rsx! {
                                                                div {
                                                                    class: if has_model {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0 cursor-pointer hover:border-primary-500 transition-all"
                                                                    } else {
                                                                        "w-8 h-8 rounded-full bg-section border border-subtle flex items-center justify-center text-sm shrink-0"
                                                                    },
                                                                    title: if has_model { "Click to change icon" } else { "" },
                                                                    onclick: move |_| {
                                                                        if has_model {
                                                                            let current = picker_open_for_slot();
                                                                            if current == Some(i) {
                                                                                picker_open_for_slot.set(None);
                                                                            } else {
                                                                                picker_open_for_slot.set(Some(i));
                                                                            }
                                                                        }
                                                                    },
                                                                    "{effective_icon}"
                                                                }
                                                            }
                                                        }
                                                        div {
                                                            class: "flex-1",
                                                            select {
                                                                class: "block w-full px-2 py-1 bg-input border border-subtle rounded text-xs",
                                                                onchange: move |evt| {
                                                                    local_settings.write().update_model_slot_for(crate::settings::LlmProvider::Claude, i, evt.value());
                                                                },
                                                                {
                                                                    let current = local_settings.read().model_slots_for(crate::settings::LlmProvider::Claude).get(i).cloned().unwrap_or_default();
                                                                    let models = crate::llm::claude_models::ClaudeModel::all_models();
                                                                    let in_list = models.iter().any(|m| m.canonical_slug() == current);
                                                                    rsx! {
                                                                        option { value: "", selected: current.is_empty(), "None" }
                                                                        if !current.is_empty() && !in_list {
                                                                            option { value: "{current}", selected: true,
                                                                                "{crate::llm::claude_models::ClaudeModel::from_slug(&current).display_name()}" }
                                                                        }
                                                                        for m in models.iter() {
                                                                            option {
                                                                                value: "{m.canonical_slug()}",
                                                                                selected: m.canonical_slug() == current,
                                                                                "{m.display_name()}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            if picker_open_for_slot() == Some(i) {
                                                                {
                                                                    let slot_model_for_picker = local_settings.read().model_slots_for(crate::settings::LlmProvider::Claude).get(i).cloned().unwrap_or_default();
                                                                    if !slot_model_for_picker.is_empty() {
                                                                        let current_custom = local_settings.read().model_icons.get(&slot_model_for_picker).cloned().unwrap_or_default();
                                                                        rsx! {
                                                                            div {
                                                                                class: "flex gap-1 flex-wrap mt-1 p-1.5 bg-card border border-subtle rounded-lg shadow-lg",
                                                                                for emoji in ["\u{26a1}", "\u{1f9e0}", "\u{1f34c}", "\u{1f916}", "\u{1f48e}", "\u{1f525}", "\u{2728}", "\u{1f680}", "\u{1f319}", "\u{2b50}"] {
                                                                                    button {
                                                                                        class: format!("w-7 h-7 rounded-full bg-section border flex items-center justify-center text-sm transition-all {}",
                                                                                            if current_custom == emoji { "border-primary-500 ring-1 ring-primary-500/30 scale-110" } else { "border-subtle hover:border-primary-500 hover:scale-110" }
                                                                                        ),
                                                                                        onclick: {
                                                                                            let model = slot_model_for_picker.clone();
                                                                                            let icon = emoji.to_string();
                                                                                            move |_| {
                                                                                                local_settings.write().model_icons.insert(model.clone(), icon.clone());
                                                                                                picker_open_for_slot.set(None);
                                                                                            }
                                                                                        },
                                                                                        "{emoji}"
                                                                                    }
                                                                                }
                                                                                button {
                                                                                    class: "w-7 h-7 rounded-full bg-section border border-subtle flex items-center justify-center text-[10px] text-fg-muted hover:border-red-500 hover:text-red-400 transition-all",
                                                                                    title: "Reset to default",
                                                                                    onclick: {
                                                                                        let model = slot_model_for_picker.clone();
                                                                                        move |_| {
                                                                                            local_settings.write().model_icons.remove(&model);
                                                                                            picker_open_for_slot.set(None);
                                                                                        }
                                                                                    },
                                                                                    "✕"
                                                                                }
                                                                            }
                                                                        }
                                                                    } else {
                                                                        rsx! {}
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        span { class: "text-[10px] text-fg-muted font-mono w-6 text-right", "^{i+1}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
    }
}
