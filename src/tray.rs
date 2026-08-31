use dioxus_signals::{GlobalSignal, Signal};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub static WINDOW_VISIBLE: GlobalSignal<bool> = Signal::global(|| true);
pub static APP_QUIT: GlobalSignal<bool> = Signal::global(|| false);

/// Start the single global `TrayIconEvent` drain thread, exactly once.
///
/// There is one tray icon and one crossbeam channel; exactly one consumer
/// must exist (competing receivers would steal each other's clicks). Clicks
/// are routed by focus mode *at event time*: while a focus session is active
/// the icon is the timer — a left-click release bumps the focus-click
/// counter (drained by an effect in `main.rs` that surfaces the window and
/// planner, never hides them); with no focus active, the historical
/// any-click visibility toggle applies.
pub fn ensure_tray_listener() {
    static LISTENER: std::sync::Once = std::sync::Once::new();
    LISTENER.call_once(|| {
        let tray_channel = TrayIconEvent::receiver();
        std::thread::spawn(move || {
            tracing::info!("Tray listener thread started.");
            loop {
                let Ok(event) = tray_channel.recv() else {
                    // The static channel never closes; if it somehow does,
                    // there is nothing left to listen for.
                    return;
                };
                if let TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                {
                    if crate::focus_tray::focus_mode_active() {
                        // One action per physical click: count only the
                        // left-button release. Never toggles visibility —
                        // focus mode must not hide the window.
                        if button == MouseButton::Left && button_state == MouseButtonState::Up {
                            tracing::info!("Tray clicked in focus mode.");
                            *crate::focus_tray::FOCUS_TRAY_CLICKS.write() += 1;
                        }
                    } else {
                        tracing::info!("Tray icon clicked, toggling visibility.");
                        let mut visible = WINDOW_VISIBLE.write();
                        *visible = !*visible;
                    }
                }
            }
        });
    });
}

pub fn init_tray() -> TrayIcon {
    // Runtime Branding: Select tray icon based on distribution variant.
    // Sandboxed (App Store) = standard favicon, Unsandboxed (Pro) = pro icon.
    let image_bytes: &[u8] = if crate::settings::is_sandboxed() {
        include_bytes!("../assets/favicon.png")
    } else {
        include_bytes!("../assets/icon-pro-tray.png")
    };
    let image = match image::load_from_memory(image_bytes) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            tracing::error!("Failed to load tray icon from memory: {}", e);
            // Return dummy 1x1 transparent image or panic if critical?
            // Since we can't easily return early here without changing return type,
            // we'll try to create a fallback 1x1 image to prevent crash.
            image::RgbaImage::new(1, 1)
        }
    };
    let (width, height) = image.dimensions();
    let icon_data = image.into_raw();
    let icon = Icon::from_rgba(icon_data, width, height).unwrap_or_else(|e| {
        tracing::error!("Failed to create tray icon from RGBA: {}", e);
        // Last resort fallback - empty icon
        Icon::from_rgba(vec![0, 0, 0, 0], 1, 1).unwrap()
    });

    // Build a tray icon without a menu to avoid the muda class conflict
    // (dioxus-desktop's muda 0.11 and tray-icon's muda 0.15 both register the
    // ObjC class `MudaMenuItem`). The main application menu is handled
    // separately in menu.rs. Use the TrayIconEvent receiver for direct clicks.
    ensure_tray_listener();

    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Hobbes")
        .build()
        .unwrap()
}
