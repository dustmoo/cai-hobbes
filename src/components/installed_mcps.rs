//! Settings → MCP: management view for all installed (non-Composio) MCP
//! servers, plus the advanced raw JSON editor (moved here from the MCP view).
//!
//! Edits mutate `mcp_servers.json` as a `serde_json::Value` so fields we
//! don't model are preserved, then trigger a manager reload.

// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use dioxus::prelude::*;
use futures_util::StreamExt;
use std::path::PathBuf;

use crate::components::syntax_highlighter::highlight_json;
use crate::mcp::manager::{McpManager, ServerStatus};
use crate::secret_manager::SecretManager;
use crate::secret_types::{mcp_bearer_key, mcp_env_key, SecretManagerTrait};
use crate::settings::{Settings, SettingsManager};

fn mcp_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("com.hobbes.app")
        .join("mcp_servers.json")
}

/// Summary row model for one configured server.
#[derive(Clone, PartialEq)]
struct InstalledServer {
    name: String,
    description: String,
    is_remote: bool,
    disabled: bool,
    source: Option<String>,
    sandbox: Option<bool>,
    allowed_paths: Vec<String>,
    allow_network: bool,
    is_legacy_smithery: bool,
}

fn parse_servers(json: &serde_json::Value) -> Vec<InstalledServer> {
    let Some(map) = json.get("mcpServers").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    let mut servers: Vec<InstalledServer> = map
        .iter()
        .map(|(name, cfg)| {
            let args_smithery = cfg
                .get("args")
                .and_then(|a| a.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|v| v.as_str())
                        .any(|s| s.contains("@smithery/cli"))
                })
                .unwrap_or(false);
            let uri = cfg.get("uri").and_then(|u| u.as_str());
            InstalledServer {
                name: name.clone(),
                description: cfg
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                is_remote: uri.is_some(),
                disabled: cfg.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false),
                source: cfg
                    .get("source")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                sandbox: cfg.get("sandbox").and_then(|s| s.as_bool()),
                allowed_paths: cfg
                    .get("allowed_paths")
                    .and_then(|p| p.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                allow_network: cfg
                    .get("allow_network")
                    .and_then(|n| n.as_bool())
                    .unwrap_or(true),
                is_legacy_smithery: args_smithery
                    || uri.map(|u| u.contains("smithery.ai")).unwrap_or(false),
            }
        })
        .collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers
}

/// Effective sandbox state mirroring `McpServerConfig::sandbox_enabled`.
fn effective_sandbox(server: &InstalledServer) -> bool {
    match server.sandbox {
        Some(v) => v,
        None => cfg!(target_os = "macos") && server.source.as_deref() == Some("glama"),
    }
}

