use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq)]
pub struct SessionIdContext(pub Signal<String>);

#[derive(Clone, Copy, PartialEq)]
pub struct DraftContext(pub Signal<String>);

#[derive(Clone, Copy, PartialEq)]
pub struct SessionToDeleteContext(pub Signal<String>);

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
    SkillCall(SkillCall),
    SkillPermissionRequest(SkillCall),
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
    Usage(UsageData),
}

/// Token usage data from LLM API response
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageData {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    #[serde(default)]
    pub thoughts_tokens: Option<i32>,
    #[serde(default)]
    pub cached_content_tokens: Option<i32>,
    /// Calculated cost in USD
    #[serde(default)]
    pub cost: Option<f64>,
}

impl ToolCall {
    pub fn new(
        server_name: String,
        tool_name: String,
        args: serde_json::Value,
        thought_signature: Option<String>,
        thought_summary: Option<String>,
    ) -> Self {
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

// ============================================================================
// Skill Types (mirrors Tool types for Claude Skills integration)
// ============================================================================

#[derive(Clone, Serialize, Deserialize, PartialEq, Copy, Debug, Default)]
pub enum SkillCallStatus {
    #[default]
    Pending,    // Awaiting user permission
    Running,    // Currently executing
    Completed,  // Successfully finished
    Error,      // Execution failed
}

impl std::fmt::Display for SkillCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillCallStatus::Pending => write!(f, "Pending"),
            SkillCallStatus::Running => write!(f, "Running"),
            SkillCallStatus::Completed => write!(f, "Completed"),
            SkillCallStatus::Error => write!(f, "Error"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct SkillCall {
    pub execution_id: String,
    pub skill_name: String,
    pub arguments: String,
    pub status: SkillCallStatus,
    pub response: String,
    pub instructions: String,  // Processed skill instructions
    #[serde(default)]
    pub path: std::path::PathBuf,     // Path to skill file
    #[serde(default)]
    pub has_scripts: bool,             // Whether skill has executable scripts
    #[serde(default)]
    pub raw_output: Option<String>,    // Clean output for use_result
    #[serde(default)]
    pub profile_color: Option<String>, // Historical profile color for rendering
}

impl SkillCall {
    #[allow(dead_code)]
    pub fn new(skill_name: String, arguments: String, instructions: String) -> Self {
        Self {
            execution_id: uuid::Uuid::new_v4().to_string(),
            skill_name,
            arguments,
            status: SkillCallStatus::Pending,
            response: String::new(),
            instructions,
            path: std::path::PathBuf::new(),
            has_scripts: false,
            raw_output: None,
            profile_color: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SkillEnvironment {
    pub root_path: std::path::PathBuf,
    pub scripts: Vec<String>,
    pub resources: Vec<String>,
    pub user_args: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CapabilityContextPayload {
    pub skill: String,
    pub instruction_manual: String,
    pub environment: SkillEnvironment,
    pub resolved_tools: std::collections::HashMap<String, String>,
    pub warnings: Vec<String>,
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
    #[serde(default)]
    pub profile_color: Option<String>, // Historical profile color for rendering
}

/// Helper to extract JSON content from an LLM response string.
/// It handles markdown blocks (```json ... ```) and raw JSON objects.
pub fn extract_json_from_response(text: &str) -> &str {
    let text = text.trim();

    // 1. Try markdown JSON block
    if let (Some(s), Some(e)) = (text.find("```json"), text.rfind("```")) {
        if s < e {
            return text[s + 7..e].trim();
        }
    }

    // 2. Try finding literal JSON object braces
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if s < e {
            return &text[s..=e];
        }
    }

    // 3. Fallback to raw text
    text
}

#[component]
pub fn ChatBarIconButton<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    onclick: EventHandler<MouseEvent>,
    #[props(default = true)] visible: bool,
    #[props(default = String::new())] title: String,
) -> Element {
    if !visible {
        return rsx! {};
    }

    rsx! {
        button {
            class: "p-2 rounded-full text-fg-muted hover:bg-card hover:text-fg focus:outline-none focus:ring-2 focus:ring-gray-600 transition-colors",
            onclick: move |evt| onclick.call(evt),
            title: "{title}",
            dioxus_free_icons::Icon {
                width: 20,
                height: 20,
                icon: icon
            }
        }
    }
}
