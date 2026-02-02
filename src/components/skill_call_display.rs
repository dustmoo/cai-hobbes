//! Skill Call Display Components
//!
//! UI components for displaying skill calls and permission prompts.
//! Follows the patterns from tool_call_display.rs for consistency.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fi_icons;


use crate::components::shared::{SkillCall, SkillCallStatus, CapabilityContextPayload};

/// Props for the SkillPermissionPrompt component
#[derive(Props, Clone, PartialEq)]
pub struct SkillPermissionPromptProps {
    pub skill_call: SkillCall,
    pub on_approve: EventHandler<String>,  // execution_id
    pub on_deny: EventHandler<String>,     // execution_id
}

/// Permission prompt for skill execution - shows Approve/Deny buttons
#[component]
pub fn SkillPermissionPrompt(props: SkillPermissionPromptProps) -> Element {
    let mut settings = consume_context::<Signal<crate::settings::Settings>>();
    let settings_manager = consume_context::<Signal<crate::settings::SettingsManager>>();
    let skill_call = props.skill_call.clone();
    
    // Signal for "Always allow this skill" checkbox
    let mut remember_choice = use_signal(|| false);

    rsx! {
        div {
            class: "flex flex-col p-4 bg-black/20 border-t border-white/10 space-y-3",
            div {
                class: "flex items-center gap-2 text-base font-bold text-white",
                Icon {
                    width: 18,
                    height: 18,
                    icon: fi_icons::FiTerminal
                }
                "Permission Required"
            }
            div {
                class: "space-y-2 text-sm text-white/90 font-medium",
                p {
                    "The skill "
                    span { class: "font-mono font-bold bg-white/20 px-1 rounded", "{skill_call.skill_name}" }
                    " wants to execute commands."
                }
                if !skill_call.arguments.is_empty() {
                    div {
                        class: "p-2 bg-black/30 rounded font-mono text-xs break-all border border-white/5",
                        "Args: {skill_call.arguments}"
                    }
                }
            }
            // "Always allow" checkbox
            div {
                class: "flex items-center gap-2 text-white/80",
                input {
                    r#type: "checkbox",
                    id: "remember-skill-permission",
                    class: "w-4 h-4 accent-white/50 cursor-pointer",
                    checked: *remember_choice.read(),
                    onchange: move |_| {
                        let current = *remember_choice.read();
                        remember_choice.set(!current);
                    }
                }
                label {
                    r#for: "remember-skill-permission",
                    class: "text-xs cursor-pointer select-none",
                    "Always allow this skill"
                }
            }
            div {
                class: "flex justify-end gap-2",
                button {
                    class: "px-3 py-1.5 rounded text-sm font-bold bg-white/10 hover:bg-white/20 transition-colors uppercase tracking-wider",
                    onclick: {
                        let execution_id = skill_call.execution_id.clone();
                        let skill_name = skill_call.skill_name.clone();
                        let on_deny = props.on_deny;
                        move |_| {
                            let remember = *remember_choice.read();
                            if remember {
                                let mut s = settings.write();
                                s.permission_settings.skill_permissions.insert(skill_name.clone(), false);
                                let _ = settings_manager.read().save(&s);
                            }
                            on_deny.call(execution_id.clone());
                        }
                    },
                    "Deny"
                }
                button {
                    class: "px-3 py-1.5 rounded text-sm font-bold bg-white text-black hover:bg-white/90 transition-colors uppercase tracking-wider",
                    onclick: {
                        let execution_id = skill_call.execution_id.clone();
                        let skill_name = skill_call.skill_name.clone();
                        let on_approve = props.on_approve;
                        move |_| {
                            let remember = *remember_choice.read();
                            if remember {
                                let mut s = settings.write();
                                s.permission_settings.skill_permissions.insert(skill_name.clone(), true);
                                let _ = settings_manager.read().save(&s);
                            }
                            on_approve.call(execution_id.clone());
                        }
                    },
                    "Approve"
                }
            }
        }
    }
}

/// Props for SkillCallDisplay component
#[derive(Props, Clone, PartialEq)]
pub struct SkillCallDisplayProps {
    pub skill_call: SkillCall,
    #[props(default = false)]
    pub show_permission_prompt: bool,
    #[props(optional)]
    pub on_approve: Option<EventHandler<String>>,
    #[props(optional)]
    pub on_deny: Option<EventHandler<String>>,
    /// Handler when user clicks "Use Result" - drops output to draft
    #[props(optional)]
    pub on_use_result: Option<EventHandler<String>>,
    /// Handler when user clicks dynamic secondary action (e.g., "Analyze")
    #[props(optional)]
    pub on_analyze: Option<EventHandler<String>>,
}

