#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dioxus::desktop::{use_window, Config, WindowBuilder, use_wry_event_handler, muda::MenuEvent, tao::platform::macos::WindowBuilderExtMacOS};
use dioxus::prelude::*;
use dioxus::desktop::tao::dpi::PhysicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus_logger;
use tracing;
use dotenvy::dotenv;

mod components;
mod hotkey;
mod permissions;
mod menu;
mod tray;
mod session;
mod settings;
mod context;
mod processing;
mod secure_storage;
mod mcp;
mod services;
use tray::{APP_QUIT, WINDOW_VISIBLE};
use tray_icon::TrayIcon;

fn main() {
    // Try to load .env file for developer convenience.
    dotenv().ok();
    tracing::info!("Attempted to load .env file from the environment.");
    dioxus_logger::init(tracing::Level::INFO).expect("failed to init logger");

    // Load session state to get window size
    let initial_state = session::SessionState::load().unwrap_or_default();
    let initial_width = initial_state.window_width;
    let initial_height = initial_state.window_height;

    let menu = menu::build_menu();
    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_menu(menu)
                .with_window(
                    {
                        let mut window = WindowBuilder::new()
                            .with_title(env!("APP_NAME"))
                            .with_visible(true)
                            .with_resizable(true)
                            .with_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(initial_width, initial_height));
                        #[cfg(target_os = "macos")]
                        {
                            window = window
                                .with_titlebar_transparent(true);
                        }
                        window
                    }
                )
                .with_custom_head(r#"<style>html, body { height: 100%; margin: 0; padding: 0; background-color: #111827; }</style>"#.to_string() + r#"<style>"# + include_str!("../assets/tailwind.css") + r#"</style>"#)
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
    },
    mcp::manager::McpManager,
    services::document_store::DocumentStore,
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

#[component]
fn RestartRequired() -> Element {
    rsx! {
        div {
            class: "dark flex flex-col items-center justify-center h-screen bg-gray-900 text-white text-center p-8",
            h1 { class: "text-2xl font-bold mb-4", "Permissions Granted" }
            p { class: "mb-6", "Hobbes needs to be restarted for the changes to take effect." }
            p { class: "text-sm text-gray-400", "Please quit and reopen the application." }
        }
    }
}

