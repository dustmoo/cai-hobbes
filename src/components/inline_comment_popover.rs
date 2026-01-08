use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};

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
            class: "fixed z-50 bg-dark-card border border-gray-700 rounded-lg shadow-xl p-3 w-72",
            style: "top: {position_top}px; left: {position_left}px;",
            onclick: move |e| e.stop_propagation(),
            
            textarea {
                class: "w-full px-3 py-2 bg-dark-input border border-primary-600 rounded-md text-sm mb-2 text-gray-200 focus:outline-none focus:ring-1 focus:ring-primary-500",
                rows: "3",
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
                class: "flex justify-end space-x-2",
                button {
                    class: "p-1 text-gray-400 hover:text-white rounded transition-colors",
                    onclick: move |_| on_cancel.call(()),
                    title: "Cancel",
                    Icon { width: 16, height: 16, icon: fi_icons::FiX }
                }
                button {
                    class: "p-1 text-primary-400 hover:text-primary-300 rounded transition-colors",
                    onclick: move |_| {
                        if !comment_text().trim().is_empty() {
                            on_save.call(comment_text());
                        }
                    },
                    title: "Save",
                    Icon { width: 16, height: 16, icon: fi_icons::FiCheck }
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