/// Display component for a skill call (shows status and output)
#[component]
pub fn SkillCallDisplay(props: SkillCallDisplayProps) -> Element {
    let skill_call = &props.skill_call;
    
    // Consume global UI state for default preferences
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    
    // Local signals initialized from global defaults - sticky per bubble
    let mut show_arguments = use_signal(|| false);
    let mut show_response = use_signal(|| true); // Expand response by default for visibility
    let mut show_tools_detail = use_signal(|| false); // Collapsed by default to reduce noise
    let mut show_instructions = use_signal(|| ui_state.read().default_skill_instructions_open);
    
    // Try to parse response as CapabilityContextPayload
    let context_payload: Option<CapabilityContextPayload> = if skill_call.status == SkillCallStatus::Completed {
        serde_json::from_str(&skill_call.response).ok()
    } else {
        None
    };

    // If we need to show permission prompt, render that instead
    if props.show_permission_prompt && skill_call.status == SkillCallStatus::Pending {
        if let (Some(on_approve), Some(on_deny)) = (props.on_approve, props.on_deny) {
            return rsx! {
                SkillPermissionPrompt {
                    skill_call: skill_call.clone(),
                    on_approve: on_approve,
                    on_deny: on_deny,
                }
            };
        }
    }
    
    // Choose status text
    let status_text = match skill_call.status {
        SkillCallStatus::Pending => "Pending",
        SkillCallStatus::Running => "Running...",
        SkillCallStatus::Completed => "Success",
        SkillCallStatus::Error => "Error",
    };

    rsx! {
        div {
            class: "flex flex-col p-4 bg-black/20 border-t border-white/10 space-y-2",
            div {
                class: "flex items-center gap-2",
                match skill_call.status {
                    SkillCallStatus::Pending => rsx! { Icon { width: 16, height: 16, icon: fi_icons::FiClock, class: "opacity-80" } },
                    SkillCallStatus::Running => rsx! { Icon { width: 16, height: 16, icon: fi_icons::FiLoader, class: "opacity-80 anim-spin" } },
                    SkillCallStatus::Completed => rsx! { Icon { width: 16, height: 16, icon: fi_icons::FiCheck, class: "opacity-80" } },
                    SkillCallStatus::Error => rsx! { Icon { width: 16, height: 16, icon: fi_icons::FiAlertCircle, class: "opacity-80" } },
                }
                span { class: "font-mono font-bold text-sm", "{skill_call.skill_name}" }
                span { class: "text-[10px] opacity-70 uppercase tracking-wider font-bold ml-auto", "{status_text}" }
            }
            
            // Warnings (if Capability Context)
            if let Some(payload) = &context_payload {
                if !payload.warnings.is_empty() {
                    div {
                        class: "flex flex-col gap-1 p-2 bg-red-900/40 border border-red-500/30 rounded",
                        for warning in &payload.warnings {
                            div {
                                class: "flex items-start gap-2 text-xs text-red-200",
                                Icon { width: 14, height: 14, icon: fi_icons::FiAlertTriangle, class: "mt-0.5 shrink-0" }
                                "{warning}"
                            }
                        }
                    }
                }
            }

            // Arguments collapsible section
            if !skill_call.arguments.is_empty() {
                div {
                    class: "flex flex-col",
                    button {
                        class: "flex items-center gap-1 text-[11px] font-bold opacity-70 hover:opacity-100 uppercase tracking-wider",
                        onclick: move |_| show_arguments.toggle(),
                        if *show_arguments.read() {
                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronDown }
                        } else {
                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronRight }
                        }
                        "Arguments"
                    }
                    if *show_arguments.read() {
                        div {
                            class: "mt-1 p-2 bg-black/30 rounded font-mono text-[11px] break-all border border-white/5 opacity-80",
                            "{skill_call.arguments}"
                        }
                    }
                }
            }
            
            // Response collapsible section
            if !skill_call.response.is_empty() {
                div {
                    class: "flex flex-col",
                    button {
                        class: "flex items-center gap-1 text-[11px] font-bold opacity-70 hover:opacity-100 uppercase tracking-wider",
                        onclick: move |_| show_response.toggle(),
                        if *show_response.read() {
                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronDown }
                        } else {
                            Icon { width: 14, height: 14, icon: fi_icons::FiChevronRight }
                        }
                        
                        // Change label if it's a Context Payload
                        if context_payload.is_some() {
                             "Context Payload"
                        } else {
                             "Response"
                        }
                    }
                    if *show_response.read() {
                        div {
                            class: "mt-1 p-3 bg-black/40 rounded-lg text-xs border border-white/5 break-words max-h-96 overflow-y-auto custom-scrollbar",
                            if let Some(payload) = &context_payload {
                                // Structured Display for Capability Payload
                                div {
                                    class: "flex flex-col gap-2",
                                    div {
                                        class: "text-xs font-bold text-white/60 mb-1",
                                        "SKILL CONTEXT INJECTED"
                                    }
                                    div {
                                        class: "grid grid-cols-[max-content_1fr] gap-x-2 gap-y-1 text-xs font-mono",
                                        span { class: "opacity-50", "Tools:" }
                                        span { "{payload.resolved_tools.len()} Resolved" }
                                        
                                        span { class: "opacity-50", "Scripts:" }
                                        span { "{payload.environment.scripts.len()} Found" }
                                        
                                        span { class: "opacity-50", "Resources:" }
                                        span { "{payload.environment.resources.len()} Found" }
                                    }
                                    
                                    // Helper for Resolved Tools
                                    if !payload.resolved_tools.is_empty() {
                                        div {
                                            class: "mt-2 bg-black/20 rounded overflow-hidden",
                                            button {
                                                class: "w-full flex items-center justify-between p-2 hover:bg-white/5 transition-colors group/tools",
                                                onclick: move |_| show_tools_detail.toggle(),
                                                div {
                                                    class: "flex items-center gap-2",
                                                    if *show_tools_detail.read() {
                                                        Icon { width: 12, height: 12, icon: fi_icons::FiChevronDown, class: "opacity-50 group-hover/tools:opacity-100" }
                                                    } else {
                                                        Icon { width: 12, height: 12, icon: fi_icons::FiChevronRight, class: "opacity-50 group-hover/tools:opacity-100" }
                                                    }
                                                    span { class: "text-[10px] uppercase font-bold opacity-50 group-hover/tools:opacity-80 transition-opacity", "Tool Resolution" }
                                                }
                                                span { class: "text-[10px] bg-white/10 px-1.5 rounded opacity-50 font-mono", "{payload.resolved_tools.len()}" }
                                            }
                                            
                                            // Collapsible Content
                                            if *show_tools_detail.read() {
                                                div {
                                                    class: "p-2 border-t border-white/5",
                                                    {
                                                        let mut tools: Vec<_> = payload.resolved_tools.iter().collect();
                                                        tools.sort_by(|a, b| a.0.cmp(b.0));
                                                        rsx! {
                                                            for (req, text) in tools {
                                                                div { class: "flex justify-between text-[11px] py-0.5 border-b border-white/5 last:border-0",
                                                                    span { class: "opacity-70", "{req}" }
                                                                    span { class: "opacity-40 mx-1", "→" }
                                                                    span { class: "font-mono opacity-80", "{text}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Fallback markdown renderer for raw text/legacy
                                crate::components::markdown_renderer::ThinkingMarkdownRenderer {
                                    content: skill_call.response.clone(),
                                    compact: false,
                                }
                            }
                        }
                    }
                }
            }
            
            // Instructions collapsible section - rendered from instruction_manual in payload
            if let Some(payload) = &context_payload {
                if !payload.instruction_manual.is_empty() {
                    div {
                        class: "flex flex-col",
                        button {
                            class: "flex items-center gap-1 text-[11px] font-bold opacity-70 hover:opacity-100 uppercase tracking-wider",
                            onclick: move |_| show_instructions.toggle(),
                            if *show_instructions.read() {
                                Icon { width: 14, height: 14, icon: fi_icons::FiChevronDown }
                            } else {
                                Icon { width: 14, height: 14, icon: fi_icons::FiChevronRight }
                            }
                            "Instructions"
                        }
                        if *show_instructions.read() {
                            div {
                                class: "mt-1 p-3 bg-black/40 rounded-lg text-xs border border-white/5 break-words max-h-96 overflow-y-auto custom-scrollbar",
                                crate::components::markdown_renderer::ThinkingMarkdownRenderer {
                                    content: payload.instruction_manual.clone(),
                                    compact: false,
                                }
                            }
                        }
                    }
                }
            }
            

        }
    }
}
