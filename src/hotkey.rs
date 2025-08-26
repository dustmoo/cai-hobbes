use crate::{permissions, tray::WINDOW_VISIBLE};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use tracing;

/// Initializes the hotkey manager, registers the hotkey, and spawns the listener thread.
/// This should be called once from the main application event loop.
pub fn init_hotkeys() {
    if !permissions::check_and_prompt_for_accessibility() {
        tracing::warn!("Hotkeys disabled due to missing Accessibility permissions.");
        return;
    }

    match GlobalHotKeyManager::new() {
        Ok(manager) => {
            let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SUPER), Code::KeyH);
            if let Err(e) = manager.register(hotkey) {
                tracing::error!("Failed to register hotkey: {:?}", e);
                return;
            }
            tracing::info!("Hotkey registered successfully!");

            // Spawn a thread to listen for hotkey events.
            std::thread::spawn(|| {
                let receiver = GlobalHotKeyEvent::receiver();
                tracing::info!("Hotkey listener thread started.");
                loop {
                    if let Ok(event) = receiver.recv() {
                        if event.state == global_hotkey::HotKeyState::Pressed {
                            let mut visible = WINDOW_VISIBLE.write();
                            *visible = !*visible;
                            tracing::info!("Hotkey pressed, toggling visibility to: {}", !*visible);
                        }
                    }
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to create GlobalHotKeyManager: {:?}", e);
        }
    }
}