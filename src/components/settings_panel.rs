use dioxus::prelude::*;
use rfd;
use crate::settings::{Settings, SettingsManager};
use crate::{context::permissions::ToolCategory, secure_storage, session::SessionState};
use std::io::Write;
use crate::components::conflict_modal::ConflictModal;
use zip::write::{FileOptions, ZipWriter};

#[component]
pub fn SettingsPanel() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut session_state = use_context::<Signal<SessionState>>();

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

    let mut llm_config_collapsed = use_signal(|| false);
    let mut app_behavior_collapsed = use_signal(|| false);
    let mut data_management_collapsed = use_signal(|| false);
    let mut permissions_collapsed = use_signal(|| false);
    let mut show_conflict_modal = use_signal(|| false);
    let mut conflicting_sessions = use_signal(|| Vec::<(String, crate::session::Session)>::new());

    rsx! {
        div {
            class: "flex flex-col h-full p-4 bg-gray-800 text-white",
            h2 {
                class: "text-lg font-bold mb-4",
                "Settings"
            }
            if show_conflict_modal() {
                if let Some((id, _)) = conflicting_sessions.read().first() {
                    ConflictModal {
                        session_id: id.clone(),
                        on_resolve: move |(overwrite, apply_to_all)| {
                            let mut conflicts = conflicting_sessions.write();
                            if apply_to_all {
                                for (id, session) in conflicts.drain(..) {
                                    if overwrite {
                                        session_state.write().sessions.insert(id, session);
                                    }
                                }
                            } else {
                                let (id, session) = conflicts.remove(0);
                                if overwrite {
                                    session_state.write().sessions.insert(id, session);
                                }
                            }

                            if conflicts.is_empty() {
                                show_conflict_modal.set(false);
                                if let Err(e) = session_state.write().save() {
                                    tracing::error!("Failed to save session state after conflict resolution: {}", e);
                                }
                            }
                        }
                    }
                }
            }
            div {
                class: "flex-grow overflow-y-auto pr-2",
                // LLM Configuration Section
                div {
                    class: "mb-4 border border-gray-700 rounded-lg",
                    div {
                        class: "flex justify-between items-center p-3 cursor-pointer bg-gray-750 rounded-t-lg",
                        onclick: move |_| llm_config_collapsed.set(!llm_config_collapsed()),
                        h3 { class: "text-md font-semibold", "LLM Configuration" }
                        span { class: "transform transition-transform", class: if llm_config_collapsed() { "rotate-0" } else { "-rotate-180" }, "▼" }
                    }
                    if !llm_config_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "LLM Provider" }
                                select {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                    option { value: "Gemini", "Gemini" }
                                }
                            }
                            if local_settings.read().active_llm == crate::settings::LlmProvider::Gemini {
                                div {
                                    class: "pl-4 border-l-2 border-gray-700",
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-gray-300", "API Key" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                            r#type: "password",
                                            placeholder: "Using environment variable",
                                            value: "{local_settings.read().gemini_config.api_key.as_deref().unwrap_or(\"\")}",
                                            oninput: move |event| local_settings.write().gemini_config.api_key = Some(event.value())
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-gray-300", "Chat Model" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                            r#type: "text",
                                            value: "{local_settings.read().gemini_config.chat_model}",
                                            oninput: move |event| local_settings.write().gemini_config.chat_model = event.value()
                                        }
                                    }
                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-gray-300", "Summary Model" }
                                        input {
                                            class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                            r#type: "text",
                                            value: "{local_settings.read().gemini_config.summary_model}",
                                            oninput: move |event| local_settings.write().gemini_config.summary_model = event.value()
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Application Behavior Section
                div {
                    class: "mb-4 border border-gray-700 rounded-lg",
                    div {
                        class: "flex justify-between items-center p-3 cursor-pointer bg-gray-750 rounded-t-lg",
                        onclick: move |_| app_behavior_collapsed.set(!app_behavior_collapsed()),
                        h3 { class: "text-md font-semibold", "Application Behavior" }
                        span { class: "transform transition-transform", class: if app_behavior_collapsed() { "rotate-0" } else { "-rotate-180" }, "▼" }
                    }
                    if !app_behavior_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Chat History Length" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                    r#type: "number",
                                    value: "{local_settings.read().chat_history_length}",
                                    oninput: move |event| {
                                        if let Ok(val) = event.value().parse::<usize>() {
                                            local_settings.write().chat_history_length = val;
                                        }
                                    }
                                }
                            }
                            div {
                                class: "mt-4 mb-4 flex items-center justify-between",
                                label { class: "block text-sm font-medium text-gray-300", "Show Tray Icon" }
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
                                    div { class: "w-11 h-6 bg-gray-600 peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-indigo-800 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-indigo-600" }
                                }
                            }
                            div {
                                class: "mt-4 mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Global Hotkey" }
                                input {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                    r#type: "text",
                                    value: "{local_settings.read().global_hotkey}",
                                    oninput: move |event| local_settings.write().global_hotkey = event.value()
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Persona" }
                                textarea {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                    rows: "4",
                                    value: "{local_settings.read().persona}",
                                    oninput: move |event| local_settings.write().persona = event.value()
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Force Tool Use Instruction" }
                                textarea {
                                    class: "mt-1 block w-full px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                    rows: "4",
                                    value: "{local_settings.read().force_tool_use_instruction.as_deref().unwrap_or(\"\")}",
                                    oninput: move |event| local_settings.write().force_tool_use_instruction = Some(event.value())
                                }
                            }
                            div {
                                class: "mb-4",
                                label { class: "block text-sm font-medium text-gray-300", "Project Folder" }
                                div {
                                    class: "mt-1 flex items-center",
                                    p {
                                        class: "flex-grow px-3 py-2 bg-gray-700 border border-gray-600 rounded-md text-sm shadow-sm",
                                        "{local_settings.read().project_folder.clone().unwrap_or(\"None\".to_string())}"
                                    }
                                    button {
                                        class: "ml-2 px-4 py-2 bg-indigo-600 rounded-md text-white font-semibold hover:bg-indigo-700",
                                        onclick: move |_| {
                                            spawn(async move {
                                                if let Some(folder_path) = rfd::AsyncFileDialog::new().pick_folder().await {
                                                    local_settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                                }
                                            });
                                        },
                                        "Select Folder"
                                    }
                                }
                            }
                        }
                    }
                }

                // Data Management Section
                div {
                    class: "mb-4 border border-gray-700 rounded-lg",
                    div {
                        class: "flex justify-between items-center p-3 cursor-pointer bg-gray-750 rounded-t-lg",
                        onclick: move |_| data_management_collapsed.set(!data_management_collapsed()),
                        h3 { class: "text-md font-semibold", "Data Management" }
                        span { class: "transform transition-transform", class: if data_management_collapsed() { "rotate-0" } else { "-rotate-180" }, "▼" }
                    }
                    if !data_management_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex space-x-2",
                                button {
                                    class: "px-4 py-2 bg-blue-600 rounded-md text-white font-semibold hover:bg-blue-700",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_settings.zip").save_file().await {
                                                let settings_json = serde_json::to_string_pretty(&*settings.read()).unwrap();
                                                let mut zip_buffer = Vec::new();
                                                {
                                                    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
                                                    zip.start_file("settings.json", FileOptions::default()).unwrap();
                                                    zip.write_all(settings_json.as_bytes()).unwrap();
                                                    zip.finish().unwrap();
                                                }
                                                if let Err(e) = std::fs::write(path.path(), &zip_buffer) {
                                                    tracing::error!("Failed to save settings export: {}", e);
                                                }
                                            }
                                        });
                                    },
                                    "Export Settings"
                                }
                                button {
                                    class: "px-4 py-2 bg-blue-600 rounded-md text-white font-semibold hover:bg-blue-700",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_settings.zip").pick_file().await {
                                                let file = match std::fs::File::open(path.path()) {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("Failed to open file: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut archive = match zip::ZipArchive::new(file) {
                                                    Ok(a) => a,
                                                    Err(e) => {
                                                        tracing::error!("Failed to read zip archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut settings_file = match archive.by_name("settings.json") {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("'settings.json' not found in archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut contents = String::new();
                                                if let Err(e) = std::io::Read::read_to_string(&mut settings_file, &mut contents) {
                                                    tracing::error!("Failed to read settings.json from archive: {}", e);
                                                    return;
                                                }
                                                match serde_json::from_str::<Settings>(&contents) {
                                                    Ok(imported_settings) => {
                                                        local_settings.set(imported_settings);
                                                        tracing::info!("Successfully imported settings. Review and save.");
                                                    },
                                                    Err(e) => {
                                                        tracing::error!("Failed to parse imported settings.json: {}", e);
                                                    }
                                                }
                                            }
                                        });
                                    },
                                    "Import Settings"
                                }
                            }
                            div {
                                class: "flex space-x-2 mt-2",
                                button {
                                    class: "px-4 py-2 bg-green-600 rounded-md text-white font-semibold hover:bg-green-700",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_history.zip").save_file().await {
                                                let history_json = serde_json::to_string_pretty(&*session_state.read()).unwrap();
                                                let mut zip_buffer = Vec::new();
                                                {
                                                    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
                                                    zip.start_file("history.json", FileOptions::default()).unwrap();
                                                    zip.write_all(history_json.as_bytes()).unwrap();
                                                    zip.finish().unwrap();
                                                }
                                                if let Err(e) = std::fs::write(path.path(), &zip_buffer) {
                                                    tracing::error!("Failed to save history export: {}", e);
                                                }
                                            }
                                        });
                                    },
                                    "Export History"
                                }
                                button {
                                    class: "px-4 py-2 bg-green-600 rounded-md text-white font-semibold hover:bg-green-700",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_history.zip").pick_file().await {
                                                let file = match std::fs::File::open(path.path()) {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("Failed to open file: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut archive = match zip::ZipArchive::new(file) {
                                                    Ok(a) => a,
                                                    Err(e) => {
                                                        tracing::error!("Failed to read zip archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut history_file = match archive.by_name("history.json") {
                                                    Ok(f) => f,
                                                    Err(e) => {
                                                        tracing::error!("'history.json' not found in archive: {}", e);
                                                        return;
                                                    }
                                                };
                                                let mut contents = String::new();
                                                if let Err(e) = std::io::Read::read_to_string(&mut history_file, &mut contents) {
                                                    tracing::error!("Failed to read history.json from archive: {}", e);
                                                    return;
                                                }
                                                match serde_json::from_str::<SessionState>(&contents) {
                                                    Ok(imported_state) => {
                                                        let mut current_state = session_state.write();
                                                        for (id, session) in imported_state.sessions {
                                                            if current_state.sessions.contains_key(&id) {
                                                                conflicting_sessions.write().push((id, session));
                                                                // TODO: Implement conflict resolution modal
                                                            } else {
                                                                current_state.sessions.insert(id, session);
                                                            }
                                                        }
                                        
                                                        if !conflicting_sessions.read().is_empty() {
                                                            show_conflict_modal.set(true);
                                                        } else {
                                                            if let Err(e) = current_state.save() {
                                                                tracing::error!("Failed to save updated session state: {}", e);
                                                            } else {
                                                                tracing::info!("Successfully imported history with no conflicts.");
                                                            }
                                                        }
                                                    },
                                                    Err(e) => {
                                                        tracing::error!("Failed to parse imported history.json: {}", e);
                                                    }
                                                }
                                            }
                                        });
                                    },
                                    "Import History"
                                }
                            }
                        }
                    }
                }

                // Permissions Section
                div {
                    class: "mb-4 border border-gray-700 rounded-lg",
                    div {
                        class: "flex justify-between items-center p-3 cursor-pointer bg-gray-750 rounded-t-lg",
                        onclick: move |_| permissions_collapsed.set(!permissions_collapsed()),
                        h3 { class: "text-md font-semibold", "Permissions" }
                        span { class: "transform transition-transform", class: if permissions_collapsed() { "rotate-0" } else { "-rotate-180" }, "▼" }
                    }
                    if !permissions_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex items-center justify-between mb-3",
                                label { class: "block text-sm font-medium text-gray-300", "Enable Auto-Approval" }
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
                            if local_settings.read().permission_settings.auto_approval_enabled {
                                div {
                                    class: "mb-2 pl-4 border-l-2 border-gray-700",
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
                        if let Some(api_key) = settings_to_save.gemini_config.api_key.take() {
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