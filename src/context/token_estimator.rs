use crate::llm::types::{ChatMessage, ContentBlock, LlmPrompt, ToolDefinition};

/// Default chars-per-token ratio (used when no configurable value is provided).
/// 3.0 balances English prose (~4 chars/token) with JSON/code (~2.5 chars/token).
/// This is intentionally conservative — overestimating tokens is safer than
/// underestimating, which causes "Prompt Too Large" errors from the server.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.0;

/// Safety margin — reserve this fraction of the context for output + API overhead.
/// For small models (<32K), JSON tokenization overhead means our character-based
/// estimator significantly underestimates actual tokens. We use a sliding scale:
/// - 16K model → 30% margin (aggressive, needed for JSON-heavy tool use)
/// - 32K model → 20% margin
/// - 128K+ model → 15% margin (heuristic works well at scale)
const MIN_SAFETY_MARGIN: f64 = 0.15;
const MAX_SAFETY_MARGIN: f64 = 0.30;

/// Estimate token count for a text string using configurable ratio.
pub fn estimate_tokens_with_ratio(text: &str, chars_per_token: f64) -> usize {
    // Use char count (not byte count) for better accuracy with Unicode
    (text.chars().count() as f64 / chars_per_token).ceil() as usize
}

/// Estimate token count for a text string using default ratio.
#[allow(dead_code)]
pub fn estimate_tokens(text: &str) -> usize {
    estimate_tokens_with_ratio(text, DEFAULT_CHARS_PER_TOKEN)
}

/// Estimate token cost of a single tool definition.
/// Uses json_ratio (capped at 2.5) for description + parameter schemas,
/// since JSON structures tokenize more expensively than prose.
pub fn estimate_tool_definition_tokens(tool: &ToolDefinition, chars_per_token: f64) -> usize {
    let json_ratio = chars_per_token.min(2.5);
    estimate_tokens_with_ratio(&tool.name, chars_per_token)
        + (tool.description.chars().count() as f64 / json_ratio).ceil() as usize
        + (tool.parameters.to_string().chars().count() as f64 / json_ratio).ceil() as usize
        + 10 // structural overhead per tool
}

/// Estimate token count for a single ChatMessage using configurable ratio.
pub fn estimate_message_tokens_with_ratio(message: &ChatMessage, chars_per_token: f64) -> usize {
    // Each message has ~4 tokens of structural overhead (role, delimiters)
    let mut tokens = 4;
    // JSON structures tokenize more expensively — use a tighter ratio
    let json_ratio = chars_per_token.min(2.5);
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                tokens += estimate_tokens_with_ratio(text, chars_per_token);
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                tokens += estimate_tokens_with_ratio(name, chars_per_token);
                let arg_str = arguments.to_string();
                tokens += (arg_str.chars().count() as f64 / json_ratio).ceil() as usize;
            }
            ContentBlock::ToolResult { name, content, .. } => {
                tokens += estimate_tokens_with_ratio(name, chars_per_token);
                let content_str = content.to_string();
                tokens += (content_str.chars().count() as f64 / json_ratio).ceil() as usize;
            }
            ContentBlock::Image { .. } => {
                // Images vary wildly; use a conservative fixed estimate
                tokens += 256;
            }
            ContentBlock::Thinking { text, .. } => {
                tokens += estimate_tokens_with_ratio(text, chars_per_token);
            }
        }
    }
    tokens
}

/// Estimate token count for a single ChatMessage using default ratio.
#[allow(dead_code)]
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    estimate_message_tokens_with_ratio(message, DEFAULT_CHARS_PER_TOKEN)
}

/// Estimate total token count for an entire LlmPrompt using configurable ratio.
pub fn estimate_prompt_tokens_with_ratio(prompt: &LlmPrompt, chars_per_token: f64) -> usize {
    let mut tokens = 0;

    // System prompt
    if let Some(system) = &prompt.system {
        tokens += estimate_tokens_with_ratio(system, chars_per_token);
    }

    // All messages
    for msg in &prompt.messages {
        tokens += estimate_message_tokens_with_ratio(msg, chars_per_token);
    }

    // Tool definitions (JSON schemas are token-heavy)
    for tool in &prompt.tools {
        tokens += estimate_tool_definition_tokens(tool, chars_per_token);
    }

    tokens
}

/// Estimate total token count for an entire LlmPrompt using default ratio.
#[allow(dead_code)]
pub fn estimate_prompt_tokens(prompt: &LlmPrompt) -> usize {
    estimate_prompt_tokens_with_ratio(prompt, DEFAULT_CHARS_PER_TOKEN)
}

