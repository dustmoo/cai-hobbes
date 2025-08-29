#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dioxus::desktop::{use_window, Config, WindowBuilder, use_wry_event_handler};
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
mod tray;
mod session;
mod context;
mod processing;
use tray::{APP_QUIT, WINDOW_VISIBLE};

static TRAY_INITIALIZED: AtomicBool = AtomicBool::new(false);
static HOTKEY_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn main() {
    dotenv().ok();
    dioxus_logger::init(tracing::Level::INFO).expect("failed to init logger");

    #[cfg(target_os = "macos")]
    permissions::check_and_prompt_for_accessibility();

    hotkey::init_hotkeys();

    // Load session state to get window size
    let initial_state = session::SessionState::load().unwrap_or_default();
    let initial_width = initial_state.window_width;
    let initial_height = initial_state.window_height;

    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_menu(None)
                .with_window(
                    WindowBuilder::new()
                        .with_visible(true)
                        .with_resizable(true)
                        .with_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(initial_width, initial_height)),
                )
                .with_custom_head(r#"<style>html, body { height: 100%; margin: 0; padding: 0; background-color: #111827; }</style>"#.to_string() + r#"<style>"# + include_str!("../assets/output.css") + r#"</style>"#)
                .with_custom_event_handler(|_e, _| {
                    if TRAY_INITIALIZED
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        tray::init_tray();
                    }

                    // Conditionally compile the hotkey initialization for release builds only.
                    #[cfg(not(debug_assertions))]
                    {
                        if HOTKEY_INITIALIZED
                            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            hotkey::init_hotkeys();
                        }
                    }
                }),
        )
        .launch(app);
}

use crate::session::SessionState;
use crate::components::stream_manager::StreamManager;

fn app() -> Element {
    let window = use_window();
    let session_state = use_context_provider(|| Signal::new(SessionState::new()));
    let mut show_session_manager = use_signal(|| false);
    let mut last_known_size = use_signal(|| PhysicalSize::new(0, 0));
    // This handler continuously updates the last known size during a resize.
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event, .. } = event {
             if let WindowEvent::Resized(new_size) = event {
                last_known_size.set(*new_size);
            }
        }
    });
 
    // This single effect will run on every render, checking the current signal values.
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

    // Log a message in debug builds to inform the developer that hotkeys are disabled.
    #[cfg(debug_assertions)]
    use_effect(|| {
        if HOTKEY_INITIALIZED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::warn!("Hotkeys are disabled in debug mode. Use a release build for hotkey functionality.");
        }
    });



    rsx! {
        StreamManager {
            div {
                class: "dark flex flex-row h-screen",
                // When the user releases the mouse, save the last known size.
                onmouseup: {
                    let mut session_state = session_state.clone();
                    move |_| {
                        let size = last_known_size.read();
                        if size.width > 0 && size.height > 0 {
                            session_state.write().update_window_size(size.width as f64, size.height as f64);
                        }
                    }
                },
                onmouseleave: {
                    let mut session_state = session_state.clone();
                    move |_| {
                        let size = last_known_size.read();
                        if size.width > 0 && size.height > 0 {
                            session_state.write().update_window_size(size.width as f64, size.height as f64);
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

                                // Explicitly set the window size when toggling the sidebar
                                let current_size = window.inner_size();
                                let new_width = if new_show_state { 975.0 } else { 675.0 };
                                window.set_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(new_width, current_size.height as f64));
                            }
                        },
                    }
                }
            }
        }
    }
}


