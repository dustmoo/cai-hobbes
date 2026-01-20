use dioxus::prelude::*;

#[derive(Props, PartialEq, Clone)]
pub struct ConflictModalProps {
    pub session_id: String,
    pub on_resolve: EventHandler<(bool, bool)>, // (overwrite, apply_to_all)
}

#[component]
pub fn ConflictModal(props: ConflictModalProps) -> Element {
    let mut apply_to_all = use_signal(|| false);
    rsx! {
        div {
            class: "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center",
            tabindex: "0",
            autofocus: true,
            onmounted: move |evt| {
                let mounted = evt.data();
                spawn(async move {
                    let _ = mounted.set_focus(true).await;
                });
            },
            onkeydown: {
                let on_resolve = props.on_resolve.clone();
                move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape {
                        on_resolve.call((false, *apply_to_all.read()));
                    } else if evt.key() == Key::Enter {
                        let modifiers = evt.modifiers();
                        if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                            evt.prevent_default();
                            on_resolve.call((true, *apply_to_all.read()));
                        }
                    }
                }
            },
            div {
                class: "bg-gray-800 rounded-lg p-6 max-w-sm w-full",
                tabindex: "0",
                onkeydown: {
                    let on_resolve = props.on_resolve.clone();
                    move |evt: KeyboardEvent| {
                        if evt.key() == Key::Escape {
                            on_resolve.call((false, *apply_to_all.read()));
                        } else if evt.key() == Key::Enter {
                            let modifiers = evt.modifiers();
                            if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::CONTROL) {
                                evt.prevent_default();
                                on_resolve.call((true, *apply_to_all.read()));
                            }
                        }
                    }
                },
                h2 {
                    class: "text-lg font-bold mb-4",
                    "Conflict Detected"
                }
                p {
                    class: "mb-4 text-gray-300",
                    "A session with ID "
                    span { class: "font-mono bg-gray-700 px-1 rounded", "{props.session_id}" }
                    " already exists. Would you like to overwrite it?"
                }
                div {
                    class: "mb-6 flex items-center",
                    input {
                        r#type: "checkbox",
                        id: "apply_all",
                        class: "h-4 w-4 text-indigo-600 bg-gray-700 border-gray-600 rounded focus:ring-indigo-500",
                        oninput: move |event| {
                            if let Some(checked) = event.value().parse().ok() {
                                apply_to_all.set(checked);
                            }
                        }
                    }
                    label {
                        r#for: "apply_all",
                        class: "ml-2 block text-sm text-gray-300",
                        "Apply to all conflicts"
                    }
                }
                div {
                    class: "flex justify-end space-x-4",
                    button {
                        class: "px-4 py-2 bg-gray-600 rounded-md text-white font-semibold hover:bg-gray-700",
                        onclick: move |_| props.on_resolve.call((false, *apply_to_all.read())),
                        "Skip"
                    }
                    button {
                        class: "px-4 py-2 bg-red-600 rounded-md text-white font-semibold hover:bg-red-700",
                        onclick: move |_| props.on_resolve.call((true, *apply_to_all.read())),
                        "Overwrite"
                    }
                }
            }
        }
    }
}
