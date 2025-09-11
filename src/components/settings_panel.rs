use dioxus::prelude::*;
use rfd;
use crate::settings::{Settings, SettingsManager};
use crate::secure_storage;

#[component]
pub fn SettingsPanel() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    let mut has_unsaved_changes = use_signal(|| false);

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
                        has_unsaved_changes.set(true);
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
                        has_unsaved_changes.set(true);
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
                        has_unsaved_changes.set(true);
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Chat History Length"
                }
            }
            input {
                class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500",
                r#type: "number",
                value: "{settings.read().chat_history_length}",
                oninput: move |event| {
                    if let Ok(val) = event.value().parse::<usize>() {
                        settings.write().chat_history_length = val;
                        has_unsaved_changes.set(true);
                    }
                }
            }
            div {
                class: "mt-4 mb-4 flex items-center justify-between",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Show Tray Icon"
                }
                // Toggle switch
                label {
                    class: "relative inline-flex items-center cursor-pointer",
                    input {
                        r#type: "checkbox",
                        class: "sr-only peer",
                        checked: settings.read().show_tray_icon,
                        oninput: move |event| {
                            if let Some(checked) = event.value().parse().ok() {
                                settings.write().show_tray_icon = checked;
                                has_unsaved_changes.set(true);
                            }
                        }
                    }
                    div {
                        class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-indigo-800 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-indigo-600"
                    }
                }
            }
            div {
                class: "mt-4 mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Global Hotkey"
                }
                input {
                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm placeholder-gray-400 focus:outline-none focus:border-indigo-500 focus:ring-1 focus:ring-indigo-500 disabled:opacity-50",
                    r#type: "text",
                    value: "{settings.read().global_hotkey}",
                    oninput: move |event| {
                        settings.write().global_hotkey = event.value();
                        has_unsaved_changes.set(true);
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
                        has_unsaved_changes.set(true);
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
                        has_unsaved_changes.set(true);
                    }
                }
            }
            div {
                class: "mb-4",
                label {
                    class: "block text-sm font-medium text-gray-300",
                    "Project Folder"
                }
                div {
                    class: "mt-1 flex items-center",
                    p {
                        class: "flex-grow px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                        "{settings.read().project_folder.clone().unwrap_or(\"None\".to_string())}"
                    }
                    button {
                        class: "ml-2 px-4 py-2 bg-indigo-600 rounded-md text-white font-semibold hover:bg-indigo-700 focus:outline-none focus:ring-2 focus:ring-indigo-500 focus:ring-opacity-50 transition-colors",
                        onclick: move |_| {
                            spawn(async move {
                                let folder = rfd::AsyncFileDialog::new()
                                    .set_title("Select Project Folder")
                                    .pick_folder()
                                    .await;

                                if let Some(folder_path) = folder {
                                    settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                    has_unsaved_changes.set(true);
                                }
                            });
                        },
                        "Select Folder"
                    }
                }
            }
            button {
                class: if has_unsaved_changes() {
                    "mt-4 px-4 py-2 bg-purple-600 rounded-md text-white font-semibold hover:bg-purple-700 focus:outline-none focus:ring-2 focus:ring-purple-500 focus:ring-opacity-50 transition-colors"
                } else {
                    "mt-4 px-4 py-2 bg-gray-600 rounded-md text-white font-semibold cursor-not-allowed"
                },
                disabled: !has_unsaved_changes(),
                onclick: move |_| {
                    if has_unsaved_changes() {
                        let mut settings_clone = settings.read().clone();
                        if let Some(api_key) = settings_clone.api_key.take() {
                            if let Err(e) = secure_storage::save_secret("api_key", &api_key) {
                                tracing::error!("Failed to save API key: {}", e);
                            }
                        }
                        if let Err(e) = settings_manager.read().save(&settings_clone) {
                            tracing::error!("Failed to save settings: {}", e);
                        }
                        has_unsaved_changes.set(false);
                    }
                },
                "Save Settings"
            }
        }
    }
}