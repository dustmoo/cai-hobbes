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

mod agent_time_runtime;
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
mod layout_budget;
mod log_utils;
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
mod tab_list;
mod tab_tidy;
#[cfg(test)]
mod test_support;
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

/// Mirror of `StreamManagerContext.streaming_sessions`, kept in sync by
/// `StreamBridge`. `app()`'s own root-scope closures (tab close/delete, the
/// `ChatCommand` handler) cannot `consume_context::<StreamManagerContext>()`
/// themselves — that context is provided by the `StreamManager` component,
/// a *descendant* of the root scope, and Dioxus context only flows downward
/// — so they consult this GlobalSignal instead, which is reachable from any
/// scope.
static STREAMING_SESSIONS: GlobalSignal<std::collections::HashSet<String>> =
    Signal::global(std::collections::HashSet::new);

/// Session ids `delete_session_fn` wants `StreamBridge` to cancel the active
/// stream for. Set from the root scope (which cannot reach
/// `StreamManagerContext` directly — see `STREAMING_SESSIONS`), drained by
/// `StreamBridge`, which *can*. A set rather than a single id so two deletes
/// landing between two effect runs don't drop one of the cancels.
static CANCEL_STREAM_REQUEST: GlobalSignal<std::collections::HashSet<String>> =
    Signal::global(std::collections::HashSet::new);

/// Ask `StreamBridge` to cancel `session_id`'s active stream, from code
/// that cannot reach `StreamManagerContext` (the agent-time runtime's
/// Pause / Stop / budget stop). No-op when nothing is streaming there.
pub(crate) fn request_stream_cancel(session_id: &str) {
    CANCEL_STREAM_REQUEST.write().insert(session_id.to_string());
}

/// Drain `CANCEL_STREAM_REQUEST`, subscribing the caller to it.
///
/// Must be called from inside a `use_effect`: only `.read()` registers the
/// effect's reactive context (`.write()` notifies subscribers but subscribes
/// nothing), so the read below is what makes a later insert re-run the
/// effect. The read guard ends with its statement so it is never held across
/// the drain's write, and the early return keeps an idle run from writing at
/// all — an unconditional write after the read would re-dirty the effect
/// forever.
fn drain_cancel_stream_requests() -> Vec<String> {
    if CANCEL_STREAM_REQUEST.read().is_empty() {
        return Vec::new();
    }
    std::mem::take(&mut *CANCEL_STREAM_REQUEST.write())
        .into_iter()
        .collect()
}

/// Parent session ids a landed dispatch continuation wants reopened in the
/// tab strip. Written by the fleet drain loop's continuation block (a plain
/// `spawn`ed task, which runs *before* `open_tabs` is declared later in
/// `app()` and so cannot reference it directly — same reach problem as
/// `STREAMING_SESSIONS`/`CANCEL_STREAM_REQUEST` above, same fix: a
/// root-reachable `GlobalSignal`). Drained by the `ChatCommand` handler,
/// which does have `open_tabs` in scope.
static PENDING_TAB_REOPEN: GlobalSignal<std::collections::HashSet<String>> =
    Signal::global(std::collections::HashSet::new);

/// Drain `PENDING_TAB_REOPEN`, subscribing the caller to it. Same idiom as
/// `drain_cancel_stream_requests`: the read (not the write) is what makes a
/// later insert re-run the subscribed effect.
fn drain_pending_tab_reopens() -> Vec<String> {
    if PENDING_TAB_REOPEN.read().is_empty() {
        return Vec::new();
    }
    std::mem::take(&mut *PENDING_TAB_REOPEN.write())
        .into_iter()
        .collect()
}

/// One entry in the visible tab strip, in `tab_bar.rs`'s left-to-right render
/// order: open chat tabs (by index into `open_tabs`), then Planner, then
/// Fleet — the latter two only present when their `_tab_open` flag is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripItem {
    Chat(usize),
    Planner,
    Fleet,
}

/// Builds the ordered strip for `NextTab`/`PrevTab` cycling. Pure so it's
/// testable without Dioxus context; mirrors `TabBar`'s render order (chat
/// tabs loop, then the `planner_tab_open` block, then `fleet_tab_open`).
fn build_tab_strip(open_tabs_len: usize, planner_tab_open: bool, fleet_tab_open: bool) -> Vec<StripItem> {
    let mut strip: Vec<StripItem> = (0..open_tabs_len).map(StripItem::Chat).collect();
    if planner_tab_open {
        strip.push(StripItem::Planner);
    }
    if fleet_tab_open {
        strip.push(StripItem::Fleet);
    }
    strip
}

/// Next/previous index into a slice of length `len`, wrapping at both ends.
/// Callers must guard `len == 0` themselves (undefined here).
fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

/// The sidebar's visible navigation order, built from live state the same
/// way `TabSidebar` builds its rows (same facts, same sanitised lens), so the
/// shortcuts walk what is on screen. Returns the stops and whether the list
/// is grouped (grouping other than None).
fn sidebar_nav_stops(
    open_tabs: &[String],
    state: &session::SessionState,
    planner: &todo::PlannerState,
    settings: &settings::Settings,
    stored_order: tab_list::TabListOrder,
    planner_tab_open: bool,
    fleet_tab_open: bool,
) -> (Vec<tab_list::NavStop>, bool) {
    let order = stored_order.sanitized(
        &tab_list::TabSortKey::ALL,
        &tab_list::tab_group_options(settings.planner_enabled, settings.composio_profiles.len()),
    );
    let facts: Vec<tab_list::TabFacts> = open_tabs
        .iter()
        .enumerate()
        .map(|(idx, id)| {
            let s = state.sessions.get(id);
            let last_updated = s.map(|s| s.last_updated).unwrap_or_else(chrono::Utc::now);
            tab_list::TabFacts {
                idx,
                title: s.map(|s| s.display_title()).unwrap_or_default(),
                last_updated,
                started: s
                    .and_then(|s| s.messages.first())
                    .map(|m| m.created_at)
                    .unwrap_or(last_updated),
                project: s.and_then(|s| s.project_id.as_ref()).and_then(|pid| {
                    planner
                        .projects
                        .iter()
                        .find(|p| &p.id == pid)
                        .map(|p| (p.sort_order, p.id.clone(), p.title.clone()))
                }),
                profile: s.and_then(|s| {
                    settings.resolve_session_profile_display_name(s.composio_profile.as_deref())
                }),
            }
        })
        .collect();
    let groups = tab_list::arrange_tabs(&facts, order);
    (
        tab_list::nav_order(&groups, planner_tab_open, fleet_tab_open),
        order.group != tab_list::TabGroupBy::None,
    )
}

/// Snapshot for the Tidy tabs sheet: the open chats, the planner's to-dos
/// reduced to what tidying reads, and the projects a tab can be filed into
/// (open ones, A-Z, as the chat-bar chip lists them).
fn tidy_inputs(
    open_tabs: &[String],
    state: &session::SessionState,
    planner: &todo::PlannerState,
    streaming: &std::collections::HashSet<String>,
) -> (Vec<tab_tidy::TidyTab>, Vec<tab_tidy::TidyTodo>, Vec<tab_tidy::TidyProject>) {
    use tab_tidy::TidyTodoStatus;
    let tabs = open_tabs
        .iter()
        .filter_map(|id| {
            let s = state.sessions.get(id)?;
            Some(tab_tidy::TidyTab {
                session_id: id.clone(),
                title: s.display_title(),
                topic: s.tab_topic(),
                emoji: s.tab_emoji.clone(),
                last_updated: s.last_updated,
                project_id: s.project_id.clone(),
                is_active: state.active_session_id == *id,
                is_streaming: streaming.contains(id),
                has_pending_timer: s
                    .scheduled_timers
                    .iter()
                    .any(|t| t.status == timers::TimerStatus::Pending),
                pinned: false,
            })
        })
        .collect();
    let todos = planner
        .todos
        .iter()
        .map(|t| tab_tidy::TidyTodo {
            linked_session: t.linked_fleet_session.clone(),
            status: match t.status {
                todo::model::TodoStatus::Completed => TidyTodoStatus::Done,
                todo::model::TodoStatus::Cancelled => TidyTodoStatus::Cancelled,
                _ => TidyTodoStatus::Open,
            },
            done_at: t.completed_at.unwrap_or(t.updated_at),
            project_id: t.project_id.clone(),
        })
        .collect();
    let mut projects: Vec<tab_tidy::TidyProject> = planner
        .projects
        .iter()
        .filter(|p| !p.status.is_closed())
        .map(|p| tab_tidy::TidyProject {
            id: p.id.clone(),
            title: p.title.clone(),
            aliases: p.aliases.clone(),
        })
        .collect();
    projects.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    (tabs, todos, projects)
}

