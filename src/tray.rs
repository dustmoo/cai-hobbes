use dioxus_signals::{GlobalSignal, Signal};
use tray_icon::{
    TrayIconBuilder,
    Icon,
    TrayIconEvent,
    TrayIcon,
};
use tracing;

pub static WINDOW_VISIBLE: GlobalSignal<bool> = Signal::global(|| true);
pub static APP_QUIT: GlobalSignal<bool> = Signal::global(|| false);

pub fn init_tray() -> TrayIcon {
    let image_bytes = include_bytes!("../assets/favicon.png");
    let image = image::load_from_memory(image_bytes)
        .expect("Failed to load icon from memory")
        .to_rgba8();
    let (width, height) = image.dimensions();
    let icon_data = image.into_raw();
    let icon = Icon::from_rgba(icon_data, width, height).expect("Failed to create icon");

    // Build a tray icon without a menu to avoid the muda class conflict.
    // The main application menu is handled separately in menu.rs.
    // Use the TrayIconEvent receiver for direct clicks.
    let tray_channel = TrayIconEvent::receiver();

    std::thread::spawn(move || {
        tracing::info!("Tray listener thread started.");
        loop {
            if let Ok(event) = tray_channel.recv() {
                match event {
                    TrayIconEvent::Click { .. } => {
                        tracing::info!("Tray icon clicked, toggling visibility.");
                        let mut visible = WINDOW_VISIBLE.write();
                        *visible = !*visible;
                    },
                    _ => (),
                }
            }
        }
    });

    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Hobbes")
        .build()
        .unwrap()
}