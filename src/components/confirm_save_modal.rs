#![allow(non_snake_case)]
use dioxus::prelude::*;

#[derive(Props, PartialEq, Clone)]
pub struct ConfirmSaveModalProps {
    pub is_visible: Signal<bool>,
    pub on_confirm: EventHandler<bool>, // bool indicates if "remember" was checked
    pub on_cancel: EventHandler<()>,
    pub title: String,
    pub message: String,
}

#[component]
pub fn ConfirmSaveModal(props: ConfirmSaveModalProps) -> Element {
    let mut remember_choice = use_signal(|| false);

    if *props.is_visible.read() {
        rsx! {
            // Full-screen overlay with a dark, semi-transparent background
            div {
                class: "fixed inset-0 flex items-center justify-center z-50",
                onclick: move |_| props.on_cancel.call(()),

                // The modal "card" with distinct styling
                div {
                    class: "bg-dark-section border border-primary-700 rounded-lg shadow-xl p-4 w-sm",
                    onclick: |event| event.stop_propagation(), // Prevent clicks inside from closing the modal

                    h2 { class: "text-xl font-bold text-white m-4", "{props.title}" }
                    p { class: "text-gray-300 m-4", "{props.message}" }

                    div {
                        class: "flex items-center m-4",
                        input {
                            id: "remember_choice",
                            "type": "checkbox",
                            class: "h-4 w-4 text-primary-600 bg-dark-input border-primary-600 rounded focus:ring-primary-500",
                            checked: *remember_choice.read(),
                            onchange: move |evt| remember_choice.set(evt.checked()),
                        }
                        label {
                            "for": "remember_choice",
                            class: "ml-2 text-sm font-medium text-gray-400 select-none",
                            "Remember my choice"
                        }
                    }

                    // Action buttons
                    div {
                        class: "flex justify-end space-x-4",
                        button {
                            class: "px-4 py-2 bg-dark-card rounded-md text-white font-semibold hover:bg-gray-600 focus:outline-none focus:ring-2 focus:ring-gray-500",
                            onclick: move |_| props.on_cancel.call(()),
                            "Cancel"
                        }
                        button {
                            class: "px-4 py-2 bg-primary-500 rounded-md text-white font-semibold hover:bg-primary-600 focus:outline-none focus:ring-2 focus:ring-primary-500",
                            onclick: move |_| props.on_confirm.call(*remember_choice.read()),
                            "Yes, Save"
                        }
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}