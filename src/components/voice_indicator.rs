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

/// The strip's status line.
pub fn status_text(phase: Phase, mode: Option<Mode>, held: bool, brain_busy: usize) -> &'static str {
    match (phase, brain_busy > 0) {
        (Phase::Idle, _) => "",
        (Phase::Connecting, _) => "Connecting…",
        (Phase::Speaking, _) => "Speaking",
        (Phase::Listening, true) => "Listening · brain working",
        (Phase::Listening, false) => match (mode, held) {
            (Some(Mode::PushToTalk), false) => "Hold to talk",
            _ => "Listening",
        },
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

    let status = status_text(ui.phase, ui.mode, ui.held, ui.brain_busy);
    let hint = match ui.mode {
        Some(Mode::PushToTalk) => format!("Hold {key} to talk · tap to go hands-free"),
        _ => format!("Tap {key} to end"),
    };
    let echo_note = (!ui.echo_cancelled).then_some(" · headphones recommended");
    rsx! {
        div {
            class: "flex items-center justify-between gap-3 px-4 py-2 bg-primary-900/80 border-b border-primary-700 text-fg text-sm",
            div { class: "flex items-center gap-2 min-w-0",
                MetalMouth { size: 15 }
                if ui.phase != Phase::Connecting {
                    div { class: "w-2 h-2 rounded-full bg-primary-400 animate-pulse shrink-0" }
                }
                span { class: "font-medium shrink-0", "{status}" }
                if !ui.heard.is_empty() {
                    span { class: "truncate text-fg-muted", "“{ui.heard}”" }
                }
            }
            div { class: "flex items-center gap-2 shrink-0",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_naturally() {
        assert_eq!(status_text(Phase::Connecting, None, true, 0), "Connecting…");
        assert_eq!(status_text(Phase::Listening, Some(Mode::Conversation), false, 1), "Listening · brain working");
        assert_eq!(status_text(Phase::Listening, Some(Mode::PushToTalk), false, 0), "Hold to talk");
        assert_eq!(status_text(Phase::Listening, Some(Mode::PushToTalk), true, 0), "Listening");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hotkey_label_uses_mac_symbols() {
        assert_eq!(hotkey_label("Alt+Space"), "⌥Space");
        assert_eq!(hotkey_label("Shift+Cmd+V"), "⇧⌘V");
    }
}
