use crate::llm::types::{ChatMessage, ContentBlock, LlmPrompt};

/// Lightweight token estimator using a characters-per-token heuristic.
/// Uses ~4 chars/token (conservative for English text, slightly aggressive for code/JSON).
/// This avoids external tokenizer dependencies while providing useful bounds.
const CHARS_PER_TOKEN: usize = 4;

/// Safety margin — reserve this fraction of the context for output + API overhead.
/// For small models (<32K), JSON tokenization overhead means our character-based
/// estimator significantly underestimates actual tokens. We use a sliding scale:
/// - 16K model → 30% margin (aggressive, needed for JSON-heavy tool use)
/// - 32K model → 20% margin
/// - 128K+ model → 15% margin (heuristic works well at scale)
const MIN_SAFETY_MARGIN: f64 = 0.15;
const MAX_SAFETY_MARGIN: f64 = 0.30;

/// Estimate token count for a text string.
pub fn estimate_tokens(text: &str) -> usize {
    // Use char count (not byte count) for better accuracy with Unicode
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Estimate token count for a single ChatMessage.
pub fn estimate_message_tokens(message: &ChatMessage) -> usize {
    // Each message has ~4 tokens of structural overhead (role, delimiters)
    let mut tokens = 4;
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                tokens += estimate_tokens(text);
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                // JSON structures tokenize more expensively (~3 chars/token)
                tokens += estimate_tokens(name);
                let arg_str = arguments.to_string();
                tokens += arg_str.chars().count().div_ceil(3);
            }
            ContentBlock::ToolResult { name, content, .. } => {
                // JSON structures tokenize more expensively (~3 chars/token)
                tokens += estimate_tokens(name);
                let content_str = content.to_string();
                tokens += content_str.chars().count().div_ceil(3);
            }
            ContentBlock::Image { .. } => {
                // Images vary wildly; use a conservative fixed estimate
                tokens += 256;
            }
            ContentBlock::Thinking { text, .. } => {
                tokens += estimate_tokens(text);
            }
        }
    }
    tokens
}

/// Estimate total token count for an entire LlmPrompt.
pub fn estimate_prompt_tokens(prompt: &LlmPrompt) -> usize {
    let mut tokens = 0;

    // System prompt
    if let Some(system) = &prompt.system {
        tokens += estimate_tokens(system);
    }

    // All messages
    for msg in &prompt.messages {
        tokens += estimate_message_tokens(msg);
    }

    // Tool definitions (JSON schemas are token-heavy)
    for tool in &prompt.tools {
        // Tool schemas have JSON-heavy structure — use 3 chars/token
        let desc_str = tool.description.clone();
        let param_str = tool.parameters.to_string();
        tokens += estimate_tokens(&tool.name);
        tokens += desc_str.chars().count().div_ceil(3);
        tokens += param_str.chars().count().div_ceil(3);
        tokens += 10; // structural overhead per tool
    }

    tokens
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
pub fn messages_to_drop(
    prompt: &LlmPrompt,
    budget: usize,
    protected_window: usize,
) -> (usize, usize) {
    let total = estimate_prompt_tokens(prompt);
    if total <= budget {
        return (0, total);
    }

    // Cost of fixed elements (system + tools) — these are never dropped
    let fixed_cost = prompt.system.as_ref().map_or(0, |s| estimate_tokens(s))
        + prompt
            .tools
            .iter()
            .map(|t| {
                estimate_tokens(&t.name)
                    + estimate_tokens(&t.description)
                    + estimate_tokens(&t.parameters.to_string())
                    + 10
            })
            .sum::<usize>();

    // Protected messages (last N) — never dropped
    let msg_count = prompt.messages.len();
    let protected_start = msg_count.saturating_sub(protected_window);

    // Cost of protected messages
    let protected_cost: usize = prompt.messages[protected_start..]
        .iter()
        .map(estimate_message_tokens)
        .sum();

    let available_for_history = budget.saturating_sub(fixed_cost + protected_cost);

    // Walk from oldest, accumulating until we run out of budget
    let mut kept_cost = 0usize;
    let mut first_kept = 0usize;
    for (i, msg) in prompt.messages[..protected_start].iter().enumerate().rev() {
        let msg_cost = estimate_message_tokens(msg);
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
        // 12 chars → 3 tokens
        assert_eq!(estimate_tokens("hello world!"), 3);
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
        let (dropped, _) = messages_to_drop(&prompt, 1000, 2);
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
        let (dropped, estimate) = messages_to_drop(&prompt, 300, 4);
        assert!(dropped > 0, "Should drop some messages");
        assert!(estimate <= 300, "Estimate should be within budget");
    }
}
