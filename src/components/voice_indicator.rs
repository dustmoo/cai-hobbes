//! The voice strip: shown under the tab bar while a voice conversation is
//! live (what the voice is doing, what it's hearing, how to end it) or when
//! voice hit a problem. Same vocabulary as the timer toast strip.

use dioxus::prelude::*;

use crate::components::chat_input::ChatCommand;
use crate::components::metal_mouth::MetalMouth;
use crate::voice::controller::{self, Mode, Phase, VoiceCmd, VOICE_UI};

/// The hotkey as the person would read it ("Alt+Space" → "⌥Space" on macOS).
pub fn hotkey_label(raw: &str) -> String {
    if !cfg!(target_os = "macos") {
        return raw.to_string();
    }
    raw.split('+')
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "alt" | "option" => "⌥".to_string(),
            "cmd" | "command" | "super" | "meta" => "⌘".to_string(),
            "shift" => "⇧".to_string(),
            "ctrl" | "control" => "⌃".to_string(),
            _ => part.trim().to_string(),
        })
        .collect()
}

/// The strip's status line. "Listening" only while the mic is actually
/// open (push-to-talk releases it between holds).
pub fn status_text(phase: Phase, brain_busy: usize, mic_on: bool) -> &'static str {
    match (phase, brain_busy > 0, mic_on) {
        (Phase::Idle, _, _) => "",
        (Phase::Connecting, _, _) => "Connecting…",
        (Phase::Speaking, _, _) => "Speaking",
        (Phase::Listening, true, true) => "Listening · brain working",
        (Phase::Listening, true, false) => "Brain working",
        (Phase::Listening, false, true) => "Listening",
        (Phase::Listening, false, false) => "Hold to talk",
    }
}

#[component]
pub fn VoiceIndicator() -> Element {
    let ui = VOICE_UI.read().clone();
    let settings = use_context::<Signal<crate::settings::Settings>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let key = hotkey_label(&settings.read().voice.hotkey);

    if ui.phase == Phase::Idle {
        let Some(err) = ui.error.clone() else { return rsx! {} };
        return rsx! {
            div {
                class: "flex items-center justify-between px-4 py-2 bg-primary-900/80 border-b border-primary-700 text-fg text-sm",
                span { class: "flex items-center gap-2",
                    MetalMouth { size: 15 }
                    "{err}"
                }
                button {
                    class: "ml-4 px-2 py-0.5 text-xs bg-primary-800 hover:bg-primary-700 rounded transition-colors",
                    onclick: move |_| { VOICE_UI.write().error = None; },
                    "Dismiss"
                }
            }
        };
    }

    let status = status_text(ui.phase, ui.brain_busy, ui.mic_on);
    let hint = match ui.mode {
        Some(Mode::PushToTalk) => format!("Hold {key} to talk · tap to go hands-free"),
        _ => format!("Tap {key} to end"),
    };
    let echo_note = (!ui.echo_safe).then_some(" · headphones recommended");
    let level_pct = level_percent(ui.level);
    rsx! {
        div {
            class: "flex items-center justify-between gap-3 px-4 py-2 bg-primary-900/80 border-b border-primary-700 text-fg text-sm",
            div { class: "flex items-center gap-2 min-w-0",
                MetalMouth { size: 15 }
                // The live dot means the mic is open, or Hobbes is speaking.
                if ui.phase != Phase::Connecting && (ui.mic_on || ui.phase == Phase::Speaking) {
                    div { class: "w-2 h-2 rounded-full bg-primary-400 animate-pulse shrink-0" }
                }
                span { class: "font-medium shrink-0", "{status}" }
                if let Some(w) = ui.mic_warning.clone() {
                    span { class: "truncate text-amber-300", "{w}" }
                } else if !ui.heard.is_empty() {
                    span { class: "truncate text-fg-muted", "“{ui.heard}”" }
                }
            }
            div { class: "flex items-center gap-2 shrink-0",
                if !ui.mic.is_empty() {
                    span {
                        class: "flex items-center gap-1.5 text-xs text-fg-muted",
                        title: "Microphone and its level",
                        "{ui.mic}"
                        if ui.mic_on {
                            div { class: "w-10 h-1.5 rounded bg-primary-800 overflow-hidden",
                                div { class: "h-full bg-primary-400", style: "width: {level_pct}%" }
                            }
                        } else {
                            " · mic off"
                        }
                    }
                }
                span { class: "text-xs text-fg-muted", "{hint}{echo_note.unwrap_or_default()}" }
                button {
                    class: "px-2 py-0.5 text-xs bg-primary-800 hover:bg-primary-700 rounded transition-colors",
                    onclick: move |_| {
                        let id = crate::voice::session::VOICE_SESSION_ID.peek().clone();
                        if let Some(id) = id {
                            chat_command.set(Some(ChatCommand::SwitchToSession(id)));
                        }
                    },
                    "Voice tab"
                }
                button {
                    class: "px-2 py-0.5 text-xs bg-primary-800 hover:bg-primary-700 rounded transition-colors",
                    onclick: move |_| controller::send(VoiceCmd::Stop),
                    "End"
                }
            }
        }
    }
}

/// Mic peak (0–1) as a meter width: speech peaks well under full scale,
/// so the scale is stretched (√) to make normal talking fill most of it.
pub fn level_percent(peak: f32) -> u32 {
    (peak.clamp(0.0, 1.0).sqrt() * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_naturally() {
        assert_eq!(status_text(Phase::Connecting, 0, true), "Connecting…");
        assert_eq!(status_text(Phase::Listening, 1, true), "Listening · brain working");
        // Push-to-talk between holds: the mic is off, so never "Listening".
        assert_eq!(status_text(Phase::Listening, 1, false), "Brain working");
        assert_eq!(status_text(Phase::Listening, 0, false), "Hold to talk");
        assert_eq!(status_text(Phase::Listening, 0, true), "Listening");
    }

    #[test]
    fn level_meter_stretches_quiet_speech() {
        assert_eq!(level_percent(0.0), 0);
        assert_eq!(level_percent(0.25), 50);
        assert_eq!(level_percent(2.0), 100);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hotkey_label_uses_mac_symbols() {
        assert_eq!(hotkey_label("Alt+Space"), "⌥Space");
        assert_eq!(hotkey_label("Shift+Cmd+V"), "⇧⌘V");
    }
}
