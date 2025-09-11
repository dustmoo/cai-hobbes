// src/menu.rs
// This module builds the main application menu using the `dioxus::desktop::muda` components.

use dioxus::desktop::muda::{Menu, Submenu, PredefinedMenuItem};

pub fn build_menu() -> Menu {
    let menu = Menu::new();

    // On macOS, the first menu item is the application menu.
    #[cfg(target_os = "macos")]
    {
        let app_menu = Submenu::new(env!("APP_NAME"), true);
        menu.append(&app_menu).unwrap();
        app_menu.append_items(&[
            &PredefinedMenuItem::about(Some(&format!("About {}", env!("APP_NAME"))), None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ]).unwrap();
    }

    // The Edit menu is crucial for hotkeys.
    let edit_menu = Submenu::new("Edit", true);
    menu.append(&edit_menu).unwrap();
    edit_menu.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ]).unwrap();

    // A standard Window menu.
    let window_menu = Submenu::new("Window", true);
    menu.append(&window_menu).unwrap();
    window_menu.append_items(&[
        &PredefinedMenuItem::minimize(None),
    ]).unwrap();

    menu
}