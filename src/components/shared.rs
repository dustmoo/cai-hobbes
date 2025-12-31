use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum MessageContent {
    Text {
        content: String,
        #[serde(default)]
        thought_signature: Option<String>,
        #[serde(default)]
        thought_summary: Option<String>,
    },
    ToolCall(ToolCall),
    PermissionRequest(ToolCall),
    Error {
        message: String,
    },
}

impl MessageContent {
    pub fn get_text_content(&self) -> Option<String> {
        match self {
            MessageContent::Text { content, .. } => Some(content.clone()),
            _ => None,
        }
    }
}
 
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolCall {
    pub execution_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub arguments: String,
    pub status: ToolCallStatus,
    pub response: String,
    pub thought_signature: Option<String>,
    /// The actual thinking content (human-readable), separate from the encrypted signature
    #[serde(default)]
    pub thought_summary: Option<String>,
}

pub enum StreamMessage {
    Text {
        content: String,
        thought_signature: Option<String>,
        thought_summary: Option<String>,
    },
    ToolCall(ToolCall),
    Error {
        message: String,
    },
}

impl ToolCall {
    pub fn new(server_name: String, tool_name: String, args: serde_json::Value, thought_signature: Option<String>, thought_summary: Option<String>) -> Self {
        Self {
            execution_id: uuid::Uuid::new_v4().to_string(),
            server_name,
            tool_name,
            arguments: args.to_string(),
            status: ToolCallStatus::Running,
            response: String::new(),
            thought_signature,
            thought_summary,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Copy, Debug, Default)]
pub enum ToolCallStatus {
    #[default]
    Running,
    Completed,
    Error,
    AuthRequired,
}

impl std::fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolCallStatus::Running => write!(f, "Running"),
            ToolCallStatus::Completed => write!(f, "Completed"),
            ToolCallStatus::Error => write!(f, "Error"),
            ToolCallStatus::AuthRequired => write!(f, "Auth Required"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub status: ToolCallStatus,
    pub response: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub call: ToolCall,
    pub result: ToolResult,
}