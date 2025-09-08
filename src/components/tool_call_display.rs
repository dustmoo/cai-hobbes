#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use super::chat::CodeBlock;
use super::shared::{ToolCall, ToolCallStatus};
#[derive(Props, Clone, PartialEq)]
pub struct ToolCallDisplayProps {
    pub tool_call: ToolCall,
}

#[component]
pub fn ToolCallDisplay(props: ToolCallDisplayProps) -> Element {
    let mut show_arguments = use_signal(|| false);
    let mut show_response = use_signal(|| true);

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