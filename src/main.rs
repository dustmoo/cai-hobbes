#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dioxus::desktop::{use_window, Config, WindowBuilder, use_wry_event_handler, muda::MenuEvent};
use dioxus::prelude::*;
use dioxus::desktop::tao::dpi::PhysicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use std::sync::atomic::{AtomicBool, Ordering};
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
use tray::{APP_QUIT, WINDOW_VISIBLE};

static TRAY_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn main() {
    dotenv().ok();
    dioxus_logger::init(tracing::Level::INFO).expect("failed to init logger");

    #[cfg(target_os = "macos")]
    permissions::check_and_prompt_for_accessibility();


    // Load session state to get window size
    let initial_state = session::SessionState::load().unwrap_or_default();
    let initial_width = initial_state.window_width;
    let initial_height = initial_state.window_height;

    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_window(
                    WindowBuilder::new()
                        .with_visible(true)
                        .with_resizable(true)
                        .with_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(initial_width, initial_height)),
                )
                .with_custom_head(r#"<style>html, body { height: 100%; margin: 0; padding: 0; background-color: #111827; }</style>"#.to_string() + r#"<style>"# + include_str!("../assets/output.css") + r#"</style>"#)
        )
        .launch(app);
}

use crate::session::SessionState;
use crate::settings::SettingsManager;
use crate::{components::stream_manager::StreamManager, mcp::manager::McpManager};
use std::path::PathBuf;

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

fn app() -> Element {
    let window = use_window();
    let session_state = use_context_provider(|| Signal::new(SessionState::new()));
    let settings_manager = use_context_provider(|| Signal::new(SettingsManager::new(get_settings_path())));
    let mcp_manager = use_context_provider(|| Signal::new(McpManager::new(get_mcp_config_path())));
    let mcp_context = use_context_provider(|| Signal::new(mcp::manager::McpContext { servers: Vec::new() }));

    use_effect(move || {
        let manager = mcp_manager.read().clone();
        let mcp_context_signal = mcp_context.clone();
        spawn(async move {
            manager.launch_servers(mcp_context_signal).await;
        });
    });
        let _settings = use_context_provider(|| {
        let mut settings = settings_manager.read().load();
        if let Ok(api_key) = crate::secure_storage::retrieve_secret("api_key") {
            settings.api_key = Some(api_key);
        }
        Signal::new(settings)
    });
    let mut show_session_manager = use_signal(|| false);
    let mut show_settings_panel = use_signal(|| false);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    // This handler continuously updates the last known size during a resize.
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event, .. } = event {
             if let WindowEvent::Resized(new_size) = event {
                last_known_size.set(*new_size);
            }
        }
    });
 
    // One-time setup for tray icon and menu event listener
    use_effect(move || {
        if TRAY_INITIALIZED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            // Initialize the menu
            let menu = menu::build_menu();
            #[cfg(target_os = "macos")]
            menu.init_for_nsapp();
            #[cfg(target_os = "windows")]
            menu.init_for_hwnd(window.hwnd());
            
            // Initialize the tray icon
            tray::init_tray();

            // Start the menu event listener
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




    rsx! {
        StreamManager {
            div {
                class: "dark flex flex-row h-screen",
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
                            let sidebar_width = if *show_session_manager.read() { 256.0 } else { 0.0 };
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
                            let sidebar_width = if *show_session_manager.read() { 256.0 } else { 0.0 };
                            let content_width = logical_size.width - sidebar_width;
                            session_state.write().update_window_size(content_width, logical_size.height);
                        }
                    }
                },

                // Session Manager Sidebar
                if *show_session_manager.read() {
                    div {
                        class: "w-64 bg-gray-800 text-white h-full transition-all duration-300 ease-in-out",
                        components::session_manager::SessionManager {}
                    }
                }

                // Settings Panel Sidebar
                if *show_settings_panel.read() {
                    div {
                        class: "w-64 bg-gray-800 text-white h-full transition-all duration-300 ease-in-out",
                        components::settings_panel::SettingsPanel {}
                    }
                }

                // Main Chat Window
                div {
                    class: "flex-1",
                    components::chat::ChatWindow {
                        on_interaction: move |_| {
                            // No longer needed for expansion
                        },
                        on_content_resize: move |_| {},
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
                                let sidebar_width = 256.0; // w-64 in Tailwind is 16rem which is 256px
                                let current_size = window.inner_size();
                                let persisted_width = session_state.read().window_width;

                                let new_width = if new_show_state {
                                    persisted_width + sidebar_width
                                } else {
                                    persisted_width
                                };

                                window.set_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(new_width, current_size.height as f64));
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



