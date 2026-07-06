//! Install dialogs for servers from the Glama registry.
//!
//! Local install: the run command is derived from the repository manifest
//! (package.json → npx, pyproject.toml → uvx) and shown for review; env vars
//! come from the registry's JSON schema. Servers install with a warning that
//! they are unvetted, run sandboxed by default wherever an OS sandbox is
//! available (macOS sandbox-exec, Linux bwrap, Windows AppContainer shim),
//! and start with per-call permission prompts until the user promotes them.
//! Env vars with credential-looking names are stored in the OS keychain.
//!
//! Remote connect: the user pastes an endpoint URL (a direct third-party
//! SSE/HTTP URL or a Glama gateway URL) plus an optional bearer token, which
//! is stored in the OS keychain — never in mcp_servers.json.

use dioxus::prelude::*;
use std::collections::HashMap;

use crate::components::mcp_marketplace::FeaturedMcp;
use crate::mcp::glama_client::derive_run_command;
use crate::secret_manager::SecretManager;
use crate::secret_types::{is_secret_env_name, mcp_bearer_key, mcp_env_key, SecretManagerTrait};
use crate::settings::{Settings, SettingsManager};

/// Short server name for the config map key: the slug part of the qualified
/// name with a trailing "-mcp"/"mcp-" affix trimmed for readability.
fn short_server_name(qualified: &str) -> String {
    let slug = qualified.split('/').next_back().unwrap_or(qualified);
    let trimmed = slug
        .strip_suffix("-mcp")
        .or_else(|| slug.strip_prefix("mcp-"))
        .unwrap_or(slug);
    if trimmed.is_empty() {
        slug.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Register a newly installed registry server as "prompt on every tool call"
/// so unvetted tools can't run silently, then persist settings.
fn default_permissions_to_prompt(
    settings: &Signal<Settings>,
    settings_manager: &Signal<SettingsManager>,
    server_name: &str,
) {
    let mut s = *settings;
    {
        let mut w = s.write();
        w.permission_settings
            .mcp_server_permissions
            .insert(server_name.to_string(), false);
    }
    let snapshot = s.peek().clone();
    settings_manager.peek().save_async(snapshot, None);
}

/// Whether a pasted endpoint URL is cleartext http:// to a non-loopback host —
/// worth a warning (bearer tokens would travel unencrypted), but not a block:
/// LAN-hosted MCP servers are a legitimate setup.
fn is_cleartext_remote(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return false;
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    // Strip the port: bracketed IPv6 first, then host:port
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    !matches!(host, "localhost" | "127.0.0.1" | "::1" | "")
}

/// Insert a server object into the `mcpServers` map of the raw config JSON.
/// Returns the new pretty-printed content.
fn insert_server_into_config(
    config_content: &str,
    name: &str,
    server: serde_json::Value,
) -> Result<String, String> {
    let mut json: serde_json::Value = serde_json::from_str(config_content)
        .unwrap_or_else(|_| serde_json::json!({ "mcpServers": {} }));
    let servers = json
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .ok_or("Invalid MCP config structure ('mcpServers' missing)")?;
    servers.insert(name.to_string(), server);
    serde_json::to_string_pretty(&json).map_err(|e| format!("Failed to serialize config: {}", e))
}

#[component]
pub fn GlamaLocalInstallDialog(
    mcp: FeaturedMcp,
    dialog: Signal<Option<FeaturedMcp>>,
    config_content: Signal<String>,
    save_config: Coroutine<String>,
    trigger_search: Signal<i32>,
) -> Element {
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<SecretManager>>();

    let mut server_name = use_signal(|| short_server_name(&mcp.name));
    let mut command = use_signal(String::new);
    let mut args_text = use_signal(String::new);
    let mut deriving = use_signal(|| true);
    let mut derive_failed = use_signal(|| false);
    let mut env_values = use_signal(|| {
        mcp.glama_env_vars
            .iter()
            .map(|v| (v.name.clone(), v.default.clone().unwrap_or_default()))
            .collect::<HashMap<String, String>>()
    });
    let mut error = use_signal(|| Option::<String>::None);

    // Sandbox controls (on by default for registry installs whenever the
    // platform sandbox is usable: sandbox-exec / bwrap / AppContainer shim)
    let sandbox_available = crate::mcp::sandbox::sandbox_available();
    let mut sandbox_enabled = use_signal(|| sandbox_available);
    let mut allowed_paths = use_signal(Vec::<String>::new);
    let mut allow_network = use_signal(|| true);

    // Derive the run command from the repository manifest once on open.
    {
        let repo = mcp.repository_url.clone();
        use_future(move || {
            let repo = repo.clone();
            async move {
                if let Some(repo_url) = repo {
                    if let Some((cmd, args)) = derive_run_command(&repo_url).await {
                        command.set(cmd);
                        args_text.set(args.join(" "));
                        deriving.set(false);
                        return;
                    }
                }
                derive_failed.set(true);
                deriving.set(false);
            }
        });
    }

    let mut close = move || dialog.set(None);

    let env_vars = mcp.glama_env_vars.clone();
    let install = move |_| {
        let cmd = command.read().trim().to_string();
        if cmd.is_empty() {
            error.set(Some("Enter a run command (e.g. npx -y package-name)".to_string()));
            return;
        }
        // Required env vars must be filled
        for var in env_vars.iter().filter(|v| v.required) {
            let filled = env_values
                .read()
                .get(&var.name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            if !filled {
                error.set(Some(format!("{} is required", var.name)));
                return;
            }
        }
        let name = server_name.read().trim().to_string();
        if name.is_empty() {
            error.set(Some("Enter a server name".to_string()));
            return;
        }

        let args: Vec<String> = args_text
            .read()
            .split_whitespace()
            .map(str::to_string)
            .collect();

        // Env vars whose names look like credentials go to the OS keychain
        // (same treatment as remote bearer tokens); the rest are written
        // plaintext to mcp_servers.json.
        let mut env: HashMap<String, String> = HashMap::new();
        let mut secret_env: Vec<String> = Vec::new();
        for (k, v) in env_values.read().iter() {
            let value = v.trim();
            if value.is_empty() {
                continue;
            }
            if is_secret_env_name(k) {
                if let Err(e) = secret_manager
                    .write()
                    .set_indexed_secret(&mcp_env_key(&name, k), value.to_string())
                {
                    error.set(Some(format!("Failed to store {} in keychain: {}", k, e)));
                    return;
                }
                secret_env.push(k.clone());
            } else {
                env.insert(k.clone(), value.to_string());
            }
        }
        secret_env.sort();

        let mut server = serde_json::json!({
            "command": cmd,
            "args": args,
            "env": env,
            "description": mcp.description,
            "source": "glama",
            "allow_network": *allow_network.read(),
        });
        if !secret_env.is_empty() {
            server["secret_env"] = serde_json::json!(secret_env);
        }
        if sandbox_available {
            server["sandbox"] = serde_json::json!(*sandbox_enabled.read());
            let paths = allowed_paths.read().clone();
            if !paths.is_empty() {
                server["allowed_paths"] = serde_json::json!(paths);
            }
        }

        match insert_server_into_config(&config_content.read(), &name, server) {
            Ok(new_content) => {
                save_config.send(new_content);
                default_permissions_to_prompt(&settings, &settings_manager, &name);
                let current = *trigger_search.peek();
                trigger_search.set(current + 1);
                close();
            }
            Err(e) => error.set(Some(e)),
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/60 flex items-center justify-center z-50",
            div {
                class: "bg-section border border-subtle rounded-lg shadow-xl max-w-xl w-full mx-4 p-6 max-h-[85vh] overflow-y-auto",
                h2 { class: "text-lg font-bold text-fg mb-1", "Install {mcp.display_name}" }

                div {
                    class: "mb-4 p-3 bg-amber-900/30 border border-amber-700 rounded text-amber-200 text-xs",
                    "Community server from the Glama registry — it is not vetted by Hobbes. Review the command below before installing. Tool calls will ask for approval until you allow them."
                }

                div {
                    class: "flex items-center gap-3 mb-4 text-xs",
                    if let Some(page) = mcp.glama_page_url.as_ref() {
                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "{page}", target: "_blank", "View on Glama" }
                    }
                    if let Some(repo) = mcp.repository_url.as_ref() {
                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "{repo}", target: "_blank", "Repository" }
                    }
                }

                // Server name
                label { class: "block text-sm font-medium text-fg-muted mb-1", "Server name" }
                input {
                    class: "w-full px-3 py-2 mb-3 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                    value: "{server_name}",
                    oninput: move |e| server_name.set(e.value()),
                }

                // Command + args
                label { class: "block text-sm font-medium text-fg-muted mb-1", "Command" }
                if *deriving.read() {
                    div {
                        class: "flex items-center gap-2 mb-3 text-sm text-fg-muted",
                        span { class: "inline-block animate-spin h-3 w-3 border-2 border-white border-t-transparent rounded-full" }
                        "Deriving run command from the repository…"
                    }
                } else {
                    div {
                        class: "flex gap-2 mb-1",
                        input {
                            class: "w-40 px-3 py-2 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                            placeholder: "npx",
                            value: "{command}",
                            oninput: move |e| command.set(e.value()),
                        }
                        input {
                            class: "flex-1 px-3 py-2 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                            placeholder: "-y package-name",
                            value: "{args_text}",
                            oninput: move |e| args_text.set(e.value()),
                        }
                    }
                    if *derive_failed.read() {
                        p { class: "text-xs text-amber-400 mb-3", "Couldn't derive a run command from the repository — enter it manually (check the README for install instructions)." }
                    } else {
                        p { class: "text-xs text-fg-muted mb-3", "Derived from the repository manifest — edit if needed." }
                    }
                }

                // Env vars from the registry schema
                if !mcp.glama_env_vars.is_empty() {
                    label { class: "block text-sm font-medium text-fg-muted mb-2", "Environment variables" }
                    div {
                        class: "space-y-3 mb-4",
                        for var in mcp.glama_env_vars.iter().cloned() {
                            div {
                                label {
                                    class: "block text-xs font-mono text-fg mb-0.5",
                                    "{var.name}"
                                    if var.required {
                                        span { class: "text-red-400 ml-1", "*" }
                                    }
                                }
                                if let Some(desc) = var.description.as_ref() {
                                    p { class: "text-xs text-fg-muted mb-1", "{desc}" }
                                }
                                input {
                                    class: "w-full px-3 py-1.5 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                                    value: "{env_values.read().get(&var.name).cloned().unwrap_or_default()}",
                                    oninput: {
                                        let name = var.name.clone();
                                        move |e: Event<FormData>| {
                                            env_values.write().insert(name.clone(), e.value());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Sandbox section (macOS)
                if sandbox_available {
                    div {
                        class: "mb-4 p-3 bg-input/50 border border-subtle rounded",
                        label {
                            class: "flex items-center gap-2 text-sm text-fg cursor-pointer",
                            input {
                                r#type: "checkbox",
                                class: "form-checkbox bg-input border-faint text-primary-500",
                                checked: *sandbox_enabled.read(),
                                oninput: move |e| sandbox_enabled.set(e.value() == "true"),
                            }
                            span { class: "font-medium", "Run in sandbox (recommended)" }
                        }
                        if *sandbox_enabled.read() {
                            p { class: "text-xs text-fg-muted mt-1 mb-2", "The server is blocked from your documents, credentials, browser data and chat history, and can only write to temp, its tool caches, and the directories you allow below." }
                            div {
                                class: "space-y-1 mb-2",
                                for (i, path) in allowed_paths.read().iter().cloned().enumerate() {
                                    div {
                                        class: "flex items-center gap-2",
                                        span { class: "flex-1 text-xs font-mono text-fg truncate", "{path}" }
                                        button {
                                            class: "text-xs text-red-400 hover:text-red-300",
                                            onclick: move |_| { allowed_paths.write().remove(i); },
                                            "Remove"
                                        }
                                    }
                                }
                            }
                            button {
                                class: "px-3 py-1 bg-input hover:bg-input/70 border border-subtle rounded text-xs text-fg transition-colors",
                                onclick: move |_| {
                                    spawn(async move {
                                        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                                            let path = folder.path().to_string_lossy().to_string();
                                            let mut paths = allowed_paths.write();
                                            if !paths.contains(&path) {
                                                paths.push(path);
                                            }
                                        }
                                    });
                                },
                                "+ Allow directory…"
                            }
                            label {
                                class: "flex items-center gap-2 mt-2 text-xs text-fg cursor-pointer",
                                input {
                                    r#type: "checkbox",
                                    class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                    checked: *allow_network.read(),
                                    oninput: move |e| allow_network.set(e.value() == "true"),
                                }
                                span { "Allow network access" }
                            }
                            p { class: "text-xs text-fg-muted mt-1", "Most servers need this — npx/uvx also download the package on first launch. Turn it off only for fully local tools." }
                        } else {
                            p { class: "text-xs text-amber-400 mt-1", "⚠ Without the sandbox this server has full access to your files and network." }
                        }
                    }
                }

                if let Some(err) = error.read().as_ref() {
                    p { class: "text-sm text-red-400 mb-3", "{err}" }
                }

                div {
                    class: "flex justify-end gap-3",
                    button {
                        class: "px-4 py-2 bg-input hover:bg-input/70 rounded-md text-sm font-medium text-fg transition-colors",
                        onclick: move |_| close(),
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-btn-primary hover:bg-btn-primary-hover rounded-md text-sm font-medium text-fg transition-colors",
                        disabled: *deriving.read(),
                        onclick: install,
                        "Install"
                    }
                }
            }
        }
    }
}

#[component]
pub fn GlamaRemoteConnectDialog(
    mcp: FeaturedMcp,
    dialog: Signal<Option<FeaturedMcp>>,
    config_content: Signal<String>,
    save_config: Coroutine<String>,
    trigger_search: Signal<i32>,
) -> Element {
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<SecretManager>>();

    let mut server_name = use_signal(|| short_server_name(&mcp.name));
    let mut endpoint_url = use_signal(String::new);
    let mut bearer_token = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);

    let mut close = move || dialog.set(None);

    let connect = move |_| {
        let url = endpoint_url.read().trim().to_string();
        if !url.starts_with("https://") && !url.starts_with("http://") {
            error.set(Some("Enter the server's endpoint URL (https://…)".to_string()));
            return;
        }
        let name = server_name.read().trim().to_string();
        if name.is_empty() {
            error.set(Some("Enter a server name".to_string()));
            return;
        }

        // Bearer token goes to the OS keychain, never into mcp_servers.json.
        let token = bearer_token.read().trim().to_string();
        if !token.is_empty() {
            if let Err(e) = secret_manager
                .write()
                .set_indexed_secret(&mcp_bearer_key(&name), token)
            {
                error.set(Some(format!("Failed to store token in keychain: {}", e)));
                return;
            }
        }

        let server = serde_json::json!({
            "uri": url,
            "description": mcp.description,
            "source": "glama",
        });

        match insert_server_into_config(&config_content.read(), &name, server) {
            Ok(new_content) => {
                save_config.send(new_content);
                default_permissions_to_prompt(&settings, &settings_manager, &name);
                let current = *trigger_search.peek();
                trigger_search.set(current + 1);
                close();
            }
            Err(e) => error.set(Some(e)),
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/60 flex items-center justify-center z-50",
            div {
                class: "bg-section border border-subtle rounded-lg shadow-xl max-w-lg w-full mx-4 p-6",
                h2 { class: "text-lg font-bold text-fg mb-1", "Connect to {mcp.display_name}" }
                p {
                    class: "text-sm text-fg-muted mb-3",
                    "Paste the server's endpoint URL — either the provider's own remote URL, or a Glama gateway endpoint (glama.ai/endpoints/…) if you route it through your Glama account for managed OAuth and logging."
                }
                if let Some(page) = mcp.glama_page_url.as_ref() {
                    p {
                        class: "text-xs mb-4",
                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "{page}", target: "_blank", "Find the URL on the server's Glama page →" }
                    }
                }

                label { class: "block text-sm font-medium text-fg-muted mb-1", "Server name" }
                input {
                    class: "w-full px-3 py-2 mb-3 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                    value: "{server_name}",
                    oninput: move |e| server_name.set(e.value()),
                }

                label { class: "block text-sm font-medium text-fg-muted mb-1", "Endpoint URL" }
                input {
                    class: "w-full px-3 py-2 mb-1 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                    placeholder: "https://example.com/mcp",
                    value: "{endpoint_url}",
                    oninput: move |e| endpoint_url.set(e.value()),
                }
                if is_cleartext_remote(&endpoint_url.read()) {
                    p { class: "text-xs text-amber-400 mb-3", "⚠ Plain http:// sends everything — including the bearer token — unencrypted. Use https:// unless this server is on your local network." }
                } else {
                    div { class: "mb-2" }
                }

                label { class: "block text-sm font-medium text-fg-muted mb-1", "Bearer token (optional)" }
                input {
                    class: "w-full px-3 py-2 mb-1 bg-input border border-faint rounded-md text-sm text-fg font-mono",
                    r#type: "password",
                    placeholder: "Stored securely in the keychain",
                    value: "{bearer_token}",
                    oninput: move |e| bearer_token.set(e.value()),
                }
                p { class: "text-xs text-fg-muted mb-4", "If the server requires OAuth instead, you'll get an Authorize prompt after connecting." }

                if let Some(err) = error.read().as_ref() {
                    p { class: "text-sm text-red-400 mb-3", "{err}" }
                }

                div {
                    class: "flex justify-end gap-3",
                    button {
                        class: "px-4 py-2 bg-input hover:bg-input/70 rounded-md text-sm font-medium text-fg transition-colors",
                        onclick: move |_| close(),
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-btn-primary hover:bg-btn-primary-hover rounded-md text-sm font-medium text-fg transition-colors",
                        onclick: connect,
                        "Connect"
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
    fn short_name_trims_mcp_affixes() {
        assert_eq!(short_server_name("owner/github-mcp"), "github");
        assert_eq!(short_server_name("owner/mcp-obsidian"), "obsidian");
        assert_eq!(short_server_name("owner/weather"), "weather");
        assert_eq!(short_server_name("plain"), "plain");
    }

    #[test]
    fn cleartext_warning_targets_non_loopback_http() {
        assert!(is_cleartext_remote("http://example.com/mcp"));
        assert!(is_cleartext_remote("http://192.168.1.10:8080/sse"));
        assert!(is_cleartext_remote("http://user@evil.com/mcp"));
        assert!(!is_cleartext_remote("https://example.com/mcp"));
        assert!(!is_cleartext_remote("http://localhost:3000/mcp"));
        assert!(!is_cleartext_remote("http://127.0.0.1/mcp"));
        assert!(!is_cleartext_remote("http://[::1]:3000/mcp"));
        assert!(!is_cleartext_remote("not a url"));
    }

    #[test]
    fn insert_server_creates_structure_when_config_invalid() {
        let out = insert_server_into_config("", "test", serde_json::json!({"command": "npx"}))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(json["mcpServers"]["test"]["command"] == "npx");
    }

    #[test]
    fn insert_server_preserves_existing_entries() {
        let existing = r#"{ "mcpServers": { "other": { "command": "uvx" } } }"#;
        let out = insert_server_into_config(
            existing,
            "new",
            serde_json::json!({"uri": "https://x/mcp", "source": "glama"}),
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(json["mcpServers"]["other"]["command"] == "uvx");
        assert!(json["mcpServers"]["new"]["uri"] == "https://x/mcp");
    }
}
