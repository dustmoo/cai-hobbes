use super::super::SettingsCtx;
use super::super::shared::load_instance_into_draft;
use crate::SecretManagerTrait;
use crate::session::SessionState;
use crate::settings::Settings;
use dioxus::prelude::*;
use std::io::Write;
use zip::write::{FileOptions, ZipWriter};

#[component]
pub(in super::super) fn DataTab(
    ctx: SettingsCtx,
    mut data_management_collapsed: Signal<bool>,
    mut show_conflict_modal: Signal<bool>,
    mut conflicting_sessions: Signal<Vec<(String, crate::session::Session)>>,
) -> Element {
    let settings = ctx.settings;
    let session_state = ctx.session_state;
    let mut secret_manager = ctx.secret_manager;
    let mut selected_connector_id = ctx.selected_connector_id;
    let mut local_settings = ctx.local_settings;
    rsx! {
                // Data Management Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: move |_| data_management_collapsed.set(!data_management_collapsed()),
                        h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "Data Management" }
                        span { class: "text-fg-muted text-xs", if *data_management_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !data_management_collapsed() {
                        div {
                            class: "p-4 flex flex-col gap-3",
                            p { class: "text-sm text-fg-muted -mt-1", "Move your settings and chat history between machines, or wipe stored credentials." }
                            div {
                                class: "flex space-x-2",
                                button {
                                    class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
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
                                    class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
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
                                                    Ok(mut imported_settings) => {
                                                        // Pre-multi-connector exports carry only the
                                                        // legacy singleton configs — synthesize instances.
                                                        imported_settings.migrate_legacy_llm_config();
                                                        let selected = imported_settings
                                                            .active_connector()
                                                            .map(|c| c.id.clone());
                                                        if let Some(ref id) = selected {
                                                            load_instance_into_draft(&mut imported_settings, id);
                                                        }
                                                        local_settings.set(imported_settings);
                                                        selected_connector_id.set(selected);
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
                                    class: "border border-faint text-fg rounded px-3 py-1.5 text-sm hover:bg-card",
                                    onclick: move |_| {
                                        spawn(async move {
                                            if let Some(path) = rfd::AsyncFileDialog::new().set_file_name("hobbes_history.zip").save_file().await {
                                                // Assemble the export incrementally from raw stored
                                                // blobs — the full history can be hundreds of MB and
                                                // must not round-trip through a Value tree.
                                                let history_json = {
                                                    let state = session_state.read();
                                                    let mut out = String::from("{\n\"schema_version\": ");
                                                    out.push_str(&state.schema_version.to_string());
                                                    out.push_str(",\n\"active_session_id\": ");
                                                    out.push_str(&serde_json::to_string(&state.active_session_id).unwrap_or_else(|_| "\"\"".into()));
                                                    out.push_str(&format!(",\n\"window_width\": {},\n\"window_height\": {}", state.window_width, state.window_height));
                                                    out.push_str(&format!(",\n\"lifetime_cost\": {},\n\"lifetime_tokens\": {}", state.lifetime_cost, state.lifetime_tokens));
                                                    // Hydrated sessions may be newer than their stored rows
                                                    let hydrated: std::collections::HashMap<String, String> = state.sessions.iter()
                                                        .filter_map(|(id, s)| serde_json::to_string(s).ok().map(|j| (id.clone(), j)))
                                                        .collect();
                                                    drop(state);
                                                    out.push_str(",\n\"sessions\": {\n");
                                                    let mut first = true;
                                                    let push_entry = |out: &mut String, id: &str, data: &str, first: &mut bool| {
                                                        if !*first { out.push_str(",\n"); }
                                                        *first = false;
                                                        out.push_str(&serde_json::to_string(id).unwrap_or_else(|_| "\"\"".into()));
                                                        out.push_str(": ");
                                                        out.push_str(data);
                                                    };
                                                    let mut exported_ids = std::collections::HashSet::new();
                                                    let export_result = crate::session_store::export_all_raw(|id, data| {
                                                        let blob = hydrated.get(id).map(|s| s.as_str()).unwrap_or(data);
                                                        exported_ids.insert(id.to_string());
                                                        push_entry(&mut out, id, blob, &mut first);
                                                    });
                                                    if let Err(e) = export_result {
                                                        tracing::error!("Failed to export stored sessions: {}", e);
                                                    }
                                                    // Hydrated sessions whose first save hasn't landed yet
                                                    for (id, blob) in &hydrated {
                                                        if !exported_ids.contains(id) {
                                                            push_entry(&mut out, id, blob, &mut first);
                                                        }
                                                    }
                                                    out.push_str("\n}\n}");
                                                    out
                                                };
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
                                    class: "border border-faint text-fg rounded px-3 py-1.5 text-sm hover:bg-card",
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
                                                        // Imported sessions go straight to the SQLite store —
                                                        // they don't need to be hydrated into memory.
                                                        let mut imported = 0usize;
                                                        for (id, session) in imported_state.sessions {
                                                            let exists = session_state.peek().sessions.contains_key(&id)
                                                                || crate::session_store::contains(&id);
                                                            if exists {
                                                                conflicting_sessions.write().push((id, session));
                                                                // TODO: Implement conflict resolution modal
                                                            } else if crate::session_store::insert_if_absent(&session) {
                                                                imported += 1;
                                                            }
                                                        }

                                                        if !conflicting_sessions.read().is_empty() {
                                                            show_conflict_modal.set(true);
                                                        } else {
                                                            tracing::info!("Successfully imported {} sessions with no conflicts.", imported);
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
                            // Keychain Management
                            div {
                                class: "mt-4 pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-2", "Keychain Management" }
                                p {
                                    class: "text-xs text-fg-muted mb-3",
                                    "Manually reset all stored API keys. You'll need to re-enter them after resetting."
                                }
                                button {
                                    class: "border border-red-700/50 text-red-400 rounded px-3 py-1.5 text-sm",
                                    onclick: move |_| {
                                        // Delete all keychain items
                                        let deleted_keys = secret_manager.write().delete_all();
                                        tracing::info!("Reset {} keychain items.", deleted_keys.len());

                                        // Clear API keys from local settings
                                        let mut ls = local_settings.write();
                                        ls.gemini_config.api_key = None;
                                        ls.claude_config.api_key = None;
                                        ls.openai_compat_config.api_key = None;
                                        for connector in ls.llm_connectors.iter_mut() {
                                            connector.config.set_api_key(None);
                                        }
                                        ls.smithery_api_key = None;
                                        for profile in ls.composio_profiles.iter_mut() {
                                            profile.api_key = None;
                                        }
                                    },
                                    "Reset Keychain Secrets"
                                }
                            }
                        }
                    }
                }

    }
}
