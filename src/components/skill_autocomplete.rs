use crate::skills::Skill;
use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct SkillAutocompleteProps {
    pub skills: Vec<Skill>,
    pub selected_index: usize,
    pub on_select: EventHandler<Skill>,
}

#[component]
pub fn SkillAutocomplete(props: SkillAutocompleteProps) -> Element {
    // State for smart positioning
    let mut position = use_signal(|| (0.0, 0.0)); // (bottom, left)
    let mut visible = use_signal(|| false);

    // Measure input position to prevent overflow clipping (Smart Positioning)
    use_effect(move || {
        spawn(async move {
            let eval = document::eval(
                r#"
                const input = document.getElementById('chat-textarea');
                if (!input) return null;
                const rect = input.getBoundingClientRect();
                return {
                    // Position above the input (distance from bottom of viewport to top of input)
                    bottom: window.innerHeight - rect.top + 8,
                    left: rect.left
                };
            "#,
            );

            if let Ok(json) = eval.await {
                if !json.is_null() {
                    let bottom = json["bottom"].as_f64().unwrap_or(0.0);
                    let left = json["left"].as_f64().unwrap_or(0.0);
                    position.set((bottom, left));
                    visible.set(true);
                }
            }
        });
    });

    if props.skills.is_empty() {
        return rsx! {};
    }

    let (bottom, left) = *position.read();
    let opacity = if *visible.read() {
        "opacity-100"
    } else {
        "opacity-0"
    };

    rsx! {
        div {
            class: "fixed w-72 bg-card border border-subtle rounded-lg shadow-xl z-[100] overflow-hidden py-1 max-h-64 overflow-y-auto transition-opacity duration-150 {opacity}",
            style: "bottom: {bottom}px; left: {left}px;",

            // Keyboard navigation hint
            div {
                class: "px-3 py-1 text-[10px] text-fg-muted border-b border-primary-800 flex items-center gap-2",
                span { "↑↓ Navigate" }
                span { "•" }
                span { "Enter Select" }
                span { "•" }
                span { "Esc Cancel" }
            }

            for (i, skill) in props.skills.iter().enumerate() {
                button {
                    key: "{skill.metadata.name}",
                    class: if i == props.selected_index {
                        "w-full text-left px-4 py-2 text-sm text-fg bg-primary-700 flex flex-col items-start gap-0.5"
                    } else {
                        "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex flex-col items-start gap-0.5 group"
                    },
                    onclick: {
                        let s = skill.clone();
                        move |_| props.on_select.call(s.clone())
                    },
                    // Note: Hover state doesn't update selected_index in parent,
                    // keeping keyboard nav as primary source of truth for 'selected' state to avoid conflicts.
                    // If we wanted hover to update selection, we'd need an on_hover event handler prop.

                    div {
                        class: "flex items-center justify-between w-full",
                        span {
                            class: if i == props.selected_index {
                                "font-mono font-medium text-primary-200"
                            } else {
                                "font-mono font-medium text-primary-400 group-hover:text-primary-300"
                            },
                            "/{skill.metadata.name}"
                        }
                        if let Some(hint) = skill.metadata.argument_hint.as_ref() {
                            span {
                                class: "font-mono text-[10px] text-fg-muted ml-2 truncate",
                                "{hint}"
                            }
                        }
                    }
                    if !skill.metadata.description.is_empty() {
                         span { class: "text-xs text-fg-muted truncate w-full", "{skill.metadata.description}" }
                    }
                }
            }
        }
    }
}