#[cfg(test)]
mod tab_cycle_tests {
    use super::*;

    #[test]
    fn cycle_index_wraps_forward() {
        assert_eq!(cycle_index(0, 3, true), 1);
        assert_eq!(cycle_index(2, 3, true), 0);
    }

    #[test]
    fn cycle_index_wraps_backward() {
        assert_eq!(cycle_index(0, 3, false), 2);
        assert_eq!(cycle_index(1, 3, false), 0);
    }

    #[test]
    fn cycle_index_single_item_is_noop_position() {
        // Callers guard len<=1 before calling, but the arithmetic itself
        // still holds for len==1 (stays at 0).
        assert_eq!(cycle_index(0, 1, true), 0);
        assert_eq!(cycle_index(0, 1, false), 0);
    }

    #[test]
    fn build_tab_strip_orders_chat_then_planner_then_fleet() {
        let strip = build_tab_strip(2, true, true);
        assert_eq!(
            strip,
            vec![
                StripItem::Chat(0),
                StripItem::Chat(1),
                StripItem::Planner,
                StripItem::Fleet,
            ]
        );
    }

    #[test]
    fn build_tab_strip_omits_closed_tabs() {
        assert_eq!(build_tab_strip(1, false, false), vec![StripItem::Chat(0)]);
        assert_eq!(build_tab_strip(0, true, false), vec![StripItem::Planner]);
        assert_eq!(build_tab_strip(0, false, true), vec![StripItem::Fleet]);
    }

    #[test]
    fn build_tab_strip_len_le_1_is_the_noop_case() {
        assert!(build_tab_strip(1, false, false).len() <= 1);
        assert!(build_tab_strip(0, false, false).len() <= 1);
    }
}

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

/// Keeps the non-blocking file writer alive. Dropped by `flush_logs()` on the
/// quit path so the last buffered lines reach disk.
static LOG_FILE_GUARD: std::sync::OnceLock<std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>> =
    std::sync::OnceLock::new();

/// Flush and close the log file writer. Called on quit; later log lines are
/// dropped by the (now closed) channel, which is fine at that point.
fn flush_logs() {
    if let Some(slot) = LOG_FILE_GUARD.get() {
        if let Ok(mut guard) = slot.lock() {
            drop(guard.take());
        }
    }
}

/// Log directory: `<config dir>/com.hobbes.app/logs/` on macOS/Linux
/// (`~/Library/Application Support/com.hobbes.app/logs`), and
/// `%LOCALAPPDATA%\com.hobbes.app\logs` on Windows.
fn log_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let base = dirs::data_local_dir();
    #[cfg(not(target_os = "windows"))]
    let base = dirs::config_dir();
    base.map(|b| b.join("com.hobbes.app").join("logs"))
}

/// Daily-rotated `hobbes.<date>.log`, 7 files kept, written on a lossy
/// background thread. Returns `None` (no file layer) if the directory can't
/// be created or opened — logging must never take the app down.
fn build_file_writer() -> Option<(tracing_appender::non_blocking::NonBlocking, std::path::PathBuf)> {
    use tracing_appender::rolling::{Builder, Rotation};
    let dir = log_dir()?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("hobbes")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(8_192)
        .thread_name("hobbes-log")
        .finish(appender);
    let _ = LOG_FILE_GUARD.set(std::sync::Mutex::new(Some(guard)));
    Some((writer, dir))
}

/// Route panics through tracing so they land in the log file, then run the
/// previous (default) hook so stderr output is unchanged.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        tracing::error!(
            "PANIC on thread '{}' at {}: {}",
            std::thread::current().name().unwrap_or("<unnamed>"),
            location,
            msg
        );
        previous(info);
    }));
}

/// Shared setup for both build types. `default_filter` applies to the file
/// (and to stderr in release); `HOBBES_LOG` overrides it.
fn init_logger_with(default_filter: &str, build_type: &str, stderr: Option<tracing_subscriber::filter::LevelFilter>) {
    use std::io::IsTerminal;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let (filter_str, filter_warning) =
        log_utils::choose_log_filter(std::env::var("HOBBES_LOG").ok().as_deref(), default_filter);
    let make_filter = || {
        tracing_subscriber::EnvFilter::builder()
            .parse(&filter_str)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter))
    };

    let file = build_file_writer();
    let file_dir = file.as_ref().map(|(_, d)| d.display().to_string());
    let file_layer = file.map(|(writer, _)| {
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(make_filter())
    });

    // Release: stderr only when explicitly asked for or attached to a
    // terminal (a Finder-launched app's stderr goes nowhere useful).
    // Debug: always, at the given level.
    let want_stderr = match stderr {
        Some(_) => true,
        None => {
            std::env::var("HOBBES_LOG_STDERR").map(|v| v == "1").unwrap_or(false)
                || std::io::stderr().is_terminal()
        }
    };
    let stderr_layer = want_stderr.then(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal());
        match stderr {
            Some(level) => layer.with_filter(level).boxed(),
            None => layer.with_filter(make_filter()).boxed(),
        }
    });

    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    install_panic_hook();

    if let Some(w) = filter_warning {
        tracing::warn!("{}", w);
    }
    tracing::info!(
        "Hobbes {} ({} build) logging started; filter = \"{}\"; log dir = {}",
        env!("CARGO_PKG_VERSION"),
        build_type,
        filter_str,
        file_dir.as_deref().unwrap_or("<none: file logging unavailable>")
    );
}

/// Debug builds (cargo run / dx serve): DEBUG+ for this crate to the log
/// file, INFO+ to stderr.
#[cfg(debug_assertions)]
fn init_logger() {
    init_logger_with(
        &log_utils::default_debug_filter(),
        "debug",
        Some(tracing_subscriber::filter::LevelFilter::INFO),
    );
}

/// Release (Pro) builds: INFO+ for this crate, WARN+ for dependencies, to the
/// rolling log file; stderr only with `HOBBES_LOG_STDERR=1` or a terminal.
#[cfg(not(debug_assertions))]
fn init_logger() {
    init_logger_with(&log_utils::default_release_filter(), "release", None);
}

