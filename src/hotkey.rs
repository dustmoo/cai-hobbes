// src/hotkey.rs
// Global Hotkey Management using the Hybrid Pattern (Pattern 126)
// - Native Global: Only for Tray Toggle (Shift+Cmd+Space)
// - JS Local Global: For all other app shortcuts (Cmd+, Cmd+N, etc.)
//   This ensures sandbox compatibility while providing "global-like" feel when focused.

use crate::components::chat_input::ChatCommand;
use crate::settings::Settings;
use crate::{permissions, tray::WINDOW_VISIBLE};
use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use futures_util::{SinkExt, StreamExt};
use global_hotkey::hotkey::HotKey;
use std::cell::RefCell;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub enum HotkeyAction {
    ToggleTray,
}

pub fn use_hotkey_manager(permission_status: Signal<permissions::PermissionStatus>) {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();

    // Coroutine to handle actions from the Native Global Hotkey (Tray Toggle)
    let tray_action_handler = use_coroutine(|mut rx: UnboundedReceiver<HotkeyAction>| async move {
        let mut last_trigger = std::time::Instant::now();
        // Initialize simple debounce mechanism

        while let Some(action) = rx.next().await {
            // Debounce: Ignore events if they happen too close together (e.g. < 300ms)
            if last_trigger.elapsed() < std::time::Duration::from_millis(300) {
                tracing::debug!("Debouncing global hotkey trigger");
                continue;
            }

            last_trigger = std::time::Instant::now();
            tracing::info!("Global Hotkey triggered: {:?}", action);
            match action {
                HotkeyAction::ToggleTray => {
                    let mut visible = WINDOW_VISIBLE.write();
                    *visible = !*visible;
                }
            }
        }
    });

    // --- Part 1: Native Global Hotkey (Tray Toggle ONLY) ---
    // This survives even when the app is hidden, vital for "summoning" the app.
    let current_tray_shortcut = use_hook(|| RefCell::new(None::<ShortcutHandle>));

    use_effect(move || {
        // Only attempt to register native hotkeys if permissions are granted.
        if matches!(
            *permission_status.read(),
            permissions::PermissionStatus::Granted
        ) {
            let hotkey_str = settings.read().hotkeys.toggle_tray.clone();
            let tx = tray_action_handler.tx();

            // Unregister old shortcut
            if let Some(handle) = current_tray_shortcut.borrow_mut().take() {
                desktop.remove_shortcut(handle);
            }

            if let Ok(hotkey) = HotKey::from_str(&hotkey_str) {
                // We access the static directly for the callback to keep it simple and thread-safe
                if let Ok(handle) = desktop.create_shortcut(hotkey, move || {
                    let _ = tx.unbounded_send(HotkeyAction::ToggleTray);
                }) {
                    *current_tray_shortcut.borrow_mut() = Some(handle);
                    tracing::info!("Registered Native Global Hotkey: {}", &hotkey_str);
                } else {
                    tracing::error!("Failed to register Tray Hotkey: {}", &hotkey_str);
                }
            }
        } else if let Some(handle) = current_tray_shortcut.borrow_mut().take() {
            desktop.remove_shortcut(handle);
        }
    });

    // --- Part 2: JavaScript "Local Global" Listener (The Hybrid Bridge) ---
    // Receives events from the WebView when keys are pressed and NOT handled by components.
    //
    // RESERVED HOTKEY COMBINATIONS (hardcoded in JS, not user-configurable):
    //   Cmd+W / Ctrl+W          → Close Tab
    //   Cmd+Backspace / Delete  → Delete Session
    //   Cmd+1..9                → Switch Tab (by index)
    //   Control+1..9            → Switch Model (by index)
    //   Cmd+Option+1..9         → Switch Profile (by index)
    //   Cmd+Option+Shift+1..9   → Switch Provider (by index)
    //
    // User-configurable hotkeys (from settings.hotkeys) are checked FIRST in the JS listener.
    // If a configurable hotkey collides with a reserved one, the configurable one wins (shadows).
    // This is acceptable because it requires deliberate user misconfiguration.
    let js_action_handler = use_coroutine(move |mut rx: UnboundedReceiver<String>| async move {
        while let Some(msg) = rx.next().await {
            match msg.as_str() {
                "toggle_settings" => chat_command.set(Some(ChatCommand::ToggleSettings)),
                "toggle_history" => chat_command.set(Some(ChatCommand::ToggleHistory)),
                "toggle_mcp" => chat_command.set(Some(ChatCommand::ToggleMcp)),
                "toggle_profile" => chat_command.set(Some(ChatCommand::ToggleProfile)),
                "toggle_provider" => chat_command.set(Some(ChatCommand::ToggleProviderSelector)),
                "open_attachments" => chat_command.set(Some(ChatCommand::OpenAttachments)),
                "new_chat" => chat_command.set(Some(ChatCommand::NewChat)),
                "new_chat_memory" => chat_command.set(Some(ChatCommand::NewChatWithMemory)),
                "focus_chat" => chat_command.set(Some(ChatCommand::FocusChat)),
                "scroll_bottom" => chat_command.set(Some(ChatCommand::ScrollToBottom)),
                "delete_session" => {
                    let session_state = consume_context::<Signal<crate::session::SessionState>>();
                    let active_id = session_state.read().active_session_id.clone();
                    if !active_id.is_empty() {
                        chat_command.set(Some(ChatCommand::DeleteSession(active_id)));
                    }
                }
                "cancel_generation" => chat_command.set(Some(ChatCommand::CancelGeneration)),
                "close_tab" => chat_command.set(Some(ChatCommand::CloseTab)),
                // Profile switching (session-local)
                s if s.starts_with("switch_profile_") => {
                    if let Ok(idx) = s.replace("switch_profile_", "").parse::<usize>() {
                        chat_command.set(Some(ChatCommand::SwitchProfile(idx)));
                    }
                }
                s if s.starts_with("switch_tab_") => {
                    if let Ok(idx) = s.replace("switch_tab_", "").parse::<usize>() {
                        chat_command.set(Some(ChatCommand::SwitchTab(idx)));
                    }
                }
                s if s.starts_with("switch_model_") => {
                    if let Ok(idx) = s.replace("switch_model_", "").parse::<usize>() {
                        chat_command.set(Some(ChatCommand::SwitchModel(idx)));
                    }
                }
                s if s.starts_with("switch_provider_") => {
                    if let Ok(idx) = s.replace("switch_provider_", "").parse::<usize>() {
                        // Hotkey index maps to position in the canonical connector list
                        let connector_id = settings
                            .peek()
                            .llm_connectors
                            .get(idx)
                            .map(|c| c.id.clone());
                        if let Some(id) = connector_id {
                            chat_command.set(Some(ChatCommand::SwitchConnector(id)));
                        } else {
                            tracing::debug!("switch_provider_{}: no connector at index", idx);
                        }
                    }
                }
                _ => tracing::warn!("Unknown JS hotkey action: {}", msg),
            }
        }
    });

    // Inject the Listener Logic
    use_effect(move || {
        let hotkeys = settings.read().hotkeys.clone();
        let hotkey_json = serde_json::to_string(&hotkeys).unwrap_or_default();
        let mut tx = js_action_handler.tx();

        spawn(async move {
            let js_code = format!(
                r#"
                window.hobbes_hotkey_config = {{}};
                try {{
                    window.hobbes_hotkey_config = {};
                }} catch (e) {{
                   console.error("Failed to parse hotkey config", e);
                }}

                if (!window.hobbes_hotkey_listener) {{
                    window.hobbes_hotkey_listener = function(event) {{
                        // CRITICAL: Respect Scope - If event was already handled (e.g. by ChatInput), ignore.
                        if (event.defaultPrevented) return;

                        // CRITICAL: Focus Check - If focused on an input/textarea, ignore keys unless they have a command modifier (Cmd or Ctrl).
                        // This prevents global hotkeys from intercepting typing while still allowing shortcuts like Cmd+,
                        const isInput = ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName) || document.activeElement.isContentEditable;
                        if (isInput && !event.metaKey && !event.ctrlKey) return;
    
                        let config = window.hobbes_hotkey_config;
                        if (!config) return;
    
                        // Helper to match accelerators
                        const check = (setting, e) => {{
                            if (!setting) return false;
                            const parts = setting.toLowerCase().split('+');
                            let matchMeta = parts.includes('cmdorctrl') || parts.includes('cmd') || parts.includes('ctrl');
                            let matchShift = parts.includes('shift');
                            let matchAlt = parts.includes('alt') || parts.includes('option');
                            
                            // Check modifiers
                            if (matchMeta !== (e.metaKey || e.ctrlKey)) return false;
                            if (matchShift !== e.shiftKey) return false;
                            if (matchAlt !== e.altKey) return false;
    
                            // Check key code/key
                            const lastPart = parts[parts.length - 1];
                            
                            // Special keys mapping
                            if (lastPart === ',') return e.key === ',';
                            if (lastPart === '/') return e.key === '/';
                            if (lastPart === '.') return e.key === '.';
                            if (lastPart === 'space') return e.code === 'Space';
                            if (lastPart === 'arrowdown') return e.key === 'ArrowDown' || e.code === 'ArrowDown';
                            
                            // Standard check for character keys
                            if (e.key.toLowerCase() === lastPart) return true;

                            // Fallback for Option/Alt keys on Mac (which change the char, e.g. Option+N -> ~)
                            if (matchAlt && e.code === ('Key' + lastPart.toUpperCase())) return true;

                            return false;
                        }};
    
                        let action = null;
                        if (check(config.toggle_settings, event)) action = "toggle_settings";
                        else if (check(config.toggle_history, event)) action = "toggle_history";
                        else if (check(config.toggle_mcp, event)) action = "toggle_mcp";
                        else if (check(config.toggle_profile, event)) action = "toggle_profile";
                        else if (check(config.toggle_provider, event)) action = "toggle_provider";
                        else if (check(config.toggle_attachments, event)) action = "open_attachments";
                        else if (check(config.toggle_new_chat, event)) action = "new_chat";
                        else if (check(config.toggle_new_chat_with_memory, event)) action = "new_chat_memory";
                        else if (check(config.toggle_focus_chat, event)) action = "focus_chat";
                        else if (check(config.toggle_scroll_to_bottom, event)) action = "scroll_bottom";
                        else if (check(config.cancel_generation, event)) action = "cancel_generation";
                        
                        // Delete Session (Cmd+Backspace or Cmd+Delete) — only when NOT focused on an input.
                        // When focused, Cmd+Backspace is macOS "delete to beginning of line" and must pass through.
                        else if (!isInput && (event.metaKey || event.ctrlKey) && (event.key === 'Backspace' || event.key === 'Delete')) {{
                            action = "delete_session";
                        }}
                        
                        // Close Tab (Cmd+W or Ctrl+W)
                        else if ((event.metaKey || event.ctrlKey) && !event.shiftKey && !event.altKey && event.key.toLowerCase() === 'w') {{
                            action = "close_tab";
                        }}
                        
                        else if (check(config.switch_tab_1, event)) action = "switch_tab_0";
                        else if (check(config.switch_tab_2, event)) action = "switch_tab_1";
                        else if (check(config.switch_tab_3, event)) action = "switch_tab_2";
                        else if (check(config.switch_tab_4, event)) action = "switch_tab_3";
                        else if (check(config.switch_tab_5, event)) action = "switch_tab_4";
                        else if (check(config.switch_tab_6, event)) action = "switch_tab_5";
                        else if (check(config.switch_tab_7, event)) action = "switch_tab_6";
                        else if (check(config.switch_tab_8, event)) action = "switch_tab_7";
                        else if (check(config.switch_tab_9, event)) action = "switch_tab_8";
                        // Model switching: Control+1..9 (Control only, no Cmd/Meta, no Alt, no Shift)
                        // MUST be checked before Tab fallback which also matches ctrlKey
                        else if (event.ctrlKey && !event.metaKey && !event.shiftKey && !event.altKey) {{
                             if (event.key >= '1' && event.key <= '9') {{
                                 action = "switch_model_" + (parseInt(event.key) - 1);
                             }}
                        }}
                        // Tab switching: Cmd+1..9 (industry standard, no modifiers required)
                        else if ((event.metaKey || event.ctrlKey) && !event.shiftKey && !event.altKey) {{
                             if (event.key >= '1' && event.key <= '9') {{
                                 action = "switch_tab_" + (parseInt(event.key) - 1);
                             }}
                        }}
                        // Profile switching: Cmd+Option+1..9
                        // event.code fallback: with Option held, macOS composes event.key
                        // into a symbol (e.g. Option+1 -> "¡"), so the digit check alone
                        // never matches on mac keyboards.
                        else if ((event.metaKey || event.ctrlKey) && !event.shiftKey && event.altKey) {{
                             const profileDigit = (event.key >= '1' && event.key <= '9')
                                 ? event.key
                                 : ((event.code || '').match(/^Digit([1-9])$/) || [])[1];
                             if (profileDigit) {{
                                 action = "switch_profile_" + (parseInt(profileDigit) - 1);
                             }}
                        }}
                        // Provider switching: Cmd+Option+Shift+1..9
                        // Use event.code: with Option+Shift held, event.key is a composed
                        // character on macOS (e.g. Option+Shift+1 -> "⁄"), never the digit.
                        else if ((event.metaKey || event.ctrlKey) && event.shiftKey && event.altKey) {{
                             const providerDigit = ((event.code || '').match(/^Digit([1-9])$/) || [])[1];
                             if (providerDigit) {{
                                 action = "switch_provider_" + (parseInt(providerDigit) - 1);
                             }}
                        }}
    
                        if (action) {{
                            event.preventDefault(); 
                            dioxus.send(action);
                        }}
                    }};
                    
                    window.addEventListener('keydown', window.hobbes_hotkey_listener);
                }}
            "#,
                hotkey_json
            );

            // Create the eval context that listens for messages
            let mut eval = document::eval(&js_code);

            // Relay messages to the Dioxus handler
            while let Ok(msg) = eval.recv::<String>().await {
                let _ = tx.send(msg).await;
            }
        });
    });
}

