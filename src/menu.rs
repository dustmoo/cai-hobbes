// src/menu.rs
// This module builds the main application menu using the `dioxus::desktop::muda` components.

use crate::settings::Settings;
use dioxus::desktop::muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

pub fn build_menu(_settings: &Settings) -> Menu {
    let menu = Menu::new();

    // On macOS, the first menu item is the application menu.
    #[cfg(target_os = "macos")]
    {
        let app_name = crate::settings::get_app_name();
        let app_menu = Submenu::new(app_name, true);
        menu.append(&app_menu).unwrap();

        let settings_item = MenuItem::with_id("settings", "Settings...", true, None);

        app_menu
            .append_items(&[
                &PredefinedMenuItem::about(Some(&format!("About {}", app_name)), None),
                &PredefinedMenuItem::separator(),
                &settings_item,
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ])
            .unwrap();
    }

    // The Edit menu is crucial for hotkeys.
    let edit_menu = Submenu::new("Edit", true);
    menu.append(&edit_menu).unwrap();
    edit_menu
        .append_items(&[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ])
        .unwrap();

    // View Menu for App Navigation
    let view_menu = Submenu::new("View", true);
    menu.append(&view_menu).unwrap();

    let history_item = MenuItem::with_id("view_history", "History", true, None);

    let mcp_item = MenuItem::with_id("view_mcp", "MCP Config", true, None);

    let planner_item = MenuItem::with_id("view_planner", "Planner", true, None);

    let profile_item = MenuItem::with_id("view_profile", "Profile Selector", true, None);

    let attachments_item = MenuItem::with_id("view_attachments", "Add Attachments", true, None);

    view_menu
        .append_items(&[
            &history_item,
            &mcp_item,
            &planner_item,
            &profile_item,
            &attachments_item,
        ])
        .unwrap();

    // A standard Window menu.
    let window_menu = Submenu::new("Window", true);
    menu.append(&window_menu).unwrap();
    window_menu
        .append_items(&[&PredefinedMenuItem::minimize(None)])
        .unwrap();

    menu
}
