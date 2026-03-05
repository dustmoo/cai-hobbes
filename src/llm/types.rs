use serde::{Deserialize, Serialize};

/// Provider-neutral role for chat messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Tool, // Tool result messages (OpenAI/Claude use this)
}

/// A single content block within a message.
/// Models the union of all content types across providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content
    Text { text: String },

    /// Thinking/reasoning content (Gemini thinking, Claude extended thinking)
    Thinking {
        text: String,
        /// Gemini-specific: encrypted thinking signature for multi-turn continuity
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// Model requests a tool call
    ToolCall {
        /// Unique ID for correlating call->result (generated for Gemini, native for OpenAI/Claude)
        id: String,
        name: String,
        arguments: serde_json::Value,
        /// Gemini-specific: thought_signature for tool calls in thinking mode
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// App returns a tool result
    ToolResult {
        /// Correlates to the ToolCall.id
        call_id: String,
        name: String,
        content: serde_json::Value,
    },

    /// Inline image/media
    Image {
        mime_type: String,
        /// Base64-encoded data
        data: String,
    },
}

/// A single message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Vec<ContentBlock>,
}

/// Tool schema definition (provider-neutral).
/// JSON Schema based — the common denominator across all three providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub server_name: String, // Keep track of which server this tool belongs to
    pub description: String,
    /// JSON Schema for the tool's input parameters
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn from_mcp(tool: &rmcp::model::Tool, server_name: &str) -> Self {
        Self {
            name: tool.name.to_string(),
            server_name: server_name.to_string(),
            description: tool.description.as_deref().unwrap_or("").to_string(),
            parameters: serde_json::Value::Object((*tool.input_schema).clone()),
        }
    }
}

/// The provider-neutral prompt — replaces the current Gemini-specific `LlmPrompt`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmPrompt {
    /// System instruction (separate from messages per Gemini/Claude convention)
    pub system: Option<String>,
    /// Conversation history + current user message
    pub messages: Vec<ChatMessage>,
    /// Available tools
    pub tools: Vec<ToolDefinition>,
}

impl LlmPrompt {
    /// Enforce a context window budget by trimming tools and dropping oldest messages.
    ///
    /// Phase 1: If system + tools alone exceed the budget, drop the largest unused
    ///          tool definitions first (tools already called in conversation are preserved).
    /// Phase 2: Drop oldest messages to fit remaining content within budget.
    ///
    /// Returns the number of messages dropped. If 0, the prompt fit within budget.
    /// The last `protected_window` messages are never dropped.
    /// A `[Context trimmed]` marker is injected when messages are dropped.
    pub fn enforce_context_budget(
        &mut self,
        max_context_tokens: usize,
        protected_window: usize,
    ) -> usize {
        use crate::context::token_estimator::{
            effective_input_budget, estimate_tokens, messages_to_drop,
        };

        let budget = effective_input_budget(max_context_tokens);

        // Phase 1: Trim tool definitions if fixed cost (system + tools) exceeds budget.
        // This handles the degenerate case where tool schemas alone consume most of the context.
        let system_cost = self.system.as_ref().map_or(0, |s| estimate_tokens(s));
        let tool_cost: usize = self
            .tools
            .iter()
            .map(|t| {
                estimate_tokens(&t.name)
                    + estimate_tokens(&t.description)
                    + estimate_tokens(&t.parameters.to_string())
                    + 10
            })
            .sum();

        if system_cost + tool_cost > budget && !self.tools.is_empty() {
            // Collect tool names already used in conversation (never drop these)
            let used_tools: std::collections::HashSet<String> = self
                .messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { name, .. } => Some(name.clone()),
                    ContentBlock::ToolResult { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect();

            // Sort tools by estimated token cost (largest schemas first) for trimming
            self.tools.sort_by(|a, b| {
                let cost_a = estimate_tokens(&a.name)
                    + estimate_tokens(&a.description)
                    + estimate_tokens(&a.parameters.to_string())
                    + 10;
                let cost_b = estimate_tokens(&b.name)
                    + estimate_tokens(&b.description)
                    + estimate_tokens(&b.parameters.to_string())
                    + 10;
                cost_b.cmp(&cost_a)
            });

            // Drop largest unused tools until tools + system fit within budget
            let target_tool_budget = budget.saturating_sub(system_cost);
            let mut current_tool_cost = tool_cost;
            let mut to_remove = Vec::new();

            for (i, tool) in self.tools.iter().enumerate() {
                if current_tool_cost <= target_tool_budget {
                    break;
                }
                // Never drop tools already used in this conversation
                if used_tools.contains(&tool.name) {
                    continue;
                }

                let this_cost = estimate_tokens(&tool.name)
                    + estimate_tokens(&tool.description)
                    + estimate_tokens(&tool.parameters.to_string())
                    + 10;
                current_tool_cost -= this_cost;
                to_remove.push(i);
            }

            if !to_remove.is_empty() {
                // Collect names of tools being dropped BEFORE removing them
                let dropped_tool_names: Vec<String> = to_remove
                    .iter()
                    .filter_map(|&i| {
                        self.tools.get(i).map(|t| {
                            if t.server_name.is_empty() {
                                t.name.clone()
                            } else {
                                format!("{}/{}", t.server_name, t.name)
                            }
                        })
                    })
                    .collect();

                let total_available = self.tools.len();
                let kept_count = total_available - to_remove.len();

                tracing::warn!(
                    "Tool budget exceeded: {} tool tokens vs {} available. Dropping {} of {} tool definitions: {:?}",
                    tool_cost, target_tool_budget, to_remove.len(), total_available, dropped_tool_names
                );

                let to_remove_count = to_remove.len();

                // Remove in reverse to preserve indices
                for i in to_remove.into_iter().rev() {
                    self.tools.remove(i);
                }

                // Inject a note into the system prompt so the AI tells the user.
                // The user needs to know tools were excluded so they can adjust
                // their MCP tool profile or raise max_context_tokens.
                let exclusion_note = format!(
                    "\n\n<TOOL_BUDGET_WARNING>\n\
                    IMPORTANT: {} of {} available tool definitions were excluded from this request \
                    because they exceed the model's {} token context window.\n\
                    Only {} tools are active for this turn.\n\
                    Excluded tools: {}\n\
                    Tell the user about this limitation. Suggest they either:\n\
                    1. Reduce the number of loaded MCP tools in the Marketplace (unload unused toolkits)\n\
                    2. Increase 'Max Context Tokens' in LLM settings\n\
                    3. Use a model with a larger context window\n\
                    </TOOL_BUDGET_WARNING>",
                    to_remove_count, total_available, max_context_tokens,
                    kept_count, dropped_tool_names.join(", ")
                );

                if let Some(ref mut system) = self.system {
                    system.push_str(&exclusion_note);
                } else {
                    self.system = Some(exclusion_note);
                }
            }
        }

        // Phase 2: Existing message trimming
        let (drop_count, estimated_tokens) = messages_to_drop(self, budget, protected_window);

        if drop_count > 0 {
            tracing::warn!(
                "Context budget exceeded: estimated {} tokens, budget {} (max {} - 15% safety). Dropping {} oldest messages.",
                crate::context::token_estimator::estimate_prompt_tokens(self),
                budget,
                max_context_tokens,
                drop_count
            );

            // Remove oldest messages
            self.messages.drain(..drop_count);

            // Inject a context-trimmed marker as the first message
            self.messages.insert(0, ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "[Earlier conversation history ({} messages) was omitted to fit within the model's {} token context window. Estimated {} tokens used.]",
                        drop_count, max_context_tokens, estimated_tokens
                    ),
                }],
            });
        }

        drop_count
    }
}
