#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use super::chat::CodeBlock;
use super::shared::{ToolCall, ToolCallStatus};
use super::markdown_renderer::ThinkingMarkdownRenderer;

#[derive(Props, Clone, PartialEq)]
pub struct ToolCallDisplayProps {
    pub tool_call: ToolCall,
}

#[component]
pub fn ToolCallDisplay(props: ToolCallDisplayProps) -> Element {
    let mut show_arguments = use_signal(|| true);
    // Auto-expand response section for auth-required tools so user sees the connect button
    let mut show_response = use_signal(|| props.tool_call.status == ToolCallStatus::AuthRequired);
    let mut show_thought = use_signal(|| false);

    let status = props.tool_call.status;
    let response = props.tool_call.response.clone();
    // Use thought_summary (actual thinking content) not thought_signature (encrypted)
    let thought_content = props.tool_call.thought_summary.clone().unwrap_or_default();


    rsx! {
        div {
            class: "flex flex-col p-4 border rounded-lg shadow-sm bg-gray-800 overflow-hidden", // overflow-hidden prevents stretching
            div {
                class: "flex items-center gap-2 text-lg font-semibold text-gray-100", // Adjusted text color
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
            }
            div {
                class: "mt-4 pt-4 border-t border-gray-600 space-y-2", // Adjusted border color
                div {
                    class: "flex items-center gap-2",
                    span { class: "font-semibold text-gray-300", "Tool:" }
                    span { class: "font-mono text-sm text-gray-300", "{props.tool_call.tool_name}" }
                }

                // Thinking Process collapsible section (if present)
                if !thought_content.is_empty() {
                    div {
                        class: "flex flex-col",
                        button {
                            class: "flex items-center gap-1 text-sm font-semibold text-gray-400 hover:text-gray-200",
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
                                class: "text-sm p-2 bg-gray-900 rounded mt-1",
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
                        class: "flex items-center gap-1 text-sm font-semibold text-gray-400 hover:text-gray-200",
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
                        class: "flex items-center gap-1 text-sm font-semibold text-gray-400 hover:text-gray-200",
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
                                class: "flex flex-col gap-3 p-3 bg-gray-900 rounded mt-2 border border-yellow-700/50",
                                div {
                                    class: "flex items-center gap-2 text-yellow-500",
                                    Icon {
                                        width: 18,
                                        height: 18,
                                        icon: fi_icons::FiLock
                                    }
                                    span { class: "font-medium", "Authentication Required" }
                                }
                                p { class: "text-sm text-gray-300", "Please connect your account to proceed with this tool." }
                                a {
                                    class: "px-4 py-2 bg-yellow-600 text-white rounded hover:bg-yellow-500 text-center text-sm font-medium transition-colors w-fit flex items-center gap-2",
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
    let tool_call = props.tool_call.clone();
    let tool_call_deny = tool_call.clone();

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
                p { "Do you want to allow this?" }
            }
            div {
                class: "mt-4 flex justify-end gap-4",
                button {
                    class: "px-4 py-2 rounded-md bg-gray-600 text-white hover:bg-gray-500",
                    onclick: move |_| {
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
                    class: "px-4 py-2 rounded-md bg-green-600 text-white hover:bg-green-500",
                    onclick: move |_| {
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