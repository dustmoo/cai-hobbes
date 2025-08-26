#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dioxus::desktop::{use_window, Config, WindowBuilder, use_wry_event_handler};
use dioxus::prelude::*;
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
use tray::{APP_QUIT, WINDOW_VISIBLE};

static TRAY_INITIALIZED: AtomicBool = AtomicBool::new(false);
static HOTKEY_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn main() {
    dotenv().ok();
    dioxus_logger::init(tracing::Level::INFO).expect("failed to init logger");

    #[cfg(target_os = "macos")]
    permissions::check_and_prompt_for_accessibility();

    hotkey::init_hotkeys();

    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_menu(None)
                .with_window(
                    WindowBuilder::new()
                        .with_visible(false)
                        .with_resizable(true)
                        .with_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(675, 160)),
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

fn app() -> Element {
    let window = use_window();
    let mut is_locked = use_signal(|| false);
    let mut is_expanded = use_signal(|| false);
    let mut show_session_manager = use_signal(|| false);
    let mut is_dragging = use_signal(|| false);
    let last_auto_size = use_signal(|| None as Option<dioxus::desktop::tao::dpi::PhysicalSize<f64>>);

    use_context_provider(|| Signal::new(SessionState::new()));

    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event, .. } = event {
            if let WindowEvent::Resized(new_size) = event {
                if *is_dragging.read() {
                    if let Some(last_size) = *last_auto_size.read() {
                        if (new_size.width as f64 - last_size.width).abs() > 1.0
                            || (new_size.height as f64 - last_size.height).abs() > 1.0
                        {
                            tracing::info!("Manual resize detected. Locking auto-resize.");
                            is_locked.set(true);
                            is_dragging.set(false);
                        }
                    }
                }
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

    // Effect to animate the window expansion on first interaction
    let window_clone = window.clone();
    use_effect(move || {
        if *is_expanded.read() {
            let window_clone = window_clone.clone();
            spawn(async move {
                let start_height = 160.0;
                let end_height = 750.0;
                let start_width = if *show_session_manager.read() { 975.0 } else { 675.0 };
                let duration_ms: f64 = 200.0;
                let interval_ms: f64 = 16.0; // Aim for ~60 FPS
                let steps = (duration_ms / interval_ms).ceil() as u32;
                let height_increment = (end_height - start_height) / steps as f64;

                for i in 1..=steps {
                    let current_height = start_height + height_increment * i as f64;
                    let new_size = dioxus::desktop::tao::dpi::LogicalSize::new(start_width, current_height);
                    window_clone.set_inner_size(new_size);
                    tokio::time::sleep(std::time::Duration::from_millis(interval_ms as u64)).await;
                }
                // Ensure the final size is set precisely
                let final_size = dioxus::desktop::tao::dpi::LogicalSize::new(start_width, end_height);
                window_clone.set_inner_size(final_size);
                tracing::info!(?final_size, "Window expansion animation complete.");
            });
        }
    });

    let window_clone = window.clone();
    use_effect(move || {
        let width = if *show_session_manager.read() { 975.0 } else { 675.0 };
        let height = if *is_expanded.read() { 750.0 } else { 160.0 };
        window_clone.set_inner_size(dioxus::desktop::tao::dpi::LogicalSize::new(width, height));
    });


    rsx! {
        div {
            class: "dark flex flex-row h-screen",
            onmousedown: move |_| is_dragging.set(true),
            onmouseup: move |_| is_dragging.set(false),
            onmouseleave: move |_| is_dragging.set(false),

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
                        if !*is_expanded.read() {
                            is_expanded.set(true);
                        }
                    },
                    on_content_resize: move |_| {},
                    on_toggle_sessions: move |_| {
                        let new_state = !*show_session_manager.read();
                        show_session_manager.set(new_state);
                        if new_state && !*is_expanded.read() {
                            is_expanded.set(true);
                        }
                    },
                }
            }
        }
    }
}
