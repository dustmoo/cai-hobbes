// src/hotkey.rs
// This module is responsible for managing global hotkeys.

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::HotKey;
use crate::settings::Settings;
use crate::tray::WINDOW_VISIBLE;
use std::str::FromStr;
use std::cell::RefCell;

pub fn use_hotkey_manager() {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    
    // Store the current shortcut handle in a ref-cell to manage its lifecycle
    let current_shortcut_handle = use_hook(|| RefCell::new(None::<ShortcutHandle>));

    use_effect(move || {
        let hotkey_str = settings.read().global_hotkey.clone();
        
        // If there's an old shortcut, unregister it first.
        if let Some(handle) = current_shortcut_handle.borrow_mut().take() {
            desktop.remove_shortcut(handle);
        }

        if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
            if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                let mut visible = WINDOW_VISIBLE.write();
                *visible = !*visible;
            }) {
                // Store the new shortcut handle so we can unregister it later
                *current_shortcut_handle.borrow_mut() = Some(handle);
                tracing::info!("Registered global hotkey: {}", &hotkey_str);
            } else {
                tracing::error!("Failed to register global hotkey: {}", &hotkey_str);
            }
        } else {
            tracing::error!("Failed to parse hotkey string: {}", &hotkey_str);
        }
    });
}