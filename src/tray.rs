use dioxus_signals::{GlobalSignal, Signal};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder, Icon,
};
use tracing;

pub static WINDOW_VISIBLE: GlobalSignal<bool> = Signal::global(|| false);
pub static APP_QUIT: GlobalSignal<bool> = Signal::global(|| false);

pub fn init_tray() {
    let image_bytes = include_bytes!("../assets/favicon.png");
    let image = image::load_from_memory(image_bytes)
        .expect("Failed to load icon from memory")
        .to_rgba8();
    let (width, height) = image.dimensions();
    let icon_data = image.into_raw();
    let icon = Icon::from_rgba(icon_data, width, height).expect("Failed to create icon");

    let menu = Menu::new();

    let toggle_window = MenuItem::new("Toggle Window", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let toggle_window_id = toggle_window.id().clone();
    let quit_id = quit.id().clone();

    menu.append_items(&[&toggle_window, &quit]).unwrap();

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()
        .unwrap();
    std::mem::forget(tray_icon);

    let menu_channel = MenuEvent::receiver();

    std::thread::spawn(move || {
        tracing::info!("Tray listener thread started.");
        loop {
            if let Ok(event) = menu_channel.recv() {
                tracing::info!("Tray event received: {:?}", event.id);
                if event.id == toggle_window_id {
                    tracing::info!("Toggle window event received, toggling visibility.");
                    let mut visible = WINDOW_VISIBLE.write();
                    *visible = !*visible;
                }
                if event.id == quit_id {
                    tracing::info!("Quit event received, setting APP_QUIT to true.");
                    *APP_QUIT.write() = true;
                }
            }
        }
    });
}