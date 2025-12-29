#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use super::chat::CodeBlock;
use super::shared::{ToolCall, ToolCallStatus};
use crate::mcp::manager::McpManager;

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
    let thought_signature = props.tool_call.thought_signature.clone().unwrap_or_default();


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

                // Thought Signature collapsible section (if present)
                if !thought_signature.is_empty() {
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
                            "Thought Signature"
                        }
                        if *show_thought.read() {
                            div {
                                class: "text-gray-300 text-sm p-2 bg-gray-900 rounded mt-1",
                                "{thought_signature}"
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


use crate::components::continuation_controller::ContinuationController;

#[derive(Props, Clone, PartialEq)]
pub struct PermissionPromptProps {
    pub tool_call: ToolCall,
}

#[component]
pub fn PermissionPrompt(props: PermissionPromptProps) -> Element {
    let mut mcp_manager = consume_context::<Signal<McpManager>>();
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let continuation_controller = consume_context::<Signal<ContinuationController>>();
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
                        let mut state = session_state.write();
                        if let Some(msg) = state.get_message_mut_by_execution_id(&tool_call_deny.execution_id) {
                            if let super::shared::MessageContent::PermissionRequest(tc) = &mut msg.content {
                                tc.status = ToolCallStatus::Error;
                                tc.response = "Denied by user.".to_string();
                                // We need to convert it back to a ToolCall to be displayed correctly
                                msg.content = super::shared::MessageContent::ToolCall(tc.clone());
                            }
                        }
                    },
                    "Deny"
                }
                button {
                    class: "px-4 py-2 rounded-md bg-green-600 text-white hover:bg-green-500",
                    onclick: move |_| {
                        spawn({
                            let tool_call = tool_call.clone();
                            let continuation_controller = continuation_controller;
                            async move {
                                let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null);
                                let result_receiver = mcp_manager.write().use_mcp_tool(&tool_call.server_name, &tool_call.tool_name, args_json, true).await;

                                let mut state = session_state.write();
                                if let Some(msg) = state.get_message_mut_by_execution_id(&tool_call.execution_id) {
                                     if let super::shared::MessageContent::PermissionRequest(tc) = &mut msg.content {
                                        let mut updated_tc = tc.clone();
                                        match result_receiver {
                                            Ok(mut receiver) => {
                                                let mut aggregated_content: Vec<rmcp::model::Content> = Vec::new();
                                                let mut final_status = ToolCallStatus::Completed;
                                                let mut error_string = None;

                                                while let Some(result) = receiver.recv().await {
                                                    match result {
                                                        Ok(call_tool_result) => {
                                                            aggregated_content.extend(call_tool_result.content);
                                                        }
                                                        Err(e) => {
                                                            final_status = ToolCallStatus::Error;
                                                            error_string = Some(e);
                                                            break;
                                                        }
                                                    }
                                                }

                                                updated_tc.status = final_status;
                                                if final_status == ToolCallStatus::Error {
                                                updated_tc.response = error_string.unwrap_or_default();
                                            } else {
                                                 // Check for auth requirement (duplicated from stream_manager.rs - TODO: refactor)
                                                let mut auth_url = None;
                                                for content in &aggregated_content {
                                                    let json_content = serde_json::to_value(content).unwrap_or(serde_json::Value::Null);
                                                    if let Some(text) = json_content.get("text").and_then(|t| t.as_str()) {
                                                        if text.contains("Authentication required") && text.contains("connect your account") {
                                                            if let Some(start) = text.find("http") {
                                                                auth_url = Some(text[start..].trim().to_string());
                                                            }
                                                        }
                                                    }
                                                }

                                                if let Some(url) = auth_url {
                                                    updated_tc.status = ToolCallStatus::AuthRequired;
                                                    updated_tc.response = url;
                                                } else {
                                                    let final_json = serde_json::to_value(aggregated_content).unwrap_or(serde_json::Value::Null);
                                                    updated_tc.response = serde_json::to_string_pretty(&final_json).unwrap_or_default();
                                                }
                                            }
                                            },
                                            Err(e) => {
                                                updated_tc.status = ToolCallStatus::Error;
                                                updated_tc.response = e;
                                            }
                                        }
                                        msg.content = super::shared::MessageContent::ToolCall(updated_tc);
                                    }
                                }
                                // Trigger continuation after successful tool execution
                                continuation_controller.read().trigger_continuation();
                            }
                        });
                    },
                    "Approve"
                }
            }
        }
    }
}