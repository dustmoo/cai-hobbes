use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum MessageContent {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolCall {
    pub execution_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub arguments: String,
    pub status: ToolCallStatus,
    pub response: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Copy, Debug, Default)]
pub enum ToolCallStatus {
    #[default]
    Running,
    Completed,
    Error,
}

impl std::fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolCallStatus::Running => write!(f, "Running"),
            ToolCallStatus::Completed => write!(f, "Completed"),
            ToolCallStatus::Error => write!(f, "Error"),
        }
    }
}