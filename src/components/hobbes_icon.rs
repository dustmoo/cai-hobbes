//! The Hobbes tab's icon: the pixel tiger ([`TIGER_GRID`]), animated by
//! state with frame swaps and light CSS — no badges.
//!
//! - Idle: the tiger, with an occasional blink or ear-perk (calmer than the
//!   app menu's [`crate::components::pixel_icons::AnimatedTiger`]).
//! - Heartbeat on: a slow breath (`.hobbes-tab-breath`).
//! - Heartbeat waking (a Heartbeat-origin turn streaming in the session):
//!   ears up, with a pulsing green glow (`.hobbes-tab-waking`).
//! - Voice listening (mic live): ears up, with a soft blue glow
//!   (`.hobbes-tab-listening`).
//! - Voice speaking: ears up and the mouth flapping
//!   ([`TIGER_GRID_MOUTH_OPEN`]), with the blue glow.
//!
//! The `title` carries the state in words. Archives of the pre-merge
//! Heartbeat and Voice tabs keep their metal heart and mouth.

use std::time::Duration;

use dioxus::prelude::*;

use crate::components::metal_heart::MetalHeart;
use crate::components::metal_mouth::MetalMouth;
use crate::components::pixel_icons::{
    HobbesTigerFrame, TIGER_GRID, TIGER_GRID_BLINK, TIGER_GRID_EARS_UP, TIGER_GRID_MOUTH_OPEN,
};
use crate::hobbes_session::{LEGACY_HEARTBEAT_EMOJI, LEGACY_VOICE_EMOJI};
use crate::voice::controller::{Phase, VOICE_UI};

/// The Hobbes session's id, as a memo: a tab re-renders when the id is
/// assigned or rotated, not on unrelated writes.
pub fn use_hobbes_session_id() -> Memo<Option<String>> {
    use_memo(crate::system_sessions::current_reactive)
}

/// Whether a tab shows a system icon: the live Hobbes session (read through
/// the memo, so the tab re-renders when the id changes) or an archive of its
/// lineage (`system_sessions`), which keeps its icon.
pub fn is_system_tab(hobbes_id: Memo<Option<String>>, session_id: &str) -> bool {
    hobbes_id.read().as_deref() == Some(session_id) || crate::system_sessions::is_system(session_id)
}

/// What the live Hobbes tab is doing, most salient first.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabMood {
    /// Voice is talking.
    Speaking,
    /// The mic is live.
    Listening,
    /// A Heartbeat wake's turn is streaming.
    Waking,
    /// Nothing in progress (the Heartbeat may still be on).
    Idle,
}

impl TabMood {
    pub fn of(phase: Phase, mic_on: bool, waking: bool) -> Self {
        if phase == Phase::Speaking {
            TabMood::Speaking
        } else if mic_on {
            TabMood::Listening
        } else if waking {
            TabMood::Waking
        } else {
            TabMood::Idle
        }
    }
}

/// The tooltip: the state in words, since the animation is not a label.
pub fn tab_title(mood: TabMood, heartbeat_on: bool, voice_on: bool) -> String {
    let mut parts = vec!["Hobbes"];
    if mood == TabMood::Waking {
        parts.push("Heartbeat waking");
    } else if heartbeat_on {
        parts.push("Heartbeat on");
    }
    match mood {
        TabMood::Speaking => parts.push("Voice speaking"),
        TabMood::Listening => parts.push("Voice listening"),
        _ if voice_on => parts.push("Voice on"),
        _ => {}
    }
    parts.join(" · ")
}

/// One beat of the sprite loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Beat {
    Rest,
    Blink,
    EarsUp,
    MouthOpen,
}

/// The loop's tick.
const TICK: Duration = Duration::from_millis(150);

/// The calm idle schedule, in ticks (~19 s per 128): three blinks and one
/// short ear-perk, so a tab-sized tiger stays alive without pulling the eye.
fn idle_beat(tick: u64) -> Beat {
    match tick % 128 {
        22 | 86 | 118 => Beat::Blink,
        60..=62 => Beat::EarsUp,
        _ => Beat::Rest,
    }
}

/// The grid to draw for a mood and beat.
fn grid_for(mood: TabMood, beat: Beat) -> &'static [&'static str; 16] {
    match (mood, beat) {
        (TabMood::Speaking, Beat::MouthOpen) => &TIGER_GRID_MOUTH_OPEN,
        (TabMood::Speaking | TabMood::Listening | TabMood::Waking, _) => &TIGER_GRID_EARS_UP,
        (TabMood::Idle, Beat::Blink) => &TIGER_GRID_BLINK,
        (TabMood::Idle, Beat::EarsUp) => &TIGER_GRID_EARS_UP,
        (TabMood::Idle, _) => &TIGER_GRID,
    }
}

/// The CSS classes for a mood (`assets/main.css`).
fn class_for(mood: TabMood, heartbeat_on: bool) -> &'static str {
    match mood {
        TabMood::Speaking | TabMood::Listening => "hobbes-tab-tiger hobbes-tab-listening",
        TabMood::Waking => "hobbes-tab-tiger hobbes-tab-waking",
        TabMood::Idle if heartbeat_on => "hobbes-tab-tiger hobbes-tab-breath",
        TabMood::Idle => "hobbes-tab-tiger",
    }
}

