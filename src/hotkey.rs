// src/hotkey.rs
// This module is responsible for managing global hotkeys.

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::HotKey;
use crate::settings::Settings;
use crate::{permissions, tray::WINDOW_VISIBLE};
use std::str::FromStr;
use std::cell::RefCell;
use futures_util::StreamExt;


#[derive(Debug, Clone)]
pub enum HotkeyAction {
    ToggleTray,
    ToggleSettings,
    ToggleHistory,
    ToggleMcp,
    OpenAttachments,
    NewChat,
    NewChatWithMemory,
    ScrollToBottom,
    FocusChat,
    // Modal actions are handled locally by components, not via global hotkeys.
    // Retained for potential future use with other dispatch mechanisms.
    #[allow(dead_code)]
    SubmitModal,
    #[allow(dead_code)]
    CloseModal,
    #[allow(dead_code)]
    SaveModal,
}

pub fn use_hotkey_manager(permission_status: Signal<permissions::PermissionStatus>) {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command = use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    // focus_context is used for modal-specific routing; currently handled locally, but kept for future use.
    let _focus_context = use_context::<Signal<crate::components::focus_context::FocusContext>>();
    
    // Store the current shortcut handles in a ref-cell to manage their lifecycle
    let current_shortcut_handles = use_hook(|| RefCell::new(Vec::<ShortcutHandle>::new()));
    
    // Coroutine Bridge to handle hotkey actions on the main thread
    let action_tx = use_coroutine(move |mut rx: UnboundedReceiver<HotkeyAction>| async move {
        // Debounce state inside the task
        let mut last_trigger = std::time::Instant::now();
        
        while let Some(action) = rx.next().await {
            let now = std::time::Instant::now();
            if now.duration_since(last_trigger) < std::time::Duration::from_millis(300) {
                continue;
            }
            last_trigger = now;

            match action {
                HotkeyAction::ToggleTray => {
                    let mut visible = WINDOW_VISIBLE.write();
                    *visible = !*visible;
                },
                HotkeyAction::ToggleSettings => chat_command.set(Some(crate::components::chat_input::ChatCommand::ToggleSettings)),
                HotkeyAction::ToggleHistory => chat_command.set(Some(crate::components::chat_input::ChatCommand::ToggleHistory)),
                HotkeyAction::ToggleMcp => chat_command.set(Some(crate::components::chat_input::ChatCommand::ToggleMcp)),
                HotkeyAction::OpenAttachments => chat_command.set(Some(crate::components::chat_input::ChatCommand::OpenAttachments)),
                HotkeyAction::NewChat => chat_command.set(Some(crate::components::chat_input::ChatCommand::NewChat)),
                HotkeyAction::NewChatWithMemory => chat_command.set(Some(crate::components::chat_input::ChatCommand::NewChatWithMemory)),
                HotkeyAction::ScrollToBottom => chat_command.set(Some(crate::components::chat_input::ChatCommand::ScrollToBottom)),
                HotkeyAction::FocusChat => chat_command.set(Some(crate::components::chat_input::ChatCommand::FocusChat)),
                HotkeyAction::SubmitModal => chat_command.set(Some(crate::components::chat_input::ChatCommand::SubmitModal)),
                HotkeyAction::CloseModal => chat_command.set(Some(crate::components::chat_input::ChatCommand::CloseModal)),
                HotkeyAction::SaveModal => chat_command.set(Some(crate::components::chat_input::ChatCommand::SaveModal)),
            }
        }
    });

    use_effect(move || {
        // Only attempt to register hotkeys if permissions are granted.
        if matches!(permission_status.read().clone(), permissions::PermissionStatus::Granted) {
            let settings_read = settings.read();
            
            // Clear old shortcuts first
            let mut handles = current_shortcut_handles.borrow_mut();
            for handle in handles.drain(..) {
                desktop.remove_shortcut(handle);
            }
            
            // 1. Toggle Tray
            let hotkey_str = settings_read.hotkeys.toggle_tray.clone();
            if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::ToggleTray);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Tray): {}", &hotkey_str);
                }
            }

            // 2. Toggle Settings
            let settings_hotkey_str = settings_read.hotkeys.toggle_settings.clone();
            if let Ok(hotkey) = HotKey::from_str(&settings_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::ToggleSettings);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Settings): {}", &settings_hotkey_str);
                }
            }

            // 3. Toggle History
            let history_hotkey_str = settings_read.hotkeys.toggle_history.clone();
             if let Ok(hotkey) = HotKey::from_str(&history_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::ToggleHistory);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (History): {}", &history_hotkey_str);
                }
            }

            // 4. Toggle MCP
            let mcp_hotkey_str = settings_read.hotkeys.toggle_mcp.clone();
            if let Ok(hotkey) = HotKey::from_str(&mcp_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::ToggleMcp);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (MCP): {}", &mcp_hotkey_str);
                }
            }

            // 5. Open Attachments
            let attachments_hotkey_str = settings_read.hotkeys.toggle_attachments.clone();
            if let Ok(hotkey) = HotKey::from_str(&attachments_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::OpenAttachments);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Attachments): {}", &attachments_hotkey_str);
                }
            }

            // 6. New Chat
            let new_chat_hotkey_str = settings_read.hotkeys.toggle_new_chat.clone();
            if !new_chat_hotkey_str.is_empty() {
                match HotKey::from_str(&new_chat_hotkey_str) {
                    Ok(hotkey) => {
                        let tx = action_tx.clone();
                        match desktop.create_shortcut(hotkey, move || {
                            tx.send(HotkeyAction::NewChat);
                        }) {
                            Ok(handle) => {
                                handles.push(handle);
                                tracing::info!("Registered global hotkey (New Chat): {}", &new_chat_hotkey_str);
                            }
                            Err(e) => {
                                tracing::error!("Failed to create shortcut for New Chat '{}': {:?}", &new_chat_hotkey_str, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse hotkey for New Chat '{}': {:?}", &new_chat_hotkey_str, e);
                    }
                }
            }
            
            // 7. New Chat with Memory
            let new_chat_memory_hotkey_str = settings_read.hotkeys.toggle_new_chat_with_memory.clone();
            if !new_chat_memory_hotkey_str.is_empty() {
                match HotKey::from_str(&new_chat_memory_hotkey_str) {
                    Ok(hotkey) => {
                        let tx = action_tx.clone();
                        match desktop.create_shortcut(hotkey, move || {
                            tx.send(HotkeyAction::NewChatWithMemory);
                        }) {
                            Ok(handle) => {
                                handles.push(handle);
                                tracing::info!("Registered global hotkey (New Chat with Memory): {}", &new_chat_memory_hotkey_str);
                            }
                            Err(e) => {
                                tracing::error!("Failed to create shortcut for New Chat with Memory '{}': {:?}", &new_chat_memory_hotkey_str, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse hotkey for New Chat with Memory '{}': {:?}", &new_chat_memory_hotkey_str, e);
                    }
                }
            }

            // 7. Scroll to Bottom
            let scroll_bottom_hotkey_str = settings_read.hotkeys.toggle_scroll_to_bottom.clone();
            if let Ok(hotkey) = HotKey::from_str(&scroll_bottom_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::ScrollToBottom);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Scroll to Bottom): {}", &scroll_bottom_hotkey_str);
                }
            }

            // 8. Focus Chat
            let focus_chat_hotkey_str = settings_read.hotkeys.toggle_focus_chat.clone();
            if let Ok(hotkey) = HotKey::from_str(&focus_chat_hotkey_str) {
                let tx = action_tx.clone();
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(HotkeyAction::FocusChat);
                }) {
                     handles.push(handle);
                     tracing::info!("Registered global hotkey (Focus Chat): {}", &focus_chat_hotkey_str);
                }
            }
            
            // NOTE: Cmd+Enter and Escape are handled locally by modal components,
            // not as global shortcuts. This follows the "Local Primacy Policy" pattern
            // to avoid intercepting events from components like ChatInput.

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
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<crate::settings::SettingsManager>>();
    let scheduler = use_context::<Coroutine<crate::processing::summarization_scheduler::SchedulerSignal>>();

    // Store shortcut handles for cleanup
    let profile_shortcut_handles = use_hook(|| RefCell::new(Vec::<ShortcutHandle>::new()));

    // Coroutine Bridge
    let profile_tx = use_coroutine(move |mut rx: UnboundedReceiver<usize>| async move {
        while let Some(index) = rx.next().await {
            // Re-read settings safely on main thread
            let profiles = settings.read().composio_profiles.clone();
            
            if let Some(profile) = profiles.get(index) {
                let profile_name = profile.name.clone();
                
                // Update active profile
                settings.write().active_composio_profile = Some(profile_name.clone());
                
                // Save changes
                if let Err(e) = settings_manager.read().save(&settings.read()) {
                    tracing::error!("Failed to save profile change: {}", e);
                }
                
                // Trigger summary refresh
                scheduler.send(crate::processing::summarization_scheduler::SchedulerSignal::ForceRefresh);
                
                tracing::info!("Switched to profile '{}' via hotkey Cmd+{}", profile_name, index + 1);
            }
        }
    });

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
                let tx = profile_tx.clone();
                let profile_index = i;
                
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    tx.send(profile_index);
                }) {
                    handles.push(handle);
                    tracing::debug!("Registered profile hotkey: Cmd+{}", i + 1);
                }
            }
        }
    });
}