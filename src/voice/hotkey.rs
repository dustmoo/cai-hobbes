//! The voice hotkey: a native global shortcut (works while Hobbes is in the
//! background). Tap = start/end a conversation; hold = push-to-talk.
//!
//! Dioxus calls a shortcut's callback on BOTH key-down and key-up without
//! saying which (`dioxus-desktop` `ShortcutRegistry::call_handlers` doesn't
//! pass `HotKeyState`), and Carbon delivers them strictly in pairs, so the
//! callback alternates press / release. A lost key-up would swap them from
//! then on (the mic open while the key is up), so two things resync:
//! - on macOS, a callback the alternation calls a release while the key is
//!   physically down is a new press ([`key_down_now`]);
//! - a press stuck longer than [`STUCK_AFTER`] is treated as released.

use std::cell::RefCell;
use std::str::FromStr;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::{Code, HotKey};

use crate::settings::Settings;
use crate::voice::controller::{self, VoiceCmd};

/// A key-down with no key-up for this long is considered lost.
const STUCK_AFTER: Duration = Duration::from_secs(120);

/// Alternating press/release tracker (pure, for tests).
#[derive(Default)]
pub struct PressTracker {
    down_since: Option<Instant>,
}

impl PressTracker {
    /// One callback from the shortcut: the command it means. `key_down`:
    /// the hotkey's key is physically down right now, as far as the OS can
    /// say (`false` when it can't). Only a "down" reading overrides the
    /// alternation: a quick tap's key-down callback can run after the key
    /// is already up, so an "up" reading proves nothing.
    pub fn on_event(&mut self, now: Instant, key_down: bool) -> VoiceCmd {
        match self.down_since.take() {
            Some(t) if !key_down && now.duration_since(t) < STUCK_AFTER => VoiceCmd::Release {
                held_ms: now.duration_since(t).as_millis() as u64,
            },
            // Not down yet, stuck past `STUCK_AFTER`, or "released" while
            // the key is down (its key-up was lost): a fresh press.
            _ => {
                self.down_since = Some(now);
                VoiceCmd::Press
            }
        }
    }
}

/// Whether `key` is physically down now (macOS; `false` elsewhere, for keys
/// without a mapping, or when the OS won't say).
fn key_down_now(key: Code) -> bool {
    #[cfg(target_os = "macos")]
    {
        mac_keycode(key).is_some_and(mac_key_down)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        false
    }
}

#[cfg(target_os = "macos")]
fn mac_key_down(keycode: u16) -> bool {
    /// `kCGEventSourceStateHIDSystemState`: the hardware state.
    const HID_SYSTEM_STATE: i32 = 1;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    }
    // SAFETY: a pure query with plain-value arguments; no pointers.
    unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, keycode) }
}

/// macOS virtual key code (`kVK_*`) for the keys a voice hotkey is likely
/// to use. Others return `None` (alternation only).
#[cfg(any(target_os = "macos", test))]
fn mac_keycode(key: Code) -> Option<u16> {
    Some(match key {
        Code::KeyA => 0x00,
        Code::KeyS => 0x01,
        Code::KeyD => 0x02,
        Code::KeyF => 0x03,
        Code::KeyH => 0x04,
        Code::KeyG => 0x05,
        Code::KeyZ => 0x06,
        Code::KeyX => 0x07,
        Code::KeyC => 0x08,
        Code::KeyV => 0x09,
        Code::KeyB => 0x0b,
        Code::KeyQ => 0x0c,
        Code::KeyW => 0x0d,
        Code::KeyE => 0x0e,
        Code::KeyR => 0x0f,
        Code::KeyY => 0x10,
        Code::KeyT => 0x11,
        Code::Digit1 => 0x12,
        Code::Digit2 => 0x13,
        Code::Digit3 => 0x14,
        Code::Digit4 => 0x15,
        Code::Digit6 => 0x16,
        Code::Digit5 => 0x17,
        Code::Equal => 0x18,
        Code::Digit9 => 0x19,
        Code::Digit7 => 0x1a,
        Code::Minus => 0x1b,
        Code::Digit8 => 0x1c,
        Code::Digit0 => 0x1d,
        Code::BracketRight => 0x1e,
        Code::KeyO => 0x1f,
        Code::KeyU => 0x20,
        Code::BracketLeft => 0x21,
        Code::KeyI => 0x22,
        Code::KeyP => 0x23,
        Code::Enter => 0x24,
        Code::KeyL => 0x25,
        Code::KeyJ => 0x26,
        Code::Quote => 0x27,
        Code::KeyK => 0x28,
        Code::Semicolon => 0x29,
        Code::Backslash => 0x2a,
        Code::Comma => 0x2b,
        Code::Slash => 0x2c,
        Code::KeyN => 0x2d,
        Code::KeyM => 0x2e,
        Code::Period => 0x2f,
        Code::Tab => 0x30,
        Code::Space => 0x31,
        Code::Backquote => 0x32,
        Code::Escape => 0x35,
        Code::F1 => 0x7a,
        Code::F2 => 0x78,
        Code::F3 => 0x63,
        Code::F4 => 0x76,
        Code::F5 => 0x60,
        Code::F6 => 0x61,
        Code::F7 => 0x62,
        Code::F8 => 0x64,
        Code::F9 => 0x65,
        Code::F10 => 0x6d,
        Code::F11 => 0x67,
        Code::F12 => 0x6f,
        Code::F13 => 0x69,
        Code::F14 => 0x6b,
        Code::F15 => 0x71,
        Code::F16 => 0x6a,
        Code::F17 => 0x40,
        Code::F18 => 0x4f,
        Code::F19 => 0x50,
        Code::F20 => 0x5a,
        _ => return None,
    })
}

