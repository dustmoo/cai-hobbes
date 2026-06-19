use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::llm::types::{
    ChatMessage, ChatRole, ContentBlock, LlmPrompt as NeutralLlmPrompt,
};
use serde_json::json;

/// Tracks a tool result message's position in the `messages` buffer for Pass 2
/// budget allocation. Collected during Pass 1 (message linearisation loop) and
/// consumed by Pass 2 (dynamic pagination).
pub(crate) struct ToolResultPosition {
    /// Index into the `messages` Vec where the ToolResult ContentBlock lives.
    pub msg_idx: usize,
    /// Sanitised tool name (server-prefixed, e.g. `mcp__fs__read_file`).
    pub tool_name: String,
    /// Execution UUID, used to build stable page-queue IDs.
    pub execution_id: String,
    /// True if this tool result belongs to the *current turn* — i.e. it occurs
    /// at or after the most recent user message. Current-turn results are the
    /// model's live working set and are protected from lossy compression so the
    /// in-progress turn always has the data it needs. Historical results (prior
    /// turns) may be summarised or paginated by Pass 2 to fit the budget.
    pub is_active: bool,
    /// Knowledge-preserving summary of this tool result, if one has been
    /// generated in the background. When a *historical* result exceeds its Pass 2
    /// budget and a summary is available, Pass 2 substitutes the summary instead
    /// of hard-truncating — preserving the facts while reclaiming tokens. `None`
    /// means no summary yet; Pass 2 falls back to pagination.
    pub result_summary: Option<String>,
}

/// Result of building a prompt, including the prompt itself and any
/// paginated tool results that need to be stored in the session state.
pub struct PromptBuildResult {
    pub prompt: NeutralLlmPrompt,
    pub pages_to_store: Vec<(String, crate::session::PagedResult)>,
}

/// Recursively unwrap JSON values that are stringified JSON.
/// Composio (and similar wrappers) often return tool results where the actual
/// data is embedded inside a string field, e.g.:
///   `{"result": {"type": "text", "text": "{\"successfull\":true,...}"}}`
/// The TOON formatter treats `Value::String` as plain text, so these
/// nested JSON payloads render as raw escaped strings. This function detects
/// string values that parse as JSON objects/arrays and replaces them in-place,
/// allowing the TOON renderer to recurse into the full structure.
pub(crate) fn unwrap_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            // Only unwrap if the string looks like a JSON object or array
            let trimmed = s.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s) {
                    // Recursively unwrap in case of multiple layers of encoding
                    unwrap_json_strings(&mut parsed);
                    *value = parsed;
                }
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                unwrap_json_strings(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                unwrap_json_strings(v);
            }
        }
        _ => {}
    }
}

impl From<Message> for ChatMessage {
    fn from(msg: Message) -> Self {
        let role = if msg.author == "User" {
            ChatRole::User
        } else {
            ChatRole::Assistant
        };

        let mut content = Vec::new();

        match msg.content {
            MessageContent::Text {
                content: text,
                thought_summary,
                thought_signature,
                ..
            } => {
                // Include thinking summary if present (critical for Gemini 2.0 Thinking support)
                if let Some(summary) = thought_summary {
                    if !summary.is_empty() {
                        content.push(ContentBlock::Thinking {
                            text: summary,
                            signature: thought_signature,
                        });
                    }
                }

                content.push(ContentBlock::Text { text });

                for attachment in msg.attachments {
                    content.push(ContentBlock::Image {
                        mime_type: attachment.mime_type,
                        data: attachment.data,
                    });
                }
            }
            MessageContent::ToolCall(tc) => {
                // Tool calls in history are represented as an Assistant message with a ToolCall block,
                // followed by a Tool result message.
                // This converter only handles the "original" message.
                // Tool calls are handled specifically in the prompt builder loop to ensure correlation.

                // If there's a thought summary, include it as a Thinking block
                if let Some(summary) = tc.thought_summary {
                    if !summary.is_empty() {
                        content.push(ContentBlock::Thinking {
                            text: summary,
                            signature: tc.thought_signature.clone(),
                        });
                    }
                }

                let args_value: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                content.push(ContentBlock::ToolCall {
                    id: tc.execution_id.to_string(), // In neutrally converted history, we use UUID as ID
                    name: tc.tool_name.clone(),
                    arguments: args_value,
                    signature: tc.thought_signature,
                });
            }
            _ => {}
        }

        ChatMessage { role, content }
    }
}
