#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::await_holding_invalid_type)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_match)]

use dioxus::desktop::tao::dpi::PhysicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{
    muda::MenuEvent, use_window,
    use_wry_event_handler, Config, WindowBuilder,
};
#[cfg(target_os = "macos")]
use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
use dioxus::prelude::*;
use dotenvy::dotenv;
use futures_util::StreamExt;

#[cfg(target_os = "macos")]
mod biometric_auth;
mod components;
mod constants;
mod context;
mod gemini;
mod hotkey;
#[cfg(target_os = "macos")]
mod keychain_ffi;
#[cfg(all(test, target_os = "macos"))]
mod keychain_tests;
mod mcp;
mod menu;
mod permissions;
mod processing;
mod secret_types;
#[cfg(target_os = "macos")]
mod secret_manager;
#[cfg(not(target_os = "macos"))]
mod secret_manager_generic;
#[cfg(not(target_os = "macos"))]
use secret_manager_generic as secret_manager;

pub use secret_types::SecretManagerTrait;
mod services;
mod session;
mod settings;
mod skills;
mod theme;
mod tray;

use tray::{APP_QUIT, WINDOW_VISIBLE};
use tray_icon::TrayIcon;

fn main() {
    // Try to load .env file for developer convenience.
    dotenv().ok();
    tracing::debug!("Attempted to load .env file from the environment.");
    #[cfg(debug_assertions)]
    dioxus_logger::init(tracing::Level::DEBUG).expect("failed to init logger");
    #[cfg(not(debug_assertions))]
    dioxus_logger::init(tracing::Level::INFO).expect("failed to init logger");

    // Load session state to get window size
    let initial_state = session::SessionState::load().unwrap_or_default();
    let initial_width = initial_state.window_width;
    let initial_height = initial_state.window_height;

    // Load settings for menu
    let settings_manager = settings::SettingsManager::new(get_settings_path());
    let initial_settings = settings_manager.load();

    let menu = menu::build_menu(&initial_settings);
    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_menu(menu)
                .with_window(
                    {
                        #[allow(unused_mut)]
                        let mut window = WindowBuilder::new()
                            .with_title(settings::get_app_name())
                            .with_visible(true)
                            .with_resizable(true)
                            .with_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(initial_width, initial_height));

                        #[cfg(target_os = "macos")]
                        {
                            window = window
                                .with_titlebar_transparent(true);
                        }
                        #[cfg(not(target_os = "macos"))]
                        let window = window;
                        window
                    }
                )
                .with_custom_head(
                    r#"<script src="https://cdn.tailwindcss.com"></script>"#.to_string()
                    + r#"<link rel="preconnect" href="https://fonts.googleapis.com">"#
                    + r#"<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>"#
                    + r#"<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">"#
                    + r#"<style>html, body { height: 100%; margin: 0; padding: 0; font-family: 'Inter', system-ui, sans-serif; }</style>"#
                    + r#"<style>"# + include_str!("../assets/tailwind.css") + r#"</style>"#
                    + r#"<style>"# + include_str!("../assets/main.css") + r#"</style>"#
                )
        )
        .launch(app)
}

use crate::context::permissions::PermissionManager;
use crate::session::SessionState;
use crate::settings::SettingsManager;
use crate::{
    components::{
        confirm_delete_modal::ConfirmDeleteModal,
        llm::{GeminiConnector, LlmConnector},
        stream_manager::StreamManager,
        shared::{SessionIdContext, SessionToDeleteContext},
    },
    mcp::manager::McpManager,
};
use std::path::PathBuf;
use std::sync::Arc;

fn get_settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("settings.json")
}

fn get_mcp_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("mcp_servers.json")
}

fn get_ui_state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("ui_state.json")
}

#[derive(Clone, Debug)]
pub enum MenuAction {
    Quit,
    Settings,
    History,
    Mcp,
    Profile,
    Attachments,
}

use crate::components::chat_input::ChatCommand;

#[component]
fn RestartRequired() -> Element {
    rsx! {
        div {
            class: "flex flex-col items-center justify-center h-screen bg-app text-fg text-center p-8",
            h1 { class: "text-2xl font-bold mb-4", "Permissions Granted" }
            p { class: "mb-6", "Hobbes needs to be restarted for the changes to take effect." }
            p { class: "text-sm text-fg-muted", "Please quit and reopen the application." }
        }
    }
}

