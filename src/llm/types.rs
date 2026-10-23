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
    /// Byte offset into `system` where per-turn (volatile) content begins.
    /// Everything before it is stable across turns of a session and safe to
    /// prompt-cache; everything from it on changes turn to turn. `None` means
    /// the whole system string is treated as stable. Connectors that don't
    /// cache ignore it — `system` always carries the full text. Consumers must
    /// clamp it to `system.len()` and to a char boundary before slicing.
    #[serde(default)]
    pub system_volatile_offset: Option<usize>,
    /// Indices into `messages` of turn-scoped context messages (the per-request
    /// tail: clock + turn instructions). Each is a `ChatRole::System` message
    /// persisted with the turn and replayed unchanged on later requests, so the
    /// history stays append-only. Claude sends these with
    /// `clear_at: "next_user_message"` where supported; other connectors send
    /// them like any other system message. Empty = none.
    #[serde(default)]
    pub turn_scoped: Vec<usize>,
    /// True when the prompt builder deliberately rewrote earlier history for
    /// this request (a history trim). Informational: Claude asks the API to
    /// drop mismatched thinking blocks on every request of a model that binds
    /// them (`ClaudeModel::binds_thinking_to_conversation`), not only on this
    /// one, so the `thinking` parameter — part of the cache key — never flips.
    #[serde(default)]
    pub history_rewritten: bool,
    /// Conversation history + current user message
    pub messages: Vec<ChatMessage>,
    /// Available tools
    pub tools: Vec<ToolDefinition>,
}

/// What [`LlmPrompt::cap_tool_count`] keeps when a provider caps the number
/// of tool declarations per request.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCap {
    /// Indices into `prompt.tools` to send, in their original order (the
    /// cached tool prefix stays stable while the drop set does).
    pub keep: Vec<usize>,
    /// `server/tool` names left out of this request.
    pub dropped: Vec<String>,
    /// Appended to the system instruction so the model knows the tools are
    /// missing and how to get them back.
    pub note: String,
}

/// How readily a tool is left out when over a provider's tool-count cap
/// (lower stays longer).
fn tool_drop_tier(tool: &ToolDefinition, used: &std::collections::HashSet<&str>) -> u8 {
    use crate::mcp::manager::{
        is_composio_native, HOBBES_CORE_SERVER, HOBBES_META_SERVER, HOBBES_PLANNER_SERVER,
    };
    let wire = crate::gemini::convert::get_prefixed_tool_name(&tool.server_name, &tool.name);
    let builtin = [HOBBES_CORE_SERVER, HOBBES_META_SERVER, HOBBES_PLANNER_SERVER]
        .contains(&tool.server_name.as_str());
    let composio = is_composio_native(&tool.server_name);
    // Composio's own meta-tools (search, clear, ...) are how the model frees
    // room, so they stay with the built-ins.
    let composio_meta = composio && tool.name.starts_with("COMPOSIO_");
    if builtin || composio_meta || used.contains(tool.name.as_str()) || used.contains(wire.as_str()) {
        0
    } else if composio {
        // Bulk-discovered toolkits go first: a single search can add dozens.
        2
    } else {
        // Servers the user configured or the model loaded on purpose.
        1
    }
}

