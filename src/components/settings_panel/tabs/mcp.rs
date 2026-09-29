use super::super::SettingsCtx;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[component]
pub(in super::super) fn McpTab(ctx: SettingsCtx, mut composio_url_warning: Signal<Option<String>>, mut show_composio_advanced: Signal<bool>) -> Element {
    let settings = ctx.settings;
    let mut ui_state = ctx.ui_state;
    let ui_state_manager = ctx.ui_state_manager;
    let mut session_state = ctx.session_state;
    let mut local_settings = ctx.local_settings;

    let toggle_mcp_instructions_collapsed = move |_| {
        {
            let mut state = ui_state.write();
            state.mcp_instructions_collapsed = !state.mcp_instructions_collapsed;
        }
        let state = (*ui_state.read()).clone();
        let manager = (*ui_state_manager.read()).clone();
        spawn(async move {
            let _ = manager.save(&state);
        });
    };

    rsx! {
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "p-4",
                        div {
                            class: "mb-4",
                            label { class: "block text-sm font-medium text-fg-muted mb-2", "Preferred MCP Source" }
                            div {
                                class: "flex space-x-4",
                                button {
                                    class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                        "flex-1 px-4 py-2 rounded-md bg-btn-primary text-fg font-medium shadow-sm ring-2 ring-primary-400"
                                    } else {
                                        "flex-1 px-4 py-2 rounded-md bg-input text-fg-muted font-medium hover:bg-input hover:text-fg transition-colors"
                                    },
                                    onclick: move |_| {
                                        local_settings.write().preferred_mcp_source = crate::settings::McpSource::Composio;
                                    },
                                    "Composio"
                                }
                                button {
                                    class: if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                        "flex-1 px-4 py-2 rounded-md bg-red-900/50 text-red-200 font-medium shadow-sm ring-2 ring-red-700 border border-red-700"
                                    } else {
                                        "flex-1 px-4 py-2 rounded-md bg-input text-fg-muted font-medium hover:bg-input hover:text-fg transition-colors"
                                    },
                                    onclick: move |_| {
                                        local_settings.write().preferred_mcp_source = crate::settings::McpSource::Smithery;
                                    },
                                    "Smithery.ai (Deprecated)"
                                }
                            }

                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                div {
                                    class: "mt-3 p-3 bg-red-900/30 border border-red-700 rounded-lg",
                                    p {
                                        class: "text-red-200 text-sm flex items-center gap-2",
                                        Icon { width: 16, height: 16, icon: fi_icons::FiAlertTriangle }
                                        "This integration is deprecated. We recommend using Composio directly."
                                    }
                                }
                            }
                                p {
                                class: "text-xs text-fg-muted mt-2",
                                "Choose which registry to use when installing new MCP servers. Smithery uses a hosted proxy (requires API key), while Composio runs locally."
                            }

                            // Smithery API Key - shown when Smithery is selected
                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Smithery {
                                div {
                                    class: "mt-4 pt-4 border-t border-subtle",
                                    label { class: "block text-sm font-medium text-fg-muted mb-1", "Smithery API Key" }
                                    input {
                                        class: "mt-1 block w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm shadow-sm",
                                        r#type: "password",
                                        placeholder: "Enter your Smithery.ai API key",
                                        value: "{local_settings.read().smithery_api_key.as_deref().unwrap_or(\"\")}",
                                        oninput: move |event| local_settings.write().smithery_api_key = Some(event.value().trim().to_string())
                                    }
                                    p {
                                        class: "text-xs text-fg-muted mt-1",
                                        "Required for Smithery.ai marketplace access"
                                    }
                                }
                            }

                            if local_settings.read().preferred_mcp_source == crate::settings::McpSource::Composio {
                                div {
                                    class: "mt-4 pt-4 border-t border-subtle",
                                    // Instructions
                                    div {
                                        class: "mb-6 bg-app/50 rounded-lg border border-subtle/50 overflow-hidden",
                                        div {
                                            class: "flex justify-between items-center p-4 cursor-pointer bg-section/50 hover:bg-section transition-colors",
                                            onclick: toggle_mcp_instructions_collapsed,
                                            h4 { class: "text-sm font-semibold text-fg", "Setup Instructions" }
                                            span { if ui_state.read().mcp_instructions_collapsed { "▶" } else { "▼" } }
                                        }
                                        if !ui_state.read().mcp_instructions_collapsed {
                                            div {
                                                class: "p-4 border-t border-subtle/30",
                                                ol {
                                                    class: "list-decimal list-inside text-sm text-fg-muted space-y-1.5",
                                                    li {
                                                        "Get your API key from "
                                                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "https://composio.dev/settings", target: "_blank", "composio.dev/settings" }
                                                    }
                                                    li { "Click \"+ Add Profile\" and paste your API key" }
                                                    li { "Use the MCP Marketplace to connect your first tool (Gmail, GitHub, etc.)" }
                                                    li { "Your Server URL will be automatically created - you're done!" }
                                                }
                                                div {
                                                    class: "mt-3 pt-3 border-t border-subtle/30",
                                                    a {
                                                        class: "text-xs text-primary-400 hover:text-primary-300 flex items-center gap-1",
                                                        href: "https://docs.composio.dev/docs/welcome",
                                                        target: "_blank",
                                                        Icon { width: 12, height: 12, icon: fi_icons::FiExternalLink }
                                                        "View Documentation"
                                                    }
                                                }
                                            }
                                        }
                                    }

                                        // Profile list header
                                        div {
                                            class: "flex justify-between items-center mb-3",
                                            label { class: "block text-sm font-medium text-fg-muted", "Composio Profiles" }
                                            button {
                                                class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                                                onclick: move |_| {
                                                    let new_profile = crate::settings::ComposioProfile::default();
                                                    local_settings.write().add_profile(new_profile);
                                                },
                                                "+ Add Profile"
                                            }
                                        }

                                        // Profile list
                                        div {
                                            class: "space-y-2 mb-4",
                                            for profile in local_settings.read().composio_profiles.iter() {
                                                {
                                                    let profile_name = profile.name.clone();
                                                    let profile_id = profile.id.clone();
                                                    let active_id = local_settings.read().active_composio_profile.clone();
                                                    let is_active = active_id.as_ref() == Some(&profile_id);
                                                    rsx! {
                                                        div {
                                                            class: format!("flex items-center justify-between p-2 rounded-md transition-all {}",
                                                                if is_active { "bg-input border border-primary-500 ring-1 ring-primary-500/20" } else { "bg-input/50 border border-transparent hover:bg-input" }),
                                                            div {
                                                                class: "flex items-center gap-3",
                                                                input {
                                                                    r#type: "radio",
                                                                    class: "w-4 h-4 text-primary-500 focus:ring-primary-500 bg-transparent border-faint",
                                                                    name: "active_profile",
                                                                    checked: is_active,
                                                                    onchange: {
                                                                        let id = profile_id.clone();
                                                                        let mut global_settings = settings;
                                                                         move |_| {
                                                                             tracing::info!("Switching to profile ID: {}", id);
                                                                             local_settings.write().active_composio_profile = Some(id.clone());
                                                                             global_settings.write().active_composio_profile = Some(id.clone());
                                                                             // Update the active session so the sync effect doesn't snap back
                                                                             if let Some(session) = session_state.write().get_active_session_mut() {
                                                                                 session.composio_profile = Some(id.clone());
                                                                             }
                                                                         }
                                                                    }
                                                                }
                                                                span {
                                                                    class: format!("text-sm font-medium {}", if is_active { "text-fg" } else { "text-fg-muted" }),
                                                                    "{profile_name}"
                                                                }
                                                                if is_active {
                                                                    span {
                                                                        class: "px-2 py-0.5 bg-blue-600 text-fg rounded text-[10px] font-bold uppercase tracking-wider",
                                                                        "Active"
                                                                    }
                                                                }
                                                            }
                                                            button {
                                                                class: "text-xs font-medium text-red-500 hover:text-red-400 transition-colors uppercase tracking-tight",
                                                                onclick: {
                                                                    let id = profile_id.clone();
                                                                     move |_| {
                                                                         local_settings.write().remove_profile(&id);
                                                                     }
                                                                },
                                                                "Remove"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Edit active profile
                                        if let Some((active_id, active_display_name)) = {
                                            let s = local_settings.read();
                                            s.active_composio_profile.as_ref()
                                                .and_then(|id| s.composio_profiles.iter()
                                                    .find(|p| p.id == *id)
                                                    .map(|p| (id.clone(), p.name.clone())))
                                        } {
                                            div {
                                                class: "border border-subtle rounded-lg p-3",
                                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Edit Profile: {active_display_name}" }

                                                // Profile Name
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Profile Name" }
                                                    input {
                                                        class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                        value: "{active_display_name}",
                                                        oninput: {
                                                            let id = active_id.clone();
                                                             move |event: Event<FormData>| {
                                                                 let new_name = event.value();
                                                                 let mut settings = local_settings.write();
                                                                 if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.id == id) {
                                                                     profile.name = new_name.clone();
                                                                 }
                                                                 // active_composio_profile stores ID — no update needed on rename
                                                             }
                                                        },
                                                        onkeydown: |e| e.stop_propagation(),
                                                    }
                                                }

                                                // Profile Color
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Profile Color" }
                                                    div {
                                                        class: "flex gap-2 flex-wrap",
                                                        for color in ["bg-blue-600", "bg-purple-600", "bg-green-600", "bg-red-600", "bg-orange-600", "bg-pink-600", "bg-teal-600", "bg-input"] {
                                                            button {
                                                                class: format!("w-6 h-6 rounded-full cursor-pointer transition-transform hover:scale-110 {} {}",
                                                                    color,
                                                                    if local_settings.read().get_active_profile().map(|p| p.color.as_str()) == Some(color) { "ring-2 ring-white scale-110 border border-transparent shadow-md" } else { "border border-faint hover:border-white" }
                                                                ),
                                                                onclick: {
                                                                    let color = color.to_string();
                                                                    move |_| {
                                                                        let mut settings = local_settings.write();
                                                                        if let Some(profile) = settings.get_active_profile_mut() {
                                                                            profile.color = color.clone();
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                // Chrome Profile (Auth Security)
                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "Chrome Profile for Auth" }
                                                    div {
                                                        class: "flex gap-2",
                                                        select {
                                                            class: "flex-1 px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                            onchange: {
                                                                let id = active_id.clone();
                                                                move |evt: Event<FormData>| {
                                                                    let val = evt.value();
                                                                    let mut settings = local_settings.write();
                                                                    if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.id == id) {
                                                                        profile.chrome_profile_directory = if val.is_empty() { None } else { Some(val) };
                                                                    }
                                                                }
                                                            },
                                                            {
                                                                let current_chrome = local_settings.read().get_active_profile()
                                                                    .and_then(|p| p.chrome_profile_directory.clone())
                                                                    .unwrap_or_default();
                                                                 let chrome_profiles_signal = use_signal(crate::settings::discover_chrome_profiles);
                                                                 let chrome_profiles = chrome_profiles_signal.read();
                                                                rsx! {
                                                                    option { value: "", selected: current_chrome.is_empty(), "System Default (any window)" }
                                                                    for cp in chrome_profiles.iter() {
                                                                        option {
                                                                            value: "{cp.dir_name}",
                                                                            selected: current_chrome == cp.dir_name,
                                                                            if let Some(ref email) = cp.email {
                                                                                "{cp.display_name} — {email}"
                                                                            } else {
                                                                                "{cp.display_name}"
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    p {
                                                        class: "text-xs text-fg-muted mt-1",
                                                        "Ensures OAuth opens in the correct Chrome profile window."
                                                    }
                                                }


                                                div {
                                                    class: "mb-3",
                                                    label { class: "block text-xs font-medium text-fg-muted mb-1", "API Key" }
                                                    input {
                                                        r#type: "password",
                                                        class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                        placeholder: "Enter your Composio API key from composio.dev/settings",
                                                        value: "{local_settings.read().get_active_profile().and_then(|p| p.api_key.clone()).unwrap_or_default()}",
                                                        oninput: {
                                                            let id = active_id.clone();
                                                            move |event: Event<FormData>| {
                                                                let val = event.value();
                                                                let mut settings = local_settings.write();
                                                                if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.id == id) {
                                                                    profile.api_key = if val.is_empty() { None } else { Some(val) };
                                                                }
                                                            }
                                                        },
                                                        onkeydown: |e| e.stop_propagation(),
                                                    }
                                                    p {
                                                        class: "text-xs text-fg-muted mt-1",
                                                        "Get your API key from "
                                                        a { class: "text-primary-400 hover:text-primary-300 underline", href: "https://composio.dev/settings", target: "_blank", "composio.dev/settings" }
                                                    }
                                                }

                                                // Advanced Settings Toggle
                                                div {
                                                    class: "mb-3 pt-3 border-t border-subtle/50",
                                                    button {
                                                        class: "flex items-center gap-2 text-xs text-fg-muted hover:text-fg-muted transition-colors",
                                                        onclick: move |_| {
                                                            show_composio_advanced.set(!show_composio_advanced());
                                                        },
                                                        span { if show_composio_advanced() { "▼" } else { "▶" } }
                                                        span { "Advanced Settings" }
                                                        span { class: "text-fg-muted", "(User ID, Server URL)" }
                                                    }
                                                }

                                                // Advanced Section (hidden by default)
                                                if show_composio_advanced() {
                                                    div {
                                                        class: "space-y-3 p-3 bg-app/50 rounded-lg border border-subtle/30",

                                                        // User ID (Read-only + Copy/Regenerate)
                                                        div {
                                                            label { class: "block text-xs font-medium text-fg-muted mb-1", "User ID" }
                                                            div {
                                                                class: "flex gap-2",
                                                                input {
                                                                    class: "flex-1 px-3 py-2 bg-app border border-subtle rounded-md text-sm text-fg-muted cursor-not-allowed",
                                                                    readonly: true,
                                                                    value: "{local_settings.read().get_active_profile().and_then(|p| p.user_id.clone()).unwrap_or_else(|| \"Auto-generated\".to_string())}"
                                                                }
                                                                // Copy Button
                                                                button {
                                                                    class: "px-3 py-2 bg-input hover:bg-white/10 border border-primary-600 rounded-md text-fg-muted transition-colors",
                                                                    title: "Copy User ID",
                                                                    onclick: {
                                                                        let user_id = local_settings.read().get_active_profile().map(|p| p.user_id.clone()).unwrap_or_default();
                                                                        move |_| {
                                                                            if user_id.as_ref().is_none_or(|s| !s.is_empty()) {
                                                                                use std::process::Command;
                                                                                 let _ = Command::new("pbcopy")
                                                                                     .stdin(std::process::Stdio::piped())
                                                                                     .spawn()
                                                                                     .map(|mut child| {
                                                                                         use std::io::Write;
                                                                                         if let Some(mut stdin) = child.stdin.take() {
                                                                                             let _ = stdin.write_all(user_id.as_deref().unwrap_or("").as_bytes());
                                                                                         }
                                                                                     });
                                                                            }
                                                                        }
                                                                    },
                                                                    Icon { width: 16, height: 16, icon: fi_icons::FiCopy }
                                                                }
                                                                // Regenerate Button
                                                                button {
                                                                    class: "px-3 py-2 bg-input hover:bg-white/10 border border-primary-600 rounded-md text-fg-muted transition-colors",
                                                                    title: "Regenerate User ID",
                                                                    onclick: {
                                                                        let id = active_id.clone();
                                                                        move |_| {
                                                                            let mut settings = local_settings.write();
                                                                            if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.id == id) {
                                                                                profile.user_id = Some(uuid::Uuid::new_v4().to_string().to_lowercase());
                                                                            }
                                                                        }
                                                                    },
                                                                    Icon { width: 16, height: 16, icon: fi_icons::FiRefreshCw }
                                                                }
                                                            }
                                                            p { class: "text-xs text-fg-muted mt-1", "Auto-generated on profile creation. Only change if troubleshooting." }
                                                        }

                                                        // Server URL
                                                        div {
                                                            label { class: "block text-xs font-medium text-fg-muted mb-1", "Server URL (Optional)" }
                                                            input {
                                                                class: "w-full px-3 py-2 bg-input border border-primary-600 rounded-md text-sm",
                                                                placeholder: "Auto-created when you connect your first tool",
                                                                value: "{local_settings.read().get_active_profile().and_then(|p| p.base_url.clone()).unwrap_or_default()}",
                                                                oninput: {
                                                                    let id = active_id.clone();
                                                                    move |event: Event<FormData>| {
                                                                        let val = event.value();
                                                                        let mut clean_val = val.clone();
                                                                        let mut warning = None;

                                                                        // Sanitize URL if it contains user_id
                                                                        if let Ok(mut url) = url::Url::parse(&val) {
                                                                            if url.query_pairs().any(|(k, _)| k == "user_id") {
                                                                                let pairs: Vec<(String, String)> = url.query_pairs()
                                                                                    .filter(|(k, _)| k != "user_id")
                                                                                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                                                                    .collect();

                                                                                url.query_pairs_mut().clear();
                                                                                for (k, v) in pairs {
                                                                                    url.query_pairs_mut().append_pair(&k, &v);
                                                                                }

                                                                                // Clean up if query is empty (Url::to_string might leave ?)
                                                                                if url.query() == Some("") {
                                                                                    url.set_query(None);
                                                                                }

                                                                                clean_val = url.to_string();
                                                                                warning = Some("Note: Embedded User ID removed. Using the app-generated User ID.".to_string());
                                                                            }
                                                                        }

                                                                        composio_url_warning.set(warning);

                                                                        let mut settings = local_settings.write();
                                                                        if let Some(profile) = settings.composio_profiles.iter_mut().find(|p| p.id == id) {
                                                                            profile.base_url = if clean_val.is_empty() { None } else { Some(clean_val) };
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            if let Some(msg) = composio_url_warning.read().as_ref() {
                                                                p { class: "text-xs text-yellow-400 mt-1 flex items-center gap-1",
                                                                    Icon { width: 12, height: 12, icon: fi_icons::FiAlertCircle }
                                                                    "{msg}"
                                                                }
                                                            }
                                                            p { class: "text-xs text-fg-muted mt-1", "Leave empty to auto-create on first tool connection." }
                                                        }
                                                    }
                                                }

                                                // Connection status
                                                div {
                                                    class: "flex items-center justify-end gap-2 mt-2",
                                                    span { class: "h-2 w-2 rounded-full bg-green-500" }
                                                    span { class: "text-sm text-green-400", "Connected" }
                                                }
                                                p {
                                                    class: "text-xs text-fg-muted mt-2",
                                                    "Connection happens automatically when you select a profile."
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
