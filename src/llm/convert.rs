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

/// Strip leaked special thinking tokens from model output text.
///
/// Some models (Gemma 4, Qwen3, etc.) use internal channel delimiters to
/// separate thinking from response content. When the inference server doesn't
/// properly route these into the `reasoning_content` field, they leak into
/// the regular text `content` as raw tokens:
///
///   - `<|channel|>thought` / `<|channel|>response` (Gemma 4 via vLLM)
///   - `<|channel>thought` / `<|channel>response` (variant without inner pipes)
///   - `<|im_start|>think` / `<|im_end|>` (Qwen/ChatML-style)
///
/// This function splits the text into `(is_thinking, text)` segments so callers
/// can route thinking content to the thinking display and response content
/// to the main output — exactly like `split_think_tags` does for XML tags.
///
/// Designed to be called from both the OpenAI compat and Gemini streaming paths.
///
/// The `inside_channel_think` state must be persisted across chunk boundaries
/// (same pattern as `inside_think` in `split_think_tags`).
pub fn strip_channel_tokens(
    content: &str,
    inside_channel_think: &mut bool,
) -> Vec<(bool, String)> {
    // All known token variants for "start thinking" and "start response"
    const THINK_TOKENS: &[&str] = &[
        "<|channel|>thought",
        "<|channel>thought",
        "<|im_start|>think",
    ];
    const RESPONSE_TOKENS: &[&str] = &[
        "<|channel|>response",
        "<|channel>response",
        "<|im_end|>",
    ];

    let mut segments: Vec<(bool, String)> = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if *inside_channel_think {
            // Inside a thinking channel — look for the response token to switch back
            if let Some((pos, tag_len)) = find_first_token(remaining, RESPONSE_TOKENS) {
                let thinking_text = &remaining[..pos];
                if !thinking_text.is_empty() {
                    segments.push((true, thinking_text.to_string()));
                }
                *inside_channel_think = false;
                remaining = &remaining[pos + tag_len..];
                // Trim leading newline after the token (models often emit one)
                remaining = remaining.strip_prefix('\n').unwrap_or(remaining);
            } else {
                // No response token in this chunk — everything is thinking
                if !remaining.is_empty() {
                    segments.push((true, remaining.to_string()));
                }
                break;
            }
        } else {
            // Outside thinking — look for a think token
            if let Some((pos, tag_len)) = find_first_token(remaining, THINK_TOKENS) {
                let before = &remaining[..pos];
                if !before.is_empty() {
                    segments.push((false, before.to_string()));
                }
                *inside_channel_think = true;
                remaining = &remaining[pos + tag_len..];
                // Trim leading newline after the token
                remaining = remaining.strip_prefix('\n').unwrap_or(remaining);
            } else {
                // No think token — pass everything through as regular text
                if !remaining.is_empty() {
                    segments.push((false, remaining.to_string()));
                }
                break;
            }
        }
    }

    segments
}

/// Find the first occurrence of any token in the text.
/// Returns `(position, token_length)`.
fn find_first_token(text: &str, tokens: &[&str]) -> Option<(usize, usize)> {
    tokens
        .iter()
        .filter_map(|tok| text.find(tok).map(|pos| (pos, tok.len())))
        .min_by_key(|(pos, _)| *pos)
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

    // ── strip_channel_tokens tests ──

    #[test]
    fn test_strip_channel_tokens_no_tokens() {
        let mut inside = false;
        let result = strip_channel_tokens("Hello, world!", &mut inside);
        assert_eq!(result, vec![(false, "Hello, world!".to_string())]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_gemma4_thought_response() {
        let mut inside = false;
        let input = "<|channel|>thought\nI think about this.\n<|channel|>response\nHere is the answer.";
        let result = strip_channel_tokens(input, &mut inside);
        assert_eq!(result, vec![
            (true, "I think about this.\n".to_string()),
            (false, "Here is the answer.".to_string()),
        ]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_variant_without_inner_pipes() {
        let mut inside = false;
        let input = "<|channel>thought\nReasoning here.\n<|channel>response\nFinal answer.";
        let result = strip_channel_tokens(input, &mut inside);
        assert_eq!(result, vec![
            (true, "Reasoning here.\n".to_string()),
            (false, "Final answer.".to_string()),
        ]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_im_start_think() {
        let mut inside = false;
        let input = "<|im_start|>think\nThinking...\n<|im_end|>\nResult here.";
        let result = strip_channel_tokens(input, &mut inside);
        assert_eq!(result, vec![
            (true, "Thinking...\n".to_string()),
            (false, "Result here.".to_string()),
        ]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_cross_chunk_boundary() {
        let mut inside = false;

        // Chunk 1: thinking starts, no response token yet
        let result1 = strip_channel_tokens("<|channel|>thought\nPartial thinking...", &mut inside);
        assert_eq!(result1, vec![(true, "Partial thinking...".to_string())]);
        assert!(inside);

        // Chunk 2: more thinking, then response
        let result2 = strip_channel_tokens("More thought.\n<|channel|>response\nDone.", &mut inside);
        assert_eq!(result2, vec![
            (true, "More thought.\n".to_string()),
            (false, "Done.".to_string()),
        ]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_text_before_thought() {
        let mut inside = false;
        let input = "Some prefix text. <|channel>thought\nThinking...\n<|channel>response\nAnswer.";
        let result = strip_channel_tokens(input, &mut inside);
        assert_eq!(result, vec![
            (false, "Some prefix text. ".to_string()),
            (true, "Thinking...\n".to_string()),
            (false, "Answer.".to_string()),
        ]);
        assert!(!inside);
    }

    #[test]
    fn test_strip_channel_tokens_empty_string() {
        let mut inside = false;
        let result = strip_channel_tokens("", &mut inside);
        assert!(result.is_empty());
    }

    #[test]
    fn test_strip_channel_tokens_only_thought_token() {
        let mut inside = false;
        let result = strip_channel_tokens("<|channel|>thought", &mut inside);
        assert!(result.is_empty()); // No content after token
        assert!(inside); // But state should track that we're inside thinking
    }
}
