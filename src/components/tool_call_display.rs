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
    let mut show_response = use_signal(|| false);

    let status = props.tool_call.status;
    let response = props.tool_call.response.clone();


    rsx! {
        div {
            class: "flex flex-col p-4 border rounded-lg shadow-sm bg-gray-800", // Adjusted background
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
                        CodeBlock {
                            code: props.tool_call.arguments.clone(),
                            lang: "json".to_string()
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
                    if *show_response.read() && !response.is_empty() {
                        CodeBlock {
                            code: response,
                            lang: "markdown".to_string()
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
    let mut mcp_manager = consume_context::<Signal<McpManager>>();
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
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
                            async move {
                                let args_json: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or(serde_json::Value::Null);
                                let result = mcp_manager.write().use_mcp_tool(&tool_call.server_name, &tool_call.tool_name, args_json, true).await;

                                let mut state = session_state.write();
                                if let Some(msg) = state.get_message_mut_by_execution_id(&tool_call.execution_id) {
                                     if let super::shared::MessageContent::PermissionRequest(tc) = &mut msg.content {
                                        let mut updated_tc = tc.clone();
                                        match result {
                                            Ok(response) => {
                                                updated_tc.status = ToolCallStatus::Completed;
                                                updated_tc.response = serde_json::to_string_pretty(&response).unwrap_or_default();
                                            },
                                            Err(e) => {
                                                updated_tc.status = ToolCallStatus::Error;
                                                updated_tc.response = e;
                                            }
                                        }
                                        msg.content = super::shared::MessageContent::ToolCall(updated_tc);
                                    }
                                }
                            }
                        });
                    },
                    "Approve"
                }
            }
        }
    }
}