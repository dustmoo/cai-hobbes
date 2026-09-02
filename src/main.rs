#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Dioxus Signal types are held across .await — they are not real locks, just marker types.
#![allow(clippy::await_holding_invalid_type)]
// Preserve readability of nested conditionals rather than merging into long compound predicates.
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_match)]

use dioxus::desktop::tao::dpi::PhysicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
#[cfg(target_os = "macos")]
use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
use dioxus::desktop::{muda::MenuEvent, use_window, use_wry_event_handler, Config, WindowBuilder};
use dioxus::prelude::*;
use dotenvy::dotenv;
use futures_util::StreamExt;

mod async_persist;
#[cfg(target_os = "macos")]
mod biometric_auth;
mod components;
mod constants;
mod context;
mod entitlement;
mod fleet;
mod formatters;
mod gemini;
mod hotkey;
#[cfg(target_os = "macos")]
mod keychain_ffi;
#[cfg(all(test, target_os = "macos"))]
mod keychain_tests;
mod llm;
mod mcp;
mod menu;
mod permissions;
mod processing;
#[cfg(target_os = "macos")]
mod secret_manager;
#[cfg(not(target_os = "macos"))]
mod secret_manager_generic;
mod secret_types;
mod security;
#[cfg(not(target_os = "macos"))]
use secret_manager_generic as secret_manager;

pub use secret_types::SecretManagerTrait;
mod services;
mod session;
mod session_events;
mod session_store;
mod str_utils;
mod timers;
mod todo;
mod usage_log;
mod settings;
mod skills;
mod theme;
mod tray;
mod focus_tray;

use tray::{APP_QUIT, WINDOW_VISIBLE};
use tray_icon::TrayIcon;

/// Transient reminder text shown as a top-of-window toast when a timer fires.
/// Set by the timer scheduler; auto-dismisses, and can be dismissed manually.
static TIMER_TOAST: GlobalSignal<Option<String>> = Signal::global(|| None);

/// Show a timer toast and auto-clear it after a few seconds (unless a newer
/// toast replaced it in the meantime). Avoids the reminder lingering forever.
fn flash_timer_toast(msg: String) {
    *TIMER_TOAST.write() = Some(msg.clone());
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        if TIMER_TOAST.peek().as_deref() == Some(msg.as_str()) {
            *TIMER_TOAST.write() = None;
        }
    });
}

/// Debug builds (cargo run): DEBUG+ to log file, INFO+ to stderr.
/// File goes to ~/Library/Application Support/com.hobbes.app/hobbes.log (daily rotation).
#[cfg(debug_assertions)]
fn init_logger() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let log_dir = dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app");
    let file_appender = tracing_appender::rolling::daily(&log_dir, "hobbes.log");
    let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);
    // SAFETY: Leak the guard to keep the non-blocking writer alive for the process lifetime.
    // Box::leak is preferred over mem::forget — it communicates "intentional static lifetime"
    // rather than "we forgot to drop this" and avoids the clippy mem_forget lint.
    Box::leak(Box::new(file_guard));

    let (non_blocking_stderr, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());
    Box::leak(Box::new(stderr_guard));

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hobbes=debug"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_filter(env_filter);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_stderr)
        .with_ansi(true)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();
}

/// Release builds: INFO+ to stderr only, no log file created.
#[cfg(not(debug_assertions))]
fn init_logger() {
    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stderr());
    // SAFETY: Leak the guard to keep the non-blocking writer alive for the process lifetime.
    Box::leak(Box::new(guard));
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(non_blocking)
        .with_ansi(true)
        .init();
}

