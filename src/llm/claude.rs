use super::config::ClaudeConfig;
use super::convert::LlmFormatConverter;
use super::convert::StreamEvent;
use super::types::LlmPrompt;
use super::LlmConnector;
use crate::components::shared::StreamMessage;
use crate::mcp::manager::McpContext;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[allow(dead_code)]
pub struct ClaudeConnector {
    config: ClaudeConfig,
}

impl ClaudeConnector {
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl LlmConnector for ClaudeConnector {
    async fn generate_content_stream(
        &self,
        _prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
    ) {
        let _ = tx.send(StreamMessage::Error {
            message: "Claude connector is not yet implemented.".to_string(),
        });
    }

    async fn summarize_conversation(
        &self,
        _previous_summary: String,
        _recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Ok(serde_json::Value::Null)
    }
}

impl LlmFormatConverter for ClaudeConnector {
    fn to_native_request(&self, _prompt: &LlmPrompt, _streaming: bool) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn parse_stream_chunk(&self, _chunk: &str) -> Vec<StreamEvent> {
        vec![]
    }

    fn convert_mcp_tool(
        &self,
        tool: &rmcp::model::Tool,
        server_name: &str,
    ) -> Result<crate::llm::ToolDefinition, String> {
        Ok(crate::llm::ToolDefinition::from_mcp(tool, server_name))
    }

    // sanitize_tool_name: uses trait default (alphanumeric + underscore)

    fn original_tool_name(&self, sanitized: &str) -> Option<String> {
        Some(sanitized.to_string())
    }

    fn max_tools(&self) -> usize {
        100 // Claude's typical tool limit
    }
}
