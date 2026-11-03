//! The Hobbes tab's icon: the app icon as an image (never an emoji), with
//! small capability badges — the metal heart while the Heartbeat is on, and
//! the metal lips while voice is on, glowing while the mic is live.
//! Archives of the pre-merge Heartbeat and Voice tabs keep their metal heart
//! and mouth.

use std::sync::OnceLock;

use dioxus::prelude::*;

use crate::components::metal_heart::MetalHeart;
use crate::components::metal_mouth::MetalMouth;
use crate::hobbes_session::{LEGACY_HEARTBEAT_EMOJI, LEGACY_VOICE_EMOJI};

/// The app icon (64 px, drawn at 13–15 px) as a data URI, built once.
fn app_icon_uri() -> &'static str {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| {
        use base64::Engine as _;
        let png = include_bytes!("../../assets/icons/hobbes.iconset/icon_32x32@2x.png");
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
    })
}

/// The Hobbes session's id, as a memo: a tab re-renders when the id is
/// assigned or rotated, not on unrelated writes.
pub fn use_hobbes_session_id() -> Memo<Option<String>> {
    use_memo(|| crate::hobbes_session::HOBBES_SESSION_ID.read().clone())
}

/// Whether a tab shows a system icon: the live Hobbes session (read through
/// the memo, so the tab re-renders when the id changes) or an archive of its
/// lineage (`system_sessions`), which keeps its icon.
pub fn is_system_tab(hobbes_id: Memo<Option<String>>, session_id: &str) -> bool {
    hobbes_id.read().as_deref() == Some(session_id) || crate::system_sessions::is_system(session_id)
}

/// A system tab's icon. `emoji` is the session's tab emoji: a pre-merge
/// archive is told apart by it. `overlay` puts the badges on the icon's
/// corners (the narrow sidebar rail) instead of beside it.
#[component]
pub fn SystemTabIcon(
    session_id: String,
    emoji: Option<String>,
    #[props(default = 13)] size: u32,
    #[props(default = false)] overlay: bool,
) -> Element {
    let hobbes_id = use_hobbes_session_id();
    let settings = try_consume_context::<Signal<crate::settings::Settings>>();
    let mic_live = use_memo(|| crate::voice::controller::VOICE_UI.read().mic_on);

    match emoji.as_deref() {
        Some(LEGACY_HEARTBEAT_EMOJI) if hobbes_id.read().as_deref() != Some(session_id.as_str()) => {
            return rsx! { MetalHeart { size } };
        }
        Some(LEGACY_VOICE_EMOJI) if hobbes_id.read().as_deref() != Some(session_id.as_str()) => {
            return rsx! { MetalMouth { size } };
        }
        _ => {}
    }

    let live = hobbes_id.read().as_deref() == Some(session_id.as_str());
    let (heartbeat_on, voice_on) = match settings {
        Some(s) if live => {
            let s = s.read();
            (crate::heartbeat::heartbeat_active(&s), crate::voice::voice_active(&s))
        }
        _ => (false, false),
    };
    let glowing = voice_on && mic_live();
    let badge = if overlay { size.saturating_mul(3) / 5 } else { size.saturating_mul(2) / 3 }.max(7);
    let lips_style = if glowing {
        "filter: drop-shadow(0 0 1.5px #60a5fa) drop-shadow(0 0 3px #60a5fa);"
    } else {
        "opacity: 0.75;"
    };
    let lips_title = if glowing { "Voice: listening" } else { "Voice is on" };

    let icon = rsx! {
        img {
            class: "shrink-0 select-none",
            src: "{app_icon_uri()}",
            width: "{size}",
            height: "{size}",
            alt: "Hobbes",
            draggable: "false",
            style: "border-radius: 22%;",
        }
    };

    if overlay {
        rsx! {
            span {
                class: "relative inline-flex shrink-0",
                style: "width: {size}px; height: {size}px;",
                title: "Hobbes",
                {icon}
                if heartbeat_on {
                    span {
                        class: "absolute inline-flex",
                        style: "right: -3px; bottom: -3px;",
                        title: "Heartbeat is on",
                        MetalHeart { size: badge }
                    }
                }
                if voice_on {
                    span {
                        class: if glowing { "absolute inline-flex animate-pulse" } else { "absolute inline-flex" },
                        style: "left: -3px; bottom: -3px; {lips_style}",
                        title: "{lips_title}",
                        MetalMouth { size: badge }
                    }
                }
            }
        }
    } else {
        rsx! {
            span {
                class: "inline-flex items-center gap-0.5 shrink-0",
                title: "Hobbes",
                {icon}
                if heartbeat_on {
                    span { class: "inline-flex", title: "Heartbeat is on", MetalHeart { size: badge } }
                }
                if voice_on {
                    span {
                        class: if glowing { "inline-flex animate-pulse" } else { "inline-flex" },
                        style: "{lips_style}",
                        title: "{lips_title}",
                        MetalMouth { size: badge }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_icon_is_embedded_as_a_png() {
        let uri = app_icon_uri();
        assert!(uri.starts_with("data:image/png;base64,iVBORw0KGgo"), "a PNG data URI");
    }
}