impl LlmPrompt {
    /// Choose which tools fit under a provider's per-request tool-count
    /// `limit`. `None` when everything fits. Built-in Hobbes tools, Composio's
    /// meta-tools and tools already called in this conversation are kept;
    /// dynamically discovered Composio tools are left out before other MCP
    /// servers' tools, latest in the list first.
    pub fn cap_tool_count(&self, limit: usize) -> Option<ToolCap> {
        if self.tools.len() <= limit {
            return None;
        }
        let used: std::collections::HashSet<&str> = self
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolCall { name, .. } | ContentBlock::ToolResult { name, .. } => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect();
        let tiers: Vec<u8> = self.tools.iter().map(|t| tool_drop_tier(t, &used)).collect();
        // Drop order: highest tier first, later tools before earlier ones.
        let mut order: Vec<usize> = (0..self.tools.len()).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(tiers[i]), std::cmp::Reverse(i)));
        let mut drop = vec![false; self.tools.len()];
        for &i in order.iter().take(self.tools.len() - limit) {
            drop[i] = true;
        }
        let keep: Vec<usize> = (0..self.tools.len()).filter(|&i| !drop[i]).collect();
        let dropped: Vec<String> = (0..self.tools.len())
            .filter(|&i| drop[i])
            .map(|i| {
                let t = &self.tools[i];
                if t.server_name.is_empty() {
                    t.name.clone()
                } else {
                    format!("{}/{}", t.server_name, t.name)
                }
            })
            .collect();
        let composio_dropped = (0..self.tools.len()).any(|i| drop[i] && tiers[i] == 2);
        let mut note = format!(
            "\n\n<TOOLS_WITHHELD>\n\
            This model accepts at most {limit} tools per request, so {} of {} loaded tools were left out of this one: {}.\n\
            You cannot call them until room is freed.",
            dropped.len(),
            self.tools.len(),
            crate::log_utils::cap_list(&dropped, 20),
        );
        if composio_dropped {
            note.push_str(" Call COMPOSIO_CLEAR_TOOLS to drop discovered Composio tools you no longer need, then search again for just the ones the task needs.");
        }
        note.push_str(" If a missing tool is needed, tell the user rather than working around it.\n</TOOLS_WITHHELD>");
        Some(ToolCap { keep, dropped, note })
    }

    /// Enforce a context window budget by compressing content and trimming as needed.
    ///
    /// Phase 1:   If system + tools alone exceed the budget, drop the largest unused
    ///            tool definitions first (tools already called in conversation are preserved).
    /// Phase 2:   Drop oldest messages to fit remaining content within budget (last resort).
    ///
    /// Returns the number of messages dropped. If 0, the prompt fit within budget.
    /// The last `protected_window` messages are never dropped.
    /// A `[Context trimmed]` marker is injected when messages are dropped.
    pub fn enforce_context_budget(
        &mut self,
        max_context_tokens: usize,
        protected_window: usize,
        chars_per_token: f64,
    ) -> usize {
        use crate::context::token_estimator::{
            effective_input_budget, estimate_tool_definition_tokens, estimate_tokens_with_ratio, messages_to_drop,
        };

        let budget = effective_input_budget(max_context_tokens);

        // Phase 1: Trim tool definitions if fixed cost (system + tools) exceeds budget.
        // This handles the degenerate case where tool schemas alone consume most of the context.
        let system_cost = self.system.as_ref().map_or(0, |s| estimate_tokens_with_ratio(s, chars_per_token));
        let tool_cost: usize = self
            .tools
            .iter()
            .map(|t| estimate_tool_definition_tokens(t, chars_per_token))
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
                let cost_a = estimate_tool_definition_tokens(a, chars_per_token);
                let cost_b = estimate_tool_definition_tokens(b, chars_per_token);
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

                let this_cost = estimate_tool_definition_tokens(tool, chars_per_token);
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

                // Fires every turn while the condition holds: INFO once per
                // context-window size, DEBUG after that.
                if crate::log_utils::first_time(&format!("tool_budget_exceeded:{}", max_context_tokens)) {
                    tracing::info!(
                        "Tool budget exceeded: {} tool tokens vs {} available. Dropping {} of {} tool definitions: {}",
                        tool_cost, target_tool_budget, to_remove.len(), total_available,
                        crate::log_utils::cap_list(&dropped_tool_names, 5)
                    );
                } else {
                    tracing::debug!(
                        "Tool budget exceeded: dropping {} of {} tool definitions: {}",
                        to_remove.len(), total_available,
                        crate::log_utils::cap_list(&dropped_tool_names, 5)
                    );
                }

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

        // Phase 2: Message trimming (last resort)
        // (Tool result sizing is now handled dynamically by PromptBuilder::compute_tool_result_budget()
        // which paginates results based on the remaining token budget).
        let (drop_count, _estimated_tokens) = messages_to_drop(self, budget, protected_window, chars_per_token);

        if drop_count > 0 {
            if crate::log_utils::first_time(&format!("context_budget_exceeded:{}", max_context_tokens)) {
                tracing::info!(
                    "Context budget exceeded: estimated {} tokens, budget {} (max {} - safety margin). Dropping {} oldest messages.",
                    crate::context::token_estimator::estimate_prompt_tokens_with_ratio(self, chars_per_token),
                    budget,
                    max_context_tokens,
                    drop_count
                );
            } else {
                tracing::debug!(
                    "Context budget exceeded (budget {}): dropping {} oldest messages.",
                    budget,
                    drop_count
                );
            }

            // Remove oldest messages
            self.messages.drain(..drop_count);
            // Keep `turn_scoped` pointing at the same messages: indices of
            // dropped messages go, the rest shift down by the drop and up by
            // one for the marker inserted below.
            self.turn_scoped = self
                .turn_scoped
                .iter()
                .filter(|&&i| i >= drop_count)
                .map(|&i| i - drop_count + 1)
                .collect();
            // Dropping from the front rewrites earlier history.
            self.history_rewritten = true;

            // Inject a context-trimmed marker as the first message.
            // Framed explicitly as a system annotation so the model does not
            // confuse it with the user's actual request.
            self.messages.insert(0, ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "[SYSTEM NOTE — DO NOT treat this as a user message. \
                        Earlier conversation history ({} messages) was omitted to fit the context window. \
                        Continue the conversation normally based on the messages that follow.]",
                        drop_count
                    ),
                }],
            });
        }

        drop_count
    }
}