/// A system tab's icon. `emoji` is the session's tab emoji: a pre-merge
/// archive is told apart by it.
#[component]
pub fn SystemTabIcon(
    session_id: String,
    emoji: Option<String>,
    #[props(default = 13)] size: u32,
) -> Element {
    let hobbes_id = use_hobbes_session_id();
    let settings = try_consume_context::<Signal<crate::settings::Settings>>();
    let stream = try_consume_context::<crate::components::stream_manager::StreamManagerContext>();
    let sessions = try_consume_context::<Signal<crate::session::SessionState>>();

    // Narrow subscriptions only: the voice phase and mic (not the level
    // meter), the two capability flags, and whether a wake is streaming.
    // SessionState is only peeked.
    let voice = use_memo(|| {
        let ui = VOICE_UI.read();
        (ui.phase, ui.mic_on)
    });
    let flags = use_memo(move || match settings {
        Some(s) => {
            let s = s.read();
            (crate::heartbeat::heartbeat_active(&s), crate::voice::voice_active(&s))
        }
        None => (false, false),
    });
    let waking = use_memo(move || {
        let Some(id) = hobbes_id.read().clone() else { return false };
        // The Hobbes session also runs typed and voice turns: only an
        // unattended (wake) turn is the Heartbeat working.
        stream.is_some_and(|sm| sm.streaming_sessions.read().contains(&id))
            && sessions.is_some_and(|st| {
                st.peek().sessions.get(&id).is_some_and(|s| s.turn_origin.is_unattended())
            })
    });
    let live = use_memo(use_reactive(&session_id, move |sid| {
        hobbes_id.read().as_deref() == Some(sid.as_str())
    }));
    let mood = use_memo(move || {
        if !live() {
            return TabMood::Idle;
        }
        let (phase, mic_on) = voice();
        TabMood::of(phase, mic_on, waking())
    });

    // One timer per mounted icon, owned by this scope so it ends on unmount.
    // It writes `beat` only when the frame changes.
    let mut beat = use_signal(|| Beat::Rest);
    use_future(move || async move {
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(TICK).await;
            tick = tick.wrapping_add(1);
            let next = if *mood.peek() == TabMood::Speaking {
                if tick.is_multiple_of(2) { Beat::MouthOpen } else { Beat::Rest }
            } else {
                idle_beat(tick)
            };
            if *beat.peek() != next {
                beat.set(next);
            }
        }
    });

    match emoji.as_deref() {
        Some(LEGACY_HEARTBEAT_EMOJI) if !live() => return rsx! { MetalHeart { size } },
        Some(LEGACY_VOICE_EMOJI) if !live() => return rsx! { MetalMouth { size } },
        _ => {}
    }

    let (heartbeat_on, voice_on) = if live() { flags() } else { (false, false) };
    let mood = mood();
    let grid = grid_for(mood, beat());
    let title = tab_title(mood, heartbeat_on, voice_on);

    rsx! {
        span {
            class: "{class_for(mood, heartbeat_on)}",
            style: "width: {size}px; height: {size}px;",
            title: "{title}",
            dioxus_free_icons::Icon { width: size, height: size, icon: HobbesTigerFrame(grid) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaking_outranks_listening_outranks_waking() {
        assert_eq!(TabMood::of(Phase::Speaking, true, true), TabMood::Speaking);
        assert_eq!(TabMood::of(Phase::Listening, true, true), TabMood::Listening);
        assert_eq!(TabMood::of(Phase::Idle, false, true), TabMood::Waking);
        assert_eq!(TabMood::of(Phase::Idle, false, false), TabMood::Idle);
    }

    #[test]
    fn the_title_names_the_state() {
        assert_eq!(tab_title(TabMood::Idle, false, false), "Hobbes");
        assert_eq!(
            tab_title(TabMood::Listening, true, true),
            "Hobbes · Heartbeat on · Voice listening"
        );
        assert_eq!(tab_title(TabMood::Speaking, false, true), "Hobbes · Voice speaking");
        assert_eq!(tab_title(TabMood::Waking, true, true), "Hobbes · Heartbeat waking · Voice on");
    }

    #[test]
    fn listening_holds_the_ears_up_and_speaking_flaps_the_mouth() {
        for beat in [Beat::Rest, Beat::Blink, Beat::EarsUp] {
            assert_eq!(grid_for(TabMood::Listening, beat), &TIGER_GRID_EARS_UP);
            assert_eq!(grid_for(TabMood::Waking, beat), &TIGER_GRID_EARS_UP);
        }
        assert_eq!(grid_for(TabMood::Speaking, Beat::MouthOpen), &TIGER_GRID_MOUTH_OPEN);
        assert_eq!(grid_for(TabMood::Speaking, Beat::Rest), &TIGER_GRID_EARS_UP);
        assert_eq!(grid_for(TabMood::Idle, Beat::Blink), &TIGER_GRID_BLINK);
        assert_eq!(grid_for(TabMood::Idle, Beat::Rest), &TIGER_GRID);
    }

    #[test]
    fn the_idle_loop_is_mostly_at_rest() {
        let rest = (0..128).filter(|t| idle_beat(*t) == Beat::Rest).count();
        assert!(rest >= 120, "{rest} of 128 ticks at rest");
        assert!((0..128).any(|t| idle_beat(t) == Beat::Blink));
        assert!((0..128).any(|t| idle_beat(t) == Beat::EarsUp));
    }
}