fn app() -> Element {
    let window = use_window();
    let mut session_state = use_context_provider(|| Signal::new(SessionState::new()));
    let settings_manager = use_context_provider(|| Signal::new(SettingsManager::new(get_settings_path())));
    let ui_state_manager = use_context_provider(|| Signal::new(settings::UiStateManager::new(get_ui_state_path())));
    let mut ui_state = use_context_provider(|| Signal::new(ui_state_manager.read().load()));
    let mut settings = use_context_provider(|| {
        let mut settings = settings_manager.read().load();
        if let Ok(api_key) = crate::secure_storage::retrieve_secret("api_key") {
            settings.gemini_config.api_key = Some(api_key);
        }
        Signal::new(settings)
    });

    let llm_connector = use_context_provider(|| {
        let settings = settings.read();
        let connector: Arc<dyn LlmConnector> = match settings.active_llm {
            crate::settings::LlmProvider::Gemini => {
                Arc::new(GeminiConnector::new(settings.gemini_config.clone()))
            }
        };
        Signal::new(connector)
    });

    use_context_provider(|| Signal::new(processing::conversation_processor::ConversationProcessor::new(llm_connector.read().clone())));

    // Asynchronously load the session state
    let _ = use_resource(move || async move {
        let mut session_state = session_state.clone();
        match SessionState::load() {
            Ok(loaded_state) => {
                session_state.set(loaded_state);
                tracing::info!("Session state loaded successfully.");
            }
            Err(e) => {
                tracing::error!("Failed to load session state: {}. Creating a new default session.", e);
                // If loading fails, create a new session and save it
                let mut state = session_state.write();
                if state.sessions.is_empty() {
                    state.create_session();
                }
            }
        }
    });

    let permission_status_signal = use_context_provider(|| Signal::new(permissions::PermissionStatus::Denied));

    let _ = use_resource(move || async move {
        let mut status = permission_status_signal.clone();
        #[cfg(target_os = "macos")]
        {
            // Run the blocking check in a separate thread
            let result = tokio::task::spawn_blocking(move || {
                permissions::check_and_prompt_for_accessibility()
            }).await.unwrap_or(permissions::PermissionStatus::Denied);
            status.set(result);
        }
        #[cfg(not(target_os = "macos"))]
        {
            status.set(permissions::PermissionStatus::Granted);
        }
    });

    let needs_onboarding = use_signal(|| {
        let key_present = settings.read().gemini_config.api_key.is_some() || std::env::var("GEMINI_API_KEY").is_ok();
        let qdrant_present = settings.read().qdrant_url.is_some() || std::env::var("QDRANT_URL").is_ok();
        !key_present || !qdrant_present
    });
    let permission_manager = use_context_provider(|| Signal::new(PermissionManager::new(settings)));
    let mcp_manager = use_context_provider(|| Signal::new(McpManager::new(get_mcp_config_path(), permission_manager.clone())));
    let mcp_context = use_context_provider(|| Signal::new(mcp::manager::McpContext { servers: Vec::new() }));
        let document_store = use_context_provider(|| Signal::new(None));
    
        use_effect(move || {
            let mut document_store = document_store.clone();
            spawn(async move {
                let qdrant_url = settings.read().qdrant_url.clone().unwrap_or_else(|| std::env::var("QDRANT_URL").unwrap_or_else(|_| "http://localhost:6333".to_string()));
                match DocumentStore::new(&qdrant_url).await {
                    Ok(store) => {
                        document_store.set(Some(std::sync::Arc::new(store)));
                        tracing::info!("DocumentStore initialized successfully.");
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize DocumentStore: {}", e);
                    }
                }
            });
        });

    let _ = use_resource(move || async move {
        let manager = mcp_manager.read().clone();
        let mcp_context_signal = mcp_context.clone();
        let settings_clone = settings.read().clone();
        manager.launch_servers(mcp_context_signal, settings_clone).await;
    });

    let mut show_session_manager = use_signal(|| false);
    let mut show_settings_panel = use_signal(|| false);
    let mut settings_panel_width = use_signal(|| ui_state.read().settings_panel_width);
    let mut is_dragging = use_signal(|| false);
    let mut drag_start_info = use_signal(|| (0.0, 0.0)); // (start_x, start_width)
    let mut final_width_on_drag_end = use_signal(|| 0.0);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    let mut tray_icon = use_signal::<Option<TrayIcon>>(|| None);
    let mut show_confirm_modal = use_context_provider(|| Signal::new(false));
    let session_to_delete = use_context_provider(|| Signal::new(String::new()));

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

    // One-time setup for the menu
    use_effect(move || {
        let menu_channel = MenuEvent::receiver();
        std::thread::spawn(move || {
            loop {
                if let Ok(event) = menu_channel.recv() {
                    if event.id.0 == "quit" {
                        let mut app_quit = APP_QUIT.write();
                        *app_quit = true;
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
                tracing::info!("Tray icon has been created.");
            }
        } else {
            if tray_icon.peek().is_some() {
                tray_icon.set(None);
                tracing::info!("Tray icon has been removed.");
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
            tracing::info!("Window is visible, centering on current monitor.");
            let main_window = &window_clone.window;
            if let Some(monitor) = main_window.current_monitor() {
                let monitor_size = monitor.size();
                let window_size = main_window.outer_size();
                let monitor_pos = monitor.position();

                let x = monitor_pos.x + (monitor_size.width as i32 - window_size.width as i32) / 2;
                let y = monitor_pos.y + (monitor_size.height as i32 - window_size.height as i32) / 2;

                main_window.set_outer_position(dioxus::desktop::tao::dpi::PhysicalPosition::new(x, y));
            }
        } else {
            tracing::info!("Window is hidden.");
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
                    if let Err(e) = uism.save(&current_ui_state) {
                        tracing::error!("Failed to save UI state: {}", e);
                    }
                }
                final_width_on_drag_end.set(0.0); // Reset after saving
            });
        }
    });




    rsx! {
        if matches!(*permission_status_signal.read(), permissions::PermissionStatus::JustGranted) {
            RestartRequired {}
        } else if *needs_onboarding.read() {
            div {
                class: "dark flex items-center justify-center h-screen bg-gray-900 text-white",
                components::onboarding::Onboarding {
                    needs_onboarding: needs_onboarding,
                }
            }
        } else {
            processing::summarization_scheduler::SummarizationScheduler {
                StreamManager {
            ConfirmDeleteModal {
                is_visible: show_confirm_modal,
                title: "Delete Session".to_string(),
                message: "Are you sure you want to delete this session? This action cannot be undone.".to_string(),
                on_cancel: move |_| show_confirm_modal.set(false),
                on_confirm: move |remember| {
                    let id_to_delete_str = session_to_delete.read().clone();
                    if !id_to_delete_str.is_empty() {
                        session_state.write().delete_session(&id_to_delete_str);
                    }
                    if remember {
                        let mut current_settings = settings.write();
                        current_settings.confirm_on_delete = false;
                        let sm = settings_manager.read();
                        if let Err(e) = sm.save(&current_settings) {
                            tracing::error!("Failed to save settings: {}", e);
                        }
                    }
                    show_confirm_modal.set(false);
                },
            }
                div {
                    class: "dark flex flex-col h-screen", // Changed to flex-col
                    // The draggable header has been removed as per user request.
                    // Main content area
                    div {
                        class: "flex flex-row flex-1 min-h-0", // This will contain the sidebars and chat
                        // The onkeydown handler has been removed to allow native hotkeys (copy, paste, etc.) to function correctly.
                        // The global hotkey for toggling visibility is no longer required.
                        // When the user releases the mouse, save the last known size.
                        onmouseup: {
                            let mut session_state = session_state.clone();
                            let show_session_manager = show_session_manager.clone();
                            let window = window.clone();
                            move |_| {
                                let physical_size = last_known_size.read();
                                if physical_size.width > 0 && physical_size.height > 0 {
                                    let scale_factor = window.scale_factor();
                                    let logical_size = physical_size.to_logical::<f64>(scale_factor);
                                    let sidebar_width = if *show_session_manager.read() { settings_panel_width() } else { 0.0 };
                                    let content_width = logical_size.width - sidebar_width;
                                    session_state.write().update_window_size(content_width, logical_size.height);
                                }
                            }
                        },
                        onmouseleave: {
                            let mut session_state = session_state.clone();
                            let show_session_manager = show_session_manager.clone();
                            let window = window.clone();
                            move |_| {
                                let physical_size = last_known_size.read();
                                if physical_size.width > 0 && physical_size.height > 0 {
                                    let scale_factor = window.scale_factor();
                                    let logical_size = physical_size.to_logical::<f64>(scale_factor);
                                    let sidebar_width = if *show_session_manager.read() { settings_panel_width() } else { 0.0 };
                                    let content_width = logical_size.width - sidebar_width;
                                    session_state.write().update_window_size(content_width, logical_size.height);
                                }
                            }
                        },

                    // Session Manager Sidebar
                    if *show_session_manager.read() {
                        div {
                            class: "flex flex-row h-full",
                            // Session Manager Panel
                            div {
                                id: "session-manager-panel",
                                style: "width: {settings_panel_width}px;",
                                class: "bg-gray-800 text-white h-full",
                                components::session_manager::SessionManager {}
                            }
                            // Draggable Divider
                            div {
                                class: "w-2 cursor-col-resize bg-gray-700 hover:bg-indigo-500 transition-colors",
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
                                class: "bg-gray-800 text-white h-full",
                                // This is the correct location for the settings panel component
                                components::settings_panel::SettingsPanel {}
                            }
                            // Draggable Divider
                            div {
                                class: "w-2 cursor-col-resize bg-gray-700 hover:bg-indigo-500 transition-colors",
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
                                        let panel_id = if *show_settings_panel.read() { "settings-panel" } else { "session-manager-panel" };
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
                        class: "flex-1",
                        components::chat::ChatWindow {
                            on_content_resize: move |_| {},
                            on_interaction: move |_| {},
                            on_toggle_sessions: {
                                let window = window.clone();
                                move |_| {
                                    let new_show_state = !*show_session_manager.read();
                                    show_session_manager.set(new_show_state);
                                    if new_show_state {
                                        show_settings_panel.set(false); // Hide settings if showing sessions
                                    }

                                    // Adjust the window size based on the sidebar's visibility
                                    let session_state = session_state.clone();
                                    let sidebar_width = settings_panel_width();
                                    let _current_size = window.inner_size();
                                    let persisted_width = session_state.read().window_width;
                                    let persisted_height = session_state.read().window_height;

                                    let new_width = if new_show_state {
                                        persisted_width + sidebar_width
                                    } else {
                                        persisted_width
                                    };

                                    window.set_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(new_width, persisted_height as f64));
                                }
                            },
                            on_toggle_settings: move |_| {
                                let new_show_state = !*show_settings_panel.read();
                                show_settings_panel.set(new_show_state);
                                if new_show_state {
                                    show_session_manager.set(false); // Hide sessions if showing settings
                                }
                            },
                        }
                    }
                    }
                    }
                }
            }
        }
    }
}
