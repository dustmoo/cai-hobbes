use crate::components::confirm_save_modal::ConfirmSaveModal;
use crate::components::conflict_modal::ConflictModal;
use crate::components::hotkey_recorder::HotkeyRecorder;
use crate::components::llm::GeminiModel;
use crate::components::markdown_renderer::MarkdownRenderer;
use crate::components::onboarding::TOS_CONTENT;
use crate::components::tool_credentials::ToolCredentials;
use crate::mcp::composio_client::validate_composio_api_key;
use crate::settings::{get_slot_icon, is_sandboxed, HotkeySettings, Settings, SettingsManager};
use crate::{context::permissions::ToolCategory, session::SessionState};
use dioxus::prelude::*;
use crate::SecretManagerTrait;
use dioxus_free_icons::{icons::fi_icons, Icon};
use rfd;
use std::io::Write;
use zip::write::{FileOptions, ZipWriter};

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
    let mut active_composio_profile_name = use_context::<Signal<Option<String>>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    // Create a local copy of the settings for editing.
    let mut local_settings = use_signal(|| settings.read().clone());

    // This signal will track if the local state differs from the global state.
    let mut has_unsaved_changes = use_signal(|| false);

    // Signals for model fetching
    let mut available_models =
        use_signal(Vec::<crate::services::gemini_models::GeminiModel>::new);
    let mut models_loading = use_signal(|| false);
    let mut models_error = use_signal(|| Option::<String>::None);
    let skill_registry = use_context::<Signal<crate::skills::registry::SkillRegistry>>();
    let mut models_fetch_trigger = use_signal(|| 0u32);
    // show_model_slots is now persisted in UiState (defaults to open)
    let mut picker_open_for_slot: Signal<Option<usize>> = use_signal(|| None);

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

    let mut app_behavior_collapsed = use_signal(|| false);
    let mut data_management_collapsed = use_signal(|| false);
    let mut permissions_collapsed = use_signal(|| false);
    let mut show_conflict_modal = use_signal(|| false);
    let mut show_confirm_save_modal = use_signal(|| false);
    let mut show_tos_modal = use_signal(|| false);
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

    let toggle_mcp_instructions_collapsed = move |_| {
        {
            let mut state = ui_state.write();
            state.mcp_instructions_collapsed = !state.mcp_instructions_collapsed;
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
    let mut composio_url_warning = use_signal(|| Option::<String>::None);
    // Advanced mode toggle for Composio profile (hides User ID and Server URL by default)
    let mut show_composio_advanced = use_signal(|| false);

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
                                crate::session::SessionState::save_async(session_state.peek().clone(), Some(save_error));
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
                                class: "px-4 py-2 rounded-md text-fg-muted hover:text-fg",
                                onclick: move |_| {
                                    pending_keychain_mode.set(None);
                                    show_keychain_mode_confirm.set(false);
                                },
                                "Cancel"
                            }
                            button {
                                class: "px-4 py-2 bg-red-600 hover:bg-red-700 text-fg rounded-md font-semibold",
                                onclick: move |_| {
                                    if let Some(new_mode) = pending_keychain_mode.read().clone() {
                                        // Clear all keychain secrets
                                        let deleted = secret_manager.write().delete_all();
                                        tracing::info!("Cleared {} keychain items for mode switch", deleted.len());

                                        // Clear API keys from local settings
                                        {
                                            let mut ls = local_settings.write();
                                            ls.gemini_config.api_key = None;
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

                        // Critical: Update the global settings signal so the app (menus, hotkeys) reacts immediately
                        *global_settings = settings_to_save.clone();
                        has_unsaved_changes.set(false);

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
                                    let save_result = if use_biometric {
                                        // Biometric mode: device-only, Touch ID protected
                                        crate::secret_manager::set_generic_password_with_biometric_protection(&key_name, &key_value)
                                            .or_else(|e| {
                                                if let crate::secret_manager::KeychainError::SecurityError(-34018) = e {
                                                    // Fall back to regular save if entitlements missing
                                                    crate::secret_manager::set_generic_password(&key_name, &key_value)
                                                } else {
                                                    Err(e)
                                                }
                                            })
                                    } else {
                                        // iCloud sync mode: syncs across devices, no biometric
                                        crate::secret_manager::set_generic_password(&key_name, &key_value)
                                    };

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
            // Sidebar (shrink-0 to not affect content width)
            div {
                class: "w-48 shrink-0 flex flex-col border-r border-subtle bg-section",
                h2 { class: "text-lg font-bold p-4 border-b border-subtle", "Settings" }
                // Tabs
                button {
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::General { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Mcp { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Behavior { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Data { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Permissions { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Hotkeys { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::Credentials { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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
                    class: if ui_state.read().active_settings_tab == crate::settings::SettingsTab::About { "flex items-center gap-3 p-3 bg-primary-700/50 text-fg border-l-4 border-primary-500" } else { "flex items-center gap-3 p-3 text-fg-muted hover:bg-white/5 hover:text-fg border-l-4 border-transparent" },
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

            // Content Area (maintains original settings pane width)
            div {
                class: "flex-1 flex flex-col min-w-0 p-4",
                div {
                   class: "flex-1 overflow-y-auto pr-2",
                   match ui_state.read().active_settings_tab {
                       crate::settings::SettingsTab::General => rsx! {

                // LLM Configuration Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: toggle_llm_collapsed,
                        h3 { class: "text-md font-semibold", "LLM Configuration" }
                        span { if ui_state.read().llm_config_collapsed { "▶" } else { "▼" } }
                    }
                    if !ui_state.read().llm_config_collapsed {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-fg-muted", "LLM Provider" }
                                select {
                                    class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                    option { value: "Gemini", "Gemini" }
                                }
                            }
                            if local_settings.read().active_llm == crate::settings::LlmProvider::Gemini {
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
                                                    class: "flex-1 px-3 py-2 rounded-md text-sm font-medium bg-btn-primary text-fg text-center",
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
                                                let gemini_model = crate::components::llm::GeminiModel::from_slug(&current_model);

                                                match gemini_model.thinking_config_style() {
                                                    crate::components::llm::ThinkingConfigStyle::LevelPro |
                                                    crate::components::llm::ThinkingConfigStyle::LevelFlash => {
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
                                                    crate::components::llm::ThinkingConfigStyle::Budget => {
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
                                                    crate::components::llm::ThinkingConfigStyle::None => {
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
                                                            let slot_model = local_settings.read().model_slots.get(i).cloned().unwrap_or_default();
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
                                                                    let mut settings = local_settings.write();
                                                                    if i < settings.model_slots.len() {
                                                                        settings.model_slots[i] = evt.value();
                                                                    } else {
                                                                        while settings.model_slots.len() <= i {
                                                                            settings.model_slots.push("".to_string());
                                                                        }
                                                                        settings.model_slots[i] = evt.value();
                                                                    }
                                                                },
                                                                {
                                                                    let current_slot_value = local_settings.read().model_slots.get(i).cloned().unwrap_or_default();
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
                                                                    let slot_model_for_picker = local_settings.read().model_slots.get(i).cloned().unwrap_or_default();
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
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "p-4",
                        div {
                            class: "mb-4",
                            label { class: "block text-sm font-medium text-fg-muted mb-2", "Preferred MCP Source" }
                            div {
                                class: "flex space-x-4",
                                button {
                                    class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                        "flex-1 px-4 py-2 rounded-md bg-btn-primary text-fg font-medium shadow-sm ring-2 ring-primary-400"
                                    } else {
                                        "flex-1 px-4 py-2 rounded-md bg-input text-fg-muted font-medium hover:bg-input hover:text-fg transition-colors"
                                    },
                                    onclick: move |_| {
                                        local_settings.write().preferred_mcp_source = crate::settings::McpSource::Composio;
                                    },
                                    "Composio"
                                }
                                button {
                                    class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                        "flex-1 px-4 py-2 rounded-md bg-red-900/50 text-red-200 font-medium shadow-sm ring-2 ring-red-700 border border-red-700"
                                    } else {
                                        "flex-1 px-4 py-2 rounded-md bg-input text-fg-muted font-medium hover:bg-input hover:text-fg transition-colors"
                                    },
                                    onclick: move |_| {
                                        local_settings.write().preferred_mcp_source = crate::settings::McpSource::Smithery;
                                    },
                                    "Smithery.ai (Deprecated)"
                                }
                            }

                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                div {
                                    class: "mt-3 p-3 bg-red-900/30 border border-red-700 rounded-lg",
                                    p {
                                        class: "text-red-200 text-sm flex items-center gap-2",
                                        Icon { width: 16, height: 16, icon: fi_icons::FiAlertTriangle }
                                        "This integration is deprecated. We recommend using Composio directly."
                                    }
                                }
                            }
                                p {
                                class: "text-xs text-fg-muted mt-2",
                                "Choose which registry to use when installing new MCP servers. Smithery uses a hosted proxy (requires API key), while Composio runs locally."
                            }

                            // Smithery API Key - shown when Smithery is selected
                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                div {
                                    class: "mt-4 pt-4 border-t border-subtle",
                                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Smithery API Key" }
                                    input {
                                        class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                        r#type: "password",
                                        placeholder: "Enter your Smithery.ai API key",
                                        value: "{local_settings.read().smithery_api_key.as_deref().unwrap_or(\"\")}",
                                        oninput: move |event| local_settings.write().smithery_api_key = Some(event.value().trim().to_string())
                                    }
                                    p {
                                        class: "text-xs text-fg-muted mt-1",
                                        "Required for Smithery.ai marketplace access"
                                    }
                                }
                            }

                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                div {
                                    class: "mt-4 pt-4 border-t border-subtle",
                                    // Instructions
                                    div {
                                        class: "mb-6 bg-app/50 rounded-lg border border-subtle/50 overflow-hidden",
                                        div {
                                            class: "flex justify-between items-center p-4 cursor-pointer bg-section/50 hover:bg-section transition-colors",
                                            onclick: toggle_mcp_instructions_collapsed,
                                            h4 { class: "text-sm font-semibold text-fg", "Setup Instructions" }
                                            span { if ui_state.read().mcp_instructions_collapsed { "▶" } else { "▼" } }
                                        }
                                        if !ui_state.read().mcp_instructions_collapsed {
                                            div {
                                                class: "p-4 border-t border-subtle/30",
                                                ol {
                                                    class: "list-decimal list-inside text-sm text-fg-muted space-y-1.5",
                                                    li {
                                                        "Get your API key from "
                                                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "https://composio.dev/settings", target: "_blank", "composio.dev/settings" }
                                                    }
                                                    li { "Click \"+ Add Profile\" and paste your API key" }
                                                    li { "Use the MCP Marketplace to connect your first tool (Gmail, GitHub, etc.)" }
                                                    li { "Your Server URL will be automatically created - you're done!" }
                                                }
                                                div {
                                                    class: "mt-3 pt-3 border-t border-subtle/30",
                                                    a {
                                                        class: "text-xs text-primary-400 hover:text-primary-300 flex items-center gap-1",
                                                        href: "https://docs.composio.dev/docs/welcome",
                                                        target: "_blank",
                                                        Icon { width: 12, height: 12, icon: fi_icons::FiExternalLink }
                                                        "View Documentation"
                                                    }
                                                }
                                            }
                                        }
                                    }

                                        // Profile list header
                                        div {
                                            class: "flex justify-between items-center mb-3",
                                            label { class: "block text-sm font-medium text-fg-muted", "Composio Profiles" }
                                            button {
                                                class: "px-3 py-1 bg-btn-primary rounded-md text-fg text-sm font-medium hover:bg-btn-primary-hover",
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
                                                                if is_active { "bg-input border border-primary-500 ring-1 ring-primary-500/20" } else { "bg-input/50 border border-transparent hover:bg-input" }),
                                                            div {
                                                                class: "flex items-center gap-3",
                                                                input {
                                                                    r#type: "radio",
                                                                    class: "w-4 h-4 text-primary-500 focus:ring-primary-500 bg-transparent border-faint",
                                                                    name: "active_profile",
                                                                    checked: is_active,
                                                                    onchange: {
                                                                        let name = profile_name.clone();
                                                                        let mut global_settings = settings;
                                                                        move |_| {
                                                                            tracing::info!("Switching to profile: {}", name);
                                                                            local_settings.write().active_composio_profile = Some(name.clone());
                                                                            // RACE CONDITION FIX: Immediately propagate changes to global settings and signal
                                                                            active_composio_profile_name.set(Some(name.clone()));
                                                                            global_settings.write().active_composio_profile = Some(name.clone());
                                                                        }
                                                                    }
                                                                }
                                                                span {
                                                                    class: format!("text-sm font-medium {}", if is_active { "text-fg" } else { "text-fg-muted" }),
                                                                    "{profile_name}"
                                                                }
                                                                if is_active {
                                                                    span {
                                                                        class: "px-2 py-0.5 bg-blue-600 text-fg rounded text-[10px] font-bold uppercase tracking-wider",
                                                                        "Active"
                                                                    }
                                                                }
                                                            }
                                                            button {
                                                                class: "text-xs font-medium text-red-500 hover:text-red-400 transition-colors uppercase tracking-tight",
                                                                onclick: {
                                                                    let name = profile_name.clone();
                                                                    move |_| {
                                                                        if local_settings.peek().active_composio_profile.as_ref() == Some(&name) {
                                                                            active_composio_profile_name.set(None);
                                                                        }
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
                                                class: "border border-subtle rounded-lg p-3",
                                                h4 { class: "text-sm font-medium text-fg-muted mb-3", "Edit Profile: {active_name}" }

                                                // Profile Name
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Profile Name" }
                                                    input {
                                                        class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
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
                                                                    settings.active_composio_profile = Some(new_name.clone());
                                                                    active_composio_profile_name.set(Some(new_name));
                                                                }
                                                            }
                                                        }
                                                    }
                                                }

                                                // Profile Color
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Profile Color" }
                                                    div {
                                                        class: "flex gap-2 flex-wrap",
                                                        for color in ["bg-blue-600", "bg-purple-600", "bg-green-600", "bg-red-600", "bg-orange-600", "bg-pink-600", "bg-teal-600", "bg-input"] {
                                                            button {
                                                                class: format!("w-6 h-6 rounded-full cursor-pointer transition-transform hover:scale-110 {} {}",
                                                                    color,
                                                                    if local_settings.read().get_active_profile().map(|p| p.color.as_str()) == Some(color) { "ring-2 ring-white scale-110 border border-transparent shadow-md" } else { "border border-faint hover:border-white" }
                                                                ),
                                                                onclick: {
                                                                    let color = color.to_string();
                                                                    move |_| {
                                                                        let mut settings = local_settings.write();
                                                                        if let Some(profile) = settings.get_active_profile_mut() {
                                                                            profile.color = color.clone();
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                // Chrome Profile (Auth Security)
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Chrome Profile for Auth" }
                                                    div {
                                                        class: "flex gap-2",
                                                        select {
                                                            class: "flex-1 px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                            onchange: {
                                                                let name = active_name.clone();
                                                                move |evt: Event<FormData>| {
                                                                    let val = evt.value();
                                                                    let mut settings = local_settings.write();
                                                                    if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                        profile.chrome_profile_directory = if val.is_empty() { None } else { Some(val) };
                                                                    }
                                                                }
                                                            },
                                                            {
                                                                let current_chrome = local_settings.read().get_active_profile()
                                                                    .and_then(|p| p.chrome_profile_directory.clone())
                                                                    .unwrap_or_default();
                                                                let chrome_profiles = crate::settings::discover_chrome_profiles();
                                                                rsx! {
                                                                    option { value: "", selected: current_chrome.is_empty(), "System Default (any window)" }
                                                                    for cp in chrome_profiles.iter() {
                                                                        option {
                                                                            value: "{cp.dir_name}",
                                                                            selected: current_chrome == cp.dir_name,
                                                                            if let Some(ref email) = cp.email {
                                                                                "{cp.display_name} — {email}"
                                                                            } else {
                                                                                "{cp.display_name}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    p {
                                                        class: "text-xs text-fg-muted mt-1",
                                                        "Ensures OAuth opens in the correct Chrome profile window."
                                                    }
                                                }


                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "API Key" }
                                                    input {
                                                        r#type: "password",
                                                        class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                        placeholder: "Enter your Composio API key from composio.dev/settings",
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
                                                    p {
                                                        class: "text-xs text-fg-muted mt-1",
                                                        "Get your API key from "
                                                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "https://composio.dev/settings", target: "_blank", "composio.dev/settings" }
                                                    }
                                                }

                                                // Advanced Settings Toggle
                                                div {
                                                    class: "mb-3 pt-3 border-t border-subtle/50",
                                                    button {
                                                        class: "flex items-center gap-2 text-xs text-fg-muted hover:text-fg-muted transition-colors",
                                                        onclick: move |_| {
                                                            show_composio_advanced.set(!show_composio_advanced());
                                                        },
                                                        span { if show_composio_advanced() { "▼" } else { "▶" } }
                                                        span { "Advanced Settings" }
                                                        span { class: "text-fg-muted", "(User ID, Server URL)" }
                                                    }
                                                }

                                                // Advanced Section (hidden by default)
                                                if show_composio_advanced() {
                                                    div {
                                                        class: "space-y-3 p-3 bg-app/50 rounded-lg border border-subtle/30",

                                                        // User ID (Read-only + Copy/Regenerate)
                                                        div {
                                                            label { class: "block text-xs font-medium text-fg-muted mb-1", "User ID" }
                                                            div {
                                                                class: "flex gap-2",
                                                                input {
                                                                    class: "flex-1 px-3 py-2 bg-app border border-subtle rounded-md text-sm text-fg-muted cursor-not-allowed",
                                                                    readonly: true,
                                                                    value: "{local_settings.read().get_active_profile().and_then(|p| p.user_id.clone()).unwrap_or_else(|| \"Auto-generated\".to_string())}"
                                                                }
                                                                // Copy Button
                                                                button {
                                                                    class: "px-3 py-2 bg-input hover:bg-white/10 border border-primary-600 rounded-md text-fg-muted transition-colors",
                                                                    title: "Copy User ID",
                                                                    onclick: {
                                                                        let user_id = local_settings.read().get_active_profile().map(|p| p.user_id.clone()).unwrap_or_default();
                                                                        move |_| {
                                                                            if user_id.as_ref().is_none_or(|s| !s.is_empty()) {
                                                                                use std::process::Command;
                                                                                 let _ = Command::new("pbcopy")
                                                                                     .stdin(std::process::Stdio::piped())
                                                                                     .spawn()
                                                                                     .map(|mut child| {
                                                                                         use std::io::Write;
                                                                                         if let Some(mut stdin) = child.stdin.take() {
                                                                                             let _ = stdin.write_all(user_id.as_deref().unwrap_or("").as_bytes());
                                                                                         }
                                                                                     });
                                                                            }
                                                                        }
                                                                    },
                                                                    Icon { width: 16, height: 16, icon: fi_icons::FiCopy }
                                                                }
                                                                // Regenerate Button
                                                                button {
                                                                    class: "px-3 py-2 bg-input hover:bg-white/10 border border-primary-600 rounded-md text-fg-muted transition-colors",
                                                                    title: "Regenerate User ID",
                                                                    onclick: {
                                                                        let name = active_name.clone();
                                                                        move |_| {
                                                                            let mut settings = local_settings.write();
                                                                            if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                                profile.user_id = Some(uuid::Uuid::new_v4().to_string().to_lowercase());
                                                                            }
                                                                        }
                                                                    },
                                                                    Icon { width: 16, height: 16, icon: fi_icons::FiRefreshCw }
                                                                }
                                                            }
                                                            p { class: "text-xs text-fg-muted mt-1", "Auto-generated on profile creation. Only change if troubleshooting." }
                                                        }

                                                        // Server URL
                                                        div {
                                                            label { class: "block text-xs font-medium text-fg-muted mb-1", "Server URL (Optional)" }
                                                            input {
                                                                class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                                placeholder: "Auto-created when you connect your first tool",
                                                                value: "{local_settings.read().get_active_profile().and_then(|p| p.base_url.clone()).unwrap_or_default()}",
                                                                oninput: {
                                                                    let name = active_name.clone();
                                                                    move |event: Event<FormData>| {
                                                                        let val = event.value();
                                                                        let mut clean_val = val.clone();
                                                                        let mut warning = None;

                                                                        // Sanitize URL if it contains user_id
                                                                        if let Ok(mut url) = url::Url::parse(&val) {
                                                                            if url.query_pairs().any(|(k, _)| k == "user_id") {
                                                                                let pairs: Vec<(String, String)> = url.query_pairs()
                                                                                    .filter(|(k, _)| k != "user_id")
                                                                                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                                                                    .collect();

                                                                                url.query_pairs_mut().clear();
                                                                                for (k, v) in pairs {
                                                                                    url.query_pairs_mut().append_pair(&k, &v);
                                                                                }

                                                                                // Clean up if query is empty (Url::to_string might leave ?)
                                                                                if url.query() == Some("") {
                                                                                    url.set_query(None);
                                                                                }

                                                                                clean_val = url.to_string();
                                                                                warning = Some("Note: Embedded User ID removed. Using the app-generated User ID.".to_string());
                                                                            }
                                                                        }

                                                                        composio_url_warning.set(warning);

                                                                        let mut settings = local_settings.write();
                                                                        if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.name == name) {
                                                                            profile.base_url = if clean_val.is_empty() { None } else { Some(clean_val) };
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            if let Some(msg) = composio_url_warning.read().as_ref() {
                                                                p { class: "text-xs text-yellow-400 mt-1 flex items-center gap-1",
                                                                    Icon { width: 12, height: 12, icon: fi_icons::FiAlertCircle }
                                                                    "{msg}"
                                                                }
                                                            }
                                                            p { class: "text-xs text-fg-muted mt-1", "Leave empty to auto-create on first tool connection." }
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
                                                    class: "text-xs text-fg-muted mt-2",
                                                    "Connection happens automatically when you select a profile."
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                   }, // End Mcp
                   crate::settings::SettingsTab::Behavior => rsx! {
                // Application Behavior Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: move |_| app_behavior_collapsed.set(!app_behavior_collapsed()),
                        h3 { class: "text-md font-semibold", "Application Behavior" }
                        span { if *app_behavior_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !app_behavior_collapsed() {
                        div {
                            class: "p-4 space-y-6",

                            // 0. Appearance
                            div {
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Appearance" }
                                div {
                                    class: "flex flex-col gap-1",
                                    label { class: "text-sm font-medium text-fg-muted", "Theme" }
                                    p { class: "text-xs text-fg-muted", "Choose application color theme" }
                                    select {
                                        class: "bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                        onchange: move |e| {
                                            let theme = match e.value().as_str() {
                                                "Light" => crate::settings::Theme::Light,
                                                "System" => crate::settings::Theme::System,
                                                _ => crate::settings::Theme::Dark,
                                            };
                                            local_settings.write().theme = theme;
                                        },
                                        option { value: "Dark", selected: local_settings.read().theme == crate::settings::Theme::Dark, "Dark" }
                                        option { value: "Light", selected: local_settings.read().theme == crate::settings::Theme::Light, "Light" }
                                        option { value: "System", selected: local_settings.read().theme == crate::settings::Theme::System, "System" }
                                    }
                                }
                            }

                            // 1. Context & History
                            div {
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Context & History" }
                                div {
                                    class: "space-y-3",
                                    // Chat History Length
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Chat History Length" }
                                        p { class: "text-xs text-fg-muted", "Number of past messages included in context window" }
                                        input {
                                            r#type: "number",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().chat_history_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().chat_history_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Tool Output Length
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Max Tool Output Length" }
                                        p { class: "text-xs text-fg-muted", "Maximum characters displayed in tool outputs (0 for unlimited)" }
                                        input {
                                            r#type: "number",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().max_tool_output_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_tool_output_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Active Tool Output Length
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Max Active Tool Output Length" }
                                        p { class: "text-xs text-fg-muted", "Maximum characters persisted for tool context" }
                                        input {
                                            r#type: "number",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().max_active_tool_output_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_active_tool_output_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Memory Summary Length
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Max Memory Summary Length" }
                                        p { class: "text-xs text-fg-muted", "Character limit for conversation summary (~4 chars = 1 token)" }
                                        input {
                                            r#type: "number",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().max_summary_chars}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_summary_chars = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Stored Entities
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Max Stored Entities" }
                                        p { class: "text-xs text-fg-muted", "Maximum entities retained in memory (topics, facts, goals)" }
                                        input {
                                            r#type: "number",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().max_entity_count}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_entity_count = value;
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 2. Chat Bar Icons
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Chat Bar Icons" }
                                div {
                                    class: "grid grid-cols-2 gap-3",

                                    // History Icon
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show History Icon" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_history_icon}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_history_icon = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }

                                    // MCP Tools Icon
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show MCP Tools Icon" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_mcp_icon}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_mcp_icon = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }

                                    // Session Cost Icon
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show Session Cost" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_session_cost_icon}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_session_cost_icon = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }

                                    // Profile Selector
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show Profile Selector" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_profile_selector}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_profile_selector = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }

                                    // Attachments Icon
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show Attachments Icon" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_attachments_icon}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_attachments_icon = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }

                                    // Model Selector
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show Model Selector" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().show_model_selector}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.show_model_selector = e.value() == "true";
                                                let state_clone = (*state).clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                }
                            }

                            // 3. Confirmation Dialogs
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Confirmation Dialogs" }
                                div {
                                    class: "space-y-3",
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Confirm before deleting sessions" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().confirm_on_delete}",
                                            onchange: move |e| {
                                                local_settings.write().confirm_on_delete = e.value() == "true";
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Confirm before saving settings" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().confirm_on_save}",
                                            onchange: move |e| {
                                                local_settings.write().confirm_on_save = e.value() == "true";
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Confirm before deleting messages" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().confirm_on_message_delete}",
                                            onchange: move |e| {
                                                local_settings.write().confirm_on_message_delete = e.value() == "true";
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Confirm before optimizing memory" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().confirm_forget_memory}",
                                            onchange: move |e| {
                                                local_settings.write().confirm_forget_memory = e.value() == "true";
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Show System Tray Icon" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().show_tray_icon}",
                                            onchange: move |e| {
                                                local_settings.write().show_tray_icon = e.value() == "true";
                                            }
                                        }
                                    }
                                }
                            }

                            // 4. Tool Display Defaults
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Tool Display Defaults" }
                                p { class: "text-xs text-fg-muted mb-3", "Set initial state for collapsible sections in tool call bubbles." }

                                div {
                                    class: "space-y-3",
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Arguments by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_tool_arguments_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_tool_arguments_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Response by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_tool_response_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_tool_response_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Thinking Process by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_tool_thought_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_tool_thought_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                }
                            }

                            // 4b. Skill Display Defaults
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Skill Display Defaults" }
                                p { class: "text-xs text-fg-muted mb-3", "Set initial state for collapsible sections in skill call bubbles." }

                                div {
                                    class: "space-y-3",
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Arguments by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_skill_arguments_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_skill_arguments_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Output / Payload by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_skill_response_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_skill_response_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm text-fg-muted", "Expand Instructions by Default" }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{ui_state.read().default_skill_instructions_open}",
                                            onchange: move |e| {
                                                let mut state = ui_state.write();
                                                state.default_skill_instructions_open = e.value() == "true";
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                }
                            }

                            // 5. AI Behavior
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "AI Behavior" }
                                div {
                                    class: "space-y-3",
                                    // Max AI Turns
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Max AI Turns" }
                                        p { class: "text-xs text-fg-muted", "Maximum consecutive responses allowed" }
                                        select {
                                            class: "bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            onchange: move |e| {
                                                if let Ok(value) = e.value().parse::<u32>() {
                                                    local_settings.write().permission_settings.max_ai_turns = value;
                                                }
                                            },
                                            option { value: "1", selected: local_settings.read().permission_settings.max_ai_turns == 1, "1 (Strict turn-taking)" }
                                            option { value: "3", selected: local_settings.read().permission_settings.max_ai_turns == 3, "3 (Default)" }
                                            option { value: "5", selected: local_settings.read().permission_settings.max_ai_turns == 5, "5 (More autonomy)" }
                                            option { value: "10", selected: local_settings.read().permission_settings.max_ai_turns == 10, "10 (For complex tasks)" }
                                        }
                                    }

                                    // User Name
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "What should Hobbes call you?" }
                                        input {
                                            r#type: "text",
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                            placeholder: "Dr. Calvin",
                                            value: "{local_settings.read().user_name.clone().unwrap_or_default()}",
                                            oninput: move |e| {
                                                let value = e.value();
                                                local_settings.write().user_name = if value.is_empty() { None } else { Some(value) };
                                            }
                                        }
                                    }
                                }
                            }

                            // 6. Persona & Instructions
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-semibold text-fg-muted mb-3", "Persona & Instructions" }
                                div {
                                    class: "space-y-3",
                                    // Persona
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Persona" }
                                        textarea {
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg h-24 focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().persona}",
                                            oninput: move |e| local_settings.write().persona = e.value()
                                        }
                                    }

                                    // Force Tool Use Instruction
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Force Tool Use Instruction" }
                                        p { class: "text-xs text-fg-muted", "Appended to system prompt to encourage specific behaviors" }
                                        textarea {
                                            class: "w-full bg-app border border-faint rounded p-2 text-fg h-24 focus:border-blue-500 focus:outline-none",
                                            value: "{local_settings.read().force_tool_use_instruction.clone().unwrap_or_default()}",
                                            oninput: move |e| local_settings.write().force_tool_use_instruction = Some(e.value())
                                        }
                                    }

                                    // Project Folder
                                    div {
                                        class: "flex flex-col gap-1",
                                        label { class: "text-sm font-medium text-fg-muted", "Project Folder" }
                                        div {
                                            class: "flex gap-2",
                                            input {
                                                r#type: "text",
                                                class: "flex-1 bg-app border border-faint rounded p-2 text-fg focus:border-blue-500 focus:outline-none",
                                                value: "{local_settings.read().project_folder.clone().unwrap_or_default()}",
                                                readonly: true,
                                            }
                                            button {
                                                class: "px-3 py-2 bg-card hover:bg-input rounded border border-faint transition-colors",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        if let Some(folder_path) = rfd::AsyncFileDialog::new().pick_folder().await {
                                                            local_settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                                        }
                                                    });
                                                },
                                                "Browse"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                   }, // End Behavior
                   crate::settings::SettingsTab::Data => rsx! {
                // Data Management Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
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
                                    class: "px-4 py-2 bg-btn-primary rounded-md text-fg font-semibold hover:bg-btn-primary-hover",
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
                                    class: "px-4 py-2 bg-btn-primary rounded-md text-fg font-semibold hover:bg-btn-primary-hover",
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
                                    class: "px-4 py-2 bg-secondary-500 rounded-md text-fg font-semibold hover:bg-secondary-600",
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
                                    class: "px-4 py-2 bg-secondary-500 rounded-md text-fg font-semibold hover:bg-secondary-600",
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
                                                        // Intentional: branches have different side effects (insert vs. push to conflict list).
                                                        #[allow(clippy::map_entry)]
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
                                                            crate::session::SessionState::save_async(current_state.clone(), Some(save_error));
                                                            tracing::info!("Successfully imported history with no conflicts.");
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
                            // Keychain Management
                            div {
                                class: "mt-4 pt-4 border-t border-subtle",
                                h4 { class: "text-sm font-medium text-fg-muted mb-2", "Keychain Management" }
                                p {
                                    class: "text-xs text-fg-muted mb-3",
                                    "Manually reset all stored API keys. You'll need to re-enter them after resetting."
                                }
                                button {
                                    class: "px-4 py-2 bg-red-600 rounded-md text-fg font-semibold hover:bg-red-700",
                                    onclick: move |_| {
                                        // Delete all keychain items
                                        let deleted_keys = secret_manager.write().delete_all();
                                        tracing::info!("Reset {} keychain items.", deleted_keys.len());

                                        // Clear API keys from local settings
                                        let mut ls = local_settings.write();
                                        ls.gemini_config.api_key = None;
                                        ls.smithery_api_key = None;
                                        for profile in ls.composio_profiles.iter_mut() {
                                            profile.api_key = None;
                                        }
                                    },
                                    "Reset Keychain Secrets"
                                }
                            }
                        }
                    }
                }

                   }, // End Data
                   crate::settings::SettingsTab::Permissions => rsx! {
                // Permissions Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: move |_| permissions_collapsed.set(!permissions_collapsed()),
                        h3 { class: "text-md font-semibold", "Permissions" }
                        span { if *permissions_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !permissions_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex items-center justify-between mb-4",
                                label { class: "block text-sm font-medium text-fg-muted", "Enable Auto-Approval" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().permission_settings.auto_approval_enabled,
                                        oninput: move |event| {
                                            if let Ok(checked) = event.value().parse() {
                                                local_settings.write().permission_settings.auto_approval_enabled = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            if local_settings.read().permission_settings.auto_approval_enabled {
                                div {
                                    class: "pl-4 border-l-2 border-subtle space-y-2",
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm font-medium text-fg-muted", "MCP Tools (Global)" }
                                        label {
                                            class: "relative inline-flex items-center cursor-pointer",
                                            input {
                                                r#type: "checkbox",
                                                class: "sr-only peer",
                                                checked: local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false),
                                                oninput: move |event| {
                                                    if let Ok(checked) = event.value().parse() {
                                                        local_settings.write().permission_settings.granular_permissions.insert(ToolCategory::Mcp, checked);
                                                    }
                                                }
                                            }
                                            div { class: if local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false) { "toggle-switch active" } else { "toggle-switch" } }
                                        }
                                    }

                                    if local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false) {
                                        div {
                                            class: "mt-2 pl-4 border-l border-faint space-y-2",
                                            h4 { class: "text-xs font-semibold text-fg-muted uppercase tracking-wider mb-2", "Granular MCP Permissions" }
                                            for server in _mcp_context.read().servers.iter() {
                                                {
                                                    let server_name = server.name.clone();
                                                    let is_allowed = local_settings.read().permission_settings.mcp_server_permissions.get(&server_name).copied().unwrap_or(true);

                                                    rsx! {
                                                        div {
                                                            key: "{server_name}",
                                                            class: "flex items-center justify-between",
                                                            div {
                                                                class: "flex flex-col",
                                                                span { class: "text-sm text-fg-muted", "{server_name}" }
                                                                if !server.description.is_empty() {
                                                                    span { class: "text-[10px] text-fg-muted", "{server.description}" }
                                                                }
                                                            }
                                                            label {
                                                                class: "relative inline-flex items-center cursor-pointer",
                                                                input {
                                                                    r#type: "checkbox",
                                                                    class: "sr-only peer",
                                                                    checked: is_allowed,
                                                                    oninput: {
                                                                        let server_name = server_name.clone();
                                                                        move |event: Event<FormData>| {
                                                                            if let Ok(checked) = event.value().parse() {
                                                                                local_settings.write().permission_settings.mcp_server_permissions.insert(server_name.clone(), checked);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                div { class: if is_allowed { "toggle-switch-sm active" } else { "toggle-switch-sm" } }
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

            // Skill Permissions Section
            div {
                class: "border border-subtle rounded-lg mb-4",
                div {
                    class: "p-4 bg-section rounded-lg",
                    h3 { class: "text-md font-semibold mb-4", "Skill Permissions" }
                    p { class: "text-xs text-fg-muted mb-4",
                        "Manage auto-approval for specific skills. When enabled, these skills will execute without prompting."
                    }
                    
                    {
                        // Get all skills from registry AND saved permissions
                        let registry = skill_registry.read();
                        let saved_permissions = local_settings.read().permission_settings.skill_permissions.clone();
                        
                        // Build merged map: registry skills + any saved permissions
                        let mut all_skills: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
                        
                        // Add all registered skills (default to false if no saved permission)
                        for skill in registry.list_skills() {
                            let is_allowed = saved_permissions.get(&skill.metadata.name).copied().unwrap_or(false);
                            all_skills.insert(skill.metadata.name.clone(), is_allowed);
                        }
                        
                        // Also include any saved permissions for skills not currently in registry
                        // (in case a skill was removed but permission was saved)
                        for (name, allowed) in &saved_permissions {
                            if !all_skills.contains_key(name) {
                                all_skills.insert(name.clone(), *allowed);
                            }
                        }
                        
                        if all_skills.is_empty() {
                            rsx! {
                                div {
                                    class: "flex items-center justify-center p-8 border border-dashed border-primary-800 rounded-lg",
                                    p { class: "text-sm text-fg-muted", "No skills discovered. Add skills to ~/.hobbes/skills/ to see them here." }
                                }
                            }
                        } else {
                            // Sort skills alphabetically for cleaner UI
                            let mut sorted_skills: Vec<_> = all_skills.into_iter().collect();
                            sorted_skills.sort_by(|a, b| a.0.cmp(&b.0));

                            rsx! {
                                div {
                                    class: "space-y-3",
                                    for (skill_name, is_allowed) in sorted_skills {
                                        {
                                            // Metadata Lookup
                                        let skill_opt = registry.get_skill(&skill_name);
                                        let description = skill_opt.as_ref()
                                            .map(|s| s.metadata.description.clone())
                                            .unwrap_or_else(|| "No description available.".to_string());
                                        
                                            rsx! {
                                                div {
                                            class: "flex items-center justify-between p-4 bg-input rounded-lg border border-primary-800 hover:border-primary-600 transition-colors",
                                            div {
                                                class: "flex flex-col max-w-[70%]",
                                                div {
                                                    class: "flex items-center gap-2",
                                                    span { class: "text-sm font-bold text-fg", "/{skill_name}" }
                                                    if is_allowed {
                                                        span { class: "px-1.5 py-0.5 text-[10px] font-bold bg-action-success text-action-success-text rounded", "ALLOWED" }
                                                    }
                                                }
                                                span { class: "text-xs text-fg-muted mt-1 line-clamp-2", "{description}" }
                                            }
                                            div {
                                                class: "relative inline-block w-10 h-5 align-middle select-none transition duration-200 ease-in",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "toggle-checkbox absolute block w-5 h-5 rounded-full bg-white border-4 appearance-none cursor-pointer",
                                                    checked: is_allowed,
                                                    oninput: {
                                                        let skill_name = skill_name.clone();
                                                        move |event: Event<FormData>| {
                                                            if let Ok(checked) = event.value().parse() {
                                                                // Use write() to trigger signal notification (Pattern 33)
                                                                local_settings.write().permission_settings.skill_permissions.insert(skill_name.clone(), checked);
                                                            }
                                                        }
                                                    }
                                                }
                                                div { class: if is_allowed { "toggle-switch-sm active" } else { "toggle-switch-sm" } }
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
        }, // End Permissions
                    crate::settings::SettingsTab::Hotkeys => rsx! {
                        div {
                            class: "border border-subtle rounded-lg mb-4",
                            div {
                                class: "p-4 bg-section rounded-lg",
                                h3 { class: "text-md font-semibold mb-4", "Keyboard Shortcuts" }
                                p { class: "text-xs text-fg-muted mb-4",
                                    "Customize global shortcuts using the format: "
                                    code { class: "bg-black/30 px-1 rounded", "CmdOrCtrl+Shift+Key" }
                                ". Changes apply immediately after saving."
                                }
                                if local_settings.read().hotkeys != settings.read().hotkeys {
                                    div {
                                        class: "mb-4 p-3 bg-yellow-900/30 border border-yellow-700/50 rounded-md flex items-center gap-3",
                                        Icon {
                                            width: 18,
                                            height: 18,
                                            fill: "none",
                                            class: "text-yellow-500 min-w-[18px]",
                                            icon: fi_icons::FiAlertTriangle,
                                        }
                                        span { class: "text-xs text-yellow-200", "Restart required to apply changes." }
                                    }
                                }

                                div {
                                    class: "space-y-4",

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Submit Message" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.submit_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.submit_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Cancel Generation" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.cancel_generation.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.cancel_generation = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Focus Chat Input" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_focus_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_focus_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle Settings" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_settings.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_settings = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle History" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_history.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_history = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Toggle MCP Config" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_mcp.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_mcp = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Open Profile Selector" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_profile.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_profile = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Add Attachments" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_attachments.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_attachments = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Global Toggle (Show/Hide)" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_tray.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_tray = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "New Chat (No Memory)" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_new_chat.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_new_chat = v,
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "New Chat with Memory" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.toggle_new_chat_with_memory.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.toggle_new_chat_with_memory = v,
                                        }
                                    }

                                    div { class: "pt-2 pb-1", h4 { class: "text-xs font-bold text-fg-muted uppercase tracking-wider", "Session Tabs" } }

                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 1" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_1.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_1 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 2" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_2.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_2 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 3" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_3.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_3 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 4" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_4.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_4 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 5" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_5.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_5 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 6" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_6.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_6 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 7" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_7.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_7 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 8" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_8.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_8 = v,
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-2 items-center gap-4",
                                        label { class: "text-sm text-fg-muted", "Switch to Tab 9" }
                                        HotkeyRecorder {
                                            value: local_settings.read().hotkeys.switch_tab_9.clone(),
                                            onchange: move |v: String| local_settings.write().hotkeys.switch_tab_9 = v,
                                        }
                                    }

                                    // Reset to Defaults button
                                    div {
                                        class: "pt-4 border-t border-gray-800",
                                        button {
                                            class: "px-4 py-2 bg-input rounded-md text-sm text-fg-muted hover:bg-input hover:text-fg transition-colors",
                                            onclick: move |_| {
                                                local_settings.write().hotkeys = HotkeySettings::default();
                                            },
                                            "Reset to Defaults"
                                        }
                                    }

                                    div {
                                        class: "pt-4 border-t border-gray-800",

                                        div {
                                            class: "flex justify-between items-center text-sm",
                                            span { class: "text-fg-muted", "Switch Profile (1-9)" }
                                            span { class: "font-mono text-fg-muted", "CmdOrCtrl + [1-9]" }
                                        }
                                        p { class: "text-[10px] text-fg-muted mt-1", "Profile switching hotkeys are currently fixed." }
                                    }
                                }
                            }
                        }
                    },
                    crate::settings::SettingsTab::Credentials => rsx! {
                         ToolCredentials {}
                    },
                    crate::settings::SettingsTab::About => rsx! {
                // About & Legal Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "p-4 bg-section rounded-lg",
                        h3 { class: "text-md font-semibold mb-3", "About & Legal" }

                        // Version and app name
                        div {
                            class: "flex items-center gap-2 mb-3",
                            span { class: "text-sm text-fg-muted", "{app_name}" }
                            span { class: "text-xs text-fg-muted", "{app_version}" }
                        }

                        // Privacy statement
                        p {
                            class: "text-sm text-green-400 mb-3",
                            "🔒 Built without telemetry for your privacy."
                        }

                        // Attribution
                        p {
                            class: "text-xs text-fg-muted mb-4",
                            "{crate::settings::APP_ATTRIBUTION}"
                        }

                        // Legal links
                        div {
                            class: "flex gap-4",
                            button {
                                class: "text-sm text-primary-400 hover:text-primary-300 underline cursor-pointer bg-transparent border-none p-0",
                                onclick: move |_| show_tos_modal.set(true),
                                "Terms of Service"
                            }
                            a {
                                class: "text-sm text-primary-400 hover:text-primary-300 underline",
                                href: "https://clearmirror.ai/privacy-policy",
                                target: "_blank",
                                "Privacy Policy"
                            }
                        }

                        // TOS Modal
                        if show_tos_modal() {
                            div {
                                class: "fixed inset-0 bg-black/60 flex items-center justify-center z-50",
                                onclick: move |_| show_tos_modal.set(false),
                                div {
                                    class: "bg-section rounded-lg shadow-xl max-w-lg w-full mx-4 max-h-[80vh] flex flex-col",
                                    onclick: move |e| e.stop_propagation(),
                                    // Header
                                    div {
                                        class: "flex justify-between items-center p-4 border-b border-subtle",
                                        h2 { class: "text-lg font-bold", "Terms of Service" }
                                        button {
                                            class: "text-fg-muted hover:text-fg text-xl font-bold w-8 h-8 flex items-center justify-center rounded hover:bg-input transition-colors",
                                            onclick: move |_| show_tos_modal.set(false),
                                            "×"
                                        }
                                    }
                                    // Content
                                    div {
                                        class: "flex-1 overflow-y-auto p-4 text-sm prose prose-sm dark:prose-invert max-w-none",
                                        MarkdownRenderer {
                                            content: TOS_CONTENT.to_string(),
                                            comments: None,
                                            pending_highlight: None,
                                        }
                                    }
                                    // Footer
                                    div {
                                        class: "p-4 border-t border-subtle",
                                        button {
                                            class: "w-full py-2 px-4 bg-btn-primary hover:bg-btn-primary-hover rounded-md font-bold transition-colors",
                                            onclick: move |_| show_tos_modal.set(false),
                                            "Close"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                   } // End About
                   } // End Match
                } // End Scrollable Content

                // Footer (Sticky)
                div {
                    class: "p-4 border-t border-subtle bg-section",
                    button {
                class: if has_unsaved_changes() {
                    "mt-4 px-4 py-2 bg-btn-primary rounded-md text-fg font-semibold hover:bg-btn-primary-hover focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-opacity-50 transition-colors"
                } else {
                    "mt-4 px-4 py-2 bg-input rounded-md text-fg font-semibold cursor-not-allowed"
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
                                        let save_result = crate::secret_manager::set_generic_password_with_biometric_protection(&key_name, &key_value)
                                            .or_else(|e| {
                                                if let crate::secret_manager::KeychainError::SecurityError(-34018) = e {
                                                    crate::secret_manager::set_generic_password(&key_name, &key_value)
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
    }
}