fn main() {
    // Try to load .env file for developer convenience.
    dotenv().ok();
    tracing::debug!("Attempted to load .env file from the environment.");
    // Logger setup — cargo run (debug) vs built app (release):
    // Debug: DEBUG+ (this crate) to log file, INFO+ to stderr
    // Release: INFO+ (this crate) / WARN+ (deps) to a rolling file in
    // <config>/com.hobbes.app/logs; stderr only if HOBBES_LOG_STDERR=1 or a tty.
    // HOBBES_LOG overrides the filter in both.
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
                    // Mermaid diagrams: the 5MB+ bundle is NOT parsed here.
                    // It's loaded on demand from Rust (see
                    // `components::mermaid::ensure_mermaid_loaded`) the first
                    // time a chat actually contains a diagram, so it costs
                    // nothing at startup for the (common) case where it
                    // never renders.
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
        stream_manager::{StreamManager, StreamManagerContext},
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
    Fleet,
    QuickCapture,
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
    let mut usage_log_sig = use_context_provider(|| Signal::new(usage_log::UsageLog::load()));
    // Cache keep-alive pings run on background tasks; their usage entries
    // come back over a channel because a Signal can't be written from there.
    use_hook(move || {
        use llm::claude::keepalive;
        keepalive::seed_spend(
            usage_log_sig
                .read()
                .keepalive_spend_on_day(chrono::Local::now()),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        keepalive::install_sink(tx);
        spawn(async move {
            while let Some(entry) = rx.recv().await {
                usage_log_sig.write().record(entry);
            }
        });
    });

    let skill_registry = use_context_provider(|| Signal::new(skills::SkillRegistry::new()));
    let mut skills_loaded = use_signal(|| false);

    // The planner is hydrated in full at startup — its rows are small and
    // bounded by human effort, so there is no lazy-loading path to justify.
    // Safe here: session_store::init() ran in main() before the app launched.
    let mut planner_state = use_context_provider(|| Signal::new(todo::PlannerState::load()));
    // Which list the Planner is showing, for surfaces mounted outside it
    // (the ⌘K palette pre-seeds its project from this).
    use_context_provider(|| {
        components::planner_view::PlannerFocusHint(Signal::new(None))
    });

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

                // Dispatched runs that finished hand their result back to the
                // chat that launched them. The fleet sweep (a plain tokio
                // task) can't touch Signals, so it parks the continuations on
                // FleetShared and pokes us; this is the only side that writes
                // the chat queue.
                for c in shared.take_continuations() {
                    let todo_title = c
                        .todo_id
                        .as_deref()
                        .and_then(|id| planner_state.peek().todo(id).map(|t| t.title.clone()))
                        .unwrap_or_else(|| c.session_name.clone());
                    let fallback = c.todo_id.as_deref().and_then(|id| {
                        planner_state.peek().todo(id).and_then(|t| t.latest_progress.clone())
                    });
                    let text = fleet::continuation_text(
                        &todo_title,
                        &c.session_name,
                        c.progress.as_deref().or(fallback.as_deref()),
                    );
                    // The drain refuses to pop for a session that isn't in
                    // state (it would eat the message), so a parent whose tab
                    // was closed is hydrated back into memory first. Focus is
                    // never stolen — the turn runs in the background and the
                    // toast is the only interruption.
                    if !session_state.peek().sessions.contains_key(&c.parent_session_id) {
                        match crate::session_store::load_session(&c.parent_session_id) {
                            Some(session) => {
                                session_state
                                    .write()
                                    .sessions
                                    .insert(c.parent_session_id.clone(), session);
                            }
                            None => {
                                tracing::warn!(
                                    "dispatch continuation: parent session {} is gone; dropping",
                                    c.parent_session_id
                                );
                                continue;
                            }
                        }
                    }
                    crate::components::chat_queue::queue_push(
                        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
                        &c.parent_session_id,
                        crate::components::chat_queue::QueuedMessage::new(text, Vec::new()),
                    );
                    crate::components::chat_queue::request_drain();
                    // The parent's tab may have been closed while the run was
                    // in flight (or was never opened for a `claude_code`
                    // dispatch from a planner row) — the resumed turn needs a
                    // visible tab to show its activity in. `open_tabs` isn't
                    // reachable from this task (see `PENDING_TAB_REOPEN`), so
                    // park the id for the ChatCommand handler to reopen.
                    PENDING_TAB_REOPEN
                        .write()
                        .insert(c.parent_session_id.clone());
                    let parent_tab_name = session_state
                        .peek()
                        .sessions
                        .get(&c.parent_session_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| c.session_name.clone());
                    flash_timer_toast(format!(
                        "🛰 {todo_title} finished — continuing the chat in \"{parent_tab_name}\""
                    ));
                }
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
                // Hobbes tabs report through the bridge under the same
                // condition as the listener (which also keeps the staleness
                // sweep running whenever Hobbes rows are live).
                fleet::bridge::set_enabled(want);
                fleet::set_retire_minutes(settings.peek().fleet_retire_minutes);
                fleet::set_dispatch_auto_continue(settings.peek().dispatch_auto_continue);
                if want && server.is_none() {
                    if !hydrated {
                        // Sessions left live by a previous process re-enter
                        // the map; the sweep and span caps keep them honest,
                        // and reconciliation corrects anything that changed
                        // on disk while Hobbes wasn't listening.
                        shared.hydrate_from_store();
                        let shared_for_reconcile = shared.clone();
                        tokio::task::spawn_blocking(move || {
                            fleet::reconcile::reconcile_all(&shared_for_reconcile);
                        });
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
                let links = crate::todo::handlers::linked_session_pairs(&planner_state.peek());
                let due = fleet::briefs::collect_due(&live, &ended, &links, chrono::Utc::now());
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

                let session_title = digest.title.clone();
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
                        if !shared.set_brief(
                            &task.session_id,
                            brief.clone(),
                            started_at,
                            session_title.clone(),
                        ) {
                            // Ended (or just-ended) row: merge into the store.
                            let brief_for_merge = brief.clone();
                            let merged =
                                fleet::store::merge_session(&task.session_id, move |row| {
                                    row.brief = Some(brief_for_merge);
                                    if let Some(t) = session_title {
                                        row.session_title = Some(t);
                                    }
                                    if row.brief_dirty_at.is_none_or(|d| d <= started_at) {
                                        row.brief_dirty_at = None;
                                    }
                                });
                            if merged {
                                shared.poke();
                            }
                        }
                        // Progress flows onto linked todos (display-only —
                        // never status; progress is reported, not judged).
                        let line = fleet::briefs::progress_line(&brief);
                        let changed = crate::todo::handlers::apply_brief_progress(
                            &mut planner_state.write(),
                            &task.session_id,
                            &line,
                            chrono::Utc::now(),
                        );
                        if !changed.is_empty() {
                            let p = planner_state.peek();
                            for id in &changed {
                                if let Some(t) = p.todo(id) {
                                    if let Err(e) = crate::todo::store::save_todo(t) {
                                        tracing::error!("progress persist failed: {e}");
                                    }
                                }
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
        let mut prev_profile_signature: Signal<Option<log_utils::ProfileSignature>> = use_signal(|| None);

        use_effect(move || {
            // Don't reinitialize before secrets are loaded — the profile's api_key
            // won't be hydrated yet, causing a false "No API key" error.
            if !secrets_loaded() {
                return;
            }

            // Create a signature of the active profile properties we care about.
            // Holds the API key, so it is compared only — never logged
            // (`describe_profile_change` prints field names, not values).
            let current_signature: Option<log_utils::ProfileSignature> =
                settings.read().get_active_profile().map(|p| {
                    [
                        p.name.clone(),
                        p.api_key.clone().unwrap_or_default(),
                        p.base_url.clone().unwrap_or_default(),
                        p.entity_id.clone().unwrap_or_default(),
                        p.user_id.clone().unwrap_or_default(),
                    ]
                });

            let previous = prev_profile_signature.peek().clone();

            // Only reinitialize if the profile signature actually changed
            if current_signature != previous {
                tracing::info!(
                    "Active Composio profile properties changed, reinitializing client: {}",
                    log_utils::describe_profile_change(previous.as_ref(), current_signature.as_ref())
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
    let mut show_quick_capture = use_signal(|| false);
    // The planner is its own tab in the tab bar, not a sidebar: `open` is
    // whether the tab exists, `active` whether it (vs. the selected session's
    // chat) fills the main column. Selecting any session tab deactivates the
    // planner without closing its tab.
    // App-view tabs (planner/fleet) restore from UiState so they survive a
    // restart — including which one was focused at exit.
    let mut planner_tab_open = use_signal(|| ui_state.peek().planner_tab_open);
    let mut planner_active = use_signal(|| ui_state.peek().planner_active);
    let mut fleet_tab_open = use_signal(|| ui_state.peek().fleet_tab_open);
    let mut fleet_active = use_signal(|| {
        // Never restore both views active; planner wins the tie.
        ui_state.peek().fleet_active && !ui_state.peek().planner_active
    });
    // Tabs as a left sidebar list instead of the top strip — a display
    // preference toggled from Settings at runtime. Memoised so the frequent
    // unrelated `ui_state` writes (tab persistence, widths) don't re-render
    // the whole app; only a flip of this flag does.
    let tabs_as_sidebar = use_memo(move || ui_state.read().tabs_as_sidebar);
    // The user's requested Planner column widths. Shared with PlannerView
    // (which edits them) and the layout solver below; automatic collapsing
    // never writes them.
    let planner_widths = use_context_provider(|| components::planner_view::PlannerWidths {
        nav: Signal::new(ui_state.peek().planner_left_width),
        timeline: Signal::new(ui_state.peek().planner_today_width),
    });
    let planner_focus_hint = use_context::<components::planner_view::PlannerFocusHint>().0;
    // Drawer (History/Settings/MCP) and tab-sidebar widths as the user asked
    // for them. The layout solver below decides what actually fits; it
    // never writes back here, so saved widths return when space does.
    let mut settings_panel_width = use_signal(|| ui_state.peek().settings_panel_width);
    let mut tab_sidebar_width = use_signal(|| ui_state.peek().tab_sidebar_width);
    let mut tab_sidebar_railed = use_signal(|| ui_state.peek().tab_sidebar_railed);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    // Window logical width for the layout solver (updated on resize).
    let mut window_logical_width = use_signal(|| {
        window
            .inner_size()
            .to_logical::<f64>(window.scale_factor())
            .width
    });
    let mut tray_icon = use_signal::<Option<TrayIcon>>(|| None);
    let mut show_confirm_modal = use_context_provider(|| Signal::new(false));
    let mut show_tidy = use_signal(|| false);
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

    // ── Agent-time budget tick ───────────────────────────────────────────────
    // While a lease is held, re-check today's minutes and spend every 30 s and
    // stop the run when it is over (usage events re-check sooner). Agent time
    // only ever starts through ChatCommand::AgentTimeStart: the person's
    // Start, or the grant's scheduled window, which this tick fires at most
    // once a day (agent_time_runtime::scheduled_start_now).
    {
        let planner_state = planner_state;
        let session_state = session_state;
        let mut chat_command = chat_command;
        use_future(move || async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(
                    agent_time_runtime::TICK_SECS,
                ))
                .await;
                let streaming = STREAMING_SESSIONS.peek().clone();
                agent_time_runtime::tick(planner_state, session_state, &streaming);
                let planner_enabled = settings.peek().planner_enabled;
                let due = agent_time_runtime::scheduled_start_now(&planner_state.peek(), planner_enabled);
                if due {
                    chat_command.set(Some(ChatCommand::AgentTimeStart));
                }
            }
        });
    }

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
        ui.planner_tab_open = *planner_tab_open.read();
        ui.planner_active = *planner_active.read();
        ui.fleet_tab_open = *fleet_tab_open.read();
        ui.fleet_active = *fleet_active.read();

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
        // Selecting a chat tab brings its chat forward; the planner/fleet
        // tabs (if open) stay in the bar, just no longer active.
        planner_active.set(false);
        fleet_active.set(false);
        let tabs = open_tabs.read();
        if idx < tabs.len() {
            let session_id = tabs[idx].clone();
            active_tab_index.set(idx);
            current_session_id.set(session_id.clone());
            session_state.write().active_session_id = session_id.clone();
            sync_profile_from_session(&session_id);
        }
    };

    // Next/Prev Tab cycling: builds the visible tab strip in tab_bar.rs's
    // left-to-right render order (chat tabs, then Planner, then Fleet),
    // finds where we currently are in it, and moves with wraparound. Landing
    // on a chat item reuses `switch_tab_fn` rather than only poking
    // `active_tab_index`, so it gets whatever else that path does.
    //
    // In sidebar mode the walk follows what the sidebar shows instead (see
    // `sidebar_nav_stops`): Planner, Fleet, then chats in arranged order.
    // `by_group` jumps to the first tab of the next/previous group.
    let mut cycle_tab_fn = move |forward: bool, by_group: bool| {
        if *tabs_as_sidebar.peek() {
            let (nav, grouped) = sidebar_nav_stops(
                &open_tabs.peek(),
                &session_state.peek(),
                &planner_state.peek(),
                &settings.peek(),
                ui_state.peek().tab_sidebar_order,
                *planner_tab_open.peek(),
                *fleet_tab_open.peek(),
            );
            let current = if *planner_active.peek() {
                tab_list::NavTarget::Planner
            } else if *fleet_active.peek() {
                tab_list::NavTarget::Fleet
            } else {
                tab_list::NavTarget::Chat(*active_tab_index.peek())
            };
            let next = if by_group {
                tab_list::nav_step_group(&nav, current, forward, grouped)
            } else {
                tab_list::nav_step(&nav, current, forward)
            };
            match next {
                Some(tab_list::NavTarget::Chat(idx)) => switch_tab_fn(idx),
                Some(tab_list::NavTarget::Planner) => {
                    planner_active.set(true);
                    fleet_active.set(false);
                }
                Some(tab_list::NavTarget::Fleet) => {
                    fleet_active.set(true);
                    planner_active.set(false);
                }
                None => {}
            }
            return;
        }
        let strip = build_tab_strip(
            open_tabs.peek().len(),
            *planner_tab_open.peek(),
            *fleet_tab_open.peek(),
        );
        if strip.len() <= 1 {
            return;
        }
        let current = if *planner_active.peek() {
            strip.iter().position(|i| *i == StripItem::Planner)
        } else if *fleet_active.peek() {
            strip.iter().position(|i| *i == StripItem::Fleet)
        } else {
            let idx = *active_tab_index.peek();
            strip.iter().position(|i| *i == StripItem::Chat(idx))
        }
        .unwrap_or(0);
        let next = cycle_index(current, strip.len(), forward);
        match strip[next] {
            StripItem::Chat(idx) => switch_tab_fn(idx),
            StripItem::Planner => {
                planner_active.set(true);
                fleet_active.set(false);
            }
            StripItem::Fleet => {
                fleet_active.set(true);
                planner_active.set(false);
            }
        }
    };

    let mut delete_session_fn = move |id_to_delete: String| {
        // A stream still writing to this session would otherwise keep
        // burning tokens/API calls for output that's about to be discarded
        // (the session itself is going away). Request StreamBridge cancel
        // it — StreamManagerContext isn't reachable from this root-scope
        // closure, see STREAMING_SESSIONS/CANCEL_STREAM_REQUEST above.
        // This is best-effort/racy with the delete below by design: even if
        // the stream keeps running a moment longer, the session is removed
        // from `state.sessions` immediately, so further writes are no-ops,
        // and the session_events tombstone (session_store::delete_session,
        // enforced in write_event_rows) stops its journal from growing.
        if STREAMING_SESSIONS.peek().contains(&id_to_delete) {
            CANCEL_STREAM_REQUEST.write().insert(id_to_delete.clone());
        }

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

            // No cache keep-alive pings for a tab that is no longer open.
            llm::claude::keepalive::cancel_session(&closing_session_id);

            // The fleet row (if the bridge is armed) leaves the live map,
            // taking a closing brief from the conversation summary with it.
            let closing_summary = session_state
                .peek()
                .sessions
                .get(&closing_session_id)
                .map(|s| &s.active_context.conversation_summary)
                .filter(|cs| {
                    !cs.summary.trim().is_empty() || !cs.current_task.trim().is_empty()
                })
                .and_then(|cs| serde_json::to_value(cs).ok());
            fleet::bridge::report_with_brief(
                &closing_session_id,
                "",
                fleet::bridge::HobbesSignal::Closed,
                closing_summary,
            );

            // A closed tab's terminal shell dies with it — unless a stream
            // is still writing into this session. Per stream_manager.rs
            // (~309-312), a closed tab intentionally keeps its background
            // stream running (the session survives on disk, closing is not
            // deleting), and that stream's tool calls may still be using the
            // terminal. Killing the pty out from under a running turn would
            // break it, so skip the kill here and let it happen on a later
            // close of the same session once it's no longer streaming.
            if STREAMING_SESSIONS.peek().contains(&closing_session_id) {
                tracing::info!(
                    session_id = %closing_session_id,
                    "close_tab: session still streaming, deferring terminal kill."
                );
            } else {
                let mgr = mcp_manager.peek().clone();
                let sid = closing_session_id.clone();
                spawn(async move {
                    mgr.kill_terminal_session(&sid).await;
                });
            }

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
        fleet_active.set(false);
        let new_id = session_state.write().create_session(active_profile_id());
        let mut tabs = open_tabs.read().clone();
        tabs.push(new_id.clone());
        open_tabs.set(tabs.clone());
        active_tab_index.set(tabs.len() - 1);
        session_state.write().active_session_id = new_id.clone();
        current_session_id.set(new_id);
    };

    // Tidy tabs, confirmed: file the chosen tabs (the chat-bar chip's path:
    // set the tag, mark it user-decided, journal it, one P-009 save), then
    // close the chosen tabs through `close_tab_fn` (sessions stay in
    // History). Closing goes highest index first so earlier indices stay
    // valid; `close_tab_fn` doesn't follow the active tab when a tab to its
    // left goes, so the active chat is re-pinned afterwards.
    let apply_tidy_fn = move |apply: components::tab_tidy::TidyApply| {
        if !apply.tag.is_empty() {
            for (sid, pid) in &apply.tag {
                {
                    let mut state = session_state.write();
                    if let Some(s) = state.sessions.get_mut(sid) {
                        s.project_id = Some(pid.clone());
                        s.project_tag_user_set = true;
                    }
                }
                crate::session_events::log_event(
                    sid,
                    crate::session_events::SessionEvent::ProjectTagged {
                        project_id: Some(pid.clone()),
                        user_set: true,
                    },
                );
            }
            SessionState::save_signal(&session_state, None);
        }
        if !apply.close.is_empty() {
            let keep_id = current_session_id.peek().clone();
            let mut idxs: Vec<usize> = open_tabs
                .peek()
                .iter()
                .enumerate()
                .filter(|(_, id)| apply.close.contains(id) && **id != keep_id)
                .map(|(i, _)| i)
                .collect();
            idxs.sort_unstable_by(|a, b| b.cmp(a));
            for idx in idxs {
                close_tab_fn(idx);
            }
            let pos = open_tabs.peek().iter().position(|id| *id == keep_id);
            if let Some(pos) = pos {
                active_tab_index.set(pos);
                current_session_id.set(keep_id.clone());
                session_state.write().active_session_id = keep_id.clone();
                sync_profile_from_session(&keep_id);
            }
        }
        show_tidy.set(false);
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
    let window_for_resize = window.clone();
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event, .. } = event {
            if let WindowEvent::Resized(new_size) = event {
                last_known_size.set(*new_size);
                let logical = new_size
                    .to_logical::<f64>(window_for_resize.scale_factor())
                    .width;
                if (*window_logical_width.peek() - logical).abs() >= 0.5 {
                    window_logical_width.set(logical);
                }
            }
        }
    });

    // What gives way when the window is too narrow for the requested widths
    // (layout_budget::solve_chat). A memo over signals only, so the app
    // re-renders when the *outcome* changes, not on every resize event.
    // The Planner has its own give-way policy (layout_budget::solve_planner);
    // it only applies while the Planner is the visible view.
    let planner_layout = use_memo(move || {
        let drawer_open = *show_session_manager.read()
            || *show_settings_panel.read()
            || *show_mcp_manager.read();
        // The timeline only exists on Today (the default before the Planner
        // has mounted and reported its selection).
        let show_timeline = planner_focus_hint.read().as_ref().is_none_or(|sel| {
            *sel == components::planner_view::PlannerSelection::View(todo::views::TodoView::Today)
        });
        layout_budget::solve_planner(&layout_budget::PlannerLayoutInput {
            window_width: *window_logical_width.read(),
            sidebar: tabs_as_sidebar().then(|| layout_budget::SidebarRequest {
                width: *tab_sidebar_width.read(),
                user_railed: *tab_sidebar_railed.read(),
            }),
            drawer_width: drawer_open.then(|| *settings_panel_width.read()),
            nav_width: *planner_widths.nav.read(),
            show_timeline,
            timeline_width: *planner_widths.timeline.read(),
        })
    });
    let chat_layout = use_memo(move || {
        if *planner_active.read() {
            let p = planner_layout.read();
            return layout_budget::ChatLayout {
                sidebar_mode: p.sidebar_mode,
                sidebar_width: p.sidebar_width,
                drawer_width: p.drawer_width,
                drawer_nav_railed: p.drawer_nav_railed,
                main_width: p.main_width,
            };
        }
        let drawer_open = *show_session_manager.read()
            || *show_settings_panel.read()
            || *show_mcp_manager.read();
        layout_budget::solve_chat(&layout_budget::ChatLayoutInput {
            window_width: *window_logical_width.read(),
            sidebar: tabs_as_sidebar().then(|| layout_budget::SidebarRequest {
                width: *tab_sidebar_width.read(),
                user_railed: *tab_sidebar_railed.read(),
            }),
            drawer: drawer_open.then(|| layout_budget::DrawerRequest {
                width: *settings_panel_width.read(),
                has_inner_nav: *show_settings_panel.read(),
            }),
        })
    });

    let menu_handler = use_coroutine(move |mut rx: UnboundedReceiver<MenuAction>| async move {
        while let Some(action) = rx.next().await {
            tracing::info!("MenuHandler received action: {:?}", action);
            match action {
                MenuAction::Quit => {
                    llm::claude::keepalive::shutdown();
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
                MenuAction::Fleet => {
                    tracing::info!("MenuAction::Fleet triggered");
                    chat_command.set(Some(ChatCommand::ToggleFleet));
                }
                MenuAction::QuickCapture => {
                    chat_command.set(Some(ChatCommand::ToggleQuickCapture));
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
                        "view_fleet" => Some(MenuAction::Fleet),
                        "view_quick_capture" => Some(MenuAction::QuickCapture),
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
        focus_tray::sync_tray(
            &mut tray_icon.write(),
            show_tray,
            snapshot.as_ref(),
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
            // Flush any in-flight async saves synchronously before the
            // window (and process) goes away — `save_async` hands rows off
            // to a background blocking task, and nothing guarantees that
            // task runs to completion once `window.close()` tears the
            // runtime down. `SessionState::save()` / `SettingsManager::save()`
            // reuse the same machinery, just inline on this thread. The
            // session save is cheap when nothing changed (per-row
            // fingerprints skip clean sessions); the settings save always
            // rewrites settings.json and its .bak, changed or not.
            if let Err(e) = session_state.peek().save() {
                tracing::error!("Final session save on quit failed: {}", e);
            }
            if let Err(e) = settings_manager.peek().save(&settings.peek()) {
                tracing::error!("Final settings save on quit failed: {}", e);
            }
            tracing::info!("Quitting Hobbes");
            flush_logs();
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

    // Debounced window resize saver
    // This effect runs whenever last_known_size changes (due to resize events)
    let window_resize_handler = window.clone();
    use_effect(move || {
        let physical_size = last_known_size.read();
        if physical_size.width > 0 && physical_size.height > 0 {
            let mut session_state = session_state;

            // Capture the current values we need for calculation
            let current_physical_size = *physical_size;
            let window = window_resize_handler.clone();

            spawn(async move {
                // Wait for 1 second of inactivity before saving
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // The last one to run will win and set the correct final size.

                let scale_factor = window.scale_factor();
                let logical_size = current_physical_size.to_logical::<f64>(scale_factor);

                // Subtract the open drawer at the width it actually takes
                // (the solver may have shrunk it). The tab sidebar is
                // deliberately not counted: unlike the drawers it is still
                // showing at the next launch, so it's part of the window's
                // normal content width.
                let drawer_width = chat_layout.peek().drawer_width.unwrap_or(0.0);
                let content_width = logical_size.width - drawer_width;

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

        // Reopen tabs for continuations that landed while the parent was
        // closed (or never opened, for a `claude_code` dispatch fired from
        // a planner row) — see `PENDING_TAB_REOPEN`. Unconditional so this
        // effect also re-runs whenever the global gains a new id, not only
        // when `chat_command` itself changes.
        let reopen_ids = drain_pending_tab_reopens();
        if !reopen_ids.is_empty() {
            let mut tabs = open_tabs.read().clone();
            let mut changed = false;
            for id in reopen_ids {
                if !tabs.contains(&id) {
                    tabs.push(id);
                    changed = true;
                }
            }
            if changed {
                open_tabs.set(tabs);
            }
        }

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
                    // Sidebar mode: Cmd+N is the Nth *chat* as the sidebar
                    // lists it (Planner/Fleet are not counted, as in the
                    // strip today). Out of range does nothing.
                    if *tabs_as_sidebar.peek() {
                        let (nav, _) = sidebar_nav_stops(
                            &open_tabs.peek(),
                            &session_state.peek(),
                            &planner_state.peek(),
                            &settings.peek(),
                            ui_state.peek().tab_sidebar_order,
                            false,
                            false,
                        );
                        if let Some(real) = tab_list::nav_chat_at(&nav, idx) {
                            switch_tab_fn(real);
                        }
                    } else {
                        switch_tab_fn(idx);
                    }
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
                ChatCommand::DispatchTodoToAgent(todo_id) => {
                    // A row dispatch gets a review chat: mirrors
                    // StartTodoInChat/new_tab_fn's tab creation, but with no
                    // seeded draft — Hobbes announces the hand-off itself —
                    // and named/tagged after the todo before it's shown.
                    let todo_snapshot = planner_state.peek().todo(&todo_id).cloned();
                    if let Some(todo) = todo_snapshot {
                        let title = todo.title.clone();

                        planner_active.set(false);
                        fleet_active.set(false);
                        let new_id = session_state.write().create_session(active_profile_id());

                        let session_name = crate::fleet::truncate_summary(&title, 60);
                        session_state.write().set_session_title_programmatic(
                            &new_id,
                            Some(session_name),
                            Some("🐯".to_string()),
                        );

                        {
                            let mut state = session_state.write();
                            if let Some(s) = state.sessions.get_mut(&new_id) {
                                s.project_id = todo.project_id.clone();
                                s.project_tag_user_set = true;
                            }
                        }
                        crate::session_events::log_event(
                            &new_id,
                            crate::session_events::SessionEvent::ProjectTagged {
                                project_id: todo.project_id.clone(),
                                user_set: true,
                            },
                        );

                        let intro_msg = crate::components::chat::Message {
                            id: uuid::Uuid::new_v4(),
                            author: "Hobbes".to_string(),
                            content: crate::components::shared::MessageContent::Text {
                                content: crate::fleet::dispatch_intro_text(&title),
                                thought_signature: None,
                                thought_summary: None,
                                signature_provider: None,
                            },
                            attachments: Vec::new(),
                            comments: Vec::new(),
                            created_at: chrono::Utc::now(),
                            usage: None,
                        };
                        {
                            let mut state = session_state.write();
                            if let Some(s) = state.sessions.get_mut(&new_id) {
                                s.messages.push(intro_msg.clone());
                            }
                        }
                        crate::session_events::log_event(
                            &new_id,
                            crate::session_events::SessionEvent::AssistantMessage {
                                message: intro_msg,
                            },
                        );

                        // Open + focus the tab (new_tab_fn's own sequence).
                        let mut tabs = open_tabs.read().clone();
                        tabs.push(new_id.clone());
                        open_tabs.set(tabs.clone());
                        active_tab_index.set(tabs.len() - 1);
                        session_state.write().active_session_id = new_id.clone();
                        current_session_id.set(new_id.clone());

                        let dispatch_ctx = crate::components::builtin_tools::BuiltinToolCtx {
                            session_state,
                            settings,
                            skill_registry,
                            permission_manager,
                            mcp_context,
                            planner: planner_state,
                            chat_command,
                        };
                        match crate::components::builtin_tools::dispatch_todo_to_background(
                            dispatch_ctx,
                            &todo_id,
                            None,
                            Some(new_id.clone()),
                        ) {
                            Ok(msg) => tracing::info!("dispatch: {msg}"),
                            Err(e) => {
                                tracing::error!("dispatch: dispatch_todo_to_background failed: {e}");
                                let err_msg = crate::components::chat::Message {
                                    id: uuid::Uuid::new_v4(),
                                    author: "Hobbes".to_string(),
                                    content: crate::components::shared::MessageContent::Error {
                                        message: e,
                                    },
                                    attachments: Vec::new(),
                                    comments: Vec::new(),
                                    created_at: chrono::Utc::now(),
                                    usage: None,
                                };
                                {
                                    let mut state = session_state.write();
                                    if let Some(s) = state.sessions.get_mut(&new_id) {
                                        s.messages.push(err_msg.clone());
                                    }
                                }
                                crate::session_events::log_event(
                                    &new_id,
                                    crate::session_events::SessionEvent::AssistantMessage {
                                        message: err_msg,
                                    },
                                );
                                // Leave the tab open — the error is visible
                                // right there rather than only in the log.
                            }
                        }
                        crate::session::SessionState::save_signal(&session_state, None);
                    } else {
                        tracing::warn!(
                            "DispatchTodoToAgent: unknown todo id {todo_id}; nothing dispatched"
                        );
                    }
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
                    if *planner_active.peek() {
                        fleet_active.set(false);
                    }
                }
                ChatCommand::ToggleQuickCapture => {
                    let open = *show_quick_capture.peek();
                    show_quick_capture.set(!open);
                }
                ChatCommand::ToggleFleet => {
                    // Mirror of TogglePlanner: first invocation opens+focuses,
                    // then toggles; the ✕ is the only way to close the tab.
                    if !*fleet_tab_open.peek() {
                        fleet_tab_open.set(true);
                        fleet_active.set(true);
                    } else {
                        let now_active = !*fleet_active.peek();
                        fleet_active.set(now_active);
                    }
                    if *fleet_active.peek() {
                        planner_active.set(false);
                    }
                }
                ChatCommand::NextTab => {
                    cycle_tab_fn(true, false);
                }
                ChatCommand::PrevTab => {
                    cycle_tab_fn(false, false);
                }
                ChatCommand::NextTabGroup => {
                    cycle_tab_fn(true, true);
                }
                ChatCommand::PrevTabGroup => {
                    cycle_tab_fn(false, true);
                }
                ChatCommand::TidyTabs => {
                    show_tidy.set(true);
                }
                // Agent-time runtime controls (agent_time_runtime.rs). Start
                // comes from the person or the scheduled window; the run gets its own background
                // tab and never steals focus.
                ChatCommand::AgentTimeStart => {
                    match agent_time_runtime::start_now(planner_state, session_state, settings) {
                        Ok(chat_id) => {
                            let mut tabs = open_tabs.read().clone();
                            if !tabs.contains(&chat_id) {
                                tabs.push(chat_id);
                                open_tabs.set(tabs);
                            }
                        }
                        Err(r) => {
                            tracing::info!("Agent time not started: {}", r.message());
                            flash_timer_toast(format!("Agent time: {}", r.message()));
                        }
                    }
                }
                ChatCommand::AgentTimePause => {
                    agent_time_runtime::halt(
                        planner_state,
                        session_state,
                        crate::todo::model::AgentTimeEndReason::Paused,
                    );
                }
                ChatCommand::AgentTimeStop => {
                    agent_time_runtime::halt(
                        planner_state,
                        session_state,
                        crate::todo::model::AgentTimeEndReason::Stopped,
                    );
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
                    // Cmd+W closes what the user is looking at: an active
                    // planner/fleet view closes its own tab; only when a chat
                    // is forward does it close the chat tab.
                    if *fleet_active.peek() {
                        fleet_active.set(false);
                        fleet_tab_open.set(false);
                    } else if *planner_active.peek() {
                        planner_active.set(false);
                        planner_tab_open.set(false);
                    } else {
                        let idx = *active_tab_index.read();
                        close_tab_fn(idx);
                    }
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
                    } else {
                        // Bring the chat forward — an active planner/fleet
                        // view would otherwise keep covering the switched-to
                        // session (their tabs stay in the strip).
                        planner_active.set(false);
                        fleet_active.set(false);
                        if let Some(idx) = tabs.iter().position(|id| id == &session_id) {
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
                }
                ChatCommand::OpenTabInBackground(session_id) => {
                    // As AgentTimeStart does for its own tab: listed, not focused.
                    if session_state.peek().sessions.contains_key(&session_id) {
                        let mut tabs = open_tabs.read().clone();
                        if !tabs.contains(&session_id) {
                            tabs.push(session_id);
                            open_tabs.set(tabs);
                        }
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
                    // Mirrors `StreamManagerContext.streaming_sessions` into a
                    // root-level GlobalSignal. `StreamManagerContext` is only
                    // reachable by consume_context from *descendants* of this
                    // `StreamManager` element (Dioxus contexts flow parent →
                    // child); `app()`'s own closures (close_tab_fn,
                    // delete_session_fn, the ChatCommand handler) run in the
                    // root scope, an ancestor of `StreamManager`, so they
                    // cannot consume it directly. `StreamBridge` is mounted
                    // *inside* `StreamManager` specifically to bridge that gap.
                    StreamBridge {}
                    // Cmd+K quick capture into the Planner, over any view.
                    components::quick_capture::QuickCapture { open: show_quick_capture }
                    if show_tidy() {
                        {
                            let streaming = STREAMING_SESSIONS.read().clone();
                            let (tabs, todos, projects) = tidy_inputs(
                                &open_tabs.read(),
                                &session_state.read(),
                                &planner_state.read(),
                                &streaming,
                            );
                            rsx! {
                                components::tab_tidy::TidyTabsSheet {
                                    tabs,
                                    todos,
                                    projects,
                                    on_apply: apply_tidy_fn,
                                    on_cancel: move |_| show_tidy.set(false),
                                }
                            }
                        }
                    }
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


                            // Tab list sidebar (UiState::tabs_as_sidebar). Not part of
                            // the History/Settings/MCP group below: those are transient
                            // tool panels sharing one width slot, while this is
                            // persistent navigation with its own width — a tool panel
                            // opens to its right. Same open-tabs state and handlers as
                            // the TabBar (top-tabs mode). It carries the Hobbes menu
                            // and the only + in this mode; the top strip isn't shown.
                            if tabs_as_sidebar() {
                                div {
                                    class: "h-full shrink-0",
                                    {
                                        let tabs = open_tabs.read();
                                        let state = session_state.read();
                                        let entries: Vec<components::tab_sidebar::TabSidebarEntry> = tabs.iter().map(|id| {
                                            let s = state.sessions.get(id);
                                            let last_updated = s.map(|s| s.last_updated).unwrap_or_else(chrono::Utc::now);
                                            components::tab_sidebar::TabSidebarEntry {
                                                session_id: id.clone(),
                                                title: s.map(|s| s.display_title()).unwrap_or_else(|| "New Session".to_string()),
                                                emoji: s.and_then(|s| s.tab_emoji.clone()),
                                                date: s.map(|s| s.short_date()).unwrap_or_default(),
                                                topic: s.and_then(|s| s.tab_topic()),
                                                profile: s.and_then(|s| {
                                                    settings.read().resolve_session_profile_display_name(
                                                        s.composio_profile.as_deref(),
                                                    )
                                                }),
                                                project_id: s.and_then(|s| s.project_id.clone()),
                                                last_updated,
                                                started: s.and_then(|s| s.messages.first()).map(|m| m.created_at).unwrap_or(last_updated),
                                            }
                                        }).collect();
                                        rsx! {
                                            components::tab_sidebar::TabSidebar {
                                                tabs: entries,
                                                active_tab_index: *active_tab_index.read(),
                                                on_select_tab: switch_tab_fn,
                                                on_close_tab: close_tab_fn,
                                                on_new_tab: move |_| new_tab_fn(),
                                                planner_tab_open: *planner_tab_open.read(),
                                                planner_active: *planner_active.read(),
                                                on_select_planner: move |_| {
                                                    planner_active.set(true);
                                                    fleet_active.set(false);
                                                },
                                                on_close_planner: move |_| {
                                                    planner_active.set(false);
                                                    planner_tab_open.set(false);
                                                },
                                                fleet_tab_open: *fleet_tab_open.read(),
                                                fleet_active: *fleet_active.read(),
                                                fleet_attention: fleet_state.read().attention_count(),
                                                on_select_fleet: move |_| {
                                                    fleet_active.set(true);
                                                    planner_active.set(false);
                                                },
                                                on_close_fleet: move |_| {
                                                    fleet_active.set(false);
                                                    fleet_tab_open.set(false);
                                                },
                                                width: chat_layout().sidebar_width,
                                                rail: chat_layout().sidebar_mode.is_rail(),
                                                edge_border: chat_layout().sidebar_mode == layout_budget::SidebarMode::AutoRail,
                                            }
                                        }
                                    }
                                }
                                // The solver's automatic rail is temporary and not
                                // user-resizable; every other state gets the divider.
                                if chat_layout().sidebar_mode != layout_budget::SidebarMode::AutoRail {
                                    components::column_divider::ColumnDivider {
                                        width: tab_sidebar_width().clamp(layout_budget::SIDEBAR_MIN_WIDTH, layout_budget::SIDEBAR_MAX_WIDTH),
                                        railed: tab_sidebar_railed(),
                                        min: layout_budget::SIDEBAR_MIN_WIDTH,
                                        max: layout_budget::SIDEBAR_MAX_WIDTH,
                                        default: layout_budget::SIDEBAR_DEFAULT_WIDTH,
                                        side: components::column_divider::ColumnSide::Left,
                                        label: "Resize tab sidebar".to_string(),
                                        rail: Some(components::column_divider::RailSnap {
                                            below: layout_budget::SIDEBAR_SNAP_TO_RAIL_BELOW,
                                            rail_width: layout_budget::SIDEBAR_RAIL_WIDTH,
                                        }),
                                        on_resize: move |w: components::column_divider::ColumnWidth| {
                                            tab_sidebar_width.set(w.width);
                                            tab_sidebar_railed.set(w.railed);
                                        },
                                        on_commit: move |w: components::column_divider::ColumnWidth| {
                                            tab_sidebar_width.set(w.width);
                                            tab_sidebar_railed.set(w.railed);
                                            let mut ui = ui_state.write();
                                            if ui.tab_sidebar_width != w.width || ui.tab_sidebar_railed != w.railed {
                                                ui.tab_sidebar_width = w.width;
                                                ui.tab_sidebar_railed = w.railed;
                                                ui_state_manager.read().save_async(ui.clone(), None);
                                            }
                                        },
                                    }
                                }
                            }

                            // Drawers (History / Settings / MCP) share one width slot,
                            // resolved by the layout solver from settings_panel_width.
                            if *show_session_manager.read() {
                                div {
                                    id: "session-manager-panel",
                                    style: "width: {chat_layout().drawer_width.unwrap_or_default()}px;",
                                    class: "bg-section text-fg h-full shrink-0",
                                    components::session_manager::SessionManager {}
                                }
                            }
                            if *show_settings_panel.read() {
                                div {
                                    id: "settings-panel",
                                    style: "width: {chat_layout().drawer_width.unwrap_or_default()}px;",
                                    class: "bg-section text-fg h-full shrink-0",
                                    components::settings_panel::SettingsPanel {
                                        nav_railed: chat_layout().drawer_nav_railed,
                                    }
                                }
                            }
                            if *show_mcp_manager.read() {
                                div {
                                    id: "mcp-manager-panel",
                                    style: "width: {chat_layout().drawer_width.unwrap_or_default()}px;",
                                    class: "bg-section text-fg h-full shrink-0",
                                    components::mcp_marketplace::McpMarketplace {}
                                }
                            }
                            if let Some(drawer_width) = chat_layout().drawer_width {
                                components::column_divider::ColumnDivider {
                                    width: drawer_width,
                                    min: layout_budget::DRAWER_MIN_WIDTH,
                                    max: layout_budget::DRAWER_MAX_WIDTH,
                                    default: layout_budget::DRAWER_DEFAULT_WIDTH,
                                    side: components::column_divider::ColumnSide::Left,
                                    label: if *show_settings_panel.read() { "Resize settings".to_string() } else if *show_mcp_manager.read() { "Resize MCP tools".to_string() } else { "Resize history".to_string() },
                                    on_resize: move |w: components::column_divider::ColumnWidth| {
                                        settings_panel_width.set(w.width);
                                    },
                                    on_commit: move |w: components::column_divider::ColumnWidth| {
                                        settings_panel_width.set(w.width);
                                        let mut ui = ui_state.write();
                                        if ui.settings_panel_width != w.width {
                                            ui.settings_panel_width = w.width;
                                            ui_state_manager.read().save_async(ui.clone(), None);
                                        }
                                    },
                                }
                            }

                            // Main Chat Window
                            div {
                                class: "flex-1 flex flex-col min-h-0 min-w-0 border-t border-primary-700/50",
                                // Top tab strip — top-tabs mode only. In sidebar mode the
                                // content starts at the window's top edge.
                                if !tabs_as_sidebar() {
                                    {
                                    let tabs = open_tabs.read();
                                    let state = session_state.read();
                                    rsx! {
                                        components::tab_bar::TabBar {
                                            open_tabs: tabs.clone(),
                                            tab_names: tabs.iter().map(|id| {
                                                state.sessions.get(id)
                                                    .map(|s| s.display_title())
                                                    .unwrap_or_else(|| "New Session".to_string())
                                            }).collect(),
                                            tab_emoji: tabs.iter().map(|id| {
                                                state.sessions.get(id).and_then(|s| s.tab_emoji.clone())
                                            }).collect(),
                                            tab_date: tabs.iter().map(|id| {
                                                state.sessions.get(id)
                                                    .map(|s| s.short_date())
                                                    .unwrap_or_default()
                                            }).collect(),
                                            tab_topic: tabs.iter().map(|id| {
                                                state.sessions.get(id).and_then(|s| s.tab_topic())
                                            }).collect(),
                                            tab_profile: tabs.iter().map(|id| {
                                                state.sessions.get(id).and_then(|s| {
                                                    settings.read().resolve_session_profile_display_name(
                                                        s.composio_profile.as_deref(),
                                                    )
                                                })
                                            }).collect(),
                                            active_tab_index: *active_tab_index.read(),
                                            on_select_tab: switch_tab_fn,
                                            on_close_tab: close_tab_fn,
                                            on_new_tab: move |_| new_tab_fn(),
                                            planner_tab_open: *planner_tab_open.read(),
                                            planner_active: *planner_active.read(),
                                            on_select_planner: move |_| {
                                                planner_active.set(true);
                                                fleet_active.set(false);
                                            },
                                            on_close_planner: move |_| {
                                                planner_active.set(false);
                                                planner_tab_open.set(false);
                                            },
                                            fleet_tab_open: *fleet_tab_open.read(),
                                            fleet_active: *fleet_active.read(),
                                            fleet_attention: fleet_state.read().attention_count(),
                                            on_select_fleet: move |_| {
                                                fleet_active.set(true);
                                                planner_active.set(false);
                                            },
                                            on_close_fleet: move |_| {
                                                fleet_active.set(false);
                                                fleet_tab_open.set(false);
                                            },
                                        }
                                    }
                                    }
                                }

                                // ChatWindow stays mounted while the planner or fleet
                                // is shown — unmounting it mid-stream would drop live
                                // streaming updates, so visibility is toggled with CSS.
                                div {
                                    class: if *planner_active.read() || *fleet_active.read() { "hidden" } else { "flex-1 flex flex-col min-h-0 min-w-0" },
                                    components::chat::ChatWindow {
                                        on_content_resize: move |_| {},
                                        on_interaction: move |_| {},
                                    }
                                }
                                if *planner_active.read() {
                                    div {
                                        class: "flex-1 min-h-0 min-w-0",
                                        components::planner_view::PlannerView {
                                            nav_railed: planner_layout().nav_railed,
                                            nav_width: planner_layout().nav_width,
                                            timeline_width: planner_layout().timeline_width,
                                        }
                                    }
                                }
                                if *fleet_active.read() {
                                    div {
                                        class: "flex-1 min-h-0 min-w-0 flex flex-col",
                                        components::fleet_view::FleetView {}
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

/// Bridges `StreamManagerContext.streaming_sessions` (only reachable by
/// descendants of the `StreamManager` component) out to the root-scope
/// `STREAMING_SESSIONS` GlobalSignal, so `app()`'s own tab-close/delete
/// logic can consult live streaming state without a Dioxus context lookup.
/// Renders nothing; mounted as a child of `StreamManager` purely for its
/// scope position. See the comment at that call site and at
/// `STREAMING_SESSIONS`'s definition for why this indirection exists.
#[component]
fn StreamBridge() -> Element {
    let stream_manager = consume_context::<StreamManagerContext>();
    use_effect(move || {
        let sessions = stream_manager.streaming_sessions.read().clone();
        // Only write on a real change: an unconditional write marks the
        // global dirty every run and wakes every reader (including the tab
        // machinery) for nothing.
        if *STREAMING_SESSIONS.peek() != sessions {
            *STREAMING_SESSIONS.write() = sessions;
        }
    });
    // Reverse direction: root-scope code (delete_session_fn) can't call
    // `stream_manager.cancel_stream()` itself — it has no `StreamManagerContext`
    // — so it drops the target session id in `CANCEL_STREAM_REQUEST` and this
    // effect (which *does* hold the context) performs the actual cancel.
    use_effect(move || {
        // Drain-then-act: `drain_cancel_stream_requests` both subscribes
        // this effect to the global (only a `.read()` does that) and takes
        // the whole pending set in one write, so no guard is alive while
        // `cancel_stream` runs and a request arriving mid-flush isn't
        // clobbered by a blanket clear.
        let pending = drain_cancel_stream_requests();
        for session_id in pending {
            if let Some(message_id) = stream_manager.active_message_for_session(&session_id) {
                tracing::info!(
                    session_id = %session_id,
                    "StreamBridge: cancelling stream for deleted session."
                );
                stream_manager.cancel_stream(&message_id, &session_id);
            }
        }
    });
    rsx! {}
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

/// Regression tests for the `StreamBridge` cancel-drain effect.
///
/// The bug these pin down: inside a `use_effect`, only `.read()` registers
/// the effect's reactive context. An effect whose body *only* writes a
/// GlobalSignal runs once at mount and is never woken again, so a cancel id
/// inserted later by `delete_session_fn` was never acted on.
#[cfg(test)]
mod stream_bridge_cancel_tests {
    use super::*;
    use dioxus::dioxus_core::NoOpMutations;
    use std::cell::RefCell;

    thread_local! {
        /// One entry per run of the probe effect, holding what it drained.
        static DRAINED: RefCell<Vec<Vec<String>>> = const { RefCell::new(Vec::new()) };
    }

    /// Mirrors `StreamBridge`'s cancel effect, minus the context it needs:
    /// it calls the same drain helper and records the result.
    #[allow(non_snake_case)]
    fn CancelProbe() -> Element {
        use_effect(|| {
            let pending = drain_cancel_stream_requests();
            DRAINED.with(|d| d.borrow_mut().push(pending));
        });
        rsx! {}
    }

    /// Pump the dom a bounded number of times so a missed wake-up fails the
    /// assertion instead of hanging the test.
    fn pump(dom: &mut VirtualDom) {
        for _ in 0..8 {
            dom.render_immediate(&mut NoOpMutations);
        }
    }

    #[test]
    fn cancel_request_inserted_after_mount_is_drained() {
        let mut dom = VirtualDom::new(CancelProbe);
        crate::test_support::rebuild_or_fail(&mut dom);
        pump(&mut dom);

        // Mount run sees nothing pending.
        assert_eq!(
            DRAINED.with(|d| d.borrow().clone()),
            vec![Vec::<String>::new()],
            "effect should run once at mount with an empty queue"
        );

        // What `delete_session_fn` does, from outside the effect.
        dom.in_runtime(|| {
            CANCEL_STREAM_REQUEST
                .write()
                .insert("session-to-cancel".to_string());
        });
        pump(&mut dom);

        let runs = DRAINED.with(|d| d.borrow().clone());
        assert!(
            runs.iter()
                .any(|r| r.as_slice() == ["session-to-cancel".to_string()]),
            "a write after mount must re-run the effect (only .read() subscribes); runs were {runs:?}"
        );
        dom.in_runtime(|| {
            assert!(
                CANCEL_STREAM_REQUEST.read().is_empty(),
                "the drain must clear the queue"
            );
        });
    }

    #[test]
    fn idle_effect_runs_do_not_loop_forever() {
        let mut dom = VirtualDom::new(CancelProbe);
        crate::test_support::rebuild_or_fail(&mut dom);
        pump(&mut dom);
        dom.in_runtime(|| {
            CANCEL_STREAM_REQUEST.write().insert("a".to_string());
        });
        pump(&mut dom);

        // Mount run + the drain run. If the drain wrote unconditionally it
        // would re-dirty its own subscription and spin.
        let runs = DRAINED.with(|d| d.borrow().len());
        assert!(
            runs <= 3,
            "effect should settle after draining, ran {runs} times"
        );
    }
}
