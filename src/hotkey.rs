// src/hotkey.rs
// This module is responsible for managing global hotkeys.

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::HotKey;
use crate::settings::Settings;
use crate::{permissions, tray::WINDOW_VISIBLE};
use std::str::FromStr;
use std::cell::RefCell;


pub fn use_hotkey_manager(permission_status: Signal<permissions::PermissionStatus>) {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let chat_command = use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    
    // Store the current shortcut handles in a ref-cell to manage their lifecycle
    let current_shortcut_handles = use_hook(|| RefCell::new(Vec::<ShortcutHandle>::new()));
    
    // Debounce state specific to this hook instance
    // We use an Rc<RefCell> so it can be shared among the closures
    let last_trigger_time = use_hook(|| std::rc::Rc::new(RefCell::new(std::time::Instant::now())));

    use_effect(move || {
        // Only attempt to register hotkeys if permissions are granted.
        if matches!(permission_status.read().clone(), permissions::PermissionStatus::Granted) {
            let settings_read = settings.read();
            
            // Clear old shortcuts first
            let mut handles = current_shortcut_handles.borrow_mut();
            for handle in handles.drain(..) {
                desktop.remove_shortcut(handle);
            }
            
            // Helper to check debounce
            let should_trigger = {
                let last_trigger = last_trigger_time.clone();
                move || {
                    let now = std::time::Instant::now();
                    let mut last = last_trigger.borrow_mut();
                    if now.duration_since(*last) > std::time::Duration::from_millis(300) {
                        *last = now;
                        true
                    } else {
                        false
                    }
                }
            };

            // 1. Toggle Tray (Original)
            let hotkey_str = settings_read.hotkeys.toggle_tray.clone();
            if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        let mut visible = WINDOW_VISIBLE.write();
                        *visible = !*visible;
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Tray): {}", &hotkey_str);
                }
            }

            // 2. Toggle Settings
            let settings_hotkey_str = settings_read.hotkeys.toggle_settings.clone();
            if let Ok(hotkey) = HotKey::from_str(&settings_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        cmd.set(Some(crate::components::chat_input::ChatCommand::ToggleSettings));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Settings): {}", &settings_hotkey_str);
                }
            }

            // 3. Toggle History
            let history_hotkey_str = settings_read.hotkeys.toggle_history.clone();
             if let Ok(hotkey) = HotKey::from_str(&history_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        cmd.set(Some(crate::components::chat_input::ChatCommand::ToggleHistory));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (History): {}", &history_hotkey_str);
                }
            }

            // 4. Toggle MCP
            let mcp_hotkey_str = settings_read.hotkeys.toggle_mcp.clone();
            if let Ok(hotkey) = HotKey::from_str(&mcp_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                         cmd.set(Some(crate::components::chat_input::ChatCommand::ToggleMcp));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (MCP): {}", &mcp_hotkey_str);
                }
            }

            // 5. Open Attachments
            let attachments_hotkey_str = settings_read.hotkeys.toggle_attachments.clone();
            if let Ok(hotkey) = HotKey::from_str(&attachments_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                         cmd.set(Some(crate::components::chat_input::ChatCommand::OpenAttachments));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Attachments): {}", &attachments_hotkey_str);
                }
            }

            // 6. New Chat
            let new_chat_hotkey_str = settings_read.hotkeys.toggle_new_chat.clone();
             if let Ok(hotkey) = HotKey::from_str(&new_chat_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        cmd.set(Some(crate::components::chat_input::ChatCommand::NewChat));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (New Chat): {}", &new_chat_hotkey_str);
                }
            }
            // 7. Scroll to Bottom
            let scroll_bottom_hotkey_str = settings_read.hotkeys.toggle_scroll_to_bottom.clone();
            if let Ok(hotkey) = HotKey::from_str(&scroll_bottom_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        cmd.set(Some(crate::components::chat_input::ChatCommand::ScrollToBottom));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Scroll to Bottom): {}", &scroll_bottom_hotkey_str);
                }
            }

            // 8. Focus Chat
            let focus_chat_hotkey_str = settings_read.hotkeys.toggle_focus_chat.clone();
            if let Ok(hotkey) = HotKey::from_str(&focus_chat_hotkey_str) {
                let mut cmd = chat_command.clone();
                let check_debounce = should_trigger.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    if check_debounce() {
                        cmd.set(Some(crate::components::chat_input::ChatCommand::FocusChat));
                    }
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Focus Chat): {}", &focus_chat_hotkey_str);
                }
            }
            
        } else {
             // If permissions are not granted, ensure any existing hotkeys are unregistered.
            let mut handles = current_shortcut_handles.borrow_mut();
            for handle in handles.drain(..) {
                desktop.remove_shortcut(handle);
            }
            if !handles.is_empty() {
                 tracing::info!("Unregistered global hotkeys due to missing permissions.");
            }
        }
    });
}

/// Registers hotkeys Cmd+1 through Cmd+9 for switching Composio profiles.
pub fn use_profile_hotkeys() {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<crate::settings::SettingsManager>>();
    let scheduler = use_context::<Coroutine<crate::processing::summarization_scheduler::SchedulerSignal>>();

    // Store shortcut handles for cleanup
    let profile_shortcut_handles = use_hook(|| RefCell::new(Vec::<ShortcutHandle>::new()));

    use_effect(move || {
        // Clear old shortcuts first
        let mut handles = profile_shortcut_handles.borrow_mut();
        for handle in handles.drain(..) {
            desktop.remove_shortcut(handle);
        }

        let profile_count = settings.read().composio_profiles.len();
        
        // Only register hotkeys if there are multiple profiles
        if profile_count <= 1 {
            return;
        }

        // Register Cmd+1 through Cmd+9 (or up to profile count)
        let max_hotkeys = profile_count.min(9);
        for i in 0..max_hotkeys {
            let hotkey_str = format!("CmdOrCtrl+{}", i + 1);
            if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
                let profile_index = i;
                let mut settings_clone = settings.clone();
                let settings_manager_clone = settings_manager.clone();
                let scheduler_clone = scheduler.clone();
                
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    let profiles = settings_clone.read().composio_profiles.clone();
                    if let Some(profile) = profiles.get(profile_index) {
                        let profile_name = profile.name.clone();
                        settings_clone.write().active_composio_profile = Some(profile_name.clone());
                        
                        // Save changes
                        if let Err(e) = settings_manager_clone.read().save(&settings_clone.read()) {
                            tracing::error!("Failed to save profile change: {}", e);
                        }
                        
                        // Trigger summary refresh
                        scheduler_clone.send(crate::processing::summarization_scheduler::SchedulerSignal::ForceRefresh);
                        
                        tracing::info!("Switched to profile '{}' via hotkey Cmd+{}", profile_name, profile_index + 1);
                    }
                }) {
                    handles.push(handle);
                    tracing::debug!("Registered profile hotkey: Cmd+{}", i + 1);
                }
            }
        }
    });
}