mod calendar;
mod fleet;
mod license;
mod shared;
mod tabs;
mod terminal;
mod trust_rules;

use shared::{comparable, flush_draft_into_instance, load_instance_into_draft, selected_kind};
use tabs::{AboutTab, BehaviorTab, DataTab, HotkeysTab, McpTab, PermissionsTab, PlannerTab, SkillsTab};

use crate::components::confirm_save_modal::ConfirmSaveModal;
use crate::components::conflict_modal::ConflictModal;
use crate::components::tool_credentials::ToolCredentials;
use crate::llm::GeminiModel;
use crate::mcp::composio_client::validate_composio_api_key;
use crate::settings::{get_slot_icon, is_sandboxed, Settings, SettingsManager};
use crate::SecretManagerTrait;
use crate::session::SessionState;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

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

#[component]
pub fn SettingsPanel() -> Element {
    let app_name = crate::settings::get_app_name();
    let app_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let mut session_state = use_context::<Signal<SessionState>>();
    let _permission_manager =
        use_context::<Signal<crate::context::permissions::PermissionManager>>();
    let _mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let _mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    // The connector instance currently selected for editing in the list.
    let mut selected_connector_id = use_signal(|| {
        settings
            .read()
            .active_connector()
            .map(|c| c.id.clone())
    });

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
    let mut models_fetch_trigger = use_signal(|| 0u32);
    // show_model_slots is now persisted in UiState (defaults to open)
    let mut picker_open_for_slot: Signal<Option<usize>> = use_signal(|| None);

    // Signals for OpenAI-compat model fetching
    let mut oai_available_models = use_signal(Vec::<String>::new);
    let mut oai_models_loading = use_signal(|| false);
    let mut oai_models_error = use_signal(|| Option::<String>::None);
    let mut oai_models_fetch_trigger = use_signal(|| 0u32);

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

    // UI Persistence Helpers
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
            // Sidebar (shrink-0 to not affect content width)
            div {
                class: "w-48 shrink-0 flex flex-col border-r border-subtle bg-section",
                div {
                    class: "h-11 flex items-baseline gap-2.5 px-4 border-b border-subtle shrink-0",
                    span { class: "text-base font-semibold text-fg", "Settings" }
                    span { class: "font-mono text-[11px] text-fg-muted", "{app_version}" }
                    span { class: "font-mono text-[11px] text-fg-muted ml-auto", "{crate::hotkey::display_combo(&settings.read().hotkeys.toggle_settings)}" }
                }
                // Tabs
                div {
                    class: "flex flex-col gap-0.5 p-2",
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::General { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::General;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiSettings }
                    "General"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Mcp { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Mcp;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiCpu }
                    "MCP Tools"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Behavior { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Behavior;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiSliders }
                    "Behavior"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Planner { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Planner;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiCheckSquare }
                    "Planner"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Data { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Data;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiDatabase }
                    "Data"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Permissions { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Permissions;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiLock }
                    "Permissions"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Hotkeys { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Hotkeys;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiCommand }
                    "Hotkeys"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Credentials { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Credentials;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiKey }
                    "Credentials"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Skills { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::Skills;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiBook }
                    "Skills"
                }
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::About { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] bg-input text-fg" } else { "flex items-center gap-2 h-8 px-2.5 rounded text-[11px] uppercase tracking-[0.06em] text-fg-muted hover:bg-input hover:text-fg" },
                    onclick: move |_| {
                        ui_state.write().active_settings_tab = crate::settings::SettingsTab::About;
                        let state = (*ui_state.read()).clone();
                        let manager = (*ui_state_manager.read()).clone();
                        spawn(async move { let _ = manager.save(&state); });
                    },
                    Icon { width: 18, height: 18, icon: fi_icons::FiInfo }
                    "About"
                }
                }
            }

            // Content Area (maintains original settings pane width)
            div {
                class: "flex-1 flex flex-col min-w-0 p-4",
                div {
                   class: "flex-1 overflow-y-auto pr-2",
                   match ui_state.read().active_settings_tab {
                       crate::settings::SettingsTab::Planner => rsx! {
                           PlannerTab { ctx }
                       },
                       crate::settings::SettingsTab::General => rsx! {

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
                   }, // End General
                   crate::settings::SettingsTab::Mcp => rsx! {
                       McpTab { ctx, composio_url_warning: composio_url_warning, show_composio_advanced: show_composio_advanced }
                   },
                   crate::settings::SettingsTab::Behavior => rsx! {
                       BehaviorTab { ctx, app_behavior_collapsed: app_behavior_collapsed }
                   },
                   crate::settings::SettingsTab::Data => rsx! {
                       DataTab { ctx, data_management_collapsed: data_management_collapsed, show_conflict_modal: show_conflict_modal, conflicting_sessions: conflicting_sessions }
                   },
                   crate::settings::SettingsTab::Permissions => rsx! {
                       PermissionsTab { ctx, mcp_context: _mcp_context,permissions_collapsed: permissions_collapsed }
                   },
                    crate::settings::SettingsTab::Hotkeys => rsx! {
                        HotkeysTab { ctx }
                    },
                    crate::settings::SettingsTab::Credentials => rsx! {
                         ToolCredentials {}
                    },
                    crate::settings::SettingsTab::Skills => rsx! {
                        SkillsTab { ctx, editing_skill: editing_skill, deleting_skill: deleting_skill, skill_delete_visible: skill_delete_visible }
                    },
                    crate::settings::SettingsTab::About => rsx! {
                        AboutTab { app_name: app_name, app_version: app_version.clone(), show_tos_modal: show_tos_modal }
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