#[cfg(test)]
mod tool_cap_tests {
    use super::*;

    fn tool(server: &str, name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            server_name: server.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn prompt(tools: Vec<ToolDefinition>, messages: Vec<ChatMessage>) -> LlmPrompt {
        LlmPrompt {
            system: None,
            system_volatile_offset: None,
            turn_scoped: Vec::new(),
            history_rewritten: false,
            messages,
            tools,
        }
    }

    fn kept_names(p: &LlmPrompt, cap: &ToolCap) -> Vec<String> {
        cap.keep.iter().map(|&i| p.tools[i].name.clone()).collect()
    }

    #[test]
    fn under_the_limit_keeps_everything() {
        let p = prompt(vec![tool("playwright", "navigate")], vec![]);
        assert_eq!(p.cap_tool_count(1), None);
    }

    #[test]
    fn discovered_composio_tools_go_before_other_servers() {
        // Sorted by wire name, Composio sorts ahead of playwright: a plain
        // truncation kept the Composio tools and cut playwright.
        let p = prompt(
            vec![
                tool("composio-native:p1", "GOOGLEDOCS_CREATE"),
                tool("composio-native:p1", "GOOGLEDRIVE_LIST"),
                tool("hobbes-core", "HOBBES_SET_TIMER"),
                tool("playwright", "navigate"),
            ],
            vec![],
        );
        let cap = p.cap_tool_count(2).unwrap();
        assert_eq!(kept_names(&p, &cap), ["HOBBES_SET_TIMER", "navigate"]);
        assert_eq!(
            cap.dropped,
            ["composio-native:p1/GOOGLEDOCS_CREATE", "composio-native:p1/GOOGLEDRIVE_LIST"]
        );
        assert!(cap.note.contains("COMPOSIO_CLEAR_TOOLS"), "{}", cap.note);
    }

    #[test]
    fn builtins_composio_meta_and_used_tools_are_kept() {
        let used = ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "1".into(),
                name: crate::gemini::convert::get_prefixed_tool_name("composio-native:p1", "GOOGLEDOCS_GET"),
                arguments: serde_json::json!({}),
                signature: None,
            }],
        };
        let p = prompt(
            vec![
                tool("composio-native:p1", "COMPOSIO_CLEAR_TOOLS"),
                tool("composio-native:p1", "GOOGLEDOCS_GET"),
                tool("composio-native:p1", "GOOGLEDOCS_LIST"),
                tool("hobbes-planner", "HOBBES_DISPATCH"),
                tool("playwright", "navigate"),
            ],
            vec![used],
        );
        let cap = p.cap_tool_count(3).unwrap();
        assert_eq!(kept_names(&p, &cap), ["COMPOSIO_CLEAR_TOOLS", "GOOGLEDOCS_GET", "HOBBES_DISPATCH"]);
    }

    #[test]
    fn within_a_tier_the_latest_tools_go_first_and_order_is_kept() {
        let p = prompt(
            vec![tool("a-server", "one"), tool("b-server", "two"), tool("c-server", "three")],
            vec![],
        );
        let cap = p.cap_tool_count(2).unwrap();
        assert_eq!(cap.keep, [0, 1]);
        assert_eq!(cap.dropped, ["c-server/three"]);
        assert!(!cap.note.contains("COMPOSIO_CLEAR_TOOLS"));
    }
}