fn app() -> Element {
    let window = use_window();
    let mut session_state = use_context_provider(|| {
        let mut state = SessionState::load().unwrap_or_else(|e| {
            tracing::error!("Failed to load session state during startup: {}", e);
            // Create default state with save DISABLED to protect backup
            SessionState {
                save_disabled: true,
                ..Default::default()
            }
        });
        if state.sessions.is_empty() {
            tracing::info!("No sessions found, creating new default session.");
            state.create_session(None);
        }
        Signal::new(state)
    });
    let settings_manager =
        use_context_provider(|| Signal::new(SettingsManager::new(get_settings_path())));
    let ui_state_manager =
        use_context_provider(|| Signal::new(settings::UiStateManager::new(get_ui_state_path())));
    let mut ui_state = use_context_provider(|| Signal::new(ui_state_manager.read().load()));

    // Initialize SecretManager without loading (loading happens async below)
    let mut secret_manager =
        use_context_provider(|| Signal::new(secret_manager::SecretManager::new()));

    // Track whether secrets have been loaded (None = loading, Some = done)
    let mut secrets_loaded = use_signal(|| false);

    // Load settings initially (without API keys - they'll be added after keychain loads)
    let mut settings = use_context_provider(|| {
        let settings = settings_manager.read().load();
        Signal::new(settings)
    });

    // Global focus context for keyboard event coordination
    use_context_provider(|| Signal::new(components::focus_context::FocusContext::default()));

    let mut skill_registry = use_context_provider(|| Signal::new(skills::SkillRegistry::new()));
    let mut skills_loaded = use_signal(|| false);
    
    // Pattern: Grounded App Initialization
    // Masks transient desyncs during the first render tick
    let mut is_app_initialized = use_signal(|| false);
    use_effect(move || {
        is_app_initialized.set(true);
    });

    // Sync theme to DOM (class on <html> element)
    theme::use_theme_sync(settings);

    // Asynchronously load skills from ~/.hobbes/skills
    use_effect(move || {
        if !skills_loaded() {
            spawn(async move {
                let skills_dir = dirs::home_dir()
                    .map(|h| h.join(".hobbes/skills"))
                    .unwrap_or_default();

                if skills_dir.exists() {
                    let loaded = tokio::task::spawn_blocking(move || {
                        let temp_registry = skills::SkillRegistry::new();
                        match temp_registry.load_from_directory(&skills_dir) {
                            Ok(names) => Some((temp_registry, names)),
                            Err(e) => {
                                tracing::warn!("Failed to load skills: {}", e);
                                None
                            }
                        }
                    })
                    .await;

                    if let Ok(Some((loaded_registry, names))) = loaded {
                        // Merge loaded skills into the context signal
                        let loaded_skills = loaded_registry
                            .skills
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        *skill_registry
                            .write()
                            .skills
                            .write()
                            .unwrap_or_else(|p| p.into_inner()) = loaded_skills;
                        tracing::info!("Loaded {} skills: {:?}", names.len(), names);
                    }
                } else {
                    tracing::debug!("Skills directory not found: {:?}", skills_dir);
                }
                skills_loaded.set(true);
            });
        }
    });

    // Asynchronously load secrets from keychain using biometric authentication
    // This prompts once for Touch ID/password, then uses that context for all secrets
    #[cfg(target_os = "macos")]
    use_effect(move || {
        if !secrets_loaded() {
            // Capture profile names before entering the blocking task
            let profile_names: Vec<String> = settings
                .read()
                .composio_profiles
                .iter()
                .map(|p| p.name.clone())
                .collect();

            spawn(async move {
                // Use spawn_blocking to run keychain operations off the main thread
                let loaded_secrets = tokio::task::spawn_blocking(move || {
                    let mut sm = secret_manager::SecretManager::new();

                // Check if Provisioning Profile exists (required for Biometric Keychain Access Groups)
                let biometrics_enabled = crate::settings::is_sandboxed();

                    // Step 1: Attempt biometric authentication (single prompt)
                    let auth_context = if biometrics_enabled {
                        match biometric_auth::AuthContext::authenticate(
                            "Hobbes needs access to your saved credentials"
                        ) {
                            biometric_auth::AuthResult::Success(ctx) => {
                                tracing::info!("Biometric authentication successful, loading secrets with context");
                                Some(ctx)
                            }
                            biometric_auth::AuthResult::Cancelled => {
                                tracing::info!("User cancelled biometric auth, falling back to regular keychain access");
                                None
                            }
                            biometric_auth::AuthResult::NotAvailable(reason) => {
                                tracing::info!("Biometric auth not available ({}), using regular keychain access", reason);
                                None
                            }
                            biometric_auth::AuthResult::Failed(error) => {
                                tracing::warn!("Biometric auth failed: {}, using regular keychain access", error);
                                None
                            }
                        }
                    } else {
                        tracing::info!("Provisioning profile missing (Pro/Direct Mode). Skipping biometric auth to prevent keychain hang.");
                        None
                    };

                    // Step 2: Load secrets with or without the authenticated context
                    if let Some(ref ctx) = auth_context {
                        // Use the authenticated context for all keychain operations
                        sm.load_all_with_context(ctx);

                        // Also load Composio API keys for each known profile
                        for profile_name in &profile_names {
                            sm.load_composio_key_with_context(profile_name, Some(ctx));
                        }
                    } else {
                        // Fall back to regular keychain access (may prompt multiple times)
                        sm.load_all_from_keychain();

                        for profile_name in &profile_names {
                            sm.load_composio_key(profile_name);
                        }
                    }

                    sm
                }).await;

                if let Ok(sm) = loaded_secrets {
                    // Update secret manager with loaded secrets
                    secret_manager.set(sm);

                    // Now update settings with loaded API keys
                    let sm_read = secret_manager.read();
                    let mut current_settings = settings.write();

                    if let Some(api_key) = sm_read.get("api_key") {
                        current_settings.gemini_config.api_key = Some(api_key.clone());
                    }
                    if let Some(smithery_api_key) = sm_read.get("smithery_api_key") {
                        current_settings.smithery_api_key = Some(smithery_api_key.clone());
                    }
                    // Load Composio API key for active profile (or legacy key)
                    if let Some(composio_api_key) = sm_read.get("composio_api_key") {
                        current_settings.composio_api_key = Some(composio_api_key.clone());
                    }

                    // Load Composio API keys for each profile from the cache
                    for profile in &mut current_settings.composio_profiles {
                        if let Some(api_key) = sm_read.get_composio_key(&profile.name) {
                            profile.api_key = Some(api_key.clone());
                            tracing::debug!(
                                "Loaded API key for Composio profile: {}",
                                profile.name
                            );
                        }
                    }

                    tracing::debug!("Secrets loaded from keychain successfully");
                } else {
                    tracing::error!("Failed to load secrets from keychain");
                }
                secrets_loaded.set(true);
            });
        }
    });

    // Simple secret loading for non-macOS platforms (no biometrics)
    #[cfg(not(target_os = "macos"))]
    use_effect(move || {
        if !secrets_loaded() {
            let profile_names: Vec<String> = settings
                .read()
                .composio_profiles
                .iter()
                .map(|p| p.name.clone())
                .collect();

            spawn(async move {
                let loaded_secrets = tokio::task::spawn_blocking(move || {
                    let mut sm = secret_manager::SecretManager::new();
                    sm.load_all_from_keychain();
                    for profile_name in &profile_names {
                        sm.load_composio_key(profile_name);
                    }
                    sm
                }).await;

                if let Ok(sm) = loaded_secrets {
                    secret_manager.set(sm);
                    let sm_read = secret_manager.read();
                    let mut current_settings = settings.write();

                    if let Some(api_key) = sm_read.get("api_key") {
                        current_settings.gemini_config.api_key = Some(api_key.clone());
                    }
                    if let Some(smithery_api_key) = sm_read.get("smithery_api_key") {
                        current_settings.smithery_api_key = Some(smithery_api_key.clone());
                    }
                    if let Some(composio_api_key) = sm_read.get("composio_api_key") {
                        current_settings.composio_api_key = Some(composio_api_key.clone());
                    }

                    for profile in &mut current_settings.composio_profiles {
                        if let Some(api_key) = sm_read.get_composio_key(&profile.name) {
                            profile.api_key = Some(api_key.clone());
                        }
                    }
                    tracing::debug!("Secrets loaded from generic keychain successfully");
                }
                secrets_loaded.set(true);
            });
        }
    });

    // Show loading screen while waiting for keychain secrets
    if !secrets_loaded() {
        return rsx! {
            div {
                class: "flex flex-col items-center justify-center h-screen bg-app text-fg text-center p-8",
                div {
                    class: "animate-spin rounded-full h-12 w-12 border-b-2 border-fg mb-4"
                }
                h1 { class: "text-xl font-semibold mb-2", "Loading..." }
                p { class: "text-sm text-fg-muted", "Authenticate to access your saved credentials..." }
            }
        };
    }

    let mut llm_connector = use_context_provider(|| {
        let settings = settings.read();
        let connector: Arc<dyn LlmConnector> = match settings.active_llm {
            crate::settings::LlmProvider::Gemini => {
                Arc::new(GeminiConnector::new(settings.gemini_config.clone()))
            }
        };
        Signal::new(connector)
    });

    // Reactively update llm_connector when gemini_config changes
    use_effect(move || {
        // Use read() to subscribe to settings changes so this effect re-runs
        // when the API key is loaded from biometrics
        let (active_llm, gemini_config) = {
            let current_settings = settings.read();
            (
                current_settings.active_llm.clone(),
                current_settings.gemini_config.clone(),
            )
        };
        // Now the read borrow is dropped, safe to set the connector
        let new_connector: Arc<dyn LlmConnector> = match active_llm {
            crate::settings::LlmProvider::Gemini => Arc::new(GeminiConnector::new(gemini_config)),
        };
        llm_connector.set(new_connector);
    });

    use_context_provider(|| {
        Signal::new(processing::conversation_processor::ConversationProcessor::new(llm_connector))
    });

    // Asynchronously load the session state
    // Async session loading removed to prevent race condition. State is loaded synchronously above.

    let permission_status_signal =
        use_context_provider(|| Signal::new(permissions::PermissionStatus::Denied));

    let _ = use_resource(move || async move {
        let mut status = permission_status_signal;
        #[cfg(target_os = "macos")]
        {
            // Run the blocking check in a separate thread
            let result = tokio::task::spawn_blocking(move || {
                permissions::check_and_prompt_for_accessibility()
            })
            .await
            .unwrap_or(permissions::PermissionStatus::Denied);
            status.set(result);
        }
        #[cfg(not(target_os = "macos"))]
        {
            status.set(permissions::PermissionStatus::Granted);
        }
    });

    let needs_onboarding = use_memo(move || {
        let settings = settings.read();
        
        // Check TOS acceptance (compare against current version)
        let tos_accepted = settings
            .tos_accepted_version
            .as_ref()
            .map(|v| v == crate::settings::CURRENT_TOS_VERSION)
            .unwrap_or(false);
        
        // Check API key presence
        let key_present =
            settings.gemini_config.api_key.is_some() || std::env::var("GEMINI_API_KEY").is_ok();
        
        // Need onboarding if either TOS not accepted or API key missing
        !tos_accepted || !key_present
    });

    let permission_manager = use_context_provider(|| Signal::new(PermissionManager::new(settings)));
    let mcp_manager = use_context_provider(|| {
        Signal::new(McpManager::new(
            get_mcp_config_path(),
            permission_manager,
            secret_manager,
        ))
    });
    let mcp_context = use_context_provider(|| {
        Signal::new(mcp::manager::McpContext {
            servers: Vec::new(),
        })
    });

    // Create a global signal for the active profile name.
    // This is the ONLY source of truth for what profile is active.
    let mut active_composio_profile_name: Signal<Option<String>> = use_context_provider(|| {
        let session_state_read = session_state.peek();
        let initial = session_state_read.get_active_session()
            .and_then(|s| s.composio_profile.clone())
            .or_else(|| settings.peek().active_composio_profile.clone());
        Signal::new(initial)
    });

    // Synchronize active_composio_profile_name with session changes
    use_effect(move || {
        let session_id = session_state.read().active_session_id.clone();
        let session_state_read = session_state.read();
        let profile = session_state_read.sessions.get(&session_id)
            .and_then(|s| s.composio_profile.clone())
            .or_else(|| settings.read().active_composio_profile.clone());
        
        if active_composio_profile_name.peek().as_ref() != profile.as_ref() {
            tracing::info!("Syncing active profile signal to: {:?}", profile);
            active_composio_profile_name.set(profile);
        }
    });

    // Pattern 155: Sync signal BACK to settings.active_composio_profile
    // This closes the cascade loop so get_active_profile() returns the session's profile
    // and triggers MCP client reinitialization on tab switch.
    use_effect(move || {
        let profile_name = active_composio_profile_name.read().clone();
        let current_setting = settings.read().active_composio_profile.clone();
        
        if profile_name != current_setting {
            tracing::info!("Syncing settings.active_composio_profile to signal: {:?}", profile_name);
            settings.write().active_composio_profile = profile_name;
        }
    });

    let _ = use_resource(move || async move {
        // Wait until secrets are loaded before launching MCP servers
        // This ensures profile API keys are populated
        if !secrets_loaded() {
            tracing::debug!("Waiting for secrets to load before launching MCP servers...");
            return;
        }

        // Use peek() to avoid accidental reactive dependencies on signals that change frequently (like ui_state)
        let manager = mcp_manager.peek().clone();
        let mcp_context_signal = mcp_context;
        let settings_snapshot = settings.peek().clone();

        // Restore persisted unloaded servers state before launching
        let persisted_unloaded = ui_state.peek().unloaded_mcp_servers.clone();
        manager
            .set_initial_unloaded_servers(persisted_unloaded)
            .await;

        manager
            .launch_servers(mcp_context_signal, settings_snapshot)
            .await;
    });

    // Reinitialize Composio client when active profile changes
    // This use_effect subscribes to changes in active_composio_profile
    {
        // Track key profile properties to detect changes (API key, base URL, etc.)
        let mut prev_profile_signature: Signal<Option<String>> = use_signal(|| None);

        use_effect(move || {
            // Create a signature of the active profile properties we care about
            let current_signature = settings.read().get_active_profile().map(|p| {
                format!(
                    "{}:{}:{}:{}:{}",
                    p.name,
                    p.api_key.as_deref().unwrap_or(""),
                    p.base_url.as_deref().unwrap_or(""),
                    p.entity_id.as_deref().unwrap_or(""),
                    p.user_id.as_deref().unwrap_or("")
                )
            });

            let previous = prev_profile_signature.peek().clone();
            let active_profile_name = active_composio_profile_name.read().clone();

            // Only reinitialize if the profile signature actually changed
            if current_signature != previous {
                tracing::info!("Active Composio profile properties changed, reinitializing client: {:?}", active_profile_name);

                // Invalidate caches - profile changed
                mcp_manager.read().invalidate_status_cache();

                let mcp_context_signal = mcp_context;
                let settings_clone = settings.read().clone();

                spawn(async move {
                    mcp_manager
                        .read()
                        .reinitialize_composio_client(mcp_context_signal, settings_clone, active_profile_name)
                        .await;
                });
            }

            // Update the previous signature
            prev_profile_signature.set(current_signature);
        });
    }


    let mut show_session_manager = use_signal(|| false);
    let mut show_settings_panel = use_signal(|| false);
    let mut show_mcp_manager = use_signal(|| false);
    let mut settings_panel_width = use_signal(|| ui_state.read().settings_panel_width);
    let mut is_dragging = use_signal(|| false);
    let mut drag_start_info = use_signal(|| (0.0, 0.0)); // (start_x, start_width)
    let mut final_width_on_drag_end = use_signal(|| 0.0);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    let mut tray_icon = use_signal::<Option<TrayIcon>>(|| None);
    let mut show_confirm_modal = use_context_provider(|| Signal::new(false));
    let session_to_delete = use_context_provider(|| SessionToDeleteContext(Signal::new(String::new())));

    let mut chat_command = use_context_provider(|| Signal::new(None::<ChatCommand>));

    // Tab state - initialized from UiState or with current active session
    let mut open_tabs = use_signal(|| {
        let ui = ui_state.read();
        let state = session_state.read();
        
        // Filter tabs to ensure they only contain existing session IDs
        let tabs: Vec<String> = ui.open_tabs.iter()
            .filter(|id| state.sessions.contains_key(*id))
            .cloned()
            .collect();

        if tabs.is_empty() {
            // Bootstrap: open the currently active session as the first tab
            vec![state.active_session_id.clone()]
        } else {
            tabs
        }
    });
    let mut active_tab_index = use_signal(|| {
        let ui = ui_state.read();
        let tabs_len = open_tabs.peek().len();
        // Ensure index is within bounds
        ui.active_tab_index.min(tabs_len.saturating_sub(1))
    });

    // Single source of truth for the currently displayed session - provided via context
    let mut current_session_id = use_signal(|| {
        let tabs = open_tabs.read();
        let idx = *active_tab_index.peek();
        
        // Safely get the tab at idx, falling back to first tab, then to active_session_id
        if let Some(id) = tabs.get(idx) {
            id.clone()
        } else if let Some(id) = tabs.first() {
            id.clone()
        } else {
            session_state.read().active_session_id.clone()
        }
    });
    use_context_provider(|| SessionIdContext(current_session_id));


    // Persist tab state to UiState (Pattern 12 & 13)
    use_effect(move || {
        let mut ui = ui_state.write();
        ui.open_tabs = open_tabs.read().clone();
        ui.active_tab_index = *active_tab_index.read();
        
        // Snapshot targets for Pattern 12 persistence
        let ui_snapshot = ui.clone();
        let manager = ui_state_manager.peek().clone();
        
        manager.save_async(ui_snapshot);
    });

    // Tab switching - use signal copies directly in closures
    let mut switch_tab_fn = move |idx: usize| {
        let tabs = open_tabs.read();
        if idx < tabs.len() {
            let session_id = tabs[idx].clone();
            active_tab_index.set(idx);
            current_session_id.set(session_id.clone());
            session_state.write().active_session_id = session_id;
        }
    };

    let close_tab_fn = move |idx: usize| {
        let mut tabs = open_tabs.read().clone();
        if idx < tabs.len() {
            tabs.remove(idx);
            let mut new_idx = *active_tab_index.read();
            if tabs.is_empty() {
                let new_id = session_state.write().create_session(settings.peek().active_composio_profile.clone());
                tabs.push(new_id);
                new_idx = 0;
            } else if new_idx >= tabs.len() {
                new_idx = tabs.len().saturating_sub(1);
            }
            let new_session_id = tabs[new_idx].clone();
            
            // Atomic update
            open_tabs.set(tabs);
            active_tab_index.set(new_idx);
            current_session_id.set(new_session_id.clone());
            session_state.write().active_session_id = new_session_id;
        }
    };

    let mut new_tab_fn = move || {
        let new_id = session_state.write().create_session(settings.peek().active_composio_profile.clone());
        let mut tabs = open_tabs.read().clone();
        tabs.push(new_id.clone());
        open_tabs.set(tabs.clone());
        active_tab_index.set(tabs.len() - 1);
        session_state.write().active_session_id = new_id.clone();
        current_session_id.set(new_id);
    };


    // Call the summarization scheduler hook BEFORE the hotkey manager
    processing::summarization_scheduler::use_summarization_scheduler();

    // Unconditionally call the hotkey manager hook, passing in the permission status signal.
    // The hook itself will handle the conditional logic internally.
    hotkey::use_hotkey_manager(permission_status_signal);

    // This handler continuously updates the last known size during a resize.
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event, .. } = event {
            if let WindowEvent::Resized(new_size) = event {
                last_known_size.set(*new_size);
            }
        }
    });

    let menu_handler = use_coroutine(move |mut rx: UnboundedReceiver<MenuAction>| async move {
        while let Some(action) = rx.next().await {
            tracing::info!("MenuHandler received action: {:?}", action);
            match action {
                MenuAction::Quit => {
                    let mut app_quit = APP_QUIT.write();
                    *app_quit = true;
                }
                MenuAction::Settings => {
                    tracing::info!("MenuAction::Settings triggered");
                    chat_command.set(Some(ChatCommand::ToggleSettings));
                }
                MenuAction::History => {
                    tracing::info!("MenuAction::History triggered");
                    chat_command.set(Some(ChatCommand::ToggleHistory));
                }
                MenuAction::Mcp => {
                    tracing::info!("MenuAction::Mcp triggered");
                    chat_command.set(Some(ChatCommand::ToggleMcp));
                }
                MenuAction::Profile => {
                    tracing::info!("MenuAction::Profile triggered");
                    chat_command.set(Some(ChatCommand::ToggleProfile));
                }
                MenuAction::Attachments => {
                    tracing::info!("MenuAction::Attachments triggered");
                    chat_command.set(Some(ChatCommand::OpenAttachments));
                }
            }
        }
    });

    // One-time setup for the menu event loop
    // We use use_hook to ensure the thread is only spawned once.
    use_hook(move || {
        let menu_channel = MenuEvent::receiver();
        let tx = menu_handler.tx();
        std::thread::spawn(move || {
            tracing::info!("Menu event loop started");
            loop {
                if let Ok(event) = menu_channel.recv() {
                    tracing::info!("Native MenuEvent received: {:?}", event.id);
                    let action = match event.id.0.as_str() {
                        "quit" => Some(MenuAction::Quit),
                        "settings" => Some(MenuAction::Settings),
                        "view_history" => Some(MenuAction::History),
                        "view_mcp" => Some(MenuAction::Mcp),
                        "view_profile" => Some(MenuAction::Profile),
                        "view_attachments" => Some(MenuAction::Attachments),
                        _ => None,
                    };

                    if let Some(action) = action {
                        tracing::info!("Dispatching menu action: {:?}", action);
                        let _ = tx.unbounded_send(action);
                    }
                }
            }
        });
    });

    // Effect to manage the tray icon's visibility based on settings
    use_effect(move || {
        let show = settings.read().show_tray_icon;
        if show {
            if tray_icon.peek().is_none() {
                tray_icon.set(Some(tray::init_tray()));
                tracing::debug!("Tray icon has been created.");
            }
        } else {
            if tray_icon.peek().is_some() {
                tray_icon.set(None);
                tracing::debug!("Tray icon has been removed.");
            }
        }
    });

    // This effect handles window visibility and quitting the app
    let window_clone = window.clone();
    use_effect(move || {
        let visible = *WINDOW_VISIBLE.read();
        let app_quit = *APP_QUIT.read();

        if app_quit {
            window_clone.close();
            return;
        }

        window_clone.set_visible(visible);
        if visible {
            window_clone.set_focus();
            tracing::debug!("Window is visible, centering on current monitor.");
            let main_window = &window_clone.window;
            if let Some(monitor) = main_window.current_monitor() {
                let monitor_size = monitor.size();
                let window_size = main_window.outer_size();
                let monitor_pos = monitor.position();

                let x = monitor_pos.x + (monitor_size.width as i32 - window_size.width as i32) / 2;
                let y =
                    monitor_pos.y + (monitor_size.height as i32 - window_size.height as i32) / 2;

                main_window
                    .set_outer_position(dioxus::desktop::tao::dpi::PhysicalPosition::new(x, y));
            }
        } else {
            tracing::debug!("Window is hidden.");
        }
    });

    // This effect saves the settings panel width when the user stops dragging
    use_effect(move || {
        let new_width = final_width_on_drag_end();
        if new_width > 0.0 {
            spawn(async move {
                settings_panel_width.set(new_width);
                let mut current_ui_state = ui_state.write();
                if current_ui_state.settings_panel_width != new_width {
                    current_ui_state.settings_panel_width = new_width;
                    let uism = ui_state_manager.read();
                    uism.save_async(current_ui_state.clone());
                }
                final_width_on_drag_end.set(0.0); // Reset after saving
            });
        }
    });

    // Debounced window resize saver
    // This effect runs whenever last_known_size changes (due to resize events)
    let window_resize_handler = window.clone();
    use_effect(move || {
        let physical_size = last_known_size.read();
        if physical_size.width > 0 && physical_size.height > 0 {
            let mut session_state = session_state;
            let show_session_manager = show_session_manager;
            let show_settings_panel = show_settings_panel;
            let show_mcp_manager = show_mcp_manager;

            // Capture the current values we need for calculation
            let current_physical_size = *physical_size;
            let window = window_resize_handler.clone();

            spawn(async move {
                // Wait for 1 second of inactivity before saving
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // The last one to run will win and set the correct final size.

                let scale_factor = window.scale_factor();
                let logical_size = current_physical_size.to_logical::<f64>(scale_factor);

                let sidebar_visible = *show_session_manager.read()
                    || *show_settings_panel.read()
                    || *show_mcp_manager.read();
                let sidebar_width = if sidebar_visible {
                    settings_panel_width()
                } else {
                    0.0
                };
                let content_width = logical_size.width - sidebar_width;

                session_state
                    .write()
                    .update_window_size(content_width, logical_size.height);
                // Save asynchronously on a background thread to avoid blocking the UI
                SessionState::save_async(session_state.read().clone());
            });
        }
    });

    // Handle global SwitchToSettingsTab command
    use_effect(move || {
        if let Some(ChatCommand::SwitchToSettingsTab(tab, slug)) = chat_command.read().clone() {

            tracing::info!("App handling SwitchToSettingsTab: {:?}, slug: {:?}", tab, slug);
            // 1. Show Settings Panel
            show_settings_panel.set(true);
            show_session_manager.set(false);
            show_mcp_manager.set(false);

            // 2. Update UiState
            let mut state = ui_state.write();
            state.active_settings_tab = tab;
            state.selected_byoa_slug = slug;

            // 3. Clear command
            spawn(async move {
                chat_command.set(None);
            });
        }

        // Global Tab Switching Command
        if let Some(ChatCommand::SwitchTab(idx)) = chat_command.read().clone() {
            switch_tab_fn(idx);
            spawn(async move {
                chat_command.set(None);
            });
        }

        // Profile Switching Command (session-local via Cmd+Option+1..9)
        if let Some(ChatCommand::SwitchProfile(idx)) = chat_command.read().clone() {
            // Get profile name from settings by index
            let profiles = settings.read().composio_profiles.clone();
            if idx < profiles.len() {
                let new_profile_name = profiles[idx].name.clone();
                // Update current session's profile
                let mut did_change = false;
                if let Some(session) = session_state.write().sessions.get_mut(&*current_session_id.read()) {
                    if session.composio_profile.as_deref() != Some(&new_profile_name) {
                        tracing::info!("Hotkey: Switching session profile to index {}: {}", idx, new_profile_name);
                        
                        // Update GLOBAL signal first
                        active_composio_profile_name.set(Some(new_profile_name.clone()));
                        
                        session.composio_profile = Some(new_profile_name.clone());
                        // Pattern 152.1: Invalidate cached tool list on profile switch 
                        // to prevent LLM from using stale profile context.
                        session.active_context.mcp_tools = None;
                        did_change = true;
                    }
                }
                
                // Per KI Pattern 134: Operational paths MUST trigger MCP reinit
                if did_change {
                    // Persist the session state change (Pattern 12)
                    SessionState::save_async(session_state.read().clone());
                    
                    // Invalidate caches immediately (Pattern 150)
                    mcp_manager.read().invalidate_status_cache();
                    
                    // Trigger async client reinitialization (Pattern 151 - Atomic Lifecycle)
                    let mcp_context_signal = mcp_context;
                    let settings_clone = settings.read().clone();
                    spawn(async move {
                        mcp_manager
                            .read()
                            .reinitialize_composio_client(mcp_context_signal, settings_clone, Some(new_profile_name))
                            .await;
                    });
                }
            }
            spawn(async move {
                chat_command.set(None);
            });
        }

        // Session Switching Command (from History panel) - syncs with tab state
        if let Some(ChatCommand::SwitchToSession(session_id)) = chat_command.read().clone() {
            // Find if session is already in tabs
            let tabs = open_tabs.read().clone();
            if let Some(idx) = tabs.iter().position(|id| id == &session_id) {
                // Session is already a tab - switch to it atomically
                active_tab_index.set(idx);
                current_session_id.set(session_id.clone());
                session_state.write().active_session_id = session_id;
            } else {
                // Add session as a new tab and switch to it atomically
                let mut new_tabs = tabs;
                new_tabs.push(session_id.clone());
                let new_idx = new_tabs.len() - 1;
                open_tabs.set(new_tabs);
                active_tab_index.set(new_idx);
                current_session_id.set(session_id.clone());
                session_state.write().active_session_id = session_id;
            }
            spawn(async move {
                chat_command.set(None);
            });
        }

        // Handle NewChat / NewChatWithMemory
        if let Some(cmd) = chat_command.read().clone() {
            if matches!(cmd, ChatCommand::NewChat | ChatCommand::NewChatWithMemory) {
                new_tab_fn();
                spawn(async move {
                    chat_command.set(None);
                });
            }
        }
    });

    rsx! {
        if matches!(*permission_status_signal.read(), permissions::PermissionStatus::JustGranted) {
            RestartRequired {}
        } else if *needs_onboarding.read() {
            div {
                class: "flex items-center justify-center h-screen bg-app text-fg",
                components::onboarding::Onboarding {
                    needs_onboarding,
                }
            }
        } else {
            // SummarizationScheduler component removed; hook called above.
            if *is_app_initialized.read() {
                StreamManager {
                ConfirmDeleteModal {
                    is_visible: show_confirm_modal,
                    title: "Delete Session".to_string(),
                    message: "Are you sure you want to delete this session? This action cannot be undone.".to_string(),
                    on_cancel: move |_| show_confirm_modal.set(false),
                    on_confirm: move |remember| {
                        let id_to_delete_str = session_to_delete.0.read().clone();
                        if !id_to_delete_str.is_empty() {
                            // 1. Domain delete
                            session_state.write().delete_session(&id_to_delete_str);
                            
                            // 2. Tab prune
                            let mut tabs = open_tabs.read().clone();
                            if let Some(tab_idx) = tabs.iter().position(|id| id == &id_to_delete_str) {
                                tabs.remove(tab_idx);
                                
                                // 3. Coordinate indices
                                let mut new_idx = *active_tab_index.read();
                                if tabs.is_empty() {
                                    // Bootstrap a new session if we deleted the last tab
                                    let new_id = session_state.write().create_session(settings.peek().active_composio_profile.clone());
                                    tabs.push(new_id);
                                    new_idx = 0;
                                } else if new_idx >= tabs.len() {
                                    new_idx = tabs.len().saturating_sub(1);
                                }
                                
                                // 4. Atomic state sync
                                let new_active_id = tabs[new_idx].clone();
                                open_tabs.set(tabs);
                                active_tab_index.set(new_idx);
                                current_session_id.set(new_active_id.clone());
                                session_state.write().active_session_id = new_active_id;
                            }
                        }
                        if remember {
                            let mut current_settings = settings.write();
                            current_settings.confirm_on_delete = false;
                            let sm = settings_manager.read();
                            sm.save_async(current_settings.clone());
                        }
                        show_confirm_modal.set(false);
                    },
                }
                    div {
                        class: "flex flex-col h-screen bg-app text-fg", // Removed forced 'dark' class to allow global theme inheritance
                        // The draggable header has been removed as per user request.
                        // Main content area
                        div {
                            class: "flex flex-row flex-1 min-h-0", // This will contain the sidebars and chat
                            // The onkeydown handler has been removed to allow native hotkeys (copy, paste, etc.) to function correctly.
                            // The global hotkey for toggling visibility is no longer required.
                            // When the user releases the mouse, save the last known size.


                        // Session Manager Sidebar
                        if *show_session_manager.read() {
                            div {
                                class: "flex flex-row h-full",
                                // Session Manager Panel
                                div {
                                    id: "session-manager-panel",
                                    style: "width: {settings_panel_width}px;",
                                    class: "bg-section text-fg h-full",
                                    components::session_manager::SessionManager {}
                                }
                                // Draggable Divider
                                div {
                                    class: "w-2 cursor-col-resize bg-primary-700 hover:bg-primary-500 transition-colors",
                                    onmousedown: move |event| {
                                        drag_start_info.set((event.data.screen_coordinates().x, settings_panel_width()));
                                        is_dragging.set(true);
                                    },
                                }
                            }
                        }

                        // Settings Panel Sidebar
                        if *show_settings_panel.read() {
                            div {
                                class: "flex flex-row h-full",
                                // Settings Panel
                                div {
                                    id: "settings-panel",
                                    style: "width: {settings_panel_width}px;",
                                    class: "bg-section text-fg h-full",
                                    // This is the correct location for the settings panel component
                                    components::settings_panel::SettingsPanel {}
                                }
                                // Draggable Divider
                                div {
                                    class: "w-2 cursor-col-resize bg-primary-700 hover:bg-primary-500 transition-colors",
                                    onmousedown: move |event| {
                                        drag_start_info.set((event.data.screen_coordinates().x, settings_panel_width()));
                                        is_dragging.set(true);
                                    },
                                }
                            }
                        }

                        // MCP Manager Sidebar
                        if *show_mcp_manager.read() {
                            div {
                                class: "flex flex-row h-full",
                                // MCP Manager Panel
                                div {
                                    id: "mcp-manager-panel",
                                    style: "width: {settings_panel_width}px;",
                                    class: "bg-section text-fg h-full",
                                    components::mcp_marketplace::McpMarketplace {}
                                }
                                // Draggable Divider
                                div {
                                    class: "w-2 cursor-col-resize bg-primary-700 hover:bg-primary-500 transition-colors",
                                    onmousedown: move |event| {
                                        drag_start_info.set((event.data.screen_coordinates().x, settings_panel_width()));
                                        is_dragging.set(true);
                                    },
                                }
                            }
                        }

                        // Mouse move handler for resizing
                        if *is_dragging.read() {
                            div {
                                class: "fixed inset-0 z-50", // Covers the whole screen to capture mouse events
                                onmousemove: move |event| {
                                    if *is_dragging.read() {
                                        let (start_x, start_width) = drag_start_info();
                                        let delta_x = event.data.screen_coordinates().x - start_x;
                                        let new_width = start_width + delta_x;
                                        if new_width > 200.0 && new_width < 800.0 {
                                            let panel_id = if *show_settings_panel.read() {
                                                "settings-panel"
                                            } else if *show_mcp_manager.read() {
                                                "mcp-manager-panel"
                                            } else {
                                                "session-manager-panel"
                                            };
                                            let js = format!("document.getElementById('{}').style.width = '{}px';", panel_id, new_width);
                                            let _ = document::eval(&js);
                                            final_width_on_drag_end.set(new_width);
                                        }
                                    }
                                },
                                onmouseup: move |_| {
                                    is_dragging.set(false);
                                },
                                onmouseleave: move |_| {
                                    // If mouse leaves the overlay, stop dragging
                                    if *is_dragging.read() {
                                        is_dragging.set(false);
                                    }
                                }
                            }
                        }

                        // Main Chat Window
                        div {
                            class: "flex-1 flex flex-col min-h-0 border-t border-primary-700/50",
                            components::tab_bar::TabBar {
                                open_tabs: open_tabs.read().clone(),
                                active_tab_index: *active_tab_index.read(),
                                on_select_tab: switch_tab_fn,
                                on_close_tab: close_tab_fn,
                                on_new_tab: move |_| new_tab_fn(),
                            }

                            components::chat::ChatWindow {
                                on_content_resize: move |_| {},
                                on_interaction: move |_| {},
                                on_toggle_sessions: move |_| {
                                    let new_show_state = !*show_session_manager.peek();
                                    show_session_manager.set(new_show_state);
                                    if new_show_state {
                                        show_settings_panel.set(false);
                                        show_mcp_manager.set(false);
                                    }
                                },
                                on_toggle_settings: move |_| {
                                    let new_show_state = !*show_settings_panel.peek();
                                    show_settings_panel.set(new_show_state);
                                    if new_show_state {
                                        show_session_manager.set(false);
                                        show_mcp_manager.set(false);
                                    }
                                },
                                on_toggle_mcp_manager: move |_| {
                                    let new_show_state = !*show_mcp_manager.peek();
                                    show_mcp_manager.set(new_show_state);
                                    if new_show_state {
                                        show_session_manager.set(false);
                                        show_settings_panel.set(false);
                                    }
                                },
                            }
                        }
                    }
                }
            }
        } else {
            LoadingScreen {}
        }
    }
}
}

#[component]
fn LoadingScreen() -> Element {
    rsx! {
        div {
            class: "flex flex-col items-center justify-center h-screen bg-app text-fg",
            // Branded Loading State
            div {
                class: "flex flex-col items-center gap-6 animate-in fade-in duration-700",
                // Placeholder for Hobbes Logo/Icon
                div {
                    class: "w-20 h-20 rounded-2xl bg-primary-600/20 flex items-center justify-center border border-primary-500/30",
                    svg {
                        class: "w-12 h-12 text-primary-400 animate-pulse",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        xmlns: "http://www.w3.org/2000/svg",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "1.5",
                            d: "M13 10V3L4 14h7v7l9-11h-7z"
                        }
                    }
                }
                div {
                    class: "space-y-2 text-center",
                    h2 { class: "text-xl font-medium tracking-tight", "Hobbes Pro" }
                    p { class: "text-sm text-fg-muted", "Initializing workspace..." }
                }
            }
        }
    }
}
