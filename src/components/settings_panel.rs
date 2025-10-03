use dioxus::prelude::*;
use rfd;
use crate::settings::{Settings, SettingsManager};
use crate::{context::permissions::ToolCategory, secure_storage};

#[component]
pub fn SettingsPanel() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    // Create a local copy of the settings for editing.
    let mut local_settings = use_signal(|| settings.read().clone());

    // This signal will track if the local state differs from the global state.
    let mut has_unsaved_changes = use_signal(|| false);

    // This effect hook reactively checks for differences between the local and global settings.
    use_effect(move || {
        let global_settings = settings.read();
        let local = local_settings.read();
        has_unsaved_changes.set(*global_settings != *local);
    });

    rsx! {
        div {
            class: "flex flex-col h-full p-4 bg-gray-800 text-white",
            h2 {
                class: "text-lg font-bold mb-4",
                "Settings"
            }
            div {
                class: "flex-grow overflow-y-auto pr-2",
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
                        value: "{local_settings.read().api_key.as_deref().unwrap_or(\"\")}",
                        oninput: move |event| {
                            local_settings.write().api_key = Some(event.value());
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
                        value: "{local_settings.read().chat_model}",
                        oninput: move |event| {
                            local_settings.write().chat_model = event.value();
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
                        value: "{local_settings.read().summary_model}",
                        oninput: move |event| {
                            local_settings.write().summary_model = event.value();
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
                    value: "{local_settings.read().chat_history_length}",
                    oninput: move |event| {
                        if let Ok(val) = event.value().parse::<usize>() {
                            local_settings.write().chat_history_length = val;
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
                            checked: local_settings.read().show_tray_icon,
                            oninput: move |event| {
                                if let Some(checked) = event.value().parse().ok() {
                                    local_settings.write().show_tray_icon = checked;
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
                        value: "{local_settings.read().global_hotkey}",
                        oninput: move |event| {
                            local_settings.write().global_hotkey = event.value();
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
                        value: "{local_settings.read().persona}",
                        oninput: move |event| {
                            local_settings.write().persona = event.value();
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
                        value: "{local_settings.read().force_tool_use_instruction.as_deref().unwrap_or(\"\")}",
                        oninput: move |event| {
                            local_settings.write().force_tool_use_instruction = Some(event.value());
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
                            "{local_settings.read().project_folder.clone().unwrap_or(\"None\".to_string())}"
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
                                        local_settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                    }
                                });
                            },
                            "Select Folder"
                        }
                    }
                }
                // Auto-Approval Settings
                div {
                    class: "mt-6 pt-4 border-t border-gray-700",
                    h3 {
                        class: "mb-6 text-md font-semibold mb-3",
                        "Auto-Approval Settings"
                    }

                    // Master Toggle
                    div {
                        class: "flex items-center justify-between mb-3",
                        label {
                            class: "block text-sm font-medium text-gray-300",
                            "Enable Auto-Approval"
                        }
                        label {
                            class: "relative inline-flex items-center cursor-pointer",
                            input {
                                r#type: "checkbox",
                                class: "sr-only peer",
                                checked: local_settings.read().permission_settings.auto_approval_enabled,
                                oninput: move |event| {
                                    if let Some(checked) = event.value().parse().ok() {
                                        local_settings.write().permission_settings.auto_approval_enabled = checked;
                                    }
                                }
                            }
                            div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-indigo-800 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-indigo-600" }
                        }
                    }

                    // Granular Toggles (conditionally rendered)
                    if local_settings.read().permission_settings.auto_approval_enabled {
                        div {
                            class: "mb-2 pl-4 border-l-2 border-gray-700",
                            
                            // MCP Toggle
                            div {
                                class: "flex items-center justify-between mb-2",
                                label { "MCP Tools" }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false),
                                        oninput: move |event| {
                                            if let Some(checked) = event.value().parse().ok() {
                                                local_settings.write().permission_settings.granular_permissions.insert(ToolCategory::Mcp, checked);
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-indigo-800 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-indigo-600" }
                                }
                            }

                            // Max Requests
                            div {
                                class: "mt-3",
                                label { "Max Consecutive Requests" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().permission_settings.max_requests}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<u32>() {
                                            local_settings.write().permission_settings.max_requests = val;
                                        }
                                    }
                                }
                            }

                            // Max Cost
                            div {
                                class: "mt-3",
                                label { "Max Session Cost ($)" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm",
                                    r#type: "number",
                                    step: "0.01",
                                    value: "{local_settings.read().permission_settings.max_cost}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<f64>() {
                                            local_settings.write().permission_settings.max_cost = val;
                                        }
                                    }
                                }
                            }
                        }
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
                        // 1. Commit the local changes to the global state
                        let mut global_settings = settings.write();
                        *global_settings = local_settings.read().clone();

                        // 2. Perform the save operations
                        let mut settings_to_save = global_settings.clone();
                        if let Some(api_key) = settings_to_save.api_key.take() {
                            if let Err(e) = secure_storage::save_secret("api_key", &api_key) {
                                tracing::error!("Failed to save API key: {}", e);
                            }
                        }
                        if let Err(e) = settings_manager.read().save(&settings_to_save) {
                            tracing::error!("Failed to save settings: {}", e);
                        }
                        // The `has_unsaved_changes` signal will automatically become false
                        // because the use_effect hook will see that local and global state are now equal.
                    }
                },
                "Save Settings"
            }
        }
    }
}