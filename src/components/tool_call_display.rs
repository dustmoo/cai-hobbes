#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons::{FiCpu, FiLoader, FiChevronDown, FiChevronRight}, Icon as DioxusIcon};
use serde_json::Value;

use serde::{Deserialize, Serialize};
#[derive(PartialEq, Clone, Default, Debug, Serialize, Deserialize)]
pub enum ToolCallStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
}

#[derive(Props, PartialEq, Clone)]
pub struct ToolCallDisplayProps {
    pub tool_name: String,
    pub tool_arguments: Value,
    #[props(default)]
    pub status: ToolCallStatus,
    pub result: Option<Value>,
}

pub fn ToolCallDisplay(props: ToolCallDisplayProps) -> Element {
    let mut is_open = use_signal(|| false);

    rsx! {
        div {
            class: "flex flex-col p-4 my-2 rounded-lg bg-gray-800 text-white border border-gray-700",
            div {
                class: "flex items-center gap-3",
                DioxusIcon {
                    width: 20,
                    height: 20,
                    icon: FiCpu,
                }
                span {
                    class: "font-bold text-lg",
                    "{props.tool_name}"
                }
                div {
                    class: "flex-grow"
                }
                match props.status {
                    ToolCallStatus::InProgress => rsx!{
                        DioxusIcon {
                            width: 20,
                            height: 20,
                            class: "animate-spin",
                            icon: FiLoader,
                        }
                    },
                    _ => rsx!{
                        button {
                            class: "p-1 rounded-md hover:bg-gray-700",
                            onclick: move |_| is_open.toggle(),
                            if is_open() {
                                DioxusIcon {
                                    width: 20,
                                    height: 20,
                                    icon: FiChevronDown,
                                }
                            } else {
                                DioxusIcon {
                                    width: 20,
                                    height: 20,
                                    icon: FiChevronRight,
                                }
                            }
                        }
                    }
                }
            }
            if is_open() {
                div {
                    class: "mt-4 p-3 bg-gray-900 rounded",
                    pre {
                        code {
                            class: "text-sm font-mono",
                            if let Some(result) = &props.result {
                                "{serde_json::to_string_pretty(result).unwrap_or_default()}"
                            } else if props.status == ToolCallStatus::Failed {
                                "Tool execution failed."
                            }
                             else {
                                "No result available."
                            }
                        }
                    }
                }
            }
        }
    }
}