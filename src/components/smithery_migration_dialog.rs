//! One-time dialog shown after upgrading to a version without Smithery support.
//!
//! Detects leftover Smithery data (old `preferred_mcp_source` value, installed
//! `@smithery/cli` servers in `mcp_servers.json`, orphaned keychain API key)
//! and lets the user either keep everything (e.g. to downgrade later) or
//! remove selected items. Nothing is ever deleted without an explicit confirm.

use dioxus::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::secret_manager::SecretManager;
use crate::secret_types::{SecretManagerTrait, LEGACY_SMITHERY_KEY};
use crate::settings::{Settings, SettingsManager};

fn mcp_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("mcp_servers.json")
}

/// Names of servers in mcp_servers.json that reference Smithery
/// (command args containing `@smithery/cli` or a smithery.ai URI).
fn detect_smithery_servers(config_content: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(config_content) else {
        return Vec::new();
    };
    let Some(servers) = json.get("mcpServers").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter(|(_, cfg)| {
            let args_hit = cfg
                .get("args")
                .and_then(|a| a.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|v| v.as_str())
                        .any(|s| s.contains("@smithery/cli"))
                })
                .unwrap_or(false);
            let uri_hit = cfg
                .get("uri")
                .and_then(|u| u.as_str())
                .map(|u| u.contains("smithery.ai"))
                .unwrap_or(false);
            args_hit || uri_hit
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Remove the given server entries from the config JSON, returning the new
/// pretty-printed content (None if parsing failed or nothing changed).
fn remove_servers_from_config(config_content: &str, names: &HashSet<String>) -> Option<String> {
    let mut json = serde_json::from_str::<serde_json::Value>(config_content).ok()?;
    let servers = json.get_mut("mcpServers")?.as_object_mut()?;
    let before = servers.len();
    servers.retain(|name, _| !names.contains(name));
    if servers.len() == before {
        return None;
    }
    serde_json::to_string_pretty(&json).ok()
}

#[component]
pub fn SmitheryMigrationDialog() -> Element {
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<SecretManager>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();

    // Detected Smithery server entries (loaded once on mount).
    let mut detected_servers = use_signal(Vec::<String>::new);
    let mut detection_done = use_signal(|| false);
    // Which items the user has checked for removal.
    let mut checked_servers = use_signal(HashSet::<String>::new);
    let mut check_keychain = use_signal(|| true);
    // Whether the user opened the removal sub-view.
    let mut show_removal = use_signal(|| false);

    use_effect(move || {
        let content = std::fs::read_to_string(mcp_config_path()).unwrap_or_default();
        let servers = detect_smithery_servers(&content);
        checked_servers.set(servers.iter().cloned().collect());
        detected_servers.set(servers);
        detection_done.set(true);
    });

    let acknowledged = settings.read().smithery_migration_acknowledged;
    let settings_detected = settings.read().smithery_settings_detected;
    let has_servers = !detected_servers.read().is_empty();

    // Show only when something Smithery-related was found and the user hasn't
    // dismissed the dialog yet.
    if acknowledged || !*detection_done.read() || (!settings_detected && !has_servers) {
        return rsx! {};
    }

    let acknowledge = move || {
        let mut s = settings;
        {
            let mut w = s.write();
            w.smithery_migration_acknowledged = true;
        }
        let snapshot = s.read().clone();
        settings_manager.read().save_async(snapshot, None);
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/60 flex items-center justify-center z-50",
            div {
                class: "bg-section border border-subtle rounded-lg shadow-xl max-w-lg w-full mx-4 p-6 max-h-[80vh] overflow-y-auto",
                h2 { class: "text-lg font-bold text-fg mb-2", "Smithery support has been removed" }
                p {
                    class: "text-sm text-fg-muted mb-3",
                    "This version of Hobbes no longer supports the deprecated Smithery.ai integration. Servers installed through Smithery will not run anymore."
                }
                p {
                    class: "text-sm text-fg-muted mb-4",
                    "Your settings are untouched either way — if you prefer to keep using Smithery, you can downgrade to the previous version without losing anything."
                }

                if !detected_servers.read().is_empty() {
                    div {
                        class: "mb-4 p-3 bg-input rounded border border-subtle",
                        p { class: "text-xs font-semibold text-fg-muted mb-2", "Detected Smithery servers:" }
                        ul {
                            class: "text-sm text-fg space-y-1",
                            for name in detected_servers.read().iter() {
                                li { class: "font-mono text-xs", "• {name}" }
                            }
                        }
                    }
                }

                if !*show_removal.read() {
                    div {
                        class: "flex justify-end gap-3 mt-4",
                        button {
                            class: "px-4 py-2 bg-input hover:bg-input/70 rounded-md text-sm font-medium text-fg transition-colors",
                            onclick: move |_| acknowledge(),
                            "Keep everything (I may downgrade)"
                        }
                        button {
                            class: "px-4 py-2 bg-red-700 hover:bg-red-600 rounded-md text-sm font-medium text-fg transition-colors",
                            onclick: move |_| show_removal.set(true),
                            "Remove Smithery data…"
                        }
                    }
                } else {
                    div {
                        class: "mt-4 border-t border-subtle pt-4",
                        p { class: "text-sm font-semibold text-fg mb-2", "Select what to remove:" }
                        div {
                            class: "space-y-2 mb-4",
                            for name in detected_servers.read().iter().cloned() {
                                label {
                                    class: "flex items-center gap-2 text-sm text-fg",
                                    input {
                                        r#type: "checkbox",
                                        class: "form-checkbox bg-input border-faint text-primary-500",
                                        checked: checked_servers.read().contains(&name),
                                        oninput: {
                                            let name = name.clone();
                                            move |e: Event<FormData>| {
                                                let mut set = checked_servers.write();
                                                if e.value() == "true" {
                                                    set.insert(name.clone());
                                                } else {
                                                    set.remove(&name);
                                                }
                                            }
                                        }
                                    }
                                    span { class: "font-mono text-xs", "Server entry: {name}" }
                                }
                            }
                            label {
                                class: "flex items-center gap-2 text-sm text-fg",
                                input {
                                    r#type: "checkbox",
                                    class: "form-checkbox bg-input border-faint text-primary-500",
                                    checked: *check_keychain.read(),
                                    oninput: move |e: Event<FormData>| check_keychain.set(e.value() == "true"),
                                }
                                span { "Smithery API key stored in the keychain" }
                            }
                        }
                        div {
                            class: "flex justify-end gap-3",
                            button {
                                class: "px-4 py-2 bg-input hover:bg-input/70 rounded-md text-sm font-medium text-fg transition-colors",
                                onclick: move |_| show_removal.set(false),
                                "Back"
                            }
                            button {
                                class: "px-4 py-2 bg-red-700 hover:bg-red-600 rounded-md text-sm font-medium text-fg transition-colors",
                                onclick: move |_| {
                                    // Remove checked server entries from mcp_servers.json
                                    let to_remove = checked_servers.read().clone();
                                    if !to_remove.is_empty() {
                                        let path = mcp_config_path();
                                        if let Ok(content) = std::fs::read_to_string(&path) {
                                            if let Some(new_content) = remove_servers_from_config(&content, &to_remove) {
                                                match std::fs::write(&path, &new_content) {
                                                    Ok(()) => {
                                                        tracing::info!("Removed {} Smithery server entries", to_remove.len());
                                                        let manager = mcp_manager.read().clone();
                                                        let context_signal = mcp_context;
                                                        let current_settings = settings.read().clone();
                                                        spawn(async move {
                                                            manager.reload_config(context_signal, current_settings).await;
                                                        });
                                                    }
                                                    Err(e) => tracing::error!("Failed to write mcp_servers.json: {}", e),
                                                }
                                            }
                                        }
                                    }
                                    // Remove the orphaned keychain entry
                                    if *check_keychain.read() {
                                        match secret_manager.write().delete(LEGACY_SMITHERY_KEY) {
                                            Ok(()) => tracing::info!("Deleted legacy Smithery keychain entry"),
                                            Err(e) => tracing::debug!("Smithery keychain entry not deleted (may not exist): {}", e),
                                        }
                                    }
                                    acknowledge();
                                },
                                "Remove selected"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{
        "mcpServers": {
            "calendar": {
                "command": "npx",
                "args": ["-y", "@smithery/cli@latest", "run", "calendar", "--key", "abc"]
            },
            "remote-smithery": {
                "uri": "https://server.smithery.ai/foo/mcp"
            },
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
            }
        }
    }"#;

    #[test]
    fn detects_smithery_servers_by_args_and_uri() {
        let mut found = detect_smithery_servers(CONFIG);
        found.sort();
        assert_eq!(found, vec!["calendar", "remote-smithery"]);
    }

    #[test]
    fn removal_only_touches_selected_entries() {
        let names: HashSet<String> = ["calendar".to_string()].into_iter().collect();
        let new_content = remove_servers_from_config(CONFIG, &names).unwrap();
        let json: serde_json::Value = serde_json::from_str(&new_content).unwrap();
        let servers = json.get("mcpServers").unwrap().as_object().unwrap();
        assert!(!servers.contains_key("calendar"));
        assert!(servers.contains_key("remote-smithery"));
        assert!(servers.contains_key("filesystem"));
    }

    #[test]
    fn removal_returns_none_when_nothing_matches() {
        let names: HashSet<String> = ["nonexistent".to_string()].into_iter().collect();
        assert!(remove_servers_from_config(CONFIG, &names).is_none());
    }

    #[test]
    fn detection_tolerates_invalid_json() {
        assert!(detect_smithery_servers("not json").is_empty());
    }
}
