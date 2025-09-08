use dioxus::prelude::*;
use crate::settings::{Settings, SettingsManager};
use crate::secure_storage;

#[component]
pub fn SettingsPanel() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    rsx! {
        div {
            class: "flex flex-col h-full p-4 bg-gray-800 text-white",
            h2 {
                class: "text-lg font-bold mb-4",
                "Settings"
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "API Key"
                }
                input {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                    r#type: "password",
                    placeholder: "Using environment variable",
                    value: "{settings.read().api_key.as_deref().unwrap_or(\"\")}",
                    oninput: move |event| {
                        settings.write().api_key = Some(event.value());
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Chat Model"
                }
                input {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                    r#type: "text",
                    value: "{settings.read().chat_model}",
                    oninput: move |event| {
                        settings.write().chat_model = event.value();
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Summary Model"
                }
                input {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                    r#type: "text",
                    value: "{settings.read().summary_model}",
                    oninput: move |event| {
                        settings.write().summary_model = event.value();
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Persona"
                }
                textarea {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                    rows: "4",
                    value: "{settings.read().persona}",
                    oninput: move |event| {
                        settings.write().persona = event.value();
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Force Tool Use Instruction"
                }
                textarea {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                    rows: "4",
                    value: "{settings.read().force_tool_use_instruction.as_deref().unwrap_or(\"\")}",
                    oninput: move |event| {
                        settings.write().force_tool_use_instruction = Some(event.value());
                    }
                }
            }
            button {
                class: "px-4 py-2 bg-purple-600 rounded-md text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors",
                onclick: move |_| {
                    let mut settings_clone = settings.read().clone();
                    if let Some(api_key) = settings_clone.api_key.take() {
                        if let Err(e) = secure_storage::save_secret("api_key", &api_key) {
                            tracing::error!("Failed to save API key: {}", e);
                        }
                    }
                    if let Err(e) = settings_manager.read().save(&settings_clone) {
                        tracing::error!("Failed to save settings: {}", e);
                    }
                },
                "Save"
            }
        }
    }
}