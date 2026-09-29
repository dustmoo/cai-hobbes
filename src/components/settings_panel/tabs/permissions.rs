use super::super::SettingsCtx;
use super::super::trust_rules::TrustRulesSection;
use super::super::shared::INPUT;
use crate::context::permissions::{SkillPolicy, ToolCategory};
use dioxus::prelude::*;

#[component]
pub(in super::super) fn PermissionsTab(
    ctx: SettingsCtx,
    mcp_context: Signal<crate::mcp::manager::McpContext>,
    mut permissions_collapsed: Signal<bool>,
) -> Element {
    let _mcp_context = mcp_context;
    let mut local_settings = ctx.local_settings;
    let skill_registry = ctx.skill_registry;
    rsx! {
                // Permissions Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: move |_| permissions_collapsed.set(!permissions_collapsed()),
                        h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "Permissions" }
                        span { class: "text-fg-muted text-xs", if *permissions_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !permissions_collapsed() {
                        div {
                            class: "p-4",
                            div {
                                class: "flex items-center justify-between mb-4",
                                div {
                                    label { class: "block text-sm font-medium text-fg-muted", "Auto-approve MCP tool calls" }
                                    p { class: "text-xs text-fg-muted", "Applies to MCP tool calls only. Skills have their own per-skill setting below." }
                                }
                                label {
                                    class: "relative inline-flex items-center cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        class: "sr-only peer",
                                        checked: local_settings.read().permission_settings.auto_approval_enabled,
                                        oninput: move |event| {
                                            if let Ok(checked) = event.value().parse() {
                                                local_settings.write().permission_settings.auto_approval_enabled = checked;
                                            }
                                        }
                                    }
                                    div { class: "w-11 h-6 bg-input peer-focus:outline-none peer-focus:ring-4 peer-focus:ring-primary-700 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary-500" }
                                }
                            }
                            if local_settings.read().permission_settings.auto_approval_enabled {
                                div {
                                    class: "pl-4 border-l-2 border-subtle space-y-2",
                                    div {
                                        class: "flex items-center justify-between",
                                        label { class: "text-sm font-medium text-fg-muted", "MCP Tools (Global)" }
                                        label {
                                            class: "relative inline-flex items-center cursor-pointer",
                                            input {
                                                r#type: "checkbox",
                                                class: "sr-only peer",
                                                checked: local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false),
                                                oninput: move |event| {
                                                    if let Ok(checked) = event.value().parse() {
                                                        local_settings.write().permission_settings.granular_permissions.insert(ToolCategory::Mcp, checked);
                                                    }
                                                }
                                            }
                                            div { class: if local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false) { "toggle-switch active" } else { "toggle-switch" } }
                                        }
                                    }

                                    if local_settings.read().permission_settings.granular_permissions.get(&ToolCategory::Mcp).copied().unwrap_or(false) {
                                        div {
                                            class: "mt-2 pl-4 border-l border-faint space-y-2",
                                            h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-2", "Granular MCP Permissions" }
                                            for server in _mcp_context.read().servers.iter() {
                                                {
                                                    let server_name = server.name.clone();
                                                    let is_allowed = local_settings.read().permission_settings.mcp_server_permissions.get(&server_name).copied().unwrap_or(true);

                                                    rsx! {
                                                        div {
                                                            key: "{server_name}",
                                                            class: "flex items-center justify-between",
                                                            div {
                                                                class: "flex flex-col",
                                                                span { class: "text-sm text-fg-muted", "{server_name}" }
                                                                if !server.description.is_empty() {
                                                                    span { class: "text-[10px] text-fg-muted", "{server.description}" }
                                                                }
                                                            }
                                                            label {
                                                                class: "relative inline-flex items-center cursor-pointer",
                                                                input {
                                                                    r#type: "checkbox",
                                                                    class: "sr-only peer",
                                                                    checked: is_allowed,
                                                                    oninput: {
                                                                        let server_name = server_name.clone();
                                                                        move |event: Event<FormData>| {
                                                                            if let Ok(checked) = event.value().parse() {
                                                                                local_settings.write().permission_settings.mcp_server_permissions.insert(server_name.clone(), checked);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                div { class: if is_allowed { "toggle-switch-sm active" } else { "toggle-switch-sm" } }
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
                    }
                }

            // Trust rules: user-authored allow-rules from permission prompts.
            TrustRulesSection {}

            // Skill Permissions Section
            div {
                class: "border border-subtle rounded-lg mb-4",
                div {
                    class: "p-4 bg-section rounded-lg",
                    h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-4", "Skill Permissions" }
                    p { class: "text-xs text-fg-muted mb-4",
                        "Auto: Hobbes can load the skill by itself, and /name runs. Ask (default): Hobbes must get your approval each time it wants to load the skill; typing /name yourself runs it directly. Off: the skill is blocked — Hobbes can't see or load it and /name is refused. A skill already loaded in a chat stays loaded there until unloaded."
                    }

                    {
                        // Get all skills from registry AND saved permissions
                        let registry = skill_registry.read();
                        let permission_settings = local_settings.read().permission_settings.clone();
                        let saved_permissions = permission_settings.skill_permissions.clone();

                        // Build merged map: registry skills + any saved permissions.
                        // The shown state is the stored policy, the same one the gates read.
                        let mut all_skills: std::collections::HashMap<String, SkillPolicy> = std::collections::HashMap::new();

                        for skill in registry.list_skills() {
                            all_skills.insert(
                                skill.metadata.name.clone(),
                                permission_settings.skill_policy(&skill.metadata.name),
                            );
                        }

                        // Also include any saved permissions for skills not currently in registry
                        // (in case a skill was removed but permission was saved)
                        for name in saved_permissions.keys() {
                            if !all_skills.contains_key(name) {
                                all_skills.insert(name.clone(), permission_settings.skill_policy(name));
                            }
                        }

                        if all_skills.is_empty() {
                            rsx! {
                                div {
                                    class: "flex items-center justify-center p-8 border border-dashed border-primary-800 rounded-lg",
                                    p { class: "text-sm text-fg-muted", "No skills discovered. Add skills to ~/.hobbes/skills/ to see them here." }
                                }
                            }
                        } else {
                            // Sort skills alphabetically for cleaner UI
                            let mut sorted_skills: Vec<_> = all_skills.into_iter().collect();
                            sorted_skills.sort_by(|a, b| a.0.cmp(&b.0));

                            rsx! {
                                div {
                                    class: "space-y-3",
                                    for (skill_name, policy) in sorted_skills {
                                        {
                                            // Metadata Lookup
                                        let skill_opt = registry.get_skill(&skill_name);
                                        let description = skill_opt.as_ref()
                                            .map(|s| s.metadata.description.clone())
                                            .unwrap_or_else(|| "No description available.".to_string());

                                            rsx! {
                                                div {
                                            class: "flex items-center justify-between p-4 bg-input rounded-lg border border-primary-800 hover:border-primary-600 transition-colors",
                                            div {
                                                class: "flex flex-col max-w-[70%]",
                                                div {
                                                    class: "flex items-center gap-2",
                                                    span { class: "text-sm font-bold text-fg", "/{skill_name}" }
                                                    if policy == SkillPolicy::Auto {
                                                        span { class: "px-1.5 py-0.5 text-[10px] font-bold bg-action-success text-action-success-text rounded", "AUTO" }
                                                    }
                                                }
                                                span { class: "text-xs text-fg-muted mt-1 line-clamp-2", "{description}" }
                                            }
                                            div {
                                                class: "w-28 shrink-0",
                                                select {
                                                    class: "{INPUT}",
                                                    onchange: {
                                                        let skill_name = skill_name.clone();
                                                        move |event: Event<FormData>| {
                                                            let policy = match event.value().as_str() {
                                                                "auto" => SkillPolicy::Auto,
                                                                "off" => SkillPolicy::Off,
                                                                _ => SkillPolicy::Ask,
                                                            };
                                                            // Use write() to trigger signal notification (Pattern 33)
                                                            local_settings.write().permission_settings.set_skill_policy(&skill_name, policy);
                                                        }
                                                    },
                                                    option { value: "auto", selected: policy == SkillPolicy::Auto, "Auto" }
                                                    option { value: "ask", selected: policy == SkillPolicy::Ask, "Ask" }
                                                    option { value: "off", selected: policy == SkillPolicy::Off, "Off" }
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
            }
            }
    }
}
