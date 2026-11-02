//! The voice hotkey: a native global shortcut (works while Hobbes is in the
//! background). Tap = start/end a conversation; hold = push-to-talk.
//!
//! Dioxus calls a shortcut's callback on BOTH key-down and key-up without
//! saying which (`dioxus-desktop` `ShortcutRegistry::call_handlers` doesn't
//! pass `HotKeyState`), and Carbon delivers them strictly in pairs, so the
//! callback alternates press / release. A press stuck longer than
//! [`STUCK_AFTER`] (a lost key-up) is treated as released, which resyncs.

use std::cell::RefCell;
use std::str::FromStr;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use dioxus_desktop::{DesktopContext, ShortcutHandle};
use global_hotkey::hotkey::HotKey;

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
    /// One callback from the shortcut: the command it means.
    pub fn on_event(&mut self, now: Instant) -> VoiceCmd {
        match self.down_since.take() {
            Some(t) if now.duration_since(t) < STUCK_AFTER => VoiceCmd::Release {
                held_ms: now.duration_since(t).as_millis() as u64,
            },
            _ => {
                self.down_since = Some(now);
                VoiceCmd::Press
            }
        }
    }
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
        match desktop.create_shortcut(hotkey, move || {
            let cmd = tracker.borrow_mut().on_event(Instant::now());
            controller::send(cmd);
        }) {
            Ok(h) => {
                *handle.borrow_mut() = Some(h);
                tracing::info!("voice: hotkey registered ({hotkey_str})");
            }
            Err(e) => {
                tracing::warn!("voice: hotkey {hotkey_str} unavailable: {e:?}");
                controller::VOICE_UI.write().error = Some(format!(
                    "{HOTKEY_ERROR_PREFIX} {hotkey_str} is taken by another app. Pick another in Settings → Voice."
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
        assert_eq!(t.on_event(t0), VoiceCmd::Press);
        assert_eq!(t.on_event(t0 + Duration::from_millis(120)), VoiceCmd::Release { held_ms: 120 });
        assert_eq!(t.on_event(t0 + Duration::from_secs(1)), VoiceCmd::Press);
    }

    #[test]
    fn a_lost_key_up_resyncs() {
        let mut t = PressTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.on_event(t0), VoiceCmd::Press);
        // The key-up never came; the next event is a fresh press.
        assert_eq!(t.on_event(t0 + STUCK_AFTER + Duration::from_secs(1)), VoiceCmd::Press);
    }
}
