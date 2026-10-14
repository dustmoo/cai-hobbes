mod calendar;
mod fleet;
mod license;
mod shared;
mod tabs;
mod terminal;
mod trust_rules;

use shared::{comparable, flush_draft_into_instance, load_instance_into_draft};
use tabs::{
    AboutTab, BehaviorTab, DataTab, GeneralTab, HotkeysTab, McpTab, PermissionsTab, PlannerTab,
    SkillsTab,
};

use crate::SecretManagerTrait;
use crate::components::confirm_save_modal::ConfirmSaveModal;
use crate::components::conflict_modal::ConflictModal;
use crate::components::tool_credentials::ToolCredentials;
use crate::mcp::composio_client::validate_composio_api_key;
use crate::session::SessionState;
use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};

/// Signals shared by most settings tabs, bundled so each tab component takes
/// them as one prop. Every field is a `Copy` handle to a hook owned by
/// `SettingsPanel`, so switching tabs never resets the draft or UI state.
#[derive(Clone, Copy, PartialEq)]
struct SettingsCtx {
    settings: Signal<Settings>,
    settings_manager: Signal<SettingsManager>,
    local_settings: Signal<Settings>,
    ui_state: Signal<crate::settings::UiState>,
    ui_state_manager: Signal<crate::settings::UiStateManager>,
    session_state: Signal<SessionState>,
    secret_manager: Signal<crate::secret_manager::SecretManager>,
    save_error: Signal<Option<String>>,
    selected_connector_id: Signal<Option<String>>,
    skill_registry: Signal<crate::skills::registry::SkillRegistry>,
}

/// Nav label for a settings section (the railed header names it).
fn settings_tab_label(tab: crate::settings::SettingsTab) -> &'static str {
    use crate::settings::SettingsTab as T;
    match tab {
        T::General => "General",
        T::Mcp => "MCP Tools",
        T::Behavior => "Behavior",
        T::Planner => "Planner",
        T::Data => "Data",
        T::Permissions => "Permissions",
        T::Hotkeys => "Hotkeys",
        T::Credentials => "Credentials",
        T::Skills => "Skills",
        T::About => "About",
        T::ImageGen => "Image",
        T::Fleet => "Fleet",
        T::Terminal => "Terminal",
    }
}

/// One settings nav entry: icon + uppercase label, or icon only (with a
/// tooltip) when the nav is a 44px rail. Selecting persists the tab.
#[component]
fn SettingsNavItem<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    tab: crate::settings::SettingsTab,
    icon: I,
    railed: bool,
) -> Element {
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let active = ui_state.read().active_settings_tab == tab;
    let label = settings_tab_label(tab);
    let class = match (railed, active) {
        (false, true) => "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg",
        (false, false) => "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg",
        (true, true) => "flex items-center justify-center w-full h-8 border-l-2 border-l-primary-500 bg-card text-fg",
        (true, false) => "flex items-center justify-center w-full h-8 border-l-2 border-transparent text-fg-muted hover:bg-card hover:text-fg",
    };
    rsx! {
        button {
            class: "{class}",
            title: if railed { label } else { "" },
            "aria-label": "{label}",
            onclick: move |_| {
                ui_state.write().active_settings_tab = tab;
                let state = (*ui_state.read()).clone();
                let manager = (*ui_state_manager.read()).clone();
                spawn(async move { let _ = manager.save(&state); });
            },
            Icon { width: 18, height: 18, icon }
            if !railed {
                "{label}"
            }
        }
    }
}

