#![allow(non_snake_case)]
use super::chat::CodeBlock;
use super::markdown_renderer::ThinkingMarkdownRenderer;
use super::shared::{ToolCall, ToolCallStatus, UsageData};
use crate::settings::UiState;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[derive(Props, Clone, PartialEq)]
pub struct ToolCallDisplayProps {
    pub tool_call: ToolCall,
    #[props(default)]
    pub usage: Option<UsageData>,
    #[props(default)]
    pub token_display_mode: String,
}

#[component]
pub fn ToolCallDisplay(props: ToolCallDisplayProps) -> Element {
    // Consume global UI state for default preferences (read-only for initial values)
    let ui_state = consume_context::<Signal<UiState>>();

    // Local signals initialized from global defaults - these are NOT synced back
    let mut show_arguments = use_signal(|| ui_state.read().default_tool_arguments_open);
    // Auto-expand response section for auth-required tools so user sees the connect button
    let mut show_response = use_signal(|| {
        if props.tool_call.status == ToolCallStatus::AuthRequired {
            true
        } else {
            ui_state.read().default_tool_response_open
        }
    });
    let mut show_thought = use_signal(|| ui_state.read().default_tool_thought_open);

    let status = props.tool_call.status;
    let response = props.tool_call.response.clone();
    // Use thought_summary (actual thinking content) not thought_signature (encrypted)
    let thought_content = props.tool_call.thought_summary.clone().unwrap_or_default();

    rsx! {
        div {
            class: "flex flex-col p-4 border rounded-lg shadow-sm bg-section overflow-hidden", // overflow-hidden prevents stretching
            div {
                class: "flex items-center gap-2 text-lg font-semibold text-fg",
                Icon {
                    width: 20,
                    height: 20,
                    icon: fi_icons::FiCpu
                }
                span { "{props.tool_call.server_name}" }
                span {
                    class: format!("text-sm font-mono px-2 py-1 rounded {}", match status {
                        ToolCallStatus::Running => "bg-blue-200 text-blue-800",
                        ToolCallStatus::Completed => "bg-green-200 text-green-800",
                        ToolCallStatus::Error => "bg-red-200 text-red-800",
                        ToolCallStatus::AuthRequired => "bg-yellow-200 text-yellow-800",
                    }),
                    "{status}"
                }
                if let Some(usage) = &props.usage {
                    if props.token_display_mode != "none" {
                       div {
                            class: "ml-auto text-xs text-fg-muted font-mono flex items-center gap-2",
                            if let Some(cost) = usage.cost {
                                span { class: "text-green-400", {format!("${:.6}", cost)} }
                            }
                            span { "{usage.total_tokens}t" }
                        }
                    }
                }
            }
            div {
                class: "mt-4 pt-4 border-t border-faint space-y-2", // Adjusted border color
                div {
                    class: "flex items-center gap-2",
                    span { class: "font-semibold text-fg-muted", "Tool:" }
                    span { class: "font-mono text-sm text-fg-muted", "{props.tool_call.tool_name}" }
                }

                // Thinking Process collapsible section (if present)
                if !thought_content.is_empty() {
                    div {
                        class: "flex flex-col",
                        button {
                            class: "flex items-center gap-1 text-sm font-semibold text-fg-muted hover:text-fg",
                            onclick: move |_| show_thought.toggle(),
                            if *show_thought.read() {
                                Icon {
                                    width: 16,
                                    height: 16,
                                    icon: fi_icons::FiChevronDown
                                }
                            } else {
                                Icon {
                                    width: 16,
                                    height: 16,
                                    icon: fi_icons::FiChevronRight
                                }
                            }
                            "Thinking Process"
                        }
                        if *show_thought.read() {
                            div {
                                class: "text-sm p-2 bg-app rounded mt-1",
                                ThinkingMarkdownRenderer {
                                    content: thought_content.clone(),
                                    compact: false,
                                }
                            }
                        }
                    }
                }

                // Arguments collapsible section
                div {
                    class: "flex flex-col",
                    button {
                        class: "flex items-center gap-1 text-sm font-semibold text-fg-muted hover:text-fg",
                        onclick: move |_| show_arguments.toggle(),
                        if *show_arguments.read() {
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiChevronDown
                            }
                        } else {
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiChevronRight
                            }
                        }
                        "Arguments"
                    }
                    if *show_arguments.read() {
                        div {
                            class: "overflow-x-auto max-w-full", // Prevent horizontal overflow
                            CodeBlock {
                                code: props.tool_call.arguments.clone(),
                                lang: "json".to_string()
                            }
                        }
                    }
                }

                // Response collapsible section
                div {
                    class: "flex flex-col",
                    button {
                        class: "flex items-center gap-1 text-sm font-semibold text-fg-muted hover:text-fg",
                        onclick: move |_| show_response.toggle(),
                        if *show_response.read() {
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiChevronDown
                            }
                        } else {
                            Icon {
                                width: 16,
                                height: 16,
                                icon: fi_icons::FiChevronRight
                            }
                        }
                        "Response"
                    }
                    if *show_response.read() {
                         if status == ToolCallStatus::AuthRequired {
                            div {
                                class: "flex flex-col gap-3 p-3 bg-app rounded mt-2 border border-yellow-700/50",
                                div {
                                    class: "flex items-center gap-2 text-yellow-500",
                                    Icon {
                                        width: 18,
                                        height: 18,
                                        icon: fi_icons::FiLock
                                    }
                                    span { class: "font-medium", "Authentication Required" }
                                }
                                p { class: "text-sm text-fg-muted", "Please connect your account to proceed with this tool." }
                                a {
                                    class: "px-4 py-2 bg-yellow-600 text-fg rounded hover:bg-yellow-500 text-center text-sm font-medium transition-colors w-fit flex items-center gap-2",
                                    href: "{response}",
                                    target: "_blank",
                                    "Connect Account"
                                    Icon {
                                        width: 14,
                                        height: 14,
                                        icon: fi_icons::FiExternalLink
                                    }
                                }
                            }
                        } else if !response.is_empty() {
                            div {
                                class: "overflow-x-auto max-w-full",
                                CodeBlock {
                                    code: response,
                                    lang: "json".to_string()
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct PermissionPromptProps {
    pub tool_call: ToolCall,
}

#[component]
pub fn PermissionPrompt(props: PermissionPromptProps) -> Element {
    let session_state = consume_context::<Signal<crate::session::SessionState>>();
    let has_pending_approvals = consume_context::<Signal<bool>>();
    let mut settings = consume_context::<Signal<crate::settings::Settings>>();
    let settings_manager = consume_context::<Signal<crate::settings::SettingsManager>>();
    let planner = consume_context::<Signal<crate::todo::PlannerState>>();
    let tool_call = props.tool_call.clone();
    let tool_call_deny = tool_call.clone();

    // Trust-rule authoring context: what a rule created from THIS prompt
    // would cover. Terminal execs get a command-prefix rule (first two
    // tokens); everything else a server+tool rule; both optionally scoped
    // to the active session's project.
    let parsed_args: Option<serde_json::Value> =
        serde_json::from_str(&tool_call.arguments).ok();
    let command_prefix: Option<String> = parsed_args
        .as_ref()
        .filter(|_| {
            tool_call.server_name == crate::mcp::manager::HOBBES_TERMINAL_SERVER
                || tool_call.server_name == "local-on-demand"
        })
        .and_then(|a| a.get("command").and_then(|c| c.as_str()))
        .map(|cmd| {
            cmd.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|p| !p.is_empty());
    let session_project: Option<(String, String)> = {
        let state = session_state.read();
        state
            .sessions
            .get(&state.active_session_id)
            .and_then(|s| s.project_id.clone())
            .and_then(|pid| {
                crate::services::project_tagger::project_title(&planner.read().projects, &pid)
                    .map(|t| (pid.clone(), t.to_string()))
            })
    };
    let allow_label = match &command_prefix {
        Some(p) => format!("Always allow `{p}`"),
        None => format!(
            "Always allow {} on {}",
            tool_call.tool_name, tool_call.server_name
        ),
    };
    let mut make_rule = use_signal(|| false);
    let mut scope_to_project = use_signal(|| false);

    // Shared decision logger with this prompt's identity.
    let decision_base = {
        let tc = tool_call.clone();
        let args = parsed_args.clone().unwrap_or(serde_json::Value::Null);
        let session_id = session_state.peek().active_session_id.clone();
        let project_id = session_project.as_ref().map(|(id, _)| id.clone());
        move |kind: crate::context::trust_store::DecisionKind,
              rule_id: Option<String>| {
            crate::context::trust_store::log_decision(
                crate::context::trust_store::TrustDecision::new(
                    kind,
                    &tc.server_name,
                    &tc.tool_name,
                    crate::context::trust_store::arg_summary(&args),
                    Some(session_id.clone()),
                    project_id.clone(),
                    rule_id,
                ),
            );
        }
    };
    let log_on_deny = decision_base.clone();
    let log_on_approve = decision_base;

    rsx! {
        div {
            class: "flex flex-col p-4 border rounded-lg shadow-sm bg-yellow-900 border-yellow-700",
            div {
                class: "flex items-center gap-2 text-lg font-semibold text-yellow-100",
                Icon {
                    width: 20,
                    height: 20,
                    icon: fi_icons::FiShield
                }
                "Permission Required"
            }
            div {
                class: "mt-4 pt-4 border-t border-yellow-800 space-y-2 text-yellow-200",
                p {
                    "The AI wants to use the tool "
                    span { class: "font-mono text-sm", "{tool_call.tool_name}" }
                    " from the server "
                    span { class: "font-mono text-sm", "{tool_call.server_name}" }
                    "."
                }
                // What is actually being approved. For the terminal that's
                // the command itself — approving a shell command you can't
                // read is not an approval.
                {
                    let detail: Option<(&'static str, String)> = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
                        .ok()
                        .and_then(|args| {
                            if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
                                let mut line = cmd.to_string();
                                if let Some(cwd) = args.get("cwd").and_then(|c| c.as_str()) {
                                    line = format!("cd {cwd} && {line}");
                                }
                                Some(("Command", line))
                            } else if args.as_object().is_some_and(|o| !o.is_empty()) {
                                serde_json::to_string_pretty(&args)
                                    .ok()
                                    .map(|p| ("Arguments", p))
                            } else {
                                None
                            }
                        });
                    rsx! {
                        if let Some((label, mut text)) = detail {
                            {
                                if text.chars().count() > 1200 {
                                    text = format!("{}…", text.chars().take(1200).collect::<String>());
                                }
                                rsx! {
                                    div {
                                        p { class: "text-xs uppercase tracking-wider text-yellow-400 mb-1", "{label}" }
                                        pre {
                                            class: "rounded bg-black/40 p-3 text-sm font-mono text-yellow-100 whitespace-pre-wrap break-all max-h-48 overflow-y-auto",
                                            "{text}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                p { "Do you want to allow this?" }
                // Trust-rule authoring at the moment of decision (the skill
                // prompt's remember-choice idiom).
                div {
                    class: "flex flex-col gap-1 pt-1",
                    div {
                        class: "flex items-center gap-2 text-yellow-200/80",
                        input {
                            r#type: "checkbox",
                            id: "trust-rule-make",
                            class: "w-4 h-4 accent-white/50 cursor-pointer",
                            checked: *make_rule.read(),
                            onchange: move |_| {
                                let v = !*make_rule.peek();
                                make_rule.set(v);
                            }
                        }
                        label {
                            r#for: "trust-rule-make",
                            class: "text-xs cursor-pointer select-none font-mono",
                            "{allow_label}"
                        }
                    }
                    if let Some((_, title)) = &session_project {
                        div {
                            class: "flex items-center gap-2 ml-6 text-yellow-200/60",
                            input {
                                r#type: "checkbox",
                                id: "trust-rule-project",
                                class: "w-3.5 h-3.5 accent-white/50 cursor-pointer",
                                disabled: !*make_rule.read(),
                                checked: *scope_to_project.read(),
                                onchange: move |_| {
                                    let v = !*scope_to_project.peek();
                                    scope_to_project.set(v);
                                }
                            }
                            label {
                                r#for: "trust-rule-project",
                                class: "text-xs cursor-pointer select-none",
                                "only in {title}"
                            }
                        }
                    }
                }
            }
            div {
                class: "mt-4 flex justify-end gap-4",
                button {
                    class: "px-4 py-2 rounded-md bg-input text-fg hover:bg-gray-500",
                    onclick: move |_| {
                        log_on_deny(crate::context::trust_store::DecisionKind::Denied, None);
                        // Deny: Convert to error/skipped state so UI can clean up
                        spawn({
                            let tool_call_deny = tool_call_deny.clone();
                            let mut session_state = session_state;
                            async move {
                                {
                                    let mut state = session_state.write();
                                    if let Some(msg) = state.get_message_mut_by_execution_id(&tool_call_deny.execution_id) {
                                        if let super::shared::MessageContent::PermissionRequest(tc) = &mut msg.content {
                                            let mut denied_tc = tc.clone();
                                            denied_tc.status = ToolCallStatus::Error;
                                            denied_tc.response = "Denied by user.".to_string();
                                            msg.content = super::shared::MessageContent::ToolCall(denied_tc);
                                        }
                                    }
                                }
                            }
                        });
                    },
                    "Deny"
                }
                button {
                    class: "px-4 py-2 rounded-md bg-green-600 text-fg hover:bg-green-500",
                    onclick: move |_| {
                        // Author the rule first (when asked), then approve.
                        if *make_rule.peek() {
                            let rule = crate::context::permissions::TrustRule {
                                id: uuid::Uuid::new_v4().to_string(),
                                server: tool_call.server_name.clone(),
                                tool: Some(tool_call.tool_name.clone()),
                                command_prefix: command_prefix.clone(),
                                project_id: (*scope_to_project.peek())
                                    .then(|| session_project.as_ref().map(|(id, _)| id.clone()))
                                    .flatten(),
                                created_at: chrono::Utc::now(),
                            };
                            let rule_id = settings.write().add_trust_rule(rule);
                            let snapshot = settings.peek().clone();
                            if let Err(e) = settings_manager.read().save(&snapshot) {
                                tracing::error!("trust rule save failed: {e}");
                            }
                            log_on_approve(
                                crate::context::trust_store::DecisionKind::ApprovedRuleCreated,
                                Some(rule_id),
                            );
                        } else {
                            log_on_approve(
                                crate::context::trust_store::DecisionKind::Approved,
                                None,
                            );
                        }
                        // Approve: Mark as ready for execution, signal pending approvals
                        // The lifecycle (Submit button) will handle actual execution
                        spawn({
                            let tool_call = tool_call.clone();
                            let mut session_state = session_state;
                            let mut has_pending_approvals = has_pending_approvals;
                            async move {
                                {
                                    let mut state = session_state.write();
                                    if let Some(msg) = state.get_message_mut_by_execution_id(&tool_call.execution_id) {
                                        if let super::shared::MessageContent::PermissionRequest(tc) = &mut msg.content {
                                            let mut approved_tc = tc.clone();
                                            approved_tc.status = ToolCallStatus::Running;
                                            msg.content = super::shared::MessageContent::ToolCall(approved_tc);
                                        }
                                    }
                                }
                                // Signal that there are approved tools ready for execution
                                has_pending_approvals.set(true);
                            }
                        });
                    },
                    "Approve"
                }
            }
        }
    }
}