#[component]
pub fn InstalledMcps() -> Element {
    let mcp_manager = use_context::<Signal<McpManager>>();
    let mut mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<SecretManager>>();
    // Runtime load-state lives in UiState (shared with the MCP view dropdown).
    let ui_state = use_context::<Signal<crate::settings::UiState>>();
    let ui_state_manager = use_context::<Signal<crate::settings::UiStateManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    let mut raw_text = use_signal(String::new);
    let refresh = use_signal(|| 0i32);
    let mut error_message = use_signal(|| Option::<String>::None);
    let mut confirm_remove = use_signal(|| Option::<String>::None);
    let mut advanced_open = use_signal(|| false);
    let mut raw_dirty = use_signal(|| false);
    // Editor-local validation feedback shown next to the editor (not the
    // top-of-panel error banner).
    let mut editor_message = use_signal(|| Option::<String>::None);
    let mut editor_error = use_signal(|| false);

    // (Re)load the config file whenever refresh bumps — but never clobber
    // unsaved edits in the advanced editor (refresh also fires on dropdown
    // changes, which don't touch the config file).
    use_effect(move || {
        let _ = refresh.read();
        if *raw_dirty.peek() {
            return;
        }
        match std::fs::read_to_string(mcp_config_path()) {
            Ok(content) => raw_text.set(content),
            Err(_) => raw_text.set(r#"{ "mcpServers": {} }"#.to_string()),
        }
        raw_dirty.set(false);
    });

    // Live server health, keyed by name.
    let statuses = use_resource(move || {
        let _ = refresh.read();
        let mcp_manager = mcp_manager;
        async move {
            let manager = mcp_manager.read().clone();
            manager.get_all_server_statuses().await
        }
    });

    let servers: Vec<InstalledServer> = serde_json::from_str::<serde_json::Value>(&raw_text.read())
        .map(|v| parse_servers(&v))
        .unwrap_or_default();

    // Persist new content, reload the manager, and re-read state.
    let save_config = use_coroutine(move |mut rx: UnboundedReceiver<String>| {
        let mut error_message = error_message.to_owned();
        let mut refresh = refresh.to_owned();
        let mut raw_dirty = raw_dirty.to_owned();
        async move {
            while let Some(new_content) = rx.next().await {
                let path = mcp_config_path();
                let content_for_write = new_content.clone();
                let write_result =
                    tokio::task::spawn_blocking(move || std::fs::write(&path, &content_for_write))
                        .await
                        .unwrap();
                match write_result {
                    Ok(_) => {
                        error_message.set(None);
                        // Persisted — allow the reload effect to refresh from disk.
                        raw_dirty.set(false);
                        let manager = mcp_manager.read().clone();
                        let context_signal = mcp_context;
                        let current_settings = settings.read().clone();
                        spawn(async move {
                            manager.reload_config(context_signal, current_settings).await;
                        });
                        let current = *refresh.peek();
                        refresh.set(current + 1);
                    }
                    Err(e) => {
                        error_message.set(Some(format!("Failed to save config: {}", e)));
                    }
                }
            }
        }
    });

    // Mutate one field of one server in the raw JSON and save.
    let mut mutate_server = move |name: String, apply: Box<dyn FnOnce(&mut serde_json::Value)>| {
        let content = raw_text.peek().clone();
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) else {
            error_message.set(Some("Config JSON is invalid — fix it in the editor below".to_string()));
            return;
        };
        let Some(server) = json
            .get_mut("mcpServers")
            .and_then(|s| s.get_mut(&name))
        else {
            return;
        };
        apply(server);
        if let Ok(new_content) = serde_json::to_string_pretty(&json) {
            save_config.send(new_content);
        }
    };

    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4",
                label { class: "block text-sm font-medium text-fg-muted mb-1", "Installed MCP Servers" }
                p { class: "text-xs text-fg-muted mb-3", "Servers installed from Glama or added manually. Composio toolkits are managed in the profile section above." }

                if let Some(err) = error_message.read().as_ref() {
                    div { class: "mb-3 p-2 bg-red-900 text-red-200 rounded text-sm", "{err}" }
                }

                if servers.is_empty() {
                    p { class: "text-sm text-fg-muted italic", "No servers installed yet — browse the MCP store to add some." }
                }

                div {
                    class: "space-y-3",
                    for server in servers.iter().cloned() {
                        div {
                            class: "p-3 bg-input/40 border border-subtle rounded-lg",
                            // Header row: status dot, name, badges, controls
                            div {
                                class: "flex items-center gap-2 flex-wrap",
                                {
                                    let full = statuses.read().as_ref().and_then(|list| {
                                        list.iter().find(|s| s.name == server.name).cloned()
                                    });
                                    // Light is off (gray) whenever tools aren't
                                    // loaded, matching the MCP view.
                                    let dot = match &full {
                                        Some(s) if !s.is_loaded => "h-2.5 w-2.5 rounded-full bg-gray-500",
                                        Some(s) => match s.status {
                                            ServerStatus::Loaded => "h-2.5 w-2.5 rounded-full bg-green-500",
                                            ServerStatus::Error => "h-2.5 w-2.5 rounded-full bg-red-500",
                                            ServerStatus::NeedsAuth => "h-2.5 w-2.5 rounded-full bg-yellow-500",
                                            _ => "h-2.5 w-2.5 rounded-full bg-gray-500",
                                        },
                                        None => "h-2.5 w-2.5 rounded-full bg-gray-500",
                                    };
                                    rsx! { span { class: "{dot}" } }
                                }
                                span { class: "font-mono text-sm font-semibold text-fg", "{server.name}" }
                                span {
                                    class: "px-2 py-0.5 rounded-full text-[10px] font-medium bg-input text-fg-muted uppercase tracking-wide",
                                    if server.is_remote { "Remote" } else { "Local" }
                                }
                                if server.source.as_deref() == Some("glama") {
                                    span {
                                        class: "px-2 py-0.5 rounded-full text-[10px] font-medium bg-primary-900/60 text-primary-300 uppercase tracking-wide",
                                        "Glama"
                                    }
                                }
                                if server.is_legacy_smithery {
                                    span {
                                        class: "px-2 py-0.5 rounded-full text-[10px] font-medium bg-red-900/60 text-red-300 uppercase tracking-wide",
                                        "Smithery — no longer supported"
                                    }
                                }
                                div { class: "flex-1" }
                                // Load-mode selector — mirrors the MCP view dropdown
                                // (runtime state in UiState, not the config `disabled` field).
                                {
                                    let full = statuses.read().as_ref().and_then(|list| {
                                        list.iter().find(|s| s.name == server.name).cloned()
                                    });
                                    match full {
                                        Some(s) if matches!(s.status, ServerStatus::Loaded | ServerStatus::Disabled) => {
                                            let current_mode = if !s.is_loaded {
                                                "disabled"
                                            } else if s.is_on_demand {
                                                "ondemand"
                                            } else {
                                                "loaded"
                                            };
                                            let name = server.name.clone();
                                            rsx! {
                                                select {
                                                    class: "text-xs px-2 py-1 bg-section border border-faint rounded cursor-pointer text-fg",
                                                    value: current_mode,
                                                    onchange: move |event: dioxus::events::FormEvent| {
                                                        let new_mode = event.value();
                                                        let server_name = name.clone();
                                                        let mut refresh = refresh;
                                                        let mut ui_state = ui_state;
                                                        let ui_state_manager = ui_state_manager;
                                                        spawn(async move {
                                                            match new_mode.as_str() {
                                                                "loaded" => { mcp_manager.read().set_server_loaded(&server_name).await; }
                                                                "ondemand" => { mcp_manager.read().set_server_on_demand(&server_name).await; }
                                                                _ => { mcp_manager.read().unload_server(&server_name).await; }
                                                            }
                                                            // Persist runtime state to UiState (same lists the MCP view uses)
                                                            {
                                                                let mut state = ui_state.write();
                                                                state.unloaded_mcp_servers.retain(|s| s != &server_name);
                                                                state.on_demand_mcp_servers.retain(|s| s != &server_name);
                                                                match new_mode.as_str() {
                                                                    "ondemand" => state.on_demand_mcp_servers.push(server_name.clone()),
                                                                    "disabled" => state.unloaded_mcp_servers.push(server_name.clone()),
                                                                    _ => {}
                                                                }
                                                                ui_state_manager.read().save_async(state.clone(), Some(save_error));
                                                            }
                                                            let new_context = mcp_manager.read().get_mcp_context(None).await;
                                                            mcp_context.set(new_context);
                                                            let current = *refresh.peek();
                                                            refresh.set(current + 1);
                                                        });
                                                    },
                                                    option { value: "loaded", "Loaded (always)" }
                                                    option { value: "ondemand", "On-demand" }
                                                    option { value: "disabled", "Disabled" }
                                                }
                                            }
                                        }
                                        Some(s) => {
                                            // Errored / needs-auth: show a status label instead of the dropdown
                                            let label = match s.status {
                                                ServerStatus::Error => "Error",
                                                ServerStatus::NeedsAuth => "Needs auth",
                                                _ => "Not loaded",
                                            };
                                            rsx! { span { class: "text-xs text-fg-muted", "{label}" } }
                                        }
                                        None => rsx! { span { class: "text-xs text-fg-muted", "Not loaded" } },
                                    }
                                }
                                // Remove (two-step confirm)
                                if confirm_remove.read().as_deref() == Some(server.name.as_str()) {
                                    button {
                                        class: "px-2 py-1 text-xs rounded bg-red-700 hover:bg-red-600 text-fg font-medium transition-colors",
                                        onclick: {
                                            let name = server.name.clone();
                                            move |_| {
                                                let content = raw_text.peek().clone();
                                                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
                                                    if let Some(map) = json.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
                                                        // Delete keychain-stored env secrets before dropping the entry
                                                        if let Some(removed) = map.remove(&name) {
                                                            if let Some(vars) = removed.get("secret_env").and_then(|v| v.as_array()) {
                                                                for var in vars.iter().filter_map(|v| v.as_str()) {
                                                                    let _ = secret_manager.write().delete_indexed_secret(&mcp_env_key(&name, var));
                                                                }
                                                            }
                                                        }
                                                        if let Ok(new_content) = serde_json::to_string_pretty(&json) {
                                                            save_config.send(new_content);
                                                        }
                                                    }
                                                }
                                                // Clean up associated secrets & permission entries
                                                let _ = secret_manager.write().delete_indexed_secret(&mcp_bearer_key(&name));
                                                {
                                                    let mut s = settings;
                                                    s.write().permission_settings.mcp_server_permissions.remove(&name);
                                                    let snapshot = s.peek().clone();
                                                    settings_manager.peek().save_async(snapshot, None);
                                                }
                                                confirm_remove.set(None);
                                            }
                                        },
                                        "Confirm remove"
                                    }
                                    button {
                                        class: "px-2 py-1 text-xs rounded bg-input text-fg-muted hover:text-fg transition-colors",
                                        onclick: move |_| confirm_remove.set(None),
                                        "Cancel"
                                    }
                                } else {
                                    button {
                                        class: "px-2 py-1 text-xs rounded bg-input text-red-400 hover:text-red-300 transition-colors",
                                        onclick: {
                                            let name = server.name.clone();
                                            move |_| confirm_remove.set(Some(name.clone()))
                                        },
                                        "Remove"
                                    }
                                }
                            }
                            if !server.description.is_empty() {
                                p { class: "text-xs text-fg-muted mt-1", "{server.description}" }
                            }

                            // Controls row: permissions + sandbox (local servers, macOS)
                            div {
                                class: "flex items-center gap-4 mt-2 flex-wrap",
                                // Tool-call permission (uses the existing PermissionManager model)
                                {
                                    let allowed = settings.read()
                                        .permission_settings
                                        .mcp_server_permissions
                                        .get(&server.name)
                                        .copied()
                                        .unwrap_or(true);
                                    let name = server.name.clone();
                                    rsx! {
                                        label {
                                            class: "flex items-center gap-1.5 text-xs text-fg cursor-pointer",
                                            input {
                                                r#type: "checkbox",
                                                class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                                checked: !allowed,
                                                oninput: move |e: Event<FormData>| {
                                                    let prompt_each = e.value() == "true";
                                                    let mut s = settings;
                                                    s.write().permission_settings.mcp_server_permissions.insert(name.clone(), !prompt_each);
                                                    let snapshot = s.peek().clone();
                                                    settings_manager.peek().save_async(snapshot, None);
                                                }
                                            }
                                            span { "Ask before every tool call" }
                                        }
                                    }
                                }
                                if !server.is_remote && cfg!(target_os = "macos") {
                                    {
                                        let sandboxed = effective_sandbox(&server);
                                        let name = server.name.clone();
                                        rsx! {
                                            label {
                                                class: "flex items-center gap-1.5 text-xs text-fg cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                                    checked: sandboxed,
                                                    oninput: move |e: Event<FormData>| {
                                                        let enable = e.value() == "true";
                                                        mutate_server(name.clone(), Box::new(move |s| {
                                                            s["sandbox"] = serde_json::json!(enable);
                                                        }));
                                                    }
                                                }
                                                span { "Sandbox" }
                                            }
                                            if !sandboxed && server.source.as_deref() == Some("glama") {
                                                span { class: "text-xs text-amber-400", "⚠ full file & network access" }
                                            }
                                        }
                                    }
                                }
                                if !server.is_remote {
                                    {
                                        let name = server.name.clone();
                                        let allow_network = server.allow_network;
                                        rsx! {
                                            label {
                                                class: "flex items-center gap-1.5 text-xs text-fg cursor-pointer",
                                                input {
                                                    r#type: "checkbox",
                                                    class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                                    checked: allow_network,
                                                    oninput: move |e: Event<FormData>| {
                                                        let enable = e.value() == "true";
                                                        mutate_server(name.clone(), Box::new(move |s| {
                                                            s["allow_network"] = serde_json::json!(enable);
                                                        }));
                                                    }
                                                }
                                                span { "Network" }
                                            }
                                        }
                                    }
                                }
                            }

                            // Allowed directories (sandboxed local servers)
                            if !server.is_remote && cfg!(target_os = "macos") && effective_sandbox(&server) {
                                div {
                                    class: "mt-2",
                                    div {
                                        class: "flex items-center gap-2 flex-wrap",
                                        span { class: "text-xs text-fg-muted", "Allowed directories:" }
                                        for (i, path) in server.allowed_paths.iter().cloned().enumerate() {
                                            span {
                                                class: "flex items-center gap-1 px-2 py-0.5 bg-input rounded text-[11px] font-mono text-fg",
                                                "{path}"
                                                button {
                                                    class: "text-red-400 hover:text-red-300 ml-1",
                                                    onclick: {
                                                        let name = server.name.clone();
                                                        move |_| {
                                                            mutate_server(name.clone(), Box::new(move |s| {
                                                                if let Some(arr) = s.get_mut("allowed_paths").and_then(|p| p.as_array_mut()) {
                                                                    if i < arr.len() { arr.remove(i); }
                                                                }
                                                            }));
                                                        }
                                                    },
                                                    "✕"
                                                }
                                            }
                                        }
                                        button {
                                            class: "px-2 py-0.5 bg-input hover:bg-input/70 border border-subtle rounded text-[11px] text-fg transition-colors",
                                            onclick: {
                                                let name = server.name.clone();
                                                move |_| {
                                                    let name = name.clone();
                                                    spawn(async move {
                                                        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                                                            let path = folder.path().to_string_lossy().to_string();
                                                            mutate_server(name.clone(), Box::new(move |s| {
                                                                if s.get("allowed_paths").and_then(|p| p.as_array()).is_none() {
                                                                    s["allowed_paths"] = serde_json::json!([]);
                                                                }
                                                                if let Some(arr) = s.get_mut("allowed_paths").and_then(|p| p.as_array_mut()) {
                                                                    let val = serde_json::json!(path);
                                                                    if !arr.contains(&val) { arr.push(val); }
                                                                }
                                                            }));
                                                        }
                                                    });
                                                }
                                            },
                                            "+ Add"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Advanced: raw JSON editor (moved from the MCP view)
                div {
                    class: "mt-4 bg-app/50 rounded-lg border border-subtle/50 overflow-hidden",
                    div {
                        class: "flex justify-between items-center p-3 cursor-pointer bg-section/50 hover:bg-section transition-colors",
                        onclick: move |_| {
                            let open = *advanced_open.peek();
                            advanced_open.set(!open);
                        },
                        h4 { class: "text-sm font-semibold text-fg", "Advanced: edit mcp_servers.json" }
                        span { if *advanced_open.read() { "▼" } else { "▶" } }
                    }
                    if *advanced_open.read() {
                        div {
                            class: "p-3 border-t border-subtle/30",
                            // Editor-local validation feedback
                            if let Some(msg) = editor_message.read().as_ref() {
                                div {
                                    class: if *editor_error.read() {
                                        "mb-2 p-2 bg-red-900/60 text-red-200 rounded text-xs font-mono"
                                    } else {
                                        "mb-2 p-2 bg-green-900/50 text-green-200 rounded text-xs"
                                    },
                                    "{msg}"
                                }
                            }
                            div {
                                class: "relative bg-section rounded-md border border-faint h-80",
                                id: "installed-json-container",
                                // Highlighted background layer (scrolls in sync with the textarea)
                                pre {
                                    class: "absolute inset-0 p-3 text-sm font-mono pointer-events-none whitespace-pre break-normal overflow-auto",
                                    id: "installed-json-highlight",
                                    code { dangerous_inner_html: "{highlight_json(raw_text.read().clone())}" }
                                }
                                // Editable overlay
                                textarea {
                                    class: "absolute inset-0 w-full h-full p-3 bg-transparent font-mono text-sm text-transparent caret-white border-0 focus:outline-none resize-none overflow-auto whitespace-pre break-normal",
                                    id: "installed-json-editor",
                                    style: "color: transparent;",
                                    value: "{raw_text}",
                                    spellcheck: false,
                                    oninput: move |e| {
                                        raw_text.set(e.value());
                                        raw_dirty.set(true);
                                        editor_message.set(None);
                                    },
                                    onscroll: move |_| {
                                        let _ = document::eval(r#"
                                            const editor = document.getElementById('installed-json-editor');
                                            const highlight = document.getElementById('installed-json-highlight');
                                            if (editor && highlight) {
                                                highlight.scrollTop = editor.scrollTop;
                                                highlight.scrollLeft = editor.scrollLeft;
                                            }
                                        "#);
                                    },
                                }
                            }
                            div {
                                class: "flex justify-end gap-2 mt-2",
                                button {
                                    class: "px-4 py-1.5 bg-input hover:bg-input/70 rounded text-sm font-medium text-fg transition-colors",
                                    onclick: move |_| {
                                        let content = raw_text.peek().clone();
                                        match serde_json::from_str::<serde_json::Value>(&content) {
                                            Ok(parsed) => {
                                                if let Ok(formatted) = serde_json::to_string_pretty(&parsed) {
                                                    raw_text.set(formatted);
                                                    raw_dirty.set(true);
                                                    editor_error.set(false);
                                                    editor_message.set(Some("JSON formatted.".to_string()));
                                                }
                                            }
                                            Err(e) => {
                                                editor_error.set(true);
                                                editor_message.set(Some(format!("Invalid JSON: {}", e)));
                                            }
                                        }
                                    },
                                    "Format JSON"
                                }
                                button {
                                    class: "px-4 py-1.5 bg-btn-primary hover:bg-btn-primary-hover rounded text-sm font-medium transition-colors disabled:bg-input disabled:cursor-not-allowed",
                                    disabled: !*raw_dirty.read(),
                                    onclick: move |_| {
                                        let content = raw_text.peek().clone();
                                        match serde_json::from_str::<serde_json::Value>(&content) {
                                            Ok(_) => {
                                                editor_error.set(false);
                                                editor_message.set(Some("Saved. Reloading servers…".to_string()));
                                                save_config.send(content);
                                            }
                                            Err(e) => {
                                                editor_error.set(true);
                                                editor_message.set(Some(format!("Invalid JSON: {}", e)));
                                            }
                                        }
                                    },
                                    "Save & Reload"
                                }
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

    #[test]
    fn parses_installed_servers_with_flags() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
            "mcpServers": {
                "local-one": { "command": "npx", "args": ["-y", "pkg"], "source": "glama", "sandbox": true, "allowed_paths": ["/tmp"], "allow_network": false },
                "remote-one": { "uri": "https://x/mcp", "disabled": true },
                "legacy": { "command": "npx", "args": ["-y", "@smithery/cli@latest", "run", "x"] }
            }
        }"#,
        )
        .unwrap();
        let servers = parse_servers(&json);
        assert_eq!(servers.len(), 3);
        let local = servers.iter().find(|s| s.name == "local-one").unwrap();
        assert!(!local.is_remote);
        assert_eq!(local.source.as_deref(), Some("glama"));
        assert_eq!(local.sandbox, Some(true));
        assert_eq!(local.allowed_paths, vec!["/tmp"]);
        assert!(!local.allow_network);
        let remote = servers.iter().find(|s| s.name == "remote-one").unwrap();
        assert!(remote.is_remote);
        assert!(remote.disabled);
        let legacy = servers.iter().find(|s| s.name == "legacy").unwrap();
        assert!(legacy.is_legacy_smithery);
    }
}
