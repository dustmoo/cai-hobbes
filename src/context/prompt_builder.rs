use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::llm::types::{
    ChatMessage, ChatRole, ContentBlock, LlmPrompt as NeutralLlmPrompt, ToolDefinition,
};
use crate::session::Session;
use crate::settings::Settings;
use chrono::Local;
use serde_json::{self, json};

#[cfg(test)]
mod prompt_builder_tests;

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

/// Builds a structured `LlmPrompt` object for the LLM.
pub struct PromptBuilder<'a> {
    session: &'a Session,
    settings: &'a Settings,
    session_state: &'a crate::session::SessionState,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(
        session: &'a Session,
        settings: &'a Settings,
        session_state: &'a crate::session::SessionState,
    ) -> Self {
        Self {
            session,
            settings,
            session_state,
        }
    }

    /// Builds the structured `LlmPrompt` with system instructions, tools, and conversation history.
    pub fn build_prompt(
        &self,
        user_message: String,
        _last_agent_message: Option<String>,
    ) -> NeutralLlmPrompt {
        // 1. Extract and format tools from the session context.
        let mut tools = Vec::new();
        if let Some(mcp_context) = &self.session.active_context.mcp_tools {
            for server in &mcp_context.servers {
                for tool in &server.tools {
                    tools.push(ToolDefinition::from_mcp(tool, &server.name));
                }
            }
        }

        // 2. Build the system instruction from the remaining context.
        let mut active_context = self.session.active_context.clone();

        // Apply memory size limits from settings
        active_context
            .conversation_summary
            .truncate_summary(self.settings.max_summary_chars);
        active_context
            .conversation_summary
            .entities
            .prune_entities(self.settings.max_entity_count);

        let mut persona = self.settings.persona.clone();

        if let Some(instruction) = &self.settings.force_tool_use_instruction {
            persona = format!("{}\n\nCRITICAL INSTRUCTION: {}", persona, instruction);
        }

        // Check if the last message is an empty placeholder from Hobbes (continuation scenario)
        let last_message = self.session.messages.last();
        let is_continuation_placeholder = last_message.is_some_and(|m| {
            m.author == "Hobbes" && matches!(m.content, MessageContent::Text { ref content, .. } if content.is_empty())
        });

        if user_message.is_empty() {
            // Check if the last message (or the one before placeholder) was a tool call
            let message_to_check = if is_continuation_placeholder {
                if self.session.messages.len() >= 2 {
                    self.session.messages.get(self.session.messages.len() - 2)
                } else {
                    None
                }
            } else {
                last_message
            };

            let last_message_was_tool =
                message_to_check.is_some_and(|m| matches!(m.content, MessageContent::ToolCall(_)));

            if last_message_was_tool {
                let tool_completion_instruction = "\n\nTOOL COMPLETION INSTRUCTION: The tool execution has completed. Use the tool output above to answer the user's request. Do not ask the user for the tool output again. When reporting specific values from tool outputs (like dates, IDs, or file paths), present them exactly as returned. Do not transform, reformat, or convert them (e.g. date conversion) unless explicitly requested by the user.";
                persona.push_str(tool_completion_instruction);
            } else {
                let continuation_instruction = "\n\nCONTINUATION INSTRUCTION: You were the last one to speak. The user has not replied. Continue the conversation based on the existing context. Do not repeat yourself. Provide new information or ask a clarifying question.";
                persona.push_str(continuation_instruction);
            }
        }

        if self.session_state.tool_call_history.iter().any(|r| {
            matches!(
                r.result.status,
                crate::components::shared::ToolCallStatus::Error
            )
        }) {
            let recovery_instruction = "\n\nCRITICAL RECOVERY INSTRUCTION: A previous tool call failed. Analyze the error message and attempt a different tool call to accomplish the user's goal. Do not repeat the failed tool call.";
            persona.push_str(recovery_instruction);
        }

        active_context.system_persona = Some(persona);

        // Extract MCP server info BEFORE nulling mcp_tools - so LLM knows what each server is
        let mcp_servers_info: Option<Vec<serde_json::Value>> =
            active_context.mcp_tools.as_ref().map(|ctx| {
                ctx.servers
                    .iter()
                    .map(|server| {
                        serde_json::json!({
                            "name": server.name,
                            "description": server.description,
                            "tools_count": server.tools.len()
                        })
                    })
                    .collect()
            });

        active_context.mcp_tools = None; // Exclude full tool definitions from the instruction text.

        let mut system_context_map = serde_json::Map::new();
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&active_context) {
            system_context_map = map;
        }

        // Re-add summarized MCP server info so LLM understands available servers
        if let Some(servers) = mcp_servers_info {
            system_context_map.insert("mcp_servers".to_string(), serde_json::Value::Array(servers));
        }

        // Determine the user's name, prioritizing settings over conversation summary.
        let final_user_name = self
            .settings
            .user_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let name_from_summary = &active_context.conversation_summary.entities.user_name;
                if !name_from_summary.trim().is_empty() {
                    Some(name_from_summary.as_str())
                } else {
                    None
                }
            });

        if let Some(name) = final_user_name {
            // If we have a name, add it to the context and ensure the instruction is removed.
            system_context_map.insert("user_name".to_string(), json!(name));
            system_context_map.remove("user_instruction");
        } else {
            // If no name is found, add the instruction to ask for it and guide the user to settings.
            system_context_map.insert(
                "user_instruction".to_string(),
                json!("Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called. Direct them to set this in the 'Application Behavior' section of the settings."),
            );
        }

        system_context_map.insert(
            "current_time".to_string(),
            json!({
                "iso_8601": Local::now().to_rfc3339(),
                "timezone": "Local"
            }),
        );

        // Check for fully configured Composio profiles and inject context
        if self
            .settings
            .composio_profiles
            .iter()
            .any(|p| p.is_fully_configured())
        {
            let profile_id = self
                .session
                .composio_profile
                .as_deref()
                .or(self.settings.active_composio_profile.as_deref());
            let active_profile_name = profile_id
                .and_then(|id| self.settings.profile_name_for_id(id))
                .unwrap_or("Default");

            system_context_map.insert(
                "composio_context".to_string(),
                json!({
                    "info": "You have access to external tools via Composio. Integrations are managed through 'Profiles'.",
                    "active_profile": active_profile_name,
                    "instruction": format!("The currently active profile determining your available tool connections is: '{}'.", active_profile_name)
                })
            );
        }

        // Extract active skill context from messages and inject into system instruction
        for message in &self.session.messages {
            if let MessageContent::SkillCall(sc) = &message.content {
                if matches!(
                    sc.status,
                    crate::components::shared::SkillCallStatus::Completed
                ) {
                    if let Ok(payload) = serde_json::from_str::<
                        crate::components::shared::CapabilityContextPayload,
                    >(&sc.response)
                    {
                        let tool_mappings: Vec<serde_json::Value> = payload
                            .resolved_tools
                            .iter()
                            .map(|(capability, tool_name)| {
                                json!({
                                    "capability": capability,
                                    "use_tool": tool_name
                                })
                            })
                            .collect();

                        system_context_map.insert(
                            "active_skill".to_string(),
                            json!({
                                "name": sc.skill_name,
                                "priority_instruction": format!(
                                    "CRITICAL: You are executing the '{}' skill. Follow the instructions below EXACTLY. Do NOT improvise or use generic approaches.",
                                    sc.skill_name
                                ),
                                "instruction_manual": payload.instruction_manual,
                                "resolved_tools": tool_mappings,
                                "arguments": sc.arguments,
                                "warnings": payload.warnings
                            })
                        );
                    }
                }
            }
        }

        // Skill-scoped tool filtering: when a skill has resolved specific tools,
        // only include those tool definitions instead of ALL tools from ALL servers.
        // IMPORTANT: Only apply this filter when the skill call is the LAST meaningful
        // message (current turn). Once the user moves past the skill activation,
        // full tool visibility is restored. This prevents stale skill calls buried
        // in history from permanently filtering out all non-skill tools.
        let last_meaningful_msg = if is_continuation_placeholder {
            self.session.messages.iter().rev().nth(1)
        } else {
            self.session.messages.last()
        };

        let skill_tool_names: Option<Vec<String>> =
            last_meaningful_msg.and_then(|m| match &m.content {
                MessageContent::SkillCall(sc)
                    if matches!(
                        sc.status,
                        crate::components::shared::SkillCallStatus::Completed
                    ) =>
                {
                    serde_json::from_str::<crate::components::shared::CapabilityContextPayload>(
                        &sc.response,
                    )
                    .ok()
                    .map(|p| {
                        p.resolved_tools
                            .values()
                            .flat_map(|v| v.split(", "))
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.starts_with("(on-demand)"))
                            .collect()
                    })
                }
                _ => None,
            });

        if let Some(ref skill_tools) = skill_tool_names {
            if !skill_tools.is_empty() {
                let before = tools.len();
                tools.retain(|t| {
                    skill_tools
                        .iter()
                        .any(|st| t.name == *st || t.name.ends_with(st))
                });
                tracing::info!(
                    "Skill-scoped tool filter (last turn): {} → {} tools (skill resolved: {:?})",
                    before,
                    tools.len(),
                    skill_tools
                );
            }
        }

        let persona = system_context_map
            .remove("system_persona")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        // Size-based guardrail: strip oversized entity values from conversation_summary.
        // The #[serde(flatten)] catch-all on ConversationSummaryEntities can capture
        // arbitrary model output (e.g. raw message_history). Normal entities are short
        // strings/arrays; data dumps are large. Stripping values > 500 chars catches
        // legacy leaks without maintaining an explicit allowlist.
        if let Some(summary_obj) = system_context_map
            .get_mut("conversation_summary")
            .and_then(|v| v.as_object_mut())
        {
            if let Some(entities_obj) = summary_obj
                .get_mut("entities")
                .and_then(|v| v.as_object_mut())
            {
                const MAX_ENTITY_VALUE_LEN: usize = 500;
                let oversized_keys: Vec<String> = entities_obj
                    .iter()
                    .filter(|(k, v)| {
                        *k != "user_name" && v.to_string().len() > MAX_ENTITY_VALUE_LEN
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in &oversized_keys {
                    entities_obj.remove(key);
                    tracing::warn!("Stripped oversized entity '{}' from SYSTEM_CONTEXT", key);
                }
            }
        }

        let context_json = serde_json::to_string_pretty(&system_context_map).unwrap_or_default();

        let mut system = persona;
        if !context_json.is_empty() && context_json != "{}" {
            system.push_str(&format!(
                "\n\n<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>",
                context_json
            ));
        }

        // 3. Construct the conversational contents.
        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut last_thought_signature: Option<String> = None;

        let history_len = self.settings.chat_history_length;
        let session_messages = &self.session.messages;
        let mut first_message_id = None;

        // 1. Add the first user message to preserve the original intent.
        if let Some(first_message) = session_messages.iter().find(|m| m.author == "User") {
            if let MessageContent::Text { .. } = &first_message.content {
                messages.push(first_message.clone().into());
                first_message_id = Some(first_message.id);
            }
        }

        // 2. Add the last `history_len` messages.
        let start_index = session_messages.len().saturating_sub(history_len);

        for message in session_messages.iter().skip(start_index) {
            // Avoid duplicating the first message
            if Some(message.id) != first_message_id {
                // Skip placeholder
                if is_continuation_placeholder && message.id == last_message.unwrap().id {
                    continue;
                }

                match &message.content {
                    MessageContent::Text {
                        thought_signature, ..
                    } => {
                        // Track the latest thought_signature for backfilling onto
                        // subsequent tool calls that may lack their own.
                        if let Some(sig) = thought_signature {
                            last_thought_signature = Some(sig.clone());
                        }

                        let chat_msg: ChatMessage = message.clone().into();

                        // Handle comments
                        if !message.comments.is_empty() {
                            let mut comment_text =
                                String::from("[User comments on the above message:");
                            for comment in &message.comments {
                                comment_text.push_str(&format!(
                                    "\n- On \"{}\": {}",
                                    comment.text_selection, comment.comment
                                ));
                            }
                            comment_text.push(']');

                            messages.push(chat_msg);
                            messages.push(ChatMessage {
                                role: ChatRole::User,
                                content: vec![ContentBlock::Text { text: comment_text }],
                            });
                        } else {
                            messages.push(chat_msg);
                        }
                    }
                    MessageContent::ToolCall(tc) => {
                        // Backfill thought_signature from the last known one if this
                        // tool call doesn't have its own (critical for Gemini thinking mode).
                        let current_thought_signature = tc
                            .thought_signature
                            .clone()
                            .or(last_thought_signature.clone());
                        if let Some(sig) = &tc.thought_signature {
                            last_thought_signature = Some(sig.clone());
                        }

                        // Sanitize tool name — MUST match the name used in function declarations.
                        let sanitized_tool_name = crate::gemini::convert::get_prefixed_tool_name(
                            &tc.server_name,
                            &tc.tool_name,
                        );

                        // 1. Add the Assistant's ToolCall message (with thinking + function call)
                        let mut assistant_content = Vec::new();

                        // Include thinking summary BEFORE the function call (critical for Gemini)
                        if let Some(summary) = &tc.thought_summary {
                            if !summary.is_empty() {
                                assistant_content.push(ContentBlock::Thinking {
                                    text: summary.clone(),
                                    signature: current_thought_signature.clone(),
                                });
                            }
                        }

                        let args_value: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                        assistant_content.push(ContentBlock::ToolCall {
                            id: tc.execution_id.to_string(),
                            name: sanitized_tool_name.clone(),
                            arguments: args_value,
                            signature: current_thought_signature,
                        });

                        messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: assistant_content,
                        });

                        // 2. Add the Tool Result message
                        let last_meaningful_id = if is_continuation_placeholder {
                            if self.session.messages.len() >= 2 {
                                self.session
                                    .messages
                                    .get(self.session.messages.len() - 2)
                                    .map(|m| m.id)
                            } else {
                                None
                            }
                        } else {
                            last_message.map(|m| m.id)
                        };

                        let is_active_tool_call = Some(message.id) == last_meaningful_id;
                        let mut result_string = tc.response.clone();
                        let max_len = if is_active_tool_call {
                            self.settings.max_active_tool_output_length
                        } else {
                            self.settings.max_tool_output_length
                        };

                        if result_string.len() > max_len {
                            let original_len = result_string.len();
                            let mut truncated_len = max_len;
                            while truncated_len > 0
                                && !result_string.is_char_boundary(truncated_len)
                            {
                                truncated_len -= 1;
                            }
                            result_string.truncate(truncated_len);
                            result_string.push_str(&format!(
                                "... [Output truncated from {} bytes]",
                                original_len
                            ));
                        }

                        let result_value: serde_json::Value =
                            match serde_json::from_str::<serde_json::Value>(&result_string) {
                                Ok(val) => val,
                                Err(_) => json!(result_string),
                            };

                        messages.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: vec![ContentBlock::ToolResult {
                                call_id: tc.execution_id.to_string(),
                                name: sanitized_tool_name,
                                content: result_value,
                            }],
                        });
                    }
                    MessageContent::SkillCall(sc) => {
                        if let crate::components::shared::SkillCallStatus::Completed = sc.status {
                            // Only include a brief marker — the full skill instructions are
                            // already in the system context via the `active_skill` injection
                            // (lines 260-289). Including sc.response here was DOUBLE injecting
                            // the instruction manual, resolved tools, and arguments — adding
                            // thousands of tokens redundantly.
                            let context_text = format!(
                                "[Skill '{}' activated with arguments: {}. See active_skill in system context for instructions.]",
                                sc.skill_name, sc.arguments
                            );

                            messages.push(ChatMessage {
                                role: ChatRole::User,
                                content: vec![ContentBlock::Text { text: context_text }],
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // 3. Add current user message
        if !user_message.is_empty() {
            messages.push(ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: user_message }],
            });
        }

        NeutralLlmPrompt {
            system: Some(system),
            messages,
            tools,
        }
    }
}
