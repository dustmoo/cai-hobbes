//! Search your sent prompts (Claude Code's Ctrl+R): a small box above the
//! chat input. Type to filter, ↑↓ to choose, Enter to put the prompt in the
//! chat box, Esc to close.

use dioxus::prelude::*;

use crate::components::prompt_history;

/// How many matches the list shows.
const MAX_MATCHES: usize = 8;

#[derive(Props, Clone, PartialEq)]
pub struct PromptSearchProps {
    /// Sent prompts, oldest first.
    pub history: Vec<String>,
    pub on_pick: EventHandler<String>,
    pub on_close: EventHandler<()>,
}

#[component]
pub fn PromptSearch(props: PromptSearchProps) -> Element {
    let mut query = use_signal(String::new);
    let mut selected = use_signal(|| 0usize);
    let mut position = use_signal(|| (0.0, 0.0)); // (bottom, left)

    // Sit just above the chat input, like the skill autocomplete.
    use_effect(move || {
        spawn(async move {
            let eval = document::eval(
                r#"
                const input = document.getElementById('chat-textarea');
                if (!input) return null;
                const rect = input.getBoundingClientRect();
                return { bottom: window.innerHeight - rect.top + 8, left: rect.left };
            "#,
            );
            if let Ok(json) = eval.await {
                if !json.is_null() {
                    position.set((
                        json["bottom"].as_f64().unwrap_or(0.0),
                        json["left"].as_f64().unwrap_or(0.0),
                    ));
                }
            }
        });
    });

    let matches: Vec<String> = prompt_history::search(&props.history, &query.read(), MAX_MATCHES)
        .into_iter()
        .map(str::to_string)
        .collect();
    let current = (*selected.read()).min(matches.len().saturating_sub(1));
    let (bottom, left) = *position.read();

    let on_pick = props.on_pick;
    let on_close = props.on_close;
    let keyed_matches = matches.clone();

    rsx! {
        div {
            class: "fixed w-96 bg-card border border-subtle rounded-lg shadow-xl z-[100] overflow-hidden",
            style: "bottom: {bottom}px; left: {left}px;",
            role: "dialog",
            "aria-label": "Search sent prompts",
            input {
                class: "w-full px-3 py-2 text-sm bg-input text-fg border-b border-subtle focus:outline-none",
                placeholder: "Search sent prompts…",
                "aria-label": "Search sent prompts",
                value: "{query}",
                onmounted: move |evt| async move {
                    let _ = evt.data().set_focus(true).await;
                },
                oninput: move |e| {
                    query.set(e.value());
                    selected.set(0);
                },
                onkeydown: move |e| {
                    e.stop_propagation();
                    match e.key() {
                        Key::ArrowDown => {
                            e.prevent_default();
                            if current + 1 < keyed_matches.len() {
                                selected.set(current + 1);
                            }
                        }
                        Key::ArrowUp => {
                            e.prevent_default();
                            selected.set(current.saturating_sub(1));
                        }
                        Key::Enter => {
                            e.prevent_default();
                            match keyed_matches.get(current) {
                                Some(p) => on_pick.call(p.clone()),
                                None => on_close.call(()),
                            }
                        }
                        Key::Escape => {
                            e.prevent_default();
                            on_close.call(());
                        }
                        _ => {}
                    }
                },
            }
            div {
                class: "px-3 py-1 text-[10px] text-fg-muted border-b border-primary-800 flex items-center gap-2",
                span { "↑↓ Navigate" }
                span { "•" }
                span { "Enter Use" }
                span { "•" }
                span { "Esc Close" }
            }
            div {
                class: "max-h-64 overflow-y-auto py-1",
                role: "listbox",
                if matches.is_empty() {
                    p { class: "px-3 py-2 text-xs text-fg-muted", "No matching prompts" }
                }
                for (i, p) in matches.into_iter().enumerate() {
                    {
                    let pick = p.clone();
                    rsx! {
                    button {
                        key: "{i}",
                        role: "option",
                        "aria-selected": if i == current { "true" } else { "false" },
                        class: if i == current {
                            "w-full text-left px-3 py-1.5 text-sm text-fg bg-primary-700 truncate"
                        } else {
                            "w-full text-left px-3 py-1.5 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg truncate"
                        },
                        title: "{p}",
                        onclick: move |_| on_pick.call(pick.clone()),
                        "{p}"
                    }
                    }
                    }
                }
            }
        }
    }
}