#[component]
pub fn SettingsPanel(
    /// Show the nav as a 44px icon rail (the layout solver needed the room,
    /// or the drawer is narrower than ~600px); the content header then
    /// names the current section.
    #[props(default)]
    nav_railed: bool,
) -> Element {
    let app_name = crate::settings::get_app_name();
    let app_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let mut session_state = use_context::<Signal<SessionState>>();
    let _permission_manager =
        use_context::<Signal<crate::context::permissions::PermissionManager>>();
    let _mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    // The connector instance currently selected for editing in the list.
    let mut selected_connector_id =
        use_signal(|| settings.read().active_connector().map(|c| c.id.clone()));

    // Create a local copy of the settings for editing. The selected connector's
    // config is loaded into the legacy singleton slots, which the per-provider
    // editing blocks bind to.
    let mut local_settings = use_signal(|| {
        let mut draft = settings.read().clone();
        if let Some(id) = draft.active_connector().map(|c| c.id.clone()) {
            load_instance_into_draft(&mut draft, &id);
        }
        draft
    });

    // Reactively flush singleton-slot edits back into the selected connector
    // instance. Converges in one extra pass: the write only happens when the
    // instance's config actually differs from the editing mirror.
    use_effect(move || {
        let selected = selected_connector_id.read().clone();
        let needs_flush = match (&selected, &*local_settings.read()) {
            (Some(id), draft) => {
                use crate::settings::{LlmProvider, ProviderInstanceConfig};
                match draft.connector_by_id(id) {
                    Some(instance) => match (instance.provider(), &instance.config) {
                        (LlmProvider::Gemini, ProviderInstanceConfig::Gemini(c)) => {
                            *c != draft.gemini_config
                        }
                        (LlmProvider::OpenAiCompat, ProviderInstanceConfig::OpenAiCompat(c)) => {
                            *c != draft.openai_compat_config
                        }
                        (LlmProvider::Claude, ProviderInstanceConfig::Claude(c)) => {
                            *c != draft.claude_config
                        }
                        _ => false,
                    },
                    None => false,
                }
            }
            (None, _) => false,
        };
        if needs_flush {
            if let Some(id) = selected {
                flush_draft_into_instance(&mut local_settings.write(), &id);
            }
        }
    });

    // This signal will track if the local state differs from the global state.
    let mut has_unsaved_changes = use_signal(|| false);

    // Signals for model fetching
    let mut available_models = use_signal(Vec::<crate::services::gemini_models::GeminiModel>::new);
    let mut models_loading = use_signal(|| false);
    let mut models_error = use_signal(|| Option::<String>::None);
    let skill_registry = use_context::<Signal<crate::skills::registry::SkillRegistry>>();
    let models_fetch_trigger = use_signal(|| 0u32);
    // show_model_slots is now persisted in UiState (defaults to open)
    let picker_open_for_slot: Signal<Option<usize>> = use_signal(|| None);

    // Signals for OpenAI-compat model fetching
    let mut oai_available_models = use_signal(Vec::<String>::new);
    let mut oai_models_loading = use_signal(|| false);
    let mut oai_models_error = use_signal(|| Option::<String>::None);
    let oai_models_fetch_trigger = use_signal(|| 0u32);

    // Effect to fetch models when API key is available or refresh is triggered
    use_effect(move || {
        let api_key = local_settings.read().gemini_config.api_key.clone();
        let _trigger = models_fetch_trigger.read(); // Subscribe to trigger changes

        if api_key.is_some() {
            models_loading.set(true);
            models_error.set(None);

            spawn(async move {
                match crate::services::gemini_models::fetch_gemini_models(api_key.as_deref()).await
                {
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

    // Effect to fetch OpenAI-compat models — trigger-only (not endpoint-reactive)
    // Uses peek() for config reads to avoid firing on every keystroke.
    // Uses the context-aware discovery to auto-detect max_model_len from vLLM.
    use_effect(move || {
        let _trigger = oai_models_fetch_trigger.read(); // Only subscribe to trigger
        let endpoint = local_settings.peek().openai_compat_config.endpoint.clone();
        let api_key = local_settings.peek().openai_compat_config.api_key.clone();
        let current_model = local_settings.peek().openai_compat_config.model.clone();

        if !endpoint.is_empty() {
            oai_models_loading.set(true);
            oai_models_error.set(None);

            spawn(async move {
                match crate::services::openai_compat_validation::fetch_openai_compat_models_with_context(
                    &endpoint,
                    api_key.as_deref(),
                )
                .await
                {
                    Ok(discovered) => {
                        // Auto-detect context window + calibrate the tokenizer for the
                        // selected model, writing results into the SELECTED connector
                        // instance — the value the prompt builder actually reads per
                        // session. The old code wrote only the legacy singleton, so
                        // multi-connector instances kept a null window; the budgeter then
                        // treated them as unlimited and overran small servers (which
                        // silently truncate, making tool-calling models loop). The legacy
                        // singleton is mirrored too so the editing buffer stays in sync.
                        let selected_id = selected_connector_id.peek().clone();
                        if !current_model.is_empty() {
                            // Current instance config is the source of truth for this connector.
                            let current_cfg = selected_id.as_deref().and_then(|id| {
                                match settings.peek().connector_by_id(id).map(|c| &c.config) {
                                    Some(crate::settings::ProviderInstanceConfig::OpenAiCompat(c)) => Some(c.clone()),
                                    _ => None,
                                }
                            });

                            let detected_ctx = discovered
                                .iter()
                                .find(|m| m.id == current_model)
                                .and_then(|m| m.context_length);

                            // Calibrate only when the instance has no manual override.
                            let need_calibration = current_cfg
                                .as_ref()
                                .map(|c| c.context_tuning.chars_per_token.is_none())
                                .unwrap_or(false);
                            let calibrated_ratio = if need_calibration {
                                crate::services::openai_compat_validation::calibrate_tokenizer(
                                    &endpoint,
                                    &current_model,
                                    api_key.as_deref(),
                                )
                                .await
                                // Round to 1 decimal place for cleanliness.
                                .map(|ratio| (ratio * 10.0).round() / 10.0)
                            } else {
                                None
                            };

                            if let (Some(id), Some(mut cfg)) = (selected_id.as_deref(), current_cfg) {
                                let mut changed = false;
                                if let Some(ctx_len) = detected_ctx {
                                    if cfg.max_context_tokens != Some(ctx_len) {
                                        tracing::info!(
                                            "Auto-detected context window: {} tokens for model '{}'",
                                            ctx_len, current_model
                                        );
                                        cfg.max_context_tokens = Some(ctx_len);
                                        changed = true;
                                    }
                                }
                                if let Some(ratio) = calibrated_ratio {
                                    tracing::info!(
                                        "Auto-calibrated chars/token: {:.1} for model '{}'",
                                        ratio, current_model
                                    );
                                    cfg.context_tuning.chars_per_token = Some(ratio);
                                    changed = true;
                                }
                                if changed {
                                    {
                                        let mut sw = settings.write();
                                        if let Some(instance) = sw.connector_by_id_mut(id) {
                                            instance.config = crate::settings::ProviderInstanceConfig::OpenAiCompat(cfg.clone());
                                        }
                                        // Mirror into the legacy singleton editing buffer.
                                        sw.openai_compat_config = cfg;
                                    }
                                    // Persist immediately so detection survives restart
                                    // without a manual Settings → Save cycle.
                                    let sm = settings_manager.peek().clone();
                                    let s = settings.peek().clone();
                                    let _ = sm.save(&s);
                                }
                            }
                        }

                        // Extract model IDs for the dropdown
                        let model_ids: Vec<String> = discovered.into_iter().map(|m| m.id).collect();
                        oai_available_models.set(model_ids);
                        oai_models_loading.set(false);
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch OAI models: {}", e);
                        oai_models_error.set(Some(e));
                        oai_models_loading.set(false);
                    }
                }
            });
        }
    });

    // This effect hook reactively checks for differences between the local and global settings.
    use_effect(move || {
        let global_settings = settings.read();
        let local = local_settings.read();
        has_unsaved_changes.set(comparable(&global_settings) != comparable(&local));
    });

    // This effect synchronizes local_settings with global settings when the global state changes.
    // This is crucial for reflecting changes made elsewhere in the app (e.g., 'remember my choice' in a confirmation modal).
    // It uses .peek() to avoid creating a dependency on local_settings, preventing an infinite loop.
    // NOTE: This will discard any unsaved changes in the settings panel when external changes occur.
    use_effect(move || {
        let global_settings = settings.read();
        if comparable(&global_settings) != comparable(&local_settings.peek()) {
            let mut draft = global_settings.clone();
            // Re-load the selected connector's config into the editing mirrors
            let selected = selected_connector_id
                .peek()
                .clone()
                .filter(|id| draft.connector_by_id(id).is_some())
                .or_else(|| draft.active_connector().map(|c| c.id.clone()));
            if let Some(ref id) = selected {
                load_instance_into_draft(&mut draft, id);
            }
            if *selected_connector_id.peek() != selected {
                selected_connector_id.set(selected);
            }
            local_settings.set(draft);
        }
    });

    let app_behavior_collapsed = use_signal(|| false);
    let data_management_collapsed = use_signal(|| false);
    let permissions_collapsed = use_signal(|| false);
    let mut show_conflict_modal = use_signal(|| false);
    let mut show_confirm_save_modal = use_signal(|| false);
    let show_tos_modal = use_signal(|| false);
    // Skill management: None = editor closed, Some(None) = new skill,
    // Some(Some(skill)) = editing an existing skill (editing_toolkit pattern)
    let editing_skill: Signal<Option<Option<crate::skills::Skill>>> = use_signal(|| None);
    let deleting_skill: Signal<Option<String>> = use_signal(|| None);
    let skill_delete_visible = use_signal(|| false);
    let mut conflicting_sessions = use_signal(Vec::<(String, crate::session::Session)>::new);

    // Keychain mode switch confirmation
    let mut show_keychain_mode_confirm = use_signal(|| false);
    let mut pending_keychain_mode = use_signal(|| None::<crate::settings::KeychainStorageMode>);

    // Composio URL warning
    let composio_url_warning = use_signal(|| Option::<String>::None);
    // Advanced mode toggle for Composio profile (hides User ID and Server URL by default)
    let show_composio_advanced = use_signal(|| false);

    let ctx = SettingsCtx {
        settings,
        settings_manager,
        local_settings,
        ui_state,
        ui_state_manager,
        session_state,
        secret_manager,
        save_error,
        selected_connector_id,
        skill_registry,
    };

    rsx! {
        div {
            class: "flex h-full bg-app text-fg",
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
                                crate::session::SessionState::save_async(&session_state.peek(), Some(save_error));
                            }
                        }
                    }
                }
            }
            // Keychain mode switch confirmation modal
            if show_keychain_mode_confirm() {
                div {
                    class: "fixed inset-0 bg-black/70 flex items-center justify-center z-50",
                    tabindex: "0",
                    autofocus: true,
                    onmounted: move |evt| {
                        let mounted = evt.data();
                        spawn(async move {
                            let _ = mounted.set_focus(true).await;
                        });
                    },
                    onclick: move |_| show_keychain_mode_confirm.set(false),
                    onkeydown: move |evt: KeyboardEvent| {
                        if evt.key() == Key::Escape {
                            show_keychain_mode_confirm.set(false);
                            pending_keychain_mode.set(None);
                        }
                    },
                    div {
                        class: "bg-section border border-subtle rounded-lg p-6 max-w-md mx-4",
                        onclick: move |e| e.stop_propagation(),
                        h3 { class: "text-lg font-bold text-fg mb-3", "⚠️ Change API Key Storage?" }
                        p { class: "text-fg-muted mb-4",
                            "Switching storage modes will "
                            strong { class: "text-red-400", "clear all your saved API keys" }
                            ". You'll need to re-enter them after switching."
                        }
                        p { class: "text-sm text-fg-muted mb-4",
                            if pending_keychain_mode.read().as_ref() == Some(&crate::settings::KeychainStorageMode::Biometric) {
                                "Biometric mode stores keys on this device only, protected by Touch ID/passcode."
                            } else {
                                "iCloud Sync mode stores keys in your iCloud Keychain, accessible across all your devices."
                            }
                        }
                        div { class: "flex gap-3 justify-end",
                            button {
                                class: "border border-faint text-fg rounded px-3 py-1.5 text-sm hover:bg-card",
                                onclick: move |_| {
                                    pending_keychain_mode.set(None);
                                    show_keychain_mode_confirm.set(false);
                                },
                                "Cancel"
                            }
                            button {
                                class: "border border-red-700/50 text-red-400 rounded px-3 py-1.5 text-sm",
                                onclick: move |_| {
                                    if let Some(new_mode) = pending_keychain_mode.read().clone() {
                                        // Clear all keychain secrets
                                        let deleted = secret_manager.write().delete_all();
                                        tracing::info!("Cleared {} keychain items for mode switch", deleted.len());

                                        // Clear API keys from local settings
                                        {
                                            let mut ls = local_settings.write();
                                            ls.gemini_config.api_key = None;
                                            ls.claude_config.api_key = None;
                                            ls.openai_compat_config.api_key = None;
                                            for connector in ls.llm_connectors.iter_mut() {
                                                connector.config.set_api_key(None);
                                            }
                                            ls.smithery_api_key = None;
                                            for profile in ls.composio_profiles.iter_mut() {
                                                profile.api_key = None;
                                            }
                                            // Set the new mode
                                            ls.keychain_storage_mode = new_mode;
                                        }
                                    }
                                    pending_keychain_mode.set(None);
                                    show_keychain_mode_confirm.set(false);
                                },
                                "Clear Keys & Switch"
                            }
                        }
                    }
                }
            }
            if show_confirm_save_modal() {
                ConfirmSaveModal {
                    is_visible: show_confirm_save_modal,
                    title: "Save Settings".to_string(),
                    message: "Your changes will be applied immediately and affect all sessions. Continue?".to_string(),
                    on_confirm: move |remember| {
                        if remember {
                            local_settings.write().confirm_on_save = false;
                        }
                        // 1. Commit the local changes to the global state
                        let mut global_settings = settings.write();
                        // Connectors removed in this edit session lose their keychain items below
                        let old_connector_ids: Vec<String> = global_settings
                            .llm_connectors
                            .iter()
                            .map(|c| c.id.clone())
                            .collect();
                        *global_settings = local_settings.read().clone();

                        // 2. Prepare data for background saving (Clone cheap stuff or take ownership)
                        let mut settings_to_save = global_settings.clone();
                        let smithery_key_opt = settings_to_save.smithery_api_key.clone();
                        // Per-connector API keys, keyed by their per-instance keychain item
                        let llm_key_updates: Vec<(String, String)> = settings_to_save
                            .llm_connectors
                            .iter()
                            .filter_map(|c| {
                                c.config
                                    .api_key()
                                    .filter(|k| !k.trim().is_empty())
                                    .map(|k| (crate::secret_types::llm_key_name(&c.id), k.clone()))
                            })
                            .collect();
                        let removed_connector_ids: Vec<String> = old_connector_ids
                            .into_iter()
                            .filter(|id| settings_to_save.connector_by_id(id).is_none())
                            .collect();
                        // Extract Composio keys to save (using profile ID as keychain key)
                        let composio_keys: Vec<(String, String)> = settings_to_save.composio_profiles.iter()
                            .filter_map(|p| p.api_key.as_ref().map(|k| (p.id.clone(), k.clone())))
                            .collect();

                        let smithery_key_to_save = smithery_key_opt.map(|k| k.trim().to_string());
                        if let Some(ref trimmed) = smithery_key_to_save {
                             settings_to_save.smithery_api_key = Some(trimmed.clone());
                        }

                        // Critical: Update the global settings signal so the app (menus, hotkeys) reacts immediately
                        *global_settings = settings_to_save.clone();
                        has_unsaved_changes.set(false);

                        let mut secret_updates = llm_key_updates;
                        if let Some(k) = smithery_key_to_save { secret_updates.push(("smithery_api_key".to_string(), k)); }
                        tracing::debug!("Composio keys to save: {:?}", composio_keys.iter().map(|(id, _)| id).collect::<Vec<_>>());

                        spawn(async move {
                            // Validate Composio API keys before saving
                            let mut validated_composio_keys = Vec::new();
                            for (profile_id, key) in composio_keys {
                                if key.trim().is_empty() {
                                    continue; // Skip empty keys
                                }
                                match validate_composio_api_key(&key).await {
                                    Ok(()) => {
                                        tracing::info!("Composio API key for profile id '{}' validated successfully", profile_id);
                                        validated_composio_keys.push((profile_id, key));
                                    }
                                    Err(e) => {
                                        tracing::error!("Invalid Composio API key for profile id '{}': {}", profile_id, e);
                                        // Don't save invalid keys - they'll remain unchanged in keychain
                                    }
                                }
                            }

                            // Build final secret updates with validated Composio keys
                            let mut final_secret_updates = secret_updates;
                            for (profile_id, key) in validated_composio_keys {
                                final_secret_updates.push((crate::secret_types::composio_key_name(&profile_id), key));
                            }
                            tracing::debug!("Total validated secret updates: {}", final_secret_updates.len());

                            // Delete keychain items of connectors removed in this save
                            // (also prunes them from the discovery index).
                            if !removed_connector_ids.is_empty() {
                                let mut sm = secret_manager.write();
                                for id in &removed_connector_ids {
                                    if let Err(e) = sm.delete_llm_key(id) {
                                        tracing::warn!("Failed to delete key for removed connector {}: {}", id, e);
                                    }
                                }
                            }

                            // Compute the updated LLM key discovery index so newly
                            // saved per-connector keys are found on next launch.
                            let new_llm_index = {
                                let sm = secret_manager.read();
                                let mut idx = sm
                                    .get_named_index_from_keychain(crate::secret_types::LLM_KEYS_INDEX_KEY)
                                    .unwrap_or_default();
                                for (name, _) in final_secret_updates
                                    .iter()
                                    .filter(|(n, _)| n.starts_with(crate::secret_types::LLM_KEY_PREFIX))
                                {
                                    idx = crate::secret_types::add_to_index_csv(&idx, name);
                                }
                                idx
                            };

                            // Run Keychain operations in blocking task
                            // Capture the storage mode preference, BUT override for Pro builds
                            // Pro builds (no provisioning profile) can't use Biometric keychain access groups
                            let effective_mode = if crate::settings::is_sandboxed() {
                                settings_to_save.keychain_storage_mode.clone()
                            } else {
                                // Pro/Developer ID: always use LocalKeychain
                                crate::settings::KeychainStorageMode::LocalKeychain
                            };
                            let use_biometric = effective_mode == crate::settings::KeychainStorageMode::Biometric;

                            let results = tokio::task::spawn_blocking(move || {
                                let mut saved = Vec::new();
                                for (key_name, key_value) in final_secret_updates {
                                    if let Err(e) = crate::secret_manager::save_secret_to_keychain(&key_name, &key_value, use_biometric) {
                                        tracing::error!("Failed to save secret {}: {}", key_name, e);
                                    } else {
                                        saved.push((key_name, key_value));
                                    }
                                }
                                // The index must stay readable without a biometric
                                // prompt so startup discovery works pre-auth.
                                if !new_llm_index.is_empty() {
                                    if let Err(e) = crate::secret_manager::save_secret_to_keychain(
                                        crate::secret_types::LLM_KEYS_INDEX_KEY,
                                        &new_llm_index,
                                        false,
                                    ) {
                                        tracing::error!("Failed to save LLM key index: {}", e);
                                    } else {
                                        saved.push((
                                            crate::secret_types::LLM_KEYS_INDEX_KEY.to_string(),
                                            new_llm_index,
                                        ));
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

                            // Persist settings.json off the UI thread via the
                            // atomic writer, surfacing any failure to the toast
                            // (otherwise a Windows write failure is silent and
                            // the change appears to "not save" after restart).
                            settings_manager.read().save_async(settings_to_save, Some(save_error));
                        });

                        show_confirm_save_modal.set(false);
                    },
                    on_cancel: move |_| {
                        show_confirm_save_modal.set(false);
                    }
                }
            }
            // Nav (shrink-0 to not affect content width): full labels, or a
            // 44px icon rail when the drawer is short on room.
            div {
                class: if nav_railed { "w-11 shrink-0 flex flex-col border-r border-subtle bg-section" } else { "w-48 shrink-0 flex flex-col border-r border-subtle bg-section" },
                if nav_railed {
                    div {
                        class: "h-11 flex items-center justify-center border-b border-subtle shrink-0 text-fg-muted",
                        title: "Settings",
                        Icon { width: 16, height: 16, icon: fi_icons::FiSettings }
                    }
                } else {
                    div {
                        class: "h-11 flex items-baseline gap-2.5 px-4 border-b border-subtle shrink-0",
                        span { class: "text-base font-semibold text-fg", "Settings" }
                        span { class: "font-mono text-[11px] text-fg-muted", "{app_version}" }
                        span { class: "font-mono text-[11px] text-fg-muted ml-auto", "{crate::hotkey::display_combo(&settings.read().hotkeys.toggle_settings)}" }
                    }
                }
                div {
                    class: if nav_railed { "flex flex-col gap-0.5 py-2" } else { "flex flex-col gap-0.5 p-2" },
                    SettingsNavItem { tab: crate::settings::SettingsTab::General, icon: fi_icons::FiSettings, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Mcp, icon: fi_icons::FiCpu, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Behavior, icon: fi_icons::FiSliders, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Planner, icon: fi_icons::FiCheckSquare, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Fleet, icon: fi_icons::FiRadio, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Terminal, icon: fi_icons::FiTerminal, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Data, icon: fi_icons::FiDatabase, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Permissions, icon: fi_icons::FiLock, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Hotkeys, icon: fi_icons::FiCommand, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Credentials, icon: fi_icons::FiKey, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::Skills, icon: fi_icons::FiBook, railed: nav_railed }
                    SettingsNavItem { tab: crate::settings::SettingsTab::About, icon: fi_icons::FiInfo, railed: nav_railed }
                }
            }

            // Content Area (maintains original settings pane width)
            div {
                class: "flex-1 flex flex-col min-w-0 p-4",
                // Railed nav: the header moves here so the section stays named.
                if nav_railed {
                    div {
                        class: "h-11 -mx-4 -mt-4 mb-4 flex items-baseline gap-2.5 px-4 border-b border-subtle shrink-0 min-w-0",
                        span { class: "text-base font-semibold text-fg shrink-0", "Settings" }
                        span {
                            class: "font-mono text-[11px] text-fg-muted truncate",
                            "{app_version} · {settings_tab_label(ui_state.read().active_settings_tab)}"
                        }
                        span { class: "font-mono text-[11px] text-fg-muted ml-auto shrink-0", "{crate::hotkey::display_combo(&settings.read().hotkeys.toggle_settings)}" }
                    }
                }
                div {
                   class: "flex-1 overflow-y-auto pr-2",
                   match ui_state.read().active_settings_tab {
                       crate::settings::SettingsTab::Planner => rsx! { PlannerTab { ctx } },
                       crate::settings::SettingsTab::General => rsx! {
                           GeneralTab {
                               ctx,
                               available_models,
                               models_loading,
                               models_error,
                               models_fetch_trigger,
                               picker_open_for_slot,
                               oai_available_models,
                               oai_models_loading,
                               oai_models_error,
                               oai_models_fetch_trigger,
                               show_keychain_mode_confirm,
                               pending_keychain_mode,
                           }
                       },
                       crate::settings::SettingsTab::Mcp => rsx! {
                           McpTab { ctx, composio_url_warning, show_composio_advanced }
                       },
                       crate::settings::SettingsTab::Behavior => rsx! { BehaviorTab { ctx, app_behavior_collapsed } },
                       // Fleet and Terminal commit immediately (not part of
                       // the Behavior tab's draft + Save).
                       crate::settings::SettingsTab::Fleet => rsx! { fleet::FleetSection {} },
                       crate::settings::SettingsTab::Terminal => rsx! { terminal::TerminalSection {} },
                       crate::settings::SettingsTab::Data => rsx! {
                           DataTab { ctx, data_management_collapsed, show_conflict_modal, conflicting_sessions }
                       },
                       crate::settings::SettingsTab::Permissions => rsx! {
                           PermissionsTab { ctx, mcp_context: _mcp_context, permissions_collapsed }
                       },
                       crate::settings::SettingsTab::Hotkeys => rsx! { HotkeysTab { ctx } },
                       crate::settings::SettingsTab::Credentials => rsx! {
                           ToolCredentials {}
                       },
                       crate::settings::SettingsTab::Skills => rsx! {
                           SkillsTab { ctx, editing_skill, deleting_skill, skill_delete_visible }
                       },
                       crate::settings::SettingsTab::About => rsx! {
                           AboutTab { app_name, app_version: app_version.clone(), show_tos_modal }
                       },
                   // ImageGen tab removed — image model is now inline in General.
                   // Keep the arm for serialization backwards compat; render nothing.
                   crate::settings::SettingsTab::ImageGen => rsx! {
                       p { class: "text-sm text-fg-muted p-4", "Image model settings have moved to the Model tab above." }
                   },
                   } // End Match
                } // End Scrollable Content

                // Footer (Sticky)
                div {
                    class: "p-4 border-t border-subtle bg-section",
                    button {
                class: if has_unsaved_changes() {
                    "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm focus:outline-none focus:ring-2 focus:ring-primary-500"
                } else {
                    "bg-input text-fg-muted rounded px-3 py-1.5 text-sm cursor-not-allowed"
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
                            let old_connector_ids: Vec<String> = global_settings
                                .llm_connectors
                                .iter()
                                .map(|c| c.id.clone())
                                .collect();
                            *global_settings = local_settings.read().clone();

                            // 2. Prepare data for background saving
                            let mut settings_to_save = global_settings.clone();
                            let smithery_key_opt = settings_to_save.smithery_api_key.clone();
                            // Per-connector API keys, keyed by their per-instance keychain item
                            let llm_key_updates: Vec<(String, String)> = settings_to_save
                                .llm_connectors
                                .iter()
                                .filter_map(|c| {
                                    c.config
                                        .api_key()
                                        .filter(|k| !k.trim().is_empty())
                                        .map(|k| (crate::secret_types::llm_key_name(&c.id), k.clone()))
                                })
                                .collect();
                            let removed_connector_ids: Vec<String> = old_connector_ids
                                .into_iter()
                                .filter(|id| settings_to_save.connector_by_id(id).is_none())
                                .collect();
                            let composio_keys: Vec<(String, String)> = settings_to_save.composio_profiles.iter()
                                .filter_map(|p| p.api_key.as_ref().map(|k| (p.id.clone(), k.clone())))
                                .collect();

                            let smithery_key_to_save = smithery_key_opt.map(|k| k.trim().to_string());
                            if let Some(ref trimmed) = smithery_key_to_save {
                                 settings_to_save.smithery_api_key = Some(trimmed.clone());
                            }

                            let mut secret_updates = llm_key_updates;
                            if let Some(k) = smithery_key_to_save { secret_updates.push(("smithery_api_key".to_string(), k)); }
                            tracing::debug!("Composio keys to save: {:?}", composio_keys.iter().map(|(n, _)| n).collect::<Vec<_>>());

                            // Capture the storage mode preference, overriding for Pro builds
                            let effective_mode = if crate::settings::is_sandboxed() {
                                settings_to_save.keychain_storage_mode.clone()
                            } else {
                                crate::settings::KeychainStorageMode::LocalKeychain
                            };
                            let use_biometric = effective_mode == crate::settings::KeychainStorageMode::Biometric;

                            spawn(async move {
                                // Validate Composio API keys before saving
                                let mut validated_composio_keys = Vec::new();
                                for (profile_id, key) in composio_keys {
                                    if key.trim().is_empty() {
                                        continue; // Skip empty keys
                                    }
                                    match validate_composio_api_key(&key).await {
                                        Ok(()) => {
                                            tracing::info!("Composio API key for profile id '{}' validated successfully", profile_id);
                                            validated_composio_keys.push((profile_id, key));
                                        }
                                        Err(e) => {
                                            tracing::error!("Invalid Composio API key for profile id '{}': {}", profile_id, e);
                                            // Don't save invalid keys - they'll remain unchanged in keychain
                                        }
                                    }
                                }

                                // Build final secret updates with validated Composio keys
                                let mut final_secret_updates = secret_updates;
                                for (profile_id, key) in validated_composio_keys {
                                    final_secret_updates.push((crate::secret_types::composio_key_name(&profile_id), key));
                                }
                                tracing::debug!("Total validated secret updates: {}", final_secret_updates.len());

                                // Delete keychain items of connectors removed in this save
                                if !removed_connector_ids.is_empty() {
                                    let mut sm = secret_manager.write();
                                    for id in &removed_connector_ids {
                                        if let Err(e) = sm.delete_llm_key(id) {
                                            tracing::warn!("Failed to delete key for removed connector {}: {}", id, e);
                                        }
                                    }
                                }

                                // Compute the updated LLM key discovery index
                                let new_llm_index = {
                                    let sm = secret_manager.read();
                                    let mut idx = sm
                                        .get_named_index_from_keychain(crate::secret_types::LLM_KEYS_INDEX_KEY)
                                        .unwrap_or_default();
                                    for (name, _) in final_secret_updates
                                        .iter()
                                        .filter(|(n, _)| n.starts_with(crate::secret_types::LLM_KEY_PREFIX))
                                    {
                                        idx = crate::secret_types::add_to_index_csv(&idx, name);
                                    }
                                    idx
                                };

                                let results = tokio::task::spawn_blocking(move || {
                                    let mut saved = Vec::new();
                                    for (key_name, key_value) in final_secret_updates {
                                        if let Err(e) = crate::secret_manager::save_secret_to_keychain(&key_name, &key_value, use_biometric) {
                                            tracing::error!("Failed to save secret {}: {}", key_name, e);
                                        } else {
                                            saved.push((key_name, key_value));
                                        }
                                    }
                                    // Index stays readable without biometric prompt
                                    if !new_llm_index.is_empty() {
                                        if let Err(e) = crate::secret_manager::save_secret_to_keychain(
                                            crate::secret_types::LLM_KEYS_INDEX_KEY,
                                            &new_llm_index,
                                            false,
                                        ) {
                                            tracing::error!("Failed to save LLM key index: {}", e);
                                        } else {
                                            saved.push((
                                                crate::secret_types::LLM_KEYS_INDEX_KEY.to_string(),
                                                new_llm_index,
                                            ));
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

                                // Persist off the UI thread via the atomic
                                // writer; surface failures to the toast so a
                                // silent Windows write error can't masquerade as
                                // a successful save.
                                settings_manager.read().save_async(settings_to_save, Some(save_error));
                            });
                        }
                    }
                },
                "Save Settings"
            }
                }
            }
        }
    }
}
