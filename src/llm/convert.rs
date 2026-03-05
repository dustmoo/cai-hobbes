use crate::llm::types::{LlmPrompt, ToolDefinition};
use crate::llm::UsageData;
use serde::{Deserialize, Serialize};

/// Each provider implements this to convert neutral<->native formats.
/// This replaces the current monolithic Gemini-specific conversion.
pub trait LlmFormatConverter {
    /// Convert neutral prompt -> provider-native request body (serde_json::Value)
    fn to_native_request(&self, prompt: &LlmPrompt, streaming: bool) -> serde_json::Value;

    /// Convert provider-native streaming chunk -> neutral content blocks
    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent>;

    /// Convert MCP tool -> provider-neutral ToolDefinition
    #[allow(dead_code)] // Architectural — used by future provider-agnostic tool routing
    fn convert_mcp_tool(
        &self,
        tool: &rmcp::model::Tool,
        server_name: &str,
    ) -> Result<ToolDefinition, String>;

    /// Provider-specific tool name sanitization.
    /// Default: replace non-alphanumeric/underscore chars with underscore.
    /// Gemini overrides this to add a 63-char truncation rule.
    #[allow(dead_code)] // Architectural — tested + overridden by Gemini converter
    fn sanitize_tool_name(&self, name: &str) -> String {
        name.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect()
    }

    /// Reverse map a sanitized tool name back to the original MCP name
    #[allow(dead_code)] // Architectural — used by future provider-agnostic tool routing
    fn original_tool_name(&self, sanitized: &str) -> Option<String>;

    /// Provider-specific tool count limit
    #[allow(dead_code)] // Architectural — used by future provider-agnostic tool routing
    fn max_tools(&self) -> usize;
}

/// Events parsed from a streaming response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Text {
        content: String,
    },
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        arguments: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Usage(UsageData),
    Error {
        message: String,
    },
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dummy struct to test trait default implementations
    struct TestConverter;
    impl LlmFormatConverter for TestConverter {
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
        ) -> Result<ToolDefinition, String> {
            Ok(ToolDefinition::from_mcp(tool, server_name))
        }
        fn original_tool_name(&self, sanitized: &str) -> Option<String> {
            Some(sanitized.to_string())
        }
        fn max_tools(&self) -> usize {
            128
        }
    }

    #[test]
    fn test_default_sanitize_tool_name_alphanumeric() {
        let converter = TestConverter;
        assert_eq!(converter.sanitize_tool_name("hello_world"), "hello_world");
        assert_eq!(converter.sanitize_tool_name("foo123"), "foo123");
    }

    #[test]
    fn test_default_sanitize_tool_name_special_chars() {
        let converter = TestConverter;
        assert_eq!(converter.sanitize_tool_name("my-tool.name"), "my_tool_name");
        assert_eq!(converter.sanitize_tool_name("tool@v2!test"), "tool_v2_test");
        assert_eq!(converter.sanitize_tool_name("a b c"), "a_b_c");
    }

    #[test]
    fn test_default_sanitize_tool_name_unicode() {
        let converter = TestConverter;
        // Unicode letters are alphanumeric and should pass through
        let result = converter.sanitize_tool_name("café_naïve");
        assert_eq!(result, "café_naïve");
    }

    #[test]
    fn test_default_sanitize_tool_name_empty() {
        let converter = TestConverter;
        assert_eq!(converter.sanitize_tool_name(""), "");
    }
}