/// Register / re-register the voice hotkey when it (or the master switch)
/// changes — a memo, so unrelated settings saves don't re-register it. The
/// press tracker lives across re-registrations so a save while the key is
/// held can't flip press / release.
pub fn use_voice_hotkey() {
    let desktop = use_context::<DesktopContext>();
    let settings = use_context::<Signal<Settings>>();
    let handle = use_hook(|| std::rc::Rc::new(RefCell::new(None::<ShortcutHandle>)));
    let tracker = use_hook(|| std::rc::Rc::new(RefCell::new(PressTracker::default())));
    let wanted = use_memo(move || {
        let s = settings.read();
        (crate::voice::voice_active(&s), s.voice.hotkey.clone())
    });

    use_effect(move || {
        let (active, hotkey_str) = wanted.read().clone();
        if let Some(h) = handle.borrow_mut().take() {
            desktop.remove_shortcut(h);
        }
        clear_hotkey_error();
        if !active {
            return;
        }
        let hotkey = match HotKey::from_str(&hotkey_str) {
            Ok(h) => h,
            Err(e) => {
                controller::VOICE_UI.write().error =
                    Some(format!("{HOTKEY_ERROR_PREFIX} \"{hotkey_str}\" isn't valid: {e}"));
                return;
            }
        };
        let tracker = tracker.clone();
        let key = hotkey.key;
        match desktop.create_shortcut(hotkey, move || {
            let cmd = tracker.borrow_mut().on_event(Instant::now(), key_down_now(key));
            controller::send(cmd);
        }) {
            Ok(h) => {
                *handle.borrow_mut() = Some(h);
                tracing::info!("voice: hotkey registered ({hotkey_str})");
            }
            Err(e) => {
                tracing::warn!("voice: hotkey {hotkey_str} unavailable: {e:?}");
                controller::VOICE_UI.write().error = Some(format!(
                    "{HOTKEY_ERROR_PREFIX} {hotkey_str} is taken by another app. Pick another in Settings → Hobbes."
                ));
            }
        }
    });
}

const HOTKEY_ERROR_PREFIX: &str = "The voice hotkey";

/// Drop a stale hotkey error (a later registration supersedes it).
fn clear_hotkey_error() {
    let stale = controller::VOICE_UI
        .peek()
        .error
        .as_deref()
        .is_some_and(|e| e.starts_with(HOTKEY_ERROR_PREFIX));
    if stale {
        controller::VOICE_UI.write().error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_alternate_press_and_release() {
        let mut t = PressTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.on_event(t0, true), VoiceCmd::Press);
        assert_eq!(t.on_event(t0 + Duration::from_millis(120), false), VoiceCmd::Release { held_ms: 120 });
        assert_eq!(t.on_event(t0 + Duration::from_secs(1), true), VoiceCmd::Press);
    }

    #[test]
    fn a_quick_tap_read_late_still_alternates() {
        // The key-down callback ran after the key was already up: an "up"
        // reading must not swallow the press.
        let mut t = PressTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.on_event(t0, false), VoiceCmd::Press);
        assert_eq!(t.on_event(t0 + Duration::from_millis(5), false), VoiceCmd::Release { held_ms: 5 });
    }

    #[test]
    fn a_lost_key_up_resyncs_after_the_stuck_window() {
        let mut t = PressTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.on_event(t0, true), VoiceCmd::Press);
        // The key-up never came; the next event is a fresh press.
        assert_eq!(t.on_event(t0 + STUCK_AFTER + Duration::from_secs(1), false), VoiceCmd::Press);
    }

    #[test]
    fn a_lost_key_up_resyncs_on_the_next_press() {
        let mut t = PressTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.on_event(t0, true), VoiceCmd::Press);
        // Key-up lost. The next real key-down (key physically down) is a
        // press, not a release, and its key-up a release: no swap.
        assert_eq!(t.on_event(t0 + Duration::from_secs(10), true), VoiceCmd::Press);
        assert_eq!(
            t.on_event(t0 + Duration::from_secs(10) + Duration::from_millis(400), false),
            VoiceCmd::Release { held_ms: 400 }
        );
    }

    #[test]
    fn the_default_hotkey_key_is_mapped() {
        let hk = HotKey::from_str(&crate::settings::Settings::default().voice.hotkey).unwrap();
        assert_eq!(mac_keycode(hk.key), Some(0x31));
        assert_eq!(mac_keycode(Code::KeyA), Some(0x00));
        assert_eq!(mac_keycode(Code::F1), Some(0x7a));
    }
}
