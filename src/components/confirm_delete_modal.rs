#![allow(non_snake_case)]
use dioxus::prelude::*;

#[derive(Props, PartialEq, Clone)]
pub struct ConfirmDeleteModalProps {
    pub is_visible: Signal<bool>,
    pub on_confirm: EventHandler<bool>, // bool indicates if "remember" was checked
    pub on_cancel: EventHandler<()>,
    pub title: String,
    pub message: String,
    #[props(default = "Yes, Delete".to_string())]
    pub confirm_button_text: String,
    #[props(default = true)]
    pub show_dont_ask_again: bool,
}

#[component]
pub fn ConfirmDeleteModal(props: ConfirmDeleteModalProps) -> Element {
    let mut remember_choice = use_signal(|| false);

    if *props.is_visible.read() {
        rsx! {
            // Full-screen overlay with a dark, semi-transparent background
            div {
                class: "fixed inset-0 flex items-center justify-center z-50",
                tabindex: "0",
                autofocus: true,
                onmounted: move |evt| {
                    let mounted = evt.data();
                    spawn(async move {
                        let _ = mounted.set_focus(true).await;
                    });
                },
                onclick: move |_| props.on_cancel.call(()),
                onkeydown: {
                    let on_cancel = props.on_cancel;
                    let on_confirm = props.on_confirm;
                    move |evt: KeyboardEvent| {
                        if evt.key() == Key::Escape {
                            on_cancel.call(());
                        } else if evt.key() == Key::Enter {
                            let modifiers = evt.modifiers();
                            if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                                evt.prevent_default();
                                on_confirm.call(*remember_choice.read());
                            }
                        }
                    }
                },

                // The modal "card" with distinct styling
                div {
                    class: "bg-gray-800 border border-gray-700 rounded-lg shadow-xl p-4 w-sm",
                    tabindex: "0",
                    onclick: |event| event.stop_propagation(), // Prevent clicks inside from closing the modal
                    onkeydown: {
                        let on_cancel = props.on_cancel;
                        let on_confirm = props.on_confirm;
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Escape {
                                on_cancel.call(());
                            } else if evt.key() == Key::Enter {
                                let modifiers = evt.modifiers();
                                if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                                    evt.prevent_default();
                                    on_confirm.call(*remember_choice.read());
                                }
                            }
                        }
                    },

                    h2 { class: "text-xl font-bold text-white m-4", "{props.title}" }
                    p { class: "text-gray-300 m-4", "{props.message}" }

                    if props.show_dont_ask_again {
                        div {
                            class: "flex items-center m-4",
                            input {
                                id: "remember_choice",
                                "type": "checkbox",
                                class: "h-4 w-4 text-purple-600 bg-gray-700 border-gray-600 rounded focus:ring-purple-500",
                                checked: *remember_choice.read(),
                                onchange: move |evt| remember_choice.set(evt.checked()),
                            }
                            label {
                                "for": "remember_choice",
                                class: "ml-2 text-sm font-medium text-gray-400 select-none",
                                "Remember my choice"
                            }
                        }
                    }

                    // Action buttons
                    div {
                        class: "flex justify-end space-x-4",
                        button {
                            class: "px-4 py-2 bg-gray-700 rounded-md text-white font-semibold hover:bg-gray-600 focus:outline-none focus:ring-2 focus:ring-gray-500",
                            onclick: move |_| props.on_cancel.call(()),
                            "Cancel"
                        }
                        button {
                            class: "px-4 py-2 bg-red-600 rounded-md text-white font-semibold hover:bg-red-700 focus:outline-none focus:ring-2 focus:ring-red-500",
                            onclick: move |_| props.on_confirm.call(*remember_choice.read()),
                            "{props.confirm_button_text}"
                        }
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}