/// Checks if a Dioxus KeyboardEvent matches a hotkey string (e.g. "CmdOrCtrl+Enter").
pub fn matches_hotkey(evt: &KeyboardEvent, hotkey_str: &str) -> bool {
    matches_hotkey_internal(evt.modifiers(), &evt.key(), hotkey_str)
}

/// Internal logic for hotkey matching, decoupled from Event wrapper for testing.
pub fn matches_hotkey_internal(modifiers: Modifiers, key: &Key, hotkey_str: &str) -> bool {
    let parts: Vec<&str> = hotkey_str.split('+').collect();
    if parts.is_empty() {
        return false;
    }

    // Parsing Config
    let has_cmd_or_ctrl = parts.iter().any(|p| {
        let s = p.to_lowercase();
        s == "cmdorctrl" || s == "cmd" || s == "ctrl" || s == "meta" || s == "control"
    });
    let has_shift = parts.iter().any(|p| p.to_lowercase() == "shift");
    let has_alt = parts.iter().any(|p| {
        let s = p.to_lowercase();
        s == "alt" || s == "option"
    });

    // Parsing Event
    let evt_cmd = modifiers.contains(Modifiers::SUPER);
    let evt_ctrl = modifiers.contains(Modifiers::CONTROL);
    let evt_shift = modifiers.contains(Modifiers::SHIFT);
    let evt_alt = modifiers.contains(Modifiers::ALT);

    // 1. Modifier Check
    // The presence of the modifier in config must match the presence in event.
    // "CmdOrCtrl" conflates Command and Control into a single "Primary" modifier.
    if has_cmd_or_ctrl != (evt_cmd || evt_ctrl) {
        return false;
    }
    if has_shift != evt_shift {
        return false;
    }
    if has_alt != evt_alt {
        return false;
    }

    // 2. Key Check
    // The last part implies the key.
    if let Some(target_key_part) = parts.last() {
        let target = target_key_part.trim().to_lowercase();
        match target.as_str() {
            "enter" => *key == Key::Enter,
            "escape" | "esc" => *key == Key::Escape,
            "backspace" => *key == Key::Backspace,
            "delete" => *key == Key::Delete,
            "arrowup" => *key == Key::ArrowUp,
            "arrowdown" => *key == Key::ArrowDown,
            "arrowleft" => *key == Key::ArrowLeft,
            "arrowright" => *key == Key::ArrowRight,
            "tab" => *key == Key::Tab,
            "space" => {
                matches!(key, Key::Character(c) if c == " ")
                    || *key == Key::Character("Space".to_string())
            }
            k => {
                // Character match (case-insensitive)
                match key {
                    Key::Character(c) => c.to_lowercase() == k,
                    _ => false,
                }
            }
        }
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_hotkeys() {
        // Cmd+Enter
        assert!(matches_hotkey_internal(
            Modifiers::SUPER,
            &Key::Enter,
            "CmdOrCtrl+Enter"
        ));

        // Ctrl+Enter (should also match CmdOrCtrl)
        assert!(matches_hotkey_internal(
            Modifiers::CONTROL,
            &Key::Enter,
            "CmdOrCtrl+Enter"
        ));

        // Shift+Enter != Cmd+Enter
        assert!(!matches_hotkey_internal(
            Modifiers::SHIFT,
            &Key::Enter,
            "CmdOrCtrl+Enter"
        ));

        // Cmd+Shift+Enter
        assert!(matches_hotkey_internal(
            Modifiers::SUPER | Modifiers::SHIFT,
            &Key::Enter,
            "CmdOrCtrl+Shift+Enter"
        ));

        // Cmd+S
        assert!(matches_hotkey_internal(
            Modifiers::SUPER,
            &Key::Character("s".to_string()),
            "CmdOrCtrl+S"
        ));

        // Case insensitive S
        assert!(matches_hotkey_internal(
            Modifiers::SUPER,
            &Key::Character("S".to_string()),
            "CmdOrCtrl+s"
        ));
    }

    #[test]
    fn test_period_hotkey() {
        // Cmd+. (Cancel Generation)
        assert!(matches_hotkey_internal(
            Modifiers::SUPER,
            &Key::Character(".".to_string()),
            "CmdOrCtrl+."
        ));
    }
}