fn main() {
    // Try to load .env file for developer convenience.
    dotenv().ok();
    tracing::debug!("Attempted to load .env file from the environment.");
    // Logger setup — cargo run (debug) vs built app (release):
    // Debug: DEBUG+ to log file, INFO+ to stderr (avoids iTerm beach-ball)
    // Release: INFO+ to stderr only, no file created
    init_logger();

    // Initialize the SQLite session store. On first launch after the JSON→
    // SQLite migration this also imports sessions.json + sessions-archive.jsonl
    // (one-time; may take a while for a large archive).
    if let Err(e) = session_store::init() {
        tracing::error!("Failed to initialize session store: {e}");
    }

    // Read persisted window size (meta only — no session hydration)
    let (initial_width, initial_height) = session::SessionState::load_window_dims();

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
                    // Windows/Linux WebView renders classic, always-visible
                    // scrollbars (with arrow buttons) where macOS uses hidden
                    // overlay scrollbars. Style them down to a thin, button-less
                    // thumb so the chat input grows cleanly and panels match macOS.
                    // Scoped off macOS so its overlay scrollbars stay untouched.
                    + {
                        #[cfg(not(target_os = "macos"))]
                        { r#"<style>
                            * { scrollbar-width: thin; scrollbar-color: var(--color-border-muted, #4B5563) transparent; }
                            ::-webkit-scrollbar { width: 8px; height: 8px; }
                            ::-webkit-scrollbar-track { background: transparent; }
                            ::-webkit-scrollbar-thumb { background-color: var(--color-border-muted, #4B5563); border-radius: 4px; }
                            ::-webkit-scrollbar-thumb:hover { background-color: var(--color-text-muted, #9CA3AF); }
                            ::-webkit-scrollbar-button { display: none; width: 0; height: 0; }
                            ::-webkit-scrollbar-corner { background: transparent; }
                        </style>"# }
                        #[cfg(target_os = "macos")]
                        { "" }
                    }
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
        shared::{SessionIdContext, SessionToDeleteContext},
        stream_manager::StreamManager,
    },
    llm::{GeminiConnector, LlmConnector},
    mcp::manager::McpManager,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Hydrate per-connector API keys from the secret cache into
/// `settings.llm_connectors`. Connectors without a per-id keychain item claim
/// the legacy static key for their provider kind (first claimant per kind) and
/// copy it to `llm_api_key_<id>` + index. The legacy static key is kept for
/// one release of rollback safety.
fn hydrate_connector_keys(
    settings: &mut settings::Settings,
    sm: &mut secret_manager::SecretManager,
) {
    use crate::secret_types::SecretManagerTrait;
    // Match the user's chosen storage mode for the migrated key itself; the
    // index is always written non-biometric (set_llm_key handles that).
    let use_biometric = settings.use_biometric_storage();
    let mut claimed: std::collections::HashSet<settings::LlmProvider> =
        std::collections::HashSet::new();
    let connector_ids: Vec<(String, settings::LlmProvider)> = settings
        .llm_connectors
        .iter()
        .map(|c| (c.id.clone(), c.provider()))
        .collect();
    for (id, kind) in connector_ids {
        if let Some(key) = sm.get_llm_key(&id).cloned() {
            if let Some(c) = settings.connector_by_id_mut(&id) {
                c.config.set_api_key(Some(key));
            }
            continue;
        }
        if claimed.contains(&kind) {
            continue;
        }
        if let Some(key) = sm.get(kind.keychain_key()).cloned() {
            claimed.insert(kind);
            if let Some(c) = settings.connector_by_id_mut(&id) {
                c.config.set_api_key(Some(key.clone()));
            }
            if let Err(e) = sm.set_llm_key(&id, key, use_biometric) {
                tracing::error!(
                    "Failed to copy legacy {} key to connector {}: {}",
                    kind.display_name(),
                    id,
                    e
                );
            } else {
                tracing::info!(
                    "Migrated legacy {} API key to per-connector keychain item",
                    kind.display_name()
                );
            }
        }
    }
}

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
    Planner,
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
    // UI state must load before sessions: its open_tabs list determines which
    // sessions get hydrated from the store (lazy loading — closed sessions
    // stay on disk until opened from History).
    let ui_state_manager =
        use_context_provider(|| Signal::new(settings::UiStateManager::new(get_ui_state_path())));
    let mut ui_state = use_context_provider(|| Signal::new(ui_state_manager.read().load()));
    let mut session_state = use_context_provider(|| {
        let open_tabs = ui_state.peek().open_tabs.clone();
        let mut state = SessionState::load(&open_tabs).unwrap_or_else(|e| {
            tracing::error!("Failed to load session state during startup: {}", e);
            // Create default state with save DISABLED to protect stored data
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

    // Migrate session composio_profile values from names to IDs (one-time migration)
    // Runs in use_effect to avoid signal write during component render (Dioxus warning)
    use_effect(move || {
        let settings_snapshot = settings.peek().clone();
        let mut state = session_state.write();
        let migrated = state.migrate_session_profiles_to_ids(&settings_snapshot);
        drop(state); // Release write guard before borrowing for save
        if migrated {
            SessionState::save_signal(&session_state, None);
        }
    });

    // Global focus context for keyboard event coordination
    use_context_provider(|| Signal::new(components::focus_context::FocusContext::default()));

    // Usage log for cost tracking that survives session deletion
    use_context_provider(|| Signal::new(usage_log::UsageLog::load()));

    let skill_registry = use_context_provider(|| Signal::new(skills::SkillRegistry::new()));
    let mut skills_loaded = use_signal(|| false);

    // The planner is hydrated in full at startup — its rows are small and
    // bounded by human effort, so there is no lazy-loading path to justify.
    // Safe here: session_store::init() ran in main() before the app launched.
    let planner_state = use_context_provider(|| Signal::new(todo::PlannerState::load()));

    // Fleet (Pro): the hook listener runs on plain tokio tasks with its
    // canonical state in `fleet::shared()` — this signal is a UI mirror fed
    // by the drain loop below. Signals are never touched from server tasks.
    let mut fleet_state = use_context_provider(|| Signal::new(fleet::FleetState::default()));
    let mut fleet_runtime_started = use_signal(|| false);
    use_effect(move || {
        if *fleet_runtime_started.peek() {
            return;
        }
        fleet_runtime_started.set(true);

        // Drain loop: mirror state snapshots into the signal and surface
        // newly needs-attention sessions through the existing toast strip
        // (no new notification machinery).
        spawn(async move {
            let shared = fleet::shared().clone();
            let mut rx = shared.subscribe();
            loop {
                if rx.changed().await.is_err() {
                    break;
                }
                let snapshot = shared.snapshot();
                {
                    let prev = fleet_state.peek().clone();
                    for (id, s) in &snapshot.sessions {
                        let was = prev
                            .sessions
                            .get(id)
                            .map(|p| p.status.needs_attention())
                            .unwrap_or(false);
                        if s.status.needs_attention() && !was {
                            flash_timer_toast(format!("🛰 {} needs your attention", s.name));
                        }
                    }
                }
                fleet_state.set(snapshot);
            }
        });

        // Supervisor: keep the listener's lifecycle in sync with the Pro
        // entitlement (hydrated asynchronously with the secrets) and the
        // fleet_enabled setting. Polling sidesteps needing reactive plumbing
        // into the entitlement statics; a few seconds of start/stop latency
        // is fine for an observation feature.
        spawn(async move {
            let shared = fleet::shared().clone();
            let mut server: Option<fleet::server::FleetServer> = None;
            let mut hydrated = false;
            loop {
                let want = settings.peek().fleet_enabled && crate::entitlement::pro_active();
                if want && server.is_none() {
                    if !hydrated {
                        // Sessions left live by a previous process re-enter
                        // the map; the sweep and span caps keep them honest.
                        shared.hydrate_from_store();
                        hydrated = true;
                    }
                    match fleet::server::start_from_meta(shared.clone()).await {
                        Ok(s) => {
                            fleet::set_running_port(Some(s.port));
                            // Self-heal hook registration: the user already
                            // consented to Connect; when a new build changes
                            // the Hobbes-owned entry set, refresh our entries
                            // in place rather than serving a stale set from
                            // behind a green "Connected".
                            if let Some(path) = fleet::hooks_config::claude_settings_path() {
                                if fleet::hooks_config::connected_port_file(&path).is_some() {
                                    let (port, token) = fleet::server::ensure_identity();
                                    if !fleet::hooks_config::file_hooks_current(&path, port, &token)
                                    {
                                        match fleet::hooks_config::connect_file(&path, port, &token)
                                        {
                                            Ok(()) => tracing::info!(
                                                "fleet: refreshed outdated hook registration"
                                            ),
                                            Err(e) => tracing::warn!(
                                                "fleet: hook self-heal failed: {e}"
                                            ),
                                        }
                                    }
                                }
                            }
                            server = Some(s);
                        }
                        Err(e) => tracing::error!("fleet: listener failed to start: {e}"),
                    }
                } else if !want {
                    if let Some(s) = server.take() {
                        s.shutdown();
                        fleet::set_running_port(None);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });

        // Brief supervisor: turns dirty fleet sessions into LLM re-entry
        // briefs. Lives on the Dioxus side because only this half of the
        // process holds hydrated API keys (fleet tokio tasks never see
        // Settings). One task per 15s tick — with the 45s quiet period this
        // is the debounce — keeps cost and concurrency trivially bounded.
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let s = settings.peek().clone();
                if !(s.fleet_enabled
                    && s.fleet_briefs_enabled
                    && crate::entitlement::pro_active())
                {
                    continue;
                }
                let Some(instance) = s
                    .active_connector()
                    .filter(|i| s.is_connector_configured(i))
                    .or_else(|| {
                        s.llm_connectors
                            .iter()
                            .find(|i| s.is_connector_configured(i))
                    })
                    .cloned()
                else {
                    continue;
                };

                let today = chrono::Local::now().date_naive();
                let live = fleet::shared().snapshot();
                let ended = fleet::store::sessions_active_on(today);
                let due = fleet::briefs::collect_due(&live, &ended, chrono::Utc::now());
                let Some(task) = due.into_iter().next() else {
                    continue;
                };

                let started_at = chrono::Utc::now();
                let clear_dirty = |task: &fleet::briefs::BriefTask| {
                    if !fleet::shared().clear_brief_dirty(&task.session_id, started_at) {
                        fleet::store::merge_session(&task.session_id, |row| {
                            if row.brief_dirty_at.is_none_or(|d| d <= started_at) {
                                row.brief_dirty_at = None;
                            }
                        });
                    }
                };

                let tail = match fleet::transcript::read_tail(
                    &task.transcript_path,
                    fleet::transcript::TAIL_MAX_BYTES,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(
                            "fleet briefs: unreadable transcript for {}: {e}",
                            task.session_id
                        );
                        clear_dirty(&task);
                        continue;
                    }
                };
                let digest = fleet::transcript::digest_transcript(
                    &tail,
                    fleet::transcript::DIGEST_MAX_TURNS,
                    fleet::transcript::DIGEST_MAX_CHARS,
                );
                if digest.text.trim().is_empty() {
                    clear_dirty(&task);
                    continue;
                }

                let (prev, framed) = fleet::briefs::brief_framing(&task, &digest);
                let connector = llm::build_connector_for_instance(&instance, None);
                match tokio::time::timeout(
                    std::time::Duration::from_secs(90),
                    connector.generate_fleet_brief(prev, framed),
                )
                .await
                {
                    Ok(Ok(value)) => {
                        let Some(brief) = fleet::briefs::brief_from_summary_value(
                            &value,
                            chrono::Utc::now(),
                            task.is_final,
                        ) else {
                            clear_dirty(&task);
                            continue;
                        };
                        let shared = fleet::shared();
                        if !shared.set_brief(&task.session_id, brief.clone(), started_at) {
                            // Ended (or just-ended) row: merge into the store.
                            let merged =
                                fleet::store::merge_session(&task.session_id, move |row| {
                                    row.brief = Some(brief);
                                    if row.brief_dirty_at.is_none_or(|d| d <= started_at) {
                                        row.brief_dirty_at = None;
                                    }
                                });
                            if merged {
                                shared.poke();
                            }
                        }
                    }
                    Ok(Err(e)) => tracing::warn!(
                        "fleet briefs: generation failed for {}: {e}",
                        task.session_id
                    ),
                    Err(_) => tracing::warn!(
                        "fleet briefs: generation timed out for {}",
                        task.session_id
                    ),
                }
            }
        });
    });

    // Pattern: Grounded App Initialization
    // Masks transient desyncs during the first render tick
    let mut is_app_initialized = use_signal(|| false);
    use_effect(move || {
        is_app_initialized.set(true);
    });

    // Sync theme to DOM (class on <html> element)
    theme::use_theme_sync(settings);

    // Asynchronously load skills from all canonical directories (global + platform),
    // then keep them fresh with a filesystem watcher (debounced auto-reload).
    use_effect(move || {
        if !skills_loaded() {
            spawn(async move {
                skills::SkillRegistry::reload_into_signal(skill_registry).await;
                skills_loaded.set(true);
            });
            spawn(async move {
                skills::watcher::watch_skills_directories(skill_registry).await;
            });
        }
    });

    // Asynchronously load secrets from keychain using biometric authentication
    // This prompts once for Touch ID/password, then uses that context for all secrets
    #[cfg(target_os = "macos")]
    use_effect(move || {
        if !secrets_loaded() {
            // Capture profile IDs before entering the blocking task (for keychain loading)
            let profile_info: Vec<(String, String)> = settings
                .read()
                .composio_profiles
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            // Capture connector IDs so their per-instance keys load even if the
            // discovery index is missing or stale.
            let connector_ids: Vec<String> = settings
                .read()
                .llm_connectors
                .iter()
                .map(|c| c.id.clone())
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
                                tracing::warn!("User cancelled biometric auth — keys saved with biometric protection will NOT be available this session. Falling back to regular keychain access.");
                                None
                            }
                            biometric_auth::AuthResult::NotAvailable(reason) => {
                                tracing::warn!("Biometric auth not available ({}) — keys saved with biometric protection will NOT be available this session. Using regular keychain access.", reason);
                                None
                            }
                            biometric_auth::AuthResult::Failed(error) => {
                                tracing::warn!("Biometric auth failed: {} — keys saved with biometric protection will NOT be available this session. Using regular keychain access.", error);
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

                        // TODO(v0.9.48): Remove dual keychain load after migration window.
                        // MIGRATION: Load Composio keys by both ID and name to bridge the
                        // transition from name-based to ID-based keychain indexing. Keys found
                        // by name are used as a fallback for users who haven't yet triggered
                        // a settings save that would re-store under the ID key.
                        for (id, name) in &profile_info {
                            sm.load_composio_key_with_context(id, Some(ctx));
                            sm.load_composio_key_with_context(name, Some(ctx));
                        }
                        for id in &connector_ids {
                            sm.load_llm_key(id);
                        }
                    } else {
                        // Fall back to regular keychain access (may prompt multiple times)
                        sm.load_all_from_keychain();

                        // TODO(v0.9.48): Remove dual keychain load after migration window.
                        // MIGRATION: Same dual-load rationale as above (name→ID bridge).
                        for (id, name) in &profile_info {
                            sm.load_composio_key(id);
                            sm.load_composio_key(name);
                        }
                        for id in &connector_ids {
                            sm.load_llm_key(id);
                        }
                    }

                    sm
                }).await;

                if let Ok(sm) = loaded_secrets {
                    // Verify + activate the stored Pro license (if any) now
                    // that the secret cache is hydrated.
                    crate::entitlement::hydrate_from_stored_key(
                        sm.get(crate::entitlement::LICENSE_KEYCHAIN_KEY)
                            .map(|s| s.as_str()),
                    );
                    // Update secret manager with loaded secrets
                    secret_manager.set(sm);

                    // Phase 1: Read all secrets (immutable borrow) and collect migration tasks
                    let gemini_key;
                    let smithery_key;
                    let composio_legacy_key;
                    // (profile_index, api_key, needs_migration, profile_id)
                    let mut profile_keys: Vec<(usize, String, bool, String)> = Vec::new();

                    {
                        let sm_read = secret_manager.read();
                        gemini_key = sm_read.get("gemini_api_key").cloned();
                        smithery_key = sm_read.get("smithery_api_key").cloned();
                        composio_legacy_key = sm_read.get("composio_api_key").cloned();

                        let current_settings = settings.read();
                        for (i, profile) in current_settings.composio_profiles.iter().enumerate() {
                            if let Some(api_key) = sm_read.get_composio_key(&profile.id) {
                                profile_keys.push((i, api_key.clone(), false, profile.id.clone()));
                                tracing::debug!(
                                    "Loaded API key for Composio profile: {} ({})",
                                    profile.name,
                                    profile.id
                                );
                            } else if let Some(api_key) = sm_read.get_composio_key(&profile.name) {
                                profile_keys.push((i, api_key.clone(), true, profile.id.clone()));
                                tracing::info!(
                                    "Migrating Composio key for profile '{}' from name to ID '{}'",
                                    profile.name,
                                    profile.id
                                );
                            }
                        }
                    } // sm_read dropped here

                    // Phase 2: Apply settings updates
                    let mut current_settings = settings.write();
                    if let Some(api_key) = gemini_key {
                        current_settings.gemini_config.api_key = Some(api_key);
                    }
                    // Hydrate Claude key from keychain
                    if current_settings.claude_config.api_key.is_none() {
                        let sm_read = secret_manager.read();
                        if let Some(claude_key) = sm_read.get("claude_api_key").cloned() {
                            current_settings.claude_config.api_key = Some(claude_key);
                        }
                    }
                    // Hydrate OpenAI-compat key from keychain
                    if current_settings.openai_compat_config.api_key.is_none() {
                        let sm_read = secret_manager.read();
                        if let Some(oai_key) = sm_read.get("openai_compat_api_key").cloned() {
                            current_settings.openai_compat_config.api_key = Some(oai_key);
                        }
                    }
                    // Hydrate per-connector keys (claims legacy static keys on first run)
                    {
                        let mut sm_write = secret_manager.write();
                        hydrate_connector_keys(&mut current_settings, &mut sm_write);
                    }
                    if let Some(smithery_api_key) = smithery_key {
                        current_settings.smithery_api_key = Some(smithery_api_key);
                    }
                    if let Some(composio_api_key) = composio_legacy_key {
                        current_settings.composio_api_key = Some(composio_api_key);
                    }

                    for (idx, api_key, needs_migration, profile_id) in &profile_keys {
                        if let Some(profile) = current_settings.composio_profiles.get_mut(*idx) {
                            profile.api_key = Some(api_key.clone());
                        }
                        if *needs_migration {
                            let mut sm_write = secret_manager.write();
                            if let Err(e) = sm_write.set_composio_key(profile_id, api_key.clone()) {
                                tracing::error!(
                                    "Failed to migrate legacy Composio key to ID: {}",
                                    e
                                );
                            }
                        }
                    }

                    // Phase 3: Migrate profile-scoped custom tool credentials from name to ID
                    // TODO: Remove after migration window — this clones the entire secret store
                    // to iterate while holding a mutable borrow. For very large stores this could
                    // be a performance concern; once all users have migrated, this block is dead code.
                    {
                        let mut sm_write = secret_manager.write();
                        let all_secrets = sm_write.secrets_ref().clone();
                        for (key, val) in all_secrets {
                            if let Some((Some(p_name), slug, field)) =
                                crate::secret_types::parse_custom_tool_key(&key)
                            {
                                if let Some(profile) = current_settings
                                    .composio_profiles
                                    .iter()
                                    .find(|p| p.name == p_name)
                                {
                                    let id_key = crate::secret_types::format_custom_tool_key(
                                        Some(&profile.id),
                                        &slug,
                                        &field,
                                    );
                                    if !sm_write.has_key(&id_key) {
                                        tracing::info!(
                                            "Migrating custom tool credential for '{}' to ID '{}'",
                                            p_name,
                                            profile.id
                                        );
                                        let _ = sm_write.set_custom_tool_credential(
                                            Some(&profile.id),
                                            &slug,
                                            &field,
                                            val,
                                            current_settings.use_biometric_storage(),
                                        );
                                    }
                                }
                            }
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
            let profile_info: Vec<(String, String)> = settings
                .read()
                .composio_profiles
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            // Capture connector IDs so their per-instance keys load even if the
            // discovery index is missing or stale.
            let connector_ids: Vec<String> = settings
                .read()
                .llm_connectors
                .iter()
                .map(|c| c.id.clone())
                .collect();

            spawn(async move {
                let loaded_secrets = tokio::task::spawn_blocking(move || {
                    let mut sm = secret_manager::SecretManager::new();
                    sm.load_all_from_keychain();
                    // TODO(v0.9.48): Remove dual keychain load after migration window.
                    for (id, name) in &profile_info {
                        sm.load_composio_key(id);
                        sm.load_composio_key(name);
                    }
                    for id in &connector_ids {
                        sm.load_llm_key(id);
                    }
                    sm
                })
                .await;

                if let Ok(sm) = loaded_secrets {
                    // Verify + activate the stored Pro license (if any) now
                    // that the secret cache is hydrated.
                    crate::entitlement::hydrate_from_stored_key(
                        sm.get(crate::entitlement::LICENSE_KEYCHAIN_KEY)
                            .map(|s| s.as_str()),
                    );
                    secret_manager.set(sm);

                    // Phase 1: Read all secrets (immutable borrow) and collect migration tasks
                    let gemini_key;
                    let smithery_key;
                    let composio_legacy_key;
                    // (profile_index, api_key, needs_migration, profile_id)
                    let mut profile_keys: Vec<(usize, String, bool, String)> = Vec::new();

                    {
                        let sm_read = secret_manager.read();
                        gemini_key = sm_read.get("gemini_api_key").cloned();
                        smithery_key = sm_read.get("smithery_api_key").cloned();
                        composio_legacy_key = sm_read.get("composio_api_key").cloned();

                        let current_settings = settings.read();
                        for (i, profile) in current_settings.composio_profiles.iter().enumerate() {
                            if let Some(api_key) = sm_read.get_composio_key(&profile.id) {
                                profile_keys.push((i, api_key.clone(), false, profile.id.clone()));
                                tracing::debug!(
                                    "Loaded API key for Composio profile: {} ({})",
                                    profile.name,
                                    profile.id
                                );
                            } else if let Some(api_key) = sm_read.get_composio_key(&profile.name) {
                                profile_keys.push((i, api_key.clone(), true, profile.id.clone()));
                                tracing::info!(
                                    "Migrating Composio key for profile '{}' from name to ID '{}'",
                                    profile.name,
                                    profile.id
                                );
                            }
                        }
                    } // sm_read dropped here

                    // Phase 2: Apply settings updates
                    let mut current_settings = settings.write();
                    if let Some(api_key) = gemini_key {
                        current_settings.gemini_config.api_key = Some(api_key);
                    }
                    // Hydrate Claude key from keychain
                    if current_settings.claude_config.api_key.is_none() {
                        let sm_read = secret_manager.read();
                        if let Some(claude_key) = sm_read.get("claude_api_key").cloned() {
                            current_settings.claude_config.api_key = Some(claude_key);
                        }
                    }
                    // Hydrate OpenAI-compat key from keychain
                    if current_settings.openai_compat_config.api_key.is_none() {
                        let sm_read = secret_manager.read();
                        if let Some(oai_key) = sm_read.get("openai_compat_api_key").cloned() {
                            tracing::info!("Hydrated OpenAI-compat API key from keychain ({}... chars)", oai_key.len());
                            current_settings.openai_compat_config.api_key = Some(oai_key);
                        } else {
                            tracing::warn!("OpenAI-compat API key not found in keychain — SecretManager has {} keys", sm_read.secrets_ref().len());
                        }
                    } else {
                        tracing::debug!("OpenAI-compat API key already present in settings, skipping keychain hydration");
                    }
                    // Hydrate per-connector keys (claims legacy static keys on first run)
                    {
                        let mut sm_write = secret_manager.write();
                        hydrate_connector_keys(&mut current_settings, &mut sm_write);
                    }
                    if let Some(smithery_api_key) = smithery_key {
                        current_settings.smithery_api_key = Some(smithery_api_key);
                    }
                    if let Some(composio_api_key) = composio_legacy_key {
                        current_settings.composio_api_key = Some(composio_api_key);
                    }

                    for (idx, api_key, needs_migration, profile_id) in &profile_keys {
                        if let Some(profile) = current_settings.composio_profiles.get_mut(*idx) {
                            profile.api_key = Some(api_key.clone());
                        }
                        if *needs_migration {
                            let mut sm_write = secret_manager.write();
                            if let Err(e) = sm_write.set_composio_key(profile_id, api_key.clone()) {
                                tracing::error!(
                                    "Failed to migrate legacy Composio key to ID: {}",
                                    e
                                );
                            }
                        }
                    }

                    // Phase 3: Migrate profile-scoped custom tool credentials from name to ID
                    // TODO: Remove after migration window
                    {
                        let mut sm_write = secret_manager.write();
                        let all_secrets = sm_write.secrets_ref().clone();
                        for (key, val) in all_secrets {
                            if let Some((Some(p_name), slug, field)) =
                                crate::secret_types::parse_custom_tool_key(&key)
                            {
                                if let Some(profile) = current_settings
                                    .composio_profiles
                                    .iter()
                                    .find(|p| p.name == p_name)
                                {
                                    let id_key = crate::secret_types::format_custom_tool_key(
                                        Some(&profile.id),
                                        &slug,
                                        &field,
                                    );
                                    if !sm_write.has_key(&id_key) {
                                        tracing::info!(
                                            "Migrating custom tool credential for '{}' to ID '{}'",
                                            p_name,
                                            profile.id
                                        );
                                        let _ = sm_write.set_custom_tool_credential(
                                            Some(&profile.id),
                                            &slug,
                                            &field,
                                            val,
                                            current_settings.use_biometric_storage(),
                                        );
                                    }
                                }
                            }
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

    // Build the global connector from the active connector instance. Fresh
    // installs have no connectors yet — build a placeholder Gemini connector
    // and rely on the onboarding gate to prevent use until one is created.
    fn build_global_connector(settings: &settings::Settings) -> Arc<dyn LlmConnector> {
        match settings.active_connector() {
            Some(instance) => {
                if let crate::settings::ProviderInstanceConfig::OpenAiCompat(c) = &instance.config
                {
                    tracing::info!(
                        "Building OpenAI-compat connector '{}': model='{}', endpoint='{}', api_key={}",
                        instance.name,
                        c.model,
                        c.endpoint,
                        if c.api_key.is_some() { "present" } else { "MISSING" },
                    );
                }
                crate::llm::build_connector_for_instance(instance, None)
            }
            None => Arc::new(GeminiConnector::new_shared(
                crate::settings::GeminiConfig::default(),
            )),
        }
    }

    let mut llm_connector =
        use_context_provider(|| Signal::new(build_global_connector(&settings.read())));

    // Reactively rebuild the global connector when settings change (e.g. the
    // API key is loaded from biometrics, or the active connector is switched).
    use_effect(move || {
        // read() subscribes to settings changes; clone what we need, then drop
        // the borrow before setting the connector signal.
        let new_connector = build_global_connector(&settings.read());
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

        // Check that the ACTIVE connector is configured — it is what
        // build_global_connector serves to every new session, so an
        // unconfigured active connector must route to setup even when some
        // other connector is configured (fresh installs have none;
        // migrated users keep their existing configured provider).
        let connector_configured = settings
            .active_connector()
            .map(|c| settings.is_connector_configured(c))
            .unwrap_or(false);

        // Need onboarding if either TOS not accepted or no usable connector
        !tos_accepted || !connector_configured
    });

    let permission_manager = use_context_provider(|| Signal::new(PermissionManager::new(settings)));
    let mcp_manager = use_context_provider(|| {
        Signal::new(McpManager::new(
            get_mcp_config_path(),
            permission_manager,
            secret_manager,
            settings,
        ))
    });
    let mcp_context = use_context_provider(|| {
        Signal::new(mcp::manager::McpContext {
            servers: Vec::new(),
            connected_toolkit_slugs: Vec::new(),
        })
    });

    // NOTE: The sync-back effect (Pattern 155) was removed.
    // Profile ID is now written directly to settings.active_composio_profile
    // at every switch point (hotkey handler + chat_input selector),
    // so get_active_profile() works immediately without async indirection.

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

        // Restore persisted on-demand servers state before launching
        let persisted_on_demand = ui_state.peek().on_demand_mcp_servers.clone();
        manager
            .set_initial_on_demand_servers(persisted_on_demand)
            .await;

        manager
            .launch_servers(mcp_context_signal, settings_snapshot)
            .await;
    });

    // Startup context auto-detection for OpenAI-compat provider.
    // Runs once after secrets are loaded so the API key is hydrated before we hit
    // the endpoint. Ensures max_context_tokens and chars_per_token are always
    // populated — even if the user never opens the Settings panel.
    use_effect(move || {
        // Reactive on secrets_loaded only; peek() for everything else so this
        // fires exactly once (when secrets_loaded → true) and not on every keystroke.
        if !secrets_loaded() {
            return;
        }

        let oai_config = {
            let settings_peek = settings.peek();
            match settings_peek.active_connector().map(|c| &c.config) {
                Some(crate::settings::ProviderInstanceConfig::OpenAiCompat(c)) => c.clone(),
                _ => return,
            }
        };

        let endpoint = oai_config.endpoint.clone();
        let model = oai_config.model.clone();
        let api_key = oai_config.api_key.clone();

        if endpoint.is_empty() || model.is_empty() {
            tracing::debug!("Startup context auto-detect: skipping — endpoint or model not configured");
            return;
        }

        spawn(async move {
            // ── Step 1: Context window discovery ──────────────────────────────
            match crate::services::openai_compat_validation
                ::fetch_openai_compat_models_with_context(&endpoint, api_key.as_deref())
                .await
            {
                Ok(discovered) => {
                    if let Some(info) = discovered.iter().find(|m| m.id == model) {
                        if let Some(ctx_len) = info.context_length {
                            let existing = match settings.peek().active_connector().map(|c| &c.config) {
                                Some(crate::settings::ProviderInstanceConfig::OpenAiCompat(c)) => {
                                    c.max_context_tokens
                                }
                                _ => None,
                            };
                            if existing != Some(ctx_len) {
                                tracing::info!(
                                    "Startup: auto-detected context window {} tokens for model '{}'",
                                    ctx_len, model
                                );
                                {
                                    let mut settings_write = settings.write();
                                    if let Some(instance) = settings_write.active_connector_mut() {
                                        if let crate::settings::ProviderInstanceConfig::OpenAiCompat(c) =
                                            &mut instance.config
                                        {
                                            c.max_context_tokens = Some(ctx_len);
                                        }
                                    }
                                    // Keep the legacy singleton in sync during the transition
                                    settings_write.openai_compat_config.max_context_tokens = Some(ctx_len);
                                }
                                // Persist immediately so the value survives the next restart
                                // without requiring a manual Settings → Save cycle.
                                let sm = settings_manager.peek().clone();
                                let s  = settings.peek().clone();
                                let _ = sm.save(&s);
                            } else {
                                tracing::debug!(
                                    "Startup: context window already set to {} tokens for '{}', skipping write",
                                    ctx_len, model
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            "Startup: model '{}' not found in /v1/models response — cannot auto-detect context window",
                            model
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("Startup context auto-detect: discovery failed — {}", e);
                }
            }

            // ── Step 2: Tokenizer calibration (skip if manually overridden) ──
            let already_calibrated = match settings.peek().active_connector().map(|c| &c.config) {
                Some(crate::settings::ProviderInstanceConfig::OpenAiCompat(c)) => {
                    c.context_tuning.chars_per_token.is_some()
                }
                _ => true,
            };
            if !already_calibrated {
                if let Some(ratio) = crate::services::openai_compat_validation
                    ::calibrate_tokenizer(&endpoint, &model, api_key.as_deref())
                    .await
                {
                    let rounded = (ratio * 10.0).round() / 10.0;
                    tracing::info!(
                        "Startup: auto-calibrated {:.1} chars/token for model '{}'",
                        rounded, model
                    );
                    {
                        let mut settings_write = settings.write();
                        if let Some(instance) = settings_write.active_connector_mut() {
                            if let crate::settings::ProviderInstanceConfig::OpenAiCompat(c) =
                                &mut instance.config
                            {
                                c.context_tuning.chars_per_token = Some(rounded);
                            }
                        }
                        // Keep the legacy singleton in sync during the transition
                        settings_write.openai_compat_config.context_tuning.chars_per_token =
                            Some(rounded);
                    }
                    // Persist calibration ratio alongside the context window value
                    let sm = settings_manager.peek().clone();
                    let s  = settings.peek().clone();
                    let _ = sm.save(&s);
                }
            }
        });
    });

    // Reinitialize Composio client when active profile changes
    // This use_effect subscribes to changes in active_composio_profile
    {
        // Track key profile properties to detect changes (API key, base URL, etc.)
        let mut prev_profile_signature: Signal<Option<String>> = use_signal(|| None);

        use_effect(move || {
            // Don't reinitialize before secrets are loaded — the profile's api_key
            // won't be hydrated yet, causing a false "No API key" error.
            if !secrets_loaded() {
                return;
            }

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

            // Only reinitialize if the profile signature actually changed
            if current_signature != previous {
                tracing::info!(
                    "Active Composio profile properties changed, reinitializing client: {:?}",
                    current_signature
                );

                // Invalidate caches - profile changed
                mcp_manager.read().invalidate_status_cache();

                let mcp_context_signal = mcp_context;
                let settings_clone = settings.read().clone();

                spawn(async move {
                    mcp_manager
                        .read()
                        .reinitialize_composio_client(mcp_context_signal, settings_clone, None)
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
    // The planner is its own tab in the tab bar, not a sidebar: `open` is
    // whether the tab exists, `active` whether it (vs. the selected session's
    // chat) fills the main column. Selecting any session tab deactivates the
    // planner without closing its tab.
    let mut planner_tab_open = use_signal(|| false);
    let mut planner_active = use_signal(|| false);
    let mut settings_panel_width = use_signal(|| ui_state.read().settings_panel_width);
    let mut is_dragging = use_signal(|| false);
    let mut drag_start_info = use_signal(|| (0.0, 0.0)); // (start_x, start_width)
    let mut final_width_on_drag_end = use_signal(|| 0.0);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    let mut tray_icon = use_signal::<Option<TrayIcon>>(|| None);
    let mut show_confirm_modal = use_context_provider(|| Signal::new(false));
    let mut session_to_delete =
        use_context_provider(|| SessionToDeleteContext(Signal::new(String::new())));
    let mut save_error =
        use_context_provider(|| crate::components::shared::SaveErrorContext(Signal::new(None))).0;

    let mut chat_command = use_context_provider(|| Signal::new(None::<ChatCommand>));
    let mut pending_chat_seed = use_context_provider(|| {
        crate::components::shared::PendingChatSeedContext(Signal::new(None))
    })
    .0;

    // Tab state - initialized from UiState or with current active session
    let mut open_tabs = use_signal(|| {
        let ui = ui_state.read();
        let state = session_state.read();

        // Filter tabs to ensure they only contain existing session IDs
        let tabs: Vec<String> = ui
            .open_tabs
            .iter()
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

    // ── AI-settable timer scheduler ──────────────────────────────────────────
    // Polls every 5s for due timers (HOBBES_SET_TIMER) and fires them: focus the
    // window + toast, and for `prompt` mode enqueue the prompt and switch to its
    // session so ChatInput's drain runs it. Timers missed while the app was
    // closed are surfaced as a reminder but never auto-run (no surprise turns).
    {
        let mut session_state = session_state;
        let mut chat_command = chat_command;
        let settings = settings;
        let window = window.clone();
        use_future(move || {
            let window = window.clone();
            async move {
                // Startup: handle timers that came due while the app was closed.
                let now = chrono::Utc::now();
                let mut missed: Vec<String> = Vec::new();
                if session_state
                    .read()
                    .sessions
                    .values()
                    .any(|s| s.scheduled_timers.iter().any(|t| t.is_due(now)))
                {
                    let mut missed_timers: Vec<crate::timers::ScheduledTimer> = Vec::new();
                    {
                        let mut state = session_state.write();
                        for session in state.sessions.values_mut() {
                            for t in session.scheduled_timers.iter_mut() {
                                if t.is_due(now) {
                                    t.status = crate::timers::TimerStatus::Fired;
                                    missed.push(t.label.clone().unwrap_or_else(|| "(timer)".into()));
                                    missed_timers.push(t.clone());
                                }
                            }
                        }
                    }
                    for t in &missed_timers {
                        crate::session_events::log_event(
                            &t.session_id,
                            crate::session_events::SessionEvent::TimerFired { timer: t.clone() },
                        );
                    }
                }
                if !missed.is_empty() {
                    crate::session::SessionState::save_async(&session_state.read(), None);
                    flash_timer_toast(format!(
                        "⏰ Missed {} reminder(s) while away: {}",
                        missed.len(),
                        missed.join(", ")
                    ));
                }

                // Poll loop.
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let now = chrono::Utc::now();

                    // Cheap read-only check first so we don't mark the signal
                    // dirty (and re-render) every 5s when nothing is due.
                    let any_due = session_state
                        .read()
                        .sessions
                        .values()
                        .any(|s| s.scheduled_timers.iter().any(|t| t.is_due(now)));
                    if !any_due {
                        continue;
                    }

                    let mut fired: Vec<crate::timers::ScheduledTimer> = Vec::new();
                    {
                        let mut state = session_state.write();
                        for session in state.sessions.values_mut() {
                            for t in session.scheduled_timers.iter_mut() {
                                if t.is_due(now) {
                                    t.status = crate::timers::TimerStatus::Fired;
                                    fired.push(t.clone());
                                }
                            }
                        }
                    }
                    for t in &fired {
                        crate::session_events::log_event(
                            &t.session_id,
                            crate::session_events::SessionEvent::TimerFired { timer: t.clone() },
                        );
                    }
                    crate::session::SessionState::save_async(&session_state.read(), None);

                    for timer in fired {
                        // Only steal focus / raise the window if the user opted
                        // in (off by default — it's disruptive). The toast and
                        // in-app indicator surface the timer regardless.
                        if settings.peek().timer_focus_window {
                            *WINDOW_VISIBLE.write() = true;
                            window.set_visible(true);
                            window.set_focus();
                        }

                        let label = timer.label.clone().unwrap_or_else(|| "Reminder".into());
                        match timer.mode {
                            crate::timers::TimerMode::Notify => {
                                flash_timer_toast(format!("⏰ {}", label));
                            }
                            crate::timers::TimerMode::Prompt => {
                                flash_timer_toast(format!("⏰ {} — running follow-up…", label));
                                if let Some(prompt) = timer.prompt.clone() {
                                    // Reuse the chat queue: enqueue for the timer's
                                    // session, then switch to it; ChatInput's drain
                                    // runs it once that session is idle.
                                    crate::components::chat_queue::queue_push(
                                        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
                                        &timer.session_id,
                                        crate::components::chat_queue::QueuedMessage::new(
                                            prompt,
                                            Vec::new(),
                                        ),
                                    );
                                    chat_command.set(Some(ChatCommand::SwitchToSession(
                                        timer.session_id.clone(),
                                    )));
                                    // Nudge the drain in case that session is
                                    // already active (no switch → no re-trigger).
                                    crate::components::chat_queue::request_drain();
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Persist tab state to UiState (Pattern 12 & 13)
    use_effect(move || {
        let mut ui = ui_state.write();
        ui.open_tabs = open_tabs.read().clone();
        ui.active_tab_index = *active_tab_index.read();

        // Snapshot targets for Pattern 12 persistence
        let ui_snapshot = ui.clone();
        let manager = ui_state_manager.peek().clone();

        manager.save_async(ui_snapshot, Some(save_error));
    });

    let active_profile_id = move || settings.peek().get_active_profile().map(|p| p.id.clone());

    let mut sync_profile_from_session = move |session_id: &str| {
        let profile_id = session_state
            .read()
            .sessions
            .get(session_id)
            .and_then(|s| s.composio_profile.clone());
        if let Some(pid) = profile_id {
            if settings.peek().active_composio_profile.as_deref() != Some(&pid) {
                settings.write().active_composio_profile = Some(pid);
            }
        }
    };

    // Sync active profile from the initial session on app startup (Pattern 150/151)
    // Without this, settings.active_composio_profile retains whatever was last
    // persisted, which may differ from the profile of the tab being displayed.
    use_effect(move || {
        let session_id = current_session_id.read().clone();
        sync_profile_from_session(&session_id);
    });

    // Tab switching - use signal copies directly in closures
    let mut switch_tab_fn = move |idx: usize| {
        // Selecting a chat tab brings its chat forward; the planner tab (if
        // open) stays in the bar, just no longer active.
        planner_active.set(false);
        let tabs = open_tabs.read();
        if idx < tabs.len() {
            let session_id = tabs[idx].clone();
            active_tab_index.set(idx);
            current_session_id.set(session_id.clone());
            session_state.write().active_session_id = session_id.clone();
            sync_profile_from_session(&session_id);
        }
    };

    let mut delete_session_fn = move |id_to_delete: String| {
        let mut state = session_state.write();
        state.delete_session(&id_to_delete);
        drop(state);

        // Drop any messages queued for the now-deleted session.
        crate::components::chat_queue::queue_clear(
            &mut crate::components::chat_queue::CHAT_QUEUE.write(),
            &id_to_delete,
        );

        let conn = llm_connector.read().clone();
        let id_to_delete_clone = id_to_delete.clone();
        tokio::spawn(async move {
            conn.invalidate_session_cache(&id_to_delete_clone).await;
        });

        let mut tabs = open_tabs.read().clone();
        if let Some(tab_idx) = tabs.iter().position(|id| id == &id_to_delete) {
            tabs.remove(tab_idx);
            let mut new_idx = *active_tab_index.read();
            if tabs.is_empty() {
                let new_id = session_state.write().create_session(active_profile_id());
                tabs.push(new_id);
                new_idx = 0;
            } else if new_idx >= tabs.len() {
                new_idx = tabs.len().saturating_sub(1);
            }
            let new_session_id = tabs[new_idx].clone();

            open_tabs.set(tabs);
            active_tab_index.set(new_idx);
            current_session_id.set(new_session_id.clone());
            session_state.write().active_session_id = new_session_id.clone();
            sync_profile_from_session(&new_session_id);
        }
    };

    let mut close_tab_fn = move |idx: usize| {
        let mut tabs = open_tabs.read().clone();
        if idx < tabs.len() {
            let closing_session_id = tabs[idx].clone();
            tabs.remove(idx);

            // A closed tab can't drain its queue; drop it (runtime-only state).
            crate::components::chat_queue::queue_clear(
                &mut crate::components::chat_queue::CHAT_QUEUE.write(),
                &closing_session_id,
            );

            // Delete the session if it has no messages (empty tab).
            // Sessions with messages are preserved in History.
            {
                let state = session_state.read();
                let is_empty = state.sessions.get(&closing_session_id)
                    .map(|s| s.messages.is_empty())
                    .unwrap_or(true);
                if is_empty {
                    drop(state);
                    session_state.write().delete_session(&closing_session_id);
                    tracing::info!("close_tab: deleted empty session {}", closing_session_id);

                    let conn = llm_connector.read().clone();
                    let closing_id_clone = closing_session_id.clone();
                    tokio::spawn(async move {
                        conn.invalidate_session_cache(&closing_id_clone).await;
                    });
                }
            }

            let mut new_idx = *active_tab_index.read();
            if tabs.is_empty() {
                let new_id = session_state.write().create_session(active_profile_id());
                tabs.push(new_id);
                new_idx = 0;
            } else if new_idx >= tabs.len() {
                new_idx = tabs.len().saturating_sub(1);
            }
            let new_session_id = tabs[new_idx].clone();

            open_tabs.set(tabs);
            active_tab_index.set(new_idx);
            current_session_id.set(new_session_id.clone());
            session_state.write().active_session_id = new_session_id.clone();
            sync_profile_from_session(&new_session_id);
        }
    };

    let mut new_tab_fn = move || {
        planner_active.set(false);
        let new_id = session_state.write().create_session(active_profile_id());
        let mut tabs = open_tabs.read().clone();
        tabs.push(new_id.clone());
        open_tabs.set(tabs.clone());
        active_tab_index.set(tabs.len() - 1);
        session_state.write().active_session_id = new_id.clone();
        current_session_id.set(new_id);
    };

    // Call the summarization scheduler hook BEFORE the hotkey manager
    processing::summarization_scheduler::use_summarization_scheduler();

    // Background calendar sync: an immediate pass on launch, then every 15
    // minutes (or on CalendarSyncMsg::SyncNow). Gated internally on
    // settings.planner_enabled.
    todo::calendar_sync::use_calendar_sync();

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
                MenuAction::Planner => {
                    tracing::info!("MenuAction::Planner triggered");
                    chat_command.set(Some(ChatCommand::TogglePlanner));
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
                        "view_planner" => Some(MenuAction::Planner),
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

    // The single tray icon is owned by the focus-timer sync effect below —
    // it reads both `settings.show_tray_icon` and the planner's focus state,
    // so the icon exists when either wants it (the icon hosts the timer for
    // the duration of a focus session even with the setting off).

    // ── Tray icon + focus timer (single owner) ───────────────────────────
    // While a todo is in focus, the tray icon mirrors the FocusBar's live
    // readout (macOS: menu-bar title text; Windows: tooltip) and reverts to
    // plain when focus ends. The effect reacts to every planner and settings
    // change (focus start/stop/complete, show_tray_icon toggle → immediate),
    // and a ~30s ticker keeps the elapsed minutes honest in between. Runs on
    // the main thread (a macOS requirement for tray creation), like all
    // effects. Tray listening starts inside `tray::init_tray`.
    let mut focus_tray_tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            focus_tray_tick += 1;
        }
    });
    use_effect(move || {
        let _subscribe = *focus_tray_tick.read();
        let show_tray = settings.read().show_tray_icon;
        let snapshot = if settings.read().planner_enabled {
            focus_tray::focus_snapshot(&planner_state.read(), chrono::Utc::now())
        } else {
            None
        };
        // Local/Cloud indicator: resolved from the ACTIVE session's effective
        // connector (sessions can pin their own). Subscribes to the session
        // *id* and to settings — connector pinning always writes settings too
        // (SwitchModel/SwitchProvider update the global default), so peeking
        // the session map avoids re-running this on every streamed token.
        let privacy = if settings.read().show_privacy_indicator {
            let s = settings.read();
            let session_state = session_state.peek();
            let sid = current_session_id.read().clone();
            let connector = match session_state.sessions.get(&sid) {
                Some(session) => s.connector_for_session(session),
                None => s.active_connector(),
            };
            Some(focus_tray::privacy_status(
                connector,
                !s.composio_profiles.is_empty(),
            ))
        } else {
            None
        };
        focus_tray::sync_tray(
            &mut tray_icon.write(),
            show_tray,
            snapshot.as_ref(),
            privacy.as_ref(),
        );
    });

    // Focus-timer tray clicks: surface the window and reveal the focused
    // todo in the planner. Direct signal sets (not ChatCommand::TogglePlanner)
    // because a toggle would close the planner when it is already open.
    let window_for_focus_tray = window.clone();
    use_effect(move || {
        let clicks = *focus_tray::FOCUS_TRAY_CLICKS.read();
        if clicks == 0 {
            return; // initial run at mount — nothing was clicked yet
        }
        *WINDOW_VISIBLE.write() = true;
        window_for_focus_tray.set_minimized(false);
        window_for_focus_tray.set_visible(true);
        window_for_focus_tray.set_focus();
        planner_tab_open.set(true);
        planner_active.set(true);
        if let Some(id) = planner_state.peek().focused().map(|t| t.id.clone()) {
            *focus_tray::PLANNER_REVEAL_TODO.write() = Some(id);
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
                    uism.save_async(current_ui_state.clone(), None);
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
                // Window dimensions are persisted at the next natural save point
                // (message send, session switch, app close). Serializing the entire
                // session state here was blocking the UI thread for large states.
            });
        }
    });

    // Single Authority for Command Handling (Pattern 126.1)
    // Listens for global commands from hotkeys, menu, or child components
    use_effect(move || {
        let cmd_opt = chat_command.read().clone();
        if let Some(cmd) = cmd_opt {
            tracing::debug!("App handling global ChatCommand: {:?}", cmd);

            match cmd {
                ChatCommand::SwitchToSettingsTab(tab, slug) => {
                    tracing::info!(
                        "App: Switching to Settings Tab: {:?}, slug: {:?}",
                        tab,
                        slug
                    );
                    show_settings_panel.set(true);
                    show_session_manager.set(false);
                    show_mcp_manager.set(false);
                    let mut state = ui_state.write();
                    state.active_settings_tab = tab;
                    state.selected_byoa_slug = slug;
                }
                ChatCommand::SwitchTab(idx) => {
                    switch_tab_fn(idx);
                }
                ChatCommand::SwitchProfile(idx) => {
                    let profiles = settings.peek().composio_profiles.clone();
                    if idx < profiles.len() {
                        let new_profile_id = profiles[idx].id.clone();
                        let new_profile_name = profiles[idx].name.clone();
                        // Determine if Settings are stale
                        let settings_stale = settings.peek().active_composio_profile.as_deref()
                            != Some(&new_profile_id);

                        let mut session_changed = false;
                        // Scope the write lock so it's dropped before setting signals
                        {
                            let mut state = session_state.write();
                            if let Some(session) =
                                state.sessions.get_mut(&*current_session_id.read())
                            {
                                if session.composio_profile.as_deref() != Some(&new_profile_id) {
                                    session.composio_profile = Some(new_profile_id.clone());
                                    session.active_context.mcp_tools = None;
                                    session_changed = true;
                                }
                            }
                        } // write lock dropped here

                        // Set settings AFTER write lock is released to avoid stale reactive reads
                        if settings_stale || session_changed {
                            tracing::info!(
                                "SwitchProfile: forcing update (stale={} session={}) to {:?}",
                                settings_stale,
                                session_changed,
                                new_profile_name
                            );
                            // Write ID to settings IMMEDIATELY so get_active_profile() works
                            // without waiting for an async sync-back effect
                            settings.write().active_composio_profile = Some(new_profile_id);
                            // Note: settings update triggers 'signature' effect -> reinitializes MCP client automatically
                        }

                        if session_changed {
                            crate::session_events::log_event(
                                &current_session_id.read().clone(),
                                crate::session_events::SessionEvent::ComposioProfileSet {
                                    profile: settings.peek().active_composio_profile.clone(),
                                },
                            );
                            SessionState::save_async(&session_state.read(), Some(save_error));
                            mcp_manager.read().invalidate_status_cache();
                        }
                    }
                }
                ChatCommand::SwitchModel(idx) => {
                    // Slots come from the session's effective connector so the picker
                    // works in tabs pinned to a non-global connector.
                    let instance = {
                        let settings_read = settings.peek();
                        let state = session_state.peek();
                        state
                            .sessions
                            .get(&*current_session_id.read())
                            .and_then(|s| settings_read.connector_for_session(s))
                            .or_else(|| settings_read.active_connector())
                            .cloned()
                    };
                    if instance.is_none() {
                        tracing::warn!("SwitchModel: no LLM connector configured, ignoring");
                    }
                    if let Some(instance) = instance {
                    let slots = instance.config.model_slots();
                    if idx < slots.len() {
                        let new_model = slots[idx].clone();
                        if !new_model.is_empty() {
                            tracing::info!(
                                "SwitchModel: switching to slot {} model '{}' ({})",
                                idx,
                                new_model,
                                instance.name
                            );
                            // Pin the session to the chosen connector+model pair...
                            let mut session_changed = false;
                            {
                                let mut state = session_state.write();
                                if let Some(session) =
                                    state.sessions.get_mut(&*current_session_id.read())
                                {
                                    if session.llm_connector_id.as_deref()
                                        != Some(instance.id.as_str())
                                        || session.chat_model.as_deref()
                                            != Some(new_model.as_str())
                                    {
                                        session.llm_connector_id = Some(instance.id.clone());
                                        session.llm_provider = Some(instance.provider());
                                        session.chat_model = Some(new_model.clone());
                                        session_changed = true;
                                    }
                                }
                            } // write lock dropped here
                            // Session-only pin: do NOT update global settings so other
                            // tabs keep their current connector/model unaffected.
                            if session_changed {
                                crate::session_events::log_event(
                                    &current_session_id.read().clone(),
                                    crate::session_events::SessionEvent::ConnectorPinned {
                                        connector_id: Some(instance.id.clone()),
                                        provider: Some(format!("{:?}", instance.provider())),
                                        model: Some(new_model.clone()),
                                    },
                                );
                                SessionState::save_async(&session_state.read(), Some(save_error));
                            }
                        } else {
                            tracing::info!("SwitchModel: slot {} is empty, ignoring", idx);
                        }
                    } else {
                        tracing::warn!("SwitchModel: slot {} out of range ({})", idx, slots.len());
                    }
                    }
                }
                ChatCommand::SwitchConnector(connector_id) => {
                    let instance = settings.peek().connector_by_id(&connector_id).cloned();
                    match instance {
                        Some(instance)
                            if settings.peek().is_connector_configured(&instance) =>
                        {
                            // Pin the session to the chosen connector; the model follows
                            // that connector's configured default.
                            let mut session_changed = false;
                            {
                                let mut state = session_state.write();
                                if let Some(session) =
                                    state.sessions.get_mut(&*current_session_id.read())
                                {
                                    if session.llm_connector_id.as_deref()
                                        != Some(instance.id.as_str())
                                    {
                                        session.llm_connector_id = Some(instance.id.clone());
                                        session.llm_provider = Some(instance.provider());
                                        session.chat_model = None;
                                        session_changed = true;
                                    }
                                }
                            } // write lock dropped here
                            // Session-only pin: do NOT touch the global active connector so
                            // other tabs keep their current connector unaffected. Global
                            // changes belong in the Settings panel, not the per-tab picker.
                            if session_changed {
                                crate::session_events::log_event(
                                    &current_session_id.read().clone(),
                                    crate::session_events::SessionEvent::ConnectorPinned {
                                        connector_id: Some(instance.id.clone()),
                                        provider: Some(format!("{:?}", instance.provider())),
                                        model: None,
                                    },
                                );
                                SessionState::save_async(&session_state.read(), Some(save_error));
                            }
                            tracing::info!(
                                "SwitchConnector: pinned session to '{}' (session_changed={})",
                                instance.name,
                                session_changed
                            );
                        }
                        Some(instance) => {
                            tracing::info!(
                                "SwitchConnector: '{}' is not configured, ignoring",
                                instance.name
                            );
                        }
                        None => {
                            tracing::warn!(
                                "SwitchConnector: unknown connector id '{}', ignoring",
                                connector_id
                            );
                        }
                    }
                }
                ChatCommand::ToggleSettings => {
                    let new_state = !*show_settings_panel.peek();
                    show_settings_panel.set(new_state);
                    if new_state {
                        show_session_manager.set(false);
                        show_mcp_manager.set(false);
                    }
                }
                ChatCommand::ToggleHistory => {
                    let new_state = !*show_session_manager.peek();
                    show_session_manager.set(new_state);
                    if new_state {
                        show_settings_panel.set(false);
                        show_mcp_manager.set(false);
                    }
                }
                ChatCommand::ToggleMcp => {
                    let new_state = !*show_mcp_manager.peek();
                    show_mcp_manager.set(new_state);
                    if new_state {
                        show_settings_panel.set(false);
                        show_session_manager.set(false);
                    }
                }
                ChatCommand::StartTodoInChat(text) => {
                    // Sequenced in ONE effect: open the tab (which deactivates
                    // the planner), then park the seed. ChatInput's consuming
                    // effect fires after this render commits — textarea
                    // visible, seed applied exactly once. Racing two consumers
                    // on chat_command lost the seed whenever the async command
                    // clear ran first.
                    new_tab_fn();
                    pending_chat_seed.set(Some(text));
                }
                ChatCommand::TogglePlanner => {
                    // First invocation opens the tab and focuses it; afterwards
                    // it toggles focus between planner and chat. Closing the
                    // tab is only ever done from its ✕ — a toggle that silently
                    // removed the tab would make the icon feel destructive.
                    if !*planner_tab_open.peek() {
                        planner_tab_open.set(true);
                        planner_active.set(true);
                    } else {
                        let now_active = !*planner_active.peek();
                        planner_active.set(now_active);
                    }
                }
                ChatCommand::DeleteSession(target_id) => {
                    if !target_id.is_empty() {
                        if settings.peek().confirm_on_delete {
                            session_to_delete.0.set(target_id);
                            show_confirm_modal.set(true);
                        } else {
                            delete_session_fn(target_id);
                        }
                    }
                }
                ChatCommand::CloseTab => {
                    let idx = *active_tab_index.read();
                    close_tab_fn(idx);
                }
                ChatCommand::SwitchToSession(session_id) => {
                    // Lazy hydration: sessions opened from History may not be
                    // in memory yet — load them from the store first.
                    let mut hydrated = session_state.peek().sessions.contains_key(&session_id);
                    if !hydrated {
                        if let Some(session) = crate::session_store::load_session(&session_id) {
                            // Drift guard (debug builds): compare the stored row
                            // against its journal projection; warn on divergence,
                            // never block hydration.
                            #[cfg(debug_assertions)]
                            crate::session_events::debug_check_drift(&session);
                            session_state
                                .write()
                                .sessions
                                .insert(session_id.clone(), session);
                            hydrated = true;
                        } else {
                            tracing::error!(
                                "SwitchToSession: session {} not found in store",
                                session_id
                            );
                        }
                    }
                    let tabs = open_tabs.read().clone();
                    if !hydrated {
                        // Fall through to command clearing without switching.
                    } else if let Some(idx) = tabs.iter().position(|id| id == &session_id) {
                        active_tab_index.set(idx);
                        current_session_id.set(session_id.clone());
                        session_state.write().active_session_id = session_id.clone();
                        sync_profile_from_session(&session_id);
                    } else {
                        let mut new_tabs = tabs;
                        new_tabs.push(session_id.clone());
                        let new_idx = new_tabs.len() - 1;
                        open_tabs.set(new_tabs);
                        active_tab_index.set(new_idx);
                        current_session_id.set(session_id.clone());
                        session_state.write().active_session_id = session_id.clone();
                        sync_profile_from_session(&session_id);
                    }
                }
                ChatCommand::NewChat => {
                    new_tab_fn();
                }
                // Locally-handled commands (consumed by ChatInput / chat_input.rs)
                ChatCommand::ToggleProfile
                | ChatCommand::OpenAttachments
                | ChatCommand::NewChatWithMemory
                | ChatCommand::ScrollToBottom
                | ChatCommand::FocusChat
                | ChatCommand::CancelGeneration
                | ChatCommand::CopyToDraft(_)
                | ChatCommand::RestoreToDraft(_, _)
                | ChatCommand::TriggerAiAnalysis
                | ChatCommand::ToggleModelSelector
                | ChatCommand::ToggleProviderSelector => {}
            }

            // Centralized command clearing to avoid loops
            spawn(async move {
                chat_command.set(None);
            });
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
                                delete_session_fn(id_to_delete_str);
                            }
                            if remember {
                                let mut current_settings = settings.write();
                                current_settings.confirm_on_delete = false;
                                let sm = settings_manager.read();
                                sm.save_async(current_settings.clone(), Some(save_error));
                            }
                            show_confirm_modal.set(false);
                        },
                    }
                        div {
                            class: "flex flex-col h-screen bg-app text-fg",
                            // Save error toast notification
                            if let Some(err_msg) = save_error.read().as_ref() {
                                div {
                                    class: "flex items-center justify-between px-4 py-2 bg-red-900/80 border-b border-red-700 text-red-100 text-sm",
                                    span { "{err_msg}" }
                                    button {
                                        class: "ml-4 px-2 py-0.5 text-xs bg-red-800 hover:bg-red-700 rounded transition-colors",
                                        onclick: move |_| { save_error.set(None); },
                                        "Dismiss"
                                    }
                                }
                            }
                            // Timer / reminder toast notification
                            if let Some(msg) = TIMER_TOAST.read().as_ref() {
                                div {
                                    class: "flex items-center justify-between px-4 py-2 bg-primary-900/80 border-b border-primary-700 text-fg text-sm",
                                    span { "{msg}" }
                                    button {
                                        class: "ml-4 px-2 py-0.5 text-xs bg-primary-800 hover:bg-primary-700 rounded transition-colors",
                                        onclick: move |_| { *TIMER_TOAST.write() = None; },
                                        "Dismiss"
                                    }
                                }
                            }
                            // Main content area
                            div {
                                class: "flex flex-row flex-1 min-h-0 overflow-hidden", // This will contain the sidebars and chat
                                // The onkeydown handler has been removed to allow native hotkeys (copy, paste, etc.) to function correctly.
                                // The global hotkey for toggling visibility is no longer required.
                                // When the user releases the mouse, save the last known size.


                            // Session Manager Sidebar
                            if *show_session_manager.read() {
                                div {
                                    class: "flex flex-row h-full shrink-0",
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
                                    class: "flex flex-row h-full shrink-0",
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
                                    class: "flex flex-row h-full shrink-0",
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
                                class: "flex-1 flex flex-col min-h-0 min-w-0 border-t border-primary-700/50",
                                {
                                    let tabs = open_tabs.read();
                                    let state = session_state.read();
                                    rsx! {
                                        components::tab_bar::TabBar {
                                            open_tabs: tabs.clone(),
                                            tab_names: tabs.iter().map(|id| {
                                                state.sessions.get(id)
                                                    .map(|s| s.name.clone())
                                                    .unwrap_or_else(|| "New Session".to_string())
                                            }).collect(),
                                            active_tab_index: *active_tab_index.read(),
                                            on_select_tab: switch_tab_fn,
                                            on_close_tab: close_tab_fn,
                                            on_new_tab: move |_| new_tab_fn(),
                                            planner_tab_open: *planner_tab_open.read(),
                                            planner_active: *planner_active.read(),
                                            on_select_planner: move |_| planner_active.set(true),
                                            on_close_planner: move |_| {
                                                planner_active.set(false);
                                                planner_tab_open.set(false);
                                            },
                                        }
                                    }
                                }

                                // ChatWindow stays mounted while the planner is shown —
                                // unmounting it mid-stream would drop live streaming
                                // updates, so visibility is toggled with CSS instead.
                                div {
                                    class: if *planner_active.read() { "hidden" } else { "flex-1 flex flex-col min-h-0 min-w-0" },
                                    components::chat::ChatWindow {
                                        on_content_resize: move |_| {},
                                        on_interaction: move |_| {},
                                    }
                                }
                                if *planner_active.read() {
                                    div {
                                        class: "flex-1 min-h-0 min-w-0",
                                        components::planner_view::PlannerView {}
                                    }
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
