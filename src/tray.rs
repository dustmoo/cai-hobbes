use dioxus_signals::{GlobalSignal, Signal};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub static WINDOW_VISIBLE: GlobalSignal<bool> = Signal::global(|| true);
pub static APP_QUIT: GlobalSignal<bool> = Signal::global(|| false);

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

    // Build a tray icon without a menu to avoid the muda class conflict.
    // The main application menu is handled separately in menu.rs.
    // Use the TrayIconEvent receiver for direct clicks.
    let tray_channel = TrayIconEvent::receiver();

    std::thread::spawn(move || {
        tracing::info!("Tray listener thread started.");
        loop {
            if let Ok(TrayIconEvent::Click { .. }) = tray_channel.recv() {
                tracing::info!("Tray icon clicked, toggling visibility.");
                let mut visible = WINDOW_VISIBLE.write();
                *visible = !*visible;
            }
        }
    });

    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Hobbes")
        .build()
        .unwrap()
}