/// Calculate the effective input budget given a max context window.
/// Applies a scaled safety margin: larger for small models (JSON overhead
/// makes our char-based estimator less accurate), smaller for big models.
pub fn effective_input_budget(max_context_tokens: usize) -> usize {
    // Linear interpolation: 30% margin at 16K, tapering to 15% at 128K+
    let t = ((max_context_tokens as f64 - 16_000.0) / (128_000.0 - 16_000.0)).clamp(0.0, 1.0);
    let margin = MAX_SAFETY_MARGIN + t * (MIN_SAFETY_MARGIN - MAX_SAFETY_MARGIN);
    let reserved = (max_context_tokens as f64 * margin) as usize;
    max_context_tokens.saturating_sub(reserved)
}

/// Given a prompt and a token budget, determine how many messages from the
/// FRONT (oldest) of the history should be dropped to fit within budget.
/// Returns the number of messages to drop, and the estimated final token count.
///
/// Never drops: system prompt, tools, the last `protected_window` messages.
/// `chars_per_token`: configurabe ratio for token estimation accuracy.
pub fn messages_to_drop(
    prompt: &LlmPrompt,
    budget: usize,
    protected_window: usize,
    chars_per_token: f64,
) -> (usize, usize) {
    let total = estimate_prompt_tokens_with_ratio(prompt, chars_per_token);
    if total <= budget {
        return (0, total);
    }

    // Cost of fixed elements (system + tools) — these are never dropped
    let fixed_cost = prompt.system.as_ref().map_or(0, |s| estimate_tokens_with_ratio(s, chars_per_token))
        + prompt
            .tools
            .iter()
            .map(|t| estimate_tool_definition_tokens(t, chars_per_token))
            .sum::<usize>();

    // Protected messages (last N) — never dropped
    let msg_count = prompt.messages.len();
    let protected_start = msg_count.saturating_sub(protected_window);

    // Cost of protected messages
    let protected_cost: usize = prompt.messages[protected_start..]
        .iter()
        .map(|m| estimate_message_tokens_with_ratio(m, chars_per_token))
        .sum();

    let available_for_history = budget.saturating_sub(fixed_cost + protected_cost);

    // Walk from oldest, accumulating until we run out of budget
    let mut kept_cost = 0usize;
    let mut first_kept = 0usize;
    for (i, msg) in prompt.messages[..protected_start].iter().enumerate().rev() {
        let msg_cost = estimate_message_tokens_with_ratio(msg, chars_per_token);
        if kept_cost + msg_cost > available_for_history {
            first_kept = i + 1;
            break;
        }
        kept_cost += msg_cost;
    }

    let dropped = first_kept;
    let final_estimate = fixed_cost + kept_cost + protected_cost;
    (dropped, final_estimate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ChatRole;

    #[test]
    fn test_estimate_tokens_basic() {
        // 12 chars / 3.0 = 4 tokens
        assert_eq!(estimate_tokens("hello world!"), 4);
        // Empty string → 0 tokens
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_effective_budget() {
        // 32K model: margin interpolates between 30% and 15%, budget ≈ 23,674
        let budget = effective_input_budget(32768);
        assert!(budget > 22000, "Budget {} should be > 22000 for 32K model", budget);
        assert!(budget < 26000, "Budget {} should be < 26000 for 32K model", budget);
        
        // 16K model: gets 30% margin, budget ≈ 11,200
        let small_budget = effective_input_budget(16000);
        assert!(small_budget > 10000, "Small budget {} should be > 10000", small_budget);
        assert!(small_budget < 12000, "Small budget {} should be < 12000", small_budget);
        
        // 128K+ model: gets 15% margin, budget ≈ 108,800
        let large_budget = effective_input_budget(128000);
        assert!(large_budget > 108000, "Large budget {} should be > 108000", large_budget);
    }

    #[test]
    fn test_messages_to_drop_under_budget() {
        let prompt = LlmPrompt {
            system: Some("You are a helpful assistant.".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: vec![],
        };
        let (dropped, _) = messages_to_drop(&prompt, 1000, 2, DEFAULT_CHARS_PER_TOKEN);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn test_messages_to_drop_over_budget() {
        let mut messages = Vec::new();
        // Create 20 messages with ~50 tokens each
        for i in 0..20 {
            messages.push(ChatMessage {
                role: if i % 2 == 0 {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                },
                content: vec![ContentBlock::Text {
                    text: "x".repeat(200), // ~50 tokens each
                }],
            });
        }
        let prompt = LlmPrompt {
            system: Some("System".to_string()),
            messages,
            tools: vec![],
        };
        // Budget of 300 tokens — should drop most messages
        let (dropped, estimate) = messages_to_drop(&prompt, 300, 4, DEFAULT_CHARS_PER_TOKEN);
        assert!(dropped > 0, "Should drop some messages");
        assert!(estimate <= 300, "Estimate should be within budget");
    }
}
