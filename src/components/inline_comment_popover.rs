use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[component]
pub fn InlineCommentPopover(
    on_save: EventHandler<String>,
    on_cancel: EventHandler<()>,
    initial_value: Option<String>,
    position_top: f64,
    position_left: f64,
) -> Element {
    let mut comment_text = use_signal(|| initial_value.unwrap_or_default());

    rsx! {
        div {
            class: "fixed z-50 bg-dark-card border border-gray-700 rounded-lg shadow-2xl p-4 min-w-[18rem] max-w-[24rem] transition-all duration-200",
            style: "top: {position_top}px; left: {position_left}px;",
            tabindex: "0",
            onclick: move |e| e.stop_propagation(),
            onkeydown: move |evt: KeyboardEvent| {
                if evt.key() == Key::Escape {
                    on_cancel.call(());
                }
            },

            textarea {
                class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm mb-3 text-gray-200 focus:outline-none focus:ring-1 focus:ring-primary-500 min-h-[5rem] resize-none",
                placeholder: "Add a comment...",
                value: "{comment_text}",
                oninput: move |e| comment_text.set(e.value()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter {
                        let modifiers = evt.modifiers();
                        // CMD+Enter or Shift+Enter = newline (don't submit)
                        if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::SHIFT) {
                            return;
                        }
                        // Plain Enter = submit
                        if !comment_text().trim().is_empty() {
                            evt.prevent_default();
                            on_save.call(comment_text());
                        }
                    }
                },
                autofocus: true,
            }

            div {
                class: "flex justify-end space-x-2 items-center",
                span { class: "text-[10px] text-gray-500 mr-auto", "Enter to save, Shift+Enter for newline" }
                button {
                    class: "p-1.5 text-gray-400 hover:text-white hover:bg-gray-700 rounded transition-colors",
                    onclick: move |_| on_cancel.call(()),
                    title: "Cancel (Esc)",
                    Icon { width: 14, height: 14, icon: fi_icons::FiX }
                }
                button {
                    class: "p-1.5 text-primary-400 hover:text-primary-300 hover:bg-primary-900/30 rounded transition-colors",
                    onclick: move |_| {
                        if !comment_text().trim().is_empty() {
                            on_save.call(comment_text());
                        }
                    },
                    title: "Save (Enter)",
                    Icon { width: 14, height: 14, icon: fi_icons::FiCheck }
                }
            }
        }
        // Backdrop to close on click outside
        div {
            class: "fixed inset-0 z-40 bg-transparent",
            onclick: move |_| on_cancel.call(()),
        }
    }
}
