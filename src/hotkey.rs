// src/hotkey.rs
// This module is responsible for managing global hotkeys.

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::HotKey;
use crate::settings::Settings;
use crate::{permissions, tray::WINDOW_VISIBLE};
use std::str::FromStr;
use std::cell::RefCell;
use futures_util::{StreamExt, SinkExt};

pub fn use_hotkey_manager(permission_status: Signal<permissions::PermissionStatus>) {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let coroutine = use_coroutine(|mut rx: UnboundedReceiver<()>| async move {
        while rx.next().await.is_some() {
            let mut visible = WINDOW_VISIBLE.write();
            *visible = !*visible;
        }
    });

    // Store the current shortcut handle in a ref-cell to manage its lifecycle
    let current_shortcut_handle = use_hook(|| RefCell::new(None::<ShortcutHandle>));

    use_effect(move || {
        // Only attempt to register hotkeys if permissions are granted.
        if matches!(permission_status.read().clone(), permissions::PermissionStatus::Granted) {
            let hotkey_str = settings.read().global_hotkey.clone();
            
            // If there's an old shortcut, unregister it first.
            if let Some(handle) = current_shortcut_handle.borrow_mut().take() {
                desktop.remove_shortcut(handle);
            }

            if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
                let mut tx = coroutine.tx().clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    let _ = tx.send(());
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
        } else {
            // If permissions are not granted, ensure any existing hotkey is unregistered.
            if let Some(handle) = current_shortcut_handle.borrow_mut().take() {
                desktop.remove_shortcut(handle);
                tracing::info!("Unregistered global hotkey due to missing permissions.");
            }
        }
    });
}