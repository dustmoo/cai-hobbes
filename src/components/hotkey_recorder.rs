use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct HotkeyRecorderProps {
    value: String,
    onchange: EventHandler<String>,
    /// When set, a reset button restores this binding (e.g. a bare Esc,
    /// which can't be recorded: Esc cancels recording).
    #[props(default)]
    default_value: Option<String>,
    /// Offer a button that turns the shortcut off (an empty binding).
    #[props(default)]
    can_turn_off: bool,
}

/// What the recorder shows for a stored binding.
fn shown(value: &str) -> String {
    if value.trim().is_empty() {
        "Off".to_string()
    } else {
        value.to_string()
    }
}

#[component]
pub fn HotkeyRecorder(props: HotkeyRecorderProps) -> Element {
    let mut recording = use_signal(|| false);
    // Use a local signal to display the "Recording..." state or partial combo
    let mut display_value = use_signal(|| shown(&props.value));

    // Sync with props
    use_effect(use_reactive(&props.value, move |val| {
        if !*recording.read() {
            display_value.set(shown(&val));
        }
    }));

    let reset_to = props
        .default_value
        .clone()
        .filter(|d| *d != props.value && !*recording.read());
    let show_off = props.can_turn_off && !props.value.trim().is_empty() && !*recording.read();
    let onchange = props.onchange;

    let props_toggle = props.clone();
    let toggle_recording = move |_| {
        let is_recording = *recording.read();
        if is_recording {
            // Cancel/Stop
            recording.set(false);
            display_value.set(shown(&props_toggle.value));
        } else {
            // Start
            recording.set(true);
            display_value.set("Press keys...".to_string());
        }
    };

    let handle_keydown = move |evt: KeyboardEvent| {
        if !*recording.read() {
            // Keyboard access: Enter or Space on the focused field starts
            // recording, like a click.
            let key = evt.key();
            if key == Key::Enter || matches!(&key, Key::Character(c) if c == " ") {
                evt.prevent_default();
                recording.set(true);
                display_value.set("Press keys...".to_string());
            }
            return;
        }

        evt.stop_propagation();
        evt.prevent_default();

        let key = evt.key();
        let _code = evt.code(); // e.g. "KeyA", "Digit1"
        let modifiers = evt.modifiers();

        // A bare Escape stops recording (with a modifier, it is recorded).
        if key == Key::Escape && modifiers.is_empty() {
            recording.set(false);
            display_value.set(shown(&props.value));
            return;
        }

        // Build the accelerator string from modifiers
        let mut parts = Vec::new();

        // Mac-specific modifier mapping preference: CmdOrCtrl > Shift > Alt > Ctrl
        if modifiers.contains(dioxus::events::Modifiers::META) {
            parts.push("CmdOrCtrl");
        }
        if modifiers.contains(dioxus::events::Modifiers::CONTROL) {
            // On Mac, Ctrl is Control. But standard accelerators often use Cmd.
            // If Cmd is pressed, we used CmdOrCtrl.
            // If Control is ALSO pressed, we add Control.
            parts.push("Control");
        }
        if modifiers.contains(dioxus::events::Modifiers::ALT) {
            parts.push("Alt");
        }
        if modifiers.contains(dioxus::events::Modifiers::SHIFT) {
            parts.push("Shift");
        }

        // Determine if non-modifier key is pressed
        let is_modifier_key = matches!(key, Key::Meta | Key::Control | Key::Alt | Key::Shift);

        if !is_modifier_key {
            // Map keys to muda-friendly strings
            // Muda expects "KeyA" -> "A" (usually), and "Digit1" -> "1"
            // But strict accelerator parsing might need specifics.
            // Let's rely on the Key enum's string representation but cleaned up.

            let key_str = match key {
                Key::Character(c) if c == " " => "Space".to_string(),
                Key::Character(c) => c.to_uppercase(),
                Key::Enter => "Enter".to_string(),
                Key::Backspace => "Backspace".to_string(),
                Key::Tab => "Tab".to_string(),

                Key::ArrowUp => "ArrowUp".to_string(),
                Key::ArrowDown => "ArrowDown".to_string(),
                Key::ArrowLeft => "ArrowLeft".to_string(),
                Key::ArrowRight => "ArrowRight".to_string(),
                Key::Escape => "Escape".to_string(),
                _ => key.to_string(), // Fallback
            };

            parts.push(key_str.as_str());

            let final_hotkey = parts.join("+");

            // Commit
            props.onchange.call(final_hotkey.clone());
            display_value.set(final_hotkey);
            recording.set(false);
        } else {
            // Update display with current modifiers?
            // Optional: Show "Cmd+" while holding.
            // For simplicity, keep "Press keys..." until completion or implement live preview.
            // Let's implement live preview.
            let preview = parts.join("+");
            if !preview.is_empty() {
                display_value.set(preview + "...");
            }
        }
    };

    let border_color = if *recording.read() {
        "border-primary-500 ring-2 ring-primary-900"
    } else {
        "border-primary-600"
    };
    let bg_color = if *recording.read() {
        "bg-section"
    } else {
        "bg-input"
    };
    let text_color = if *recording.read() {
        "text-primary-300"
    } else {
        "text-gray-200"
    };

    rsx! {
        div {
            class: "flex items-center gap-2",
        div {
            class: "relative group cursor-pointer select-none flex-1 min-w-0",
            onclick: toggle_recording,
            onkeydown: handle_keydown,
            tabindex: "0", // Make focusable to receive keys
            role: "button",
            "aria-label": "Shortcut: {display_value}. Press Enter or click to record a new one.",
            div {
                class: "w-full px-3 py-1.5 text-sm font-mono rounded-md border {border_color} {bg_color} {text_color} flex items-center justify-between transition-colors",
                span { "{display_value}" }
                if *recording.read() {
                    span { class: "text-xs text-primary-400 animate-pulse", "REC" }
                } else {
                    // Edit icon or generic suggestion
                     span { class: "text-xs text-fg-muted opacity-0 group-hover:opacity-100 transition-opacity", "Click to Edit" }
                }
            }
        }
            if let Some(d) = reset_to {
                button {
                    class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card shrink-0",
                    title: "Reset to {d}",
                    "aria-label": "Reset to {d}",
                    onclick: move |_| onchange.call(d.clone()),
                    "Reset"
                }
            }
            if show_off {
                button {
                    class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card shrink-0",
                    title: "Turn this shortcut off",
                    "aria-label": "Turn this shortcut off",
                    onclick: move |_| onchange.call(String::new()),
                    "Off"
                }
            }
        }
    }
}
