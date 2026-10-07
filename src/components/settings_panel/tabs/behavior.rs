use super::super::SettingsCtx;
use super::super::fleet::FleetSection;
use super::super::shared::INPUT;
use super::super::shared::Toggle;
use super::super::terminal::TerminalSection;
use dioxus::prelude::*;

#[component]
pub(in super::super) fn BehaviorTab(
    ctx: SettingsCtx,
    mut app_behavior_collapsed: Signal<bool>,
) -> Element {
    let mut ui_state = ctx.ui_state;
    let ui_state_manager = ctx.ui_state_manager;
    let mut local_settings = ctx.local_settings;
    rsx! {
                // Application Behavior Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "flex justify-between items-center p-4 cursor-pointer bg-section rounded-t-lg",
                        onclick: move |_| app_behavior_collapsed.set(!app_behavior_collapsed()),
                        h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "Application Behavior" }
                        span { class: "text-fg-muted text-xs", if *app_behavior_collapsed.read() { "▶" } else { "▼" } }
                    }
                    if !app_behavior_collapsed() {
                        div {
                            class: "p-4 space-y-6",

                            // 0. Appearance
                            div {
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Appearance" }
                                div {
                                    class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                    div {
                                        label { class: "text-sm text-fg", "Theme" }
                                        p { class: "text-xs text-fg-muted", "Choose application color theme" }
                                    }
                                    select {
                                        class: "{INPUT} justify-self-end",
                                        onchange: move |e| {
                                            let theme = match e.value().as_str() {
                                                "Light" => crate::settings::Theme::Light,
                                                "System" => crate::settings::Theme::System,
                                                _ => crate::settings::Theme::Dark,
                                            };
                                            local_settings.write().theme = theme;
                                        },
                                        option { value: "Dark", selected: local_settings.read().theme == crate::settings::Theme::Dark, "Dark" }
                                        option { value: "Light", selected: local_settings.read().theme == crate::settings::Theme::Light, "Light" }
                                        option { value: "System", selected: local_settings.read().theme == crate::settings::Theme::System, "System" }
                                    }
                                }
                                div {
                                    class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-t border-faint",
                                    div {
                                        label { class: "text-sm text-fg", "Tabs as Sidebar List" }
                                        p { class: "text-xs text-fg-muted", "List open tabs in a sortable left sidebar instead of the top strip" }
                                    }
                                    div {
                                        class: "justify-self-end",
                                        Toggle {
                                            checked: ui_state.read().tabs_as_sidebar,
                                            onchange: move |v| {
                                                let mut state = ui_state.write();
                                                state.tabs_as_sidebar = v;
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                }
                                div {
                                    class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-t border-faint",
                                    div {
                                        label { class: "text-sm text-fg", "Suggest tidying tabs" }
                                        p { class: "text-xs text-fg-muted", "Show a \"Tidy tabs?\" pill in the tab sidebar when many tabs are open. Tidying is always available from the broom button" }
                                    }
                                    div {
                                        class: "justify-self-end",
                                        Toggle {
                                            checked: ui_state.read().tidy_tabs_nudge,
                                            onchange: move |v| {
                                                let mut state = ui_state.write();
                                                state.tidy_tabs_nudge = v;
                                                let state_clone = state.clone();
                                                let manager = ui_state_manager.read().clone();
                                                spawn(async move { let _ = manager.save(&state_clone); });
                                            }
                                        }
                                    }
                                }
                                if ui_state.read().tidy_tabs_nudge {
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-t border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Tidy suggestion threshold" }
                                            p { class: "text-xs text-fg-muted", "Suggest when more than this many tabs are open" }
                                        }
                                        input {
                                            r#type: "number",
                                            min: "2",
                                            class: "{INPUT} justify-self-end",
                                            value: "{ui_state.read().tidy_tabs_threshold}",
                                            onchange: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    let mut state = ui_state.write();
                                                    state.tidy_tabs_threshold = value.max(2);
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 1. Context & History (Global Defaults)
                            div {
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-1", "Context & History (Global Defaults)" }
                                p { class: "text-xs text-fg-muted mb-3 opacity-70", "These apply unless overridden per-provider in LLM Configuration above." }
                                div {
                                    // Chat History Length
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Chat History Length" }
                                            p { class: "text-xs text-fg-muted", "Max past messages included in context (dynamically capped to fit the model's window)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().chat_history_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().chat_history_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Tool Output Length
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Max Tool Output Length" }
                                            p { class: "text-xs text-fg-muted", "Maximum characters displayed in tool outputs (0 for unlimited)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().max_tool_output_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_tool_output_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Active Tool Output Length
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Max Active Tool Output Length" }
                                            p { class: "text-xs text-fg-muted", "Maximum characters persisted for tool context" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().max_active_tool_output_length}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_active_tool_output_length = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Memory Summary Length
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Max Memory Summary Length" }
                                            p { class: "text-xs text-fg-muted", "Character limit for conversation summary (~4 chars = 1 token)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().max_summary_chars}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_summary_chars = value;
                                                }
                                            }
                                        }
                                    }

                                    // Max Stored Entities
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Max Stored Entities" }
                                            p { class: "text-xs text-fg-muted", "Maximum entities retained in memory (topics, facts, goals)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().max_entity_count}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().max_entity_count = value;
                                                }
                                            }
                                        }
                                    }

                                    // Skill Idle Turns
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Skill Idle Turns" }
                                            p { class: "text-xs text-fg-muted", "User turns a loaded skill's full instructions stay in context while unused, before being replaced by a short stub (0 = never)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            value: "{local_settings.read().skill_idle_turns}",
                                            oninput: move |e| {
                                                if let Ok(value) = e.value().parse::<usize>() {
                                                    local_settings.write().skill_idle_turns = value;
                                                }
                                            }
                                        }
                                    }

                                    // Tool Result Budget Ratio
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Tool Result Budget %" }
                                            p { class: "text-xs text-fg-muted", "Percentage of context window reserved for active tool results (5–95%)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            min: "5",
                                            max: "95",
                                            step: "5",
                                            value: "{(local_settings.read().tool_result_budget_ratio * 100.0).round() as u32}",
                                            oninput: move |e| {
                                                if let Ok(pct) = e.value().parse::<f64>() {
                                                    local_settings.write().tool_result_budget_ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0);
                                                }
                                            }
                                        }
                                    }

                                    // Active Result Budget Ratio
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Active Result Share %" }
                                            p { class: "text-xs text-fg-muted", "Fraction of remaining context allocated to the active tool result (5–95%)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            min: "5",
                                            max: "95",
                                            step: "5",
                                            value: "{(local_settings.read().active_result_budget_ratio * 100.0).round() as u32}",
                                            oninput: move |e| {
                                                if let Ok(pct) = e.value().parse::<f64>() {
                                                    local_settings.write().active_result_budget_ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0);
                                                }
                                            }
                                        }
                                    }

                                    // Context Safety Margin
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Context Safety Margin %" }
                                            p { class: "text-xs text-fg-muted", "Reserved fraction of context window to prevent overflow (5–95%)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            min: "5",
                                            max: "95",
                                            step: "5",
                                            value: "{(local_settings.read().context_safety_margin * 100.0).round() as u32}",
                                            oninput: move |e| {
                                                if let Ok(pct) = e.value().parse::<f64>() {
                                                    local_settings.write().context_safety_margin = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0);
                                                }
                                            }
                                        }
                                    }

                                    // System Prompt Budget Ratio
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        div {
                                            label { class: "text-sm text-fg", "System Prompt Budget %" }
                                            p { class: "text-xs text-fg-muted", "Maximum fraction of context window for system instructions (5–95%)" }
                                        }
                                        input {
                                            r#type: "number",
                                            class: "{INPUT} justify-self-end",
                                            min: "5",
                                            max: "95",
                                            step: "5",
                                            value: "{(local_settings.read().system_prompt_budget_ratio * 100.0).round() as u32}",
                                            oninput: move |e| {
                                                if let Ok(pct) = e.value().parse::<f64>() {
                                                    local_settings.write().system_prompt_budget_ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(pct / 100.0);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 2. Chat Bar Icons
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Toolbar & Composer Controls" }
                                p { class: "text-xs text-fg-muted mb-3", "History, MCP, Planner and Fleet live in the tab strip and tiger app menu now; Session Cost, Profile, Provider, Attachments and Model are pills in the composer footer." }
                                div {
                                    // History Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show History Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_history_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_history_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // MCP Tools Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show MCP Tools Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_mcp_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_mcp_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Planner Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Planner Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_planner_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_planner_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Fleet Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Fleet Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_fleet_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_fleet_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Session Cost Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Session Cost" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_session_cost_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_session_cost_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Profile Selector
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Profile Selector" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_profile_selector,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_profile_selector = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Provider Selector
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Provider Selector" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_provider_selector,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_provider_selector = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Attachments Icon
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show Attachments Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_attachments_icon,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_attachments_icon = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }

                                    // Model Selector
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        label { class: "text-sm text-fg", "Show Model Selector" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().show_model_selector,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.show_model_selector = v;
                                                    let state_clone = (*state).clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 3. Confirmation Dialogs
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Confirmation Dialogs" }
                                div {
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Confirm before deleting sessions" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().confirm_on_delete,
                                                onchange: move |v| local_settings.write().confirm_on_delete = v,
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Confirm before saving settings" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().confirm_on_save,
                                                onchange: move |v| local_settings.write().confirm_on_save = v,
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Confirm before editing messages" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().confirm_on_message_edit,
                                                onchange: move |v| local_settings.write().confirm_on_message_edit = v,
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Confirm before optimizing memory" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().confirm_forget_memory,
                                                onchange: move |v| local_settings.write().confirm_forget_memory = v,
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Show System Tray Icon" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().show_tray_icon,
                                                onchange: move |v| local_settings.write().show_tray_icon = v,
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        div {
                                            label { class: "text-sm text-fg", "Focus window when a timer fires" }
                                            p { class: "text-xs text-fg-muted", "When off, a fired timer shows a reminder without pulling the window to the front." }
                                        }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().timer_focus_window,
                                                onchange: move |v| local_settings.write().timer_focus_window = v,
                                            }
                                        }
                                    }
                                }
                            }

                            // 4. Tool Display Defaults
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Tool Display Defaults" }
                                p { class: "text-xs text-fg-muted mb-3", "Set initial state for the Arguments/Response disclosures and thinking rows inside a turn's activity block." }

                                div {
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Expand Arguments by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_tool_arguments_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_tool_arguments_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Expand Response by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_tool_response_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_tool_response_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        label { class: "text-sm text-fg", "Expand Thinking Process by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_tool_thought_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_tool_thought_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 4b. Skill Display Defaults
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Skill Display Defaults" }
                                p { class: "text-xs text-fg-muted mb-3", "Set initial state for collapsible sections in skill call bubbles." }

                                div {
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Expand Arguments by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_skill_arguments_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_skill_arguments_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Expand Output / Payload by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_skill_response_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_skill_response_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        label { class: "text-sm text-fg", "Expand Instructions by Default" }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: ui_state.read().default_skill_instructions_open,
                                                onchange: move |v| {
                                                    let mut state = ui_state.write();
                                                    state.default_skill_instructions_open = v;
                                                    let state_clone = state.clone();
                                                    let manager = ui_state_manager.read().clone();
                                                    spawn(async move { let _ = manager.save(&state_clone); });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 5. AI Behavior
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "AI Behavior" }
                                div {
                                    // Max AI Turns
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Max AI Turns" }
                                            p { class: "text-xs text-fg-muted", "Maximum consecutive responses allowed" }
                                        }
                                        select {
                                            class: "{INPUT} justify-self-end",
                                            onchange: move |e| {
                                                if let Ok(value) = e.value().parse::<u32>() {
                                                    local_settings.write().permission_settings.max_ai_turns = value;
                                                }
                                            },
                                            option { value: "1", selected: local_settings.read().permission_settings.max_ai_turns == 1, "1 (Strict turn-taking)" }
                                            option { value: "5", selected: local_settings.read().permission_settings.max_ai_turns == 5, "5 (Conservative)" }
                                            option { value: "10", selected: local_settings.read().permission_settings.max_ai_turns == 10, "10 (Moderate)" }
                                            option { value: "25", selected: local_settings.read().permission_settings.max_ai_turns == 25, "25 (Default)" }
                                            option { value: "50", selected: local_settings.read().permission_settings.max_ai_turns == 50, "50 (High autonomy)" }
                                            option { value: "100", selected: local_settings.read().permission_settings.max_ai_turns == 100, "100 (Very high autonomy)" }
                                            option { value: "200", selected: local_settings.read().permission_settings.max_ai_turns == 200, "200 (Local models / max autonomy)" }
                                        }
                                    }

                                    // User Name
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "What should Hobbes call you?" }
                                        input {
                                            r#type: "text",
                                            class: "{INPUT} justify-self-end",
                                            placeholder: "Dr. Calvin",
                                            value: "{local_settings.read().user_name.clone().unwrap_or_default()}",
                                            oninput: move |e| {
                                                let value = e.value();
                                                local_settings.write().user_name = if value.is_empty() { None } else { Some(value) };
                                            }
                                        }
                                    }

                                    // Background Summarization
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Background Summarization" }
                                            p { class: "text-xs text-fg-muted", "Automatically summarize conversation context during idle periods and tool loops" }
                                        }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().enable_summarization,
                                                onchange: move |v| local_settings.write().enable_summarization = v,
                                            }
                                        }
                                    }

                                    // Compact Tool Results
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                                        div {
                                            label { class: "text-sm text-fg", "Compact Tool Results (TOON)" }
                                            p { class: "text-xs text-fg-muted", "Convert tool results to a compact tabular format before sending to the AI. Reduces token usage by ~30–50%. Can be overridden per-provider in Context Tuning." }
                                        }
                                        div {
                                            class: "justify-self-end",
                                            Toggle {
                                                checked: local_settings.read().compact_tool_results,
                                                onchange: move |v| local_settings.write().compact_tool_results = v,
                                            }
                                        }
                                    }
                                }
                            }

                            // 6. Persona & Instructions
                            div {
                                class: "pt-4 border-t border-subtle",
                                h4 { class: "text-xs uppercase tracking-[0.06em] text-fg-muted mt-2 mb-3", "Persona & Instructions" }
                                div {
                                    // Persona
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-start py-2.5 border-b border-faint",
                                        label { class: "text-sm text-fg", "Persona" }
                                        textarea {
                                            class: "col-span-2 bg-app border border-faint rounded px-2.5 py-2 text-sm text-fg h-24 focus:outline-none focus:ring-2 focus:ring-primary-500 w-full",
                                            value: "{local_settings.read().persona}",
                                            oninput: move |e| local_settings.write().persona = e.value()
                                        }
                                    }

                                    // Force Tool Use Instruction
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-start py-2.5 border-b border-faint",
                                        div {
                                            label { class: "text-sm text-fg", "Force Tool Use Instruction" }
                                            p { class: "text-xs text-fg-muted", "Appended to system prompt to encourage specific behaviors" }
                                        }
                                        textarea {
                                            class: "col-span-2 bg-app border border-faint rounded px-2.5 py-2 text-sm text-fg h-24 focus:outline-none focus:ring-2 focus:ring-primary-500 w-full",
                                            value: "{local_settings.read().force_tool_use_instruction.clone().unwrap_or_default()}",
                                            oninput: move |e| local_settings.write().force_tool_use_instruction = Some(e.value())
                                        }
                                    }

                                    // Project Folder
                                    div {
                                        class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-start py-2.5",
                                        label { class: "text-sm text-fg", "Project Folder" }
                                        div {
                                            class: "col-span-2 flex gap-2",
                                            input {
                                                r#type: "text",
                                                class: "flex-1 {INPUT}",
                                                value: "{local_settings.read().project_folder.clone().unwrap_or_default()}",
                                                readonly: true,
                                            }
                                            button {
                                                class: "px-3 py-2 bg-card hover:bg-input rounded border border-faint transition-colors",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        if let Some(folder_path) = rfd::AsyncFileDialog::new().pick_folder().await {
                                                            local_settings.write().project_folder = Some(folder_path.path().to_string_lossy().to_string());
                                                        }
                                                    });
                                                },
                                                "Browse"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Fleet observation (Pro): its own card so the hook
                // registration flow doesn't hide inside the behavior wall.
                // Commits immediately (not part of this tab's draft + Save).
                FleetSection {}

                // Native terminal: opt-in, commits immediately.
                TerminalSection {}

    }
}
