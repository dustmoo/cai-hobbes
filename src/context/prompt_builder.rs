use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::llm::types::{
    ChatMessage, ChatRole, ContentBlock, LlmPrompt as NeutralLlmPrompt, ToolDefinition,
};
use crate::session::Session;
use crate::settings::Settings;
use chrono::Local;
use serde_json::{self, json};

/// Convenience re-export so code in this file doesn't need fully-qualified paths.
use crate::str_utils::{find_split_point, floor_char_boundary};

/// Tracks a tool result message's position in the `messages` buffer for Pass 2
/// budget allocation. Collected during Pass 1 (message linearisation loop) and
/// consumed by Pass 2 (dynamic pagination).
struct ToolResultPosition {
    /// Index into the `messages` Vec where the ToolResult ContentBlock lives.
    msg_idx: usize,
    /// Sanitised tool name (server-prefixed, e.g. `mcp__fs__read_file`).
    tool_name: String,
    /// Execution UUID, used to build stable page-queue IDs.
    execution_id: String,
    /// True if this is the most recent (active) tool call this turn.
    is_active: bool,
    /// True if Pass 1 TOON condensation already stashed this result in the
    /// PageQueue. When set, Pass 2 must skip this entry so it doesn't corrupt
    /// the HOBBES_PAGE_RESULT footer that Pass 1 already embedded.
    already_condensed: bool,
}

/// Recursively unwrap JSON values that are stringified JSON.
/// Composio (and similar wrappers) often return tool results where the actual
/// data is embedded inside a string field, e.g.:
///   `{"result": {"type": "text", "text": "{\"successfull\":true,...}"}}`
/// The TOON formatter treats `Value::String` as plain text, so these
/// nested JSON payloads render as raw escaped strings. This function detects
/// string values that parse as JSON objects/arrays and replaces them in-place,
/// allowing the TOON renderer to recurse into the full structure.
fn unwrap_json_strings(value: &mut serde_json::Value) {
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

/// Result of building a prompt, including the prompt itself and any
/// paginated tool results that need to be stored in the session state.
pub struct PromptBuildResult {
    pub prompt: NeutralLlmPrompt,
    pub pages_to_store: Vec<(String, crate::session::PagedResult)>,
}

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
    ) -> PromptBuildResult {
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

        // Resolve per-provider context tuning once for this prompt build
        let tuning = self.settings.effective_context_tuning();

        // Apply memory size limits from resolved tuning (respects per-provider overrides)
        active_context
            .conversation_summary
            .truncate_summary(tuning.max_summary_chars);
        active_context
            .conversation_summary
            .entities
            .prune_entities(tuning.max_entity_count);

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

        // Inject skills from session.loaded_skills using turn-relevant lazy loading.
        //
        // Strategy: Scan the last 5 messages to detect which skill's tools were recently
        // used. Only that skill receives its full `instruction_manual`; all others are
        // stubbed (name + tool list only). This eliminates Tier 2 compression under
        // normal single-skill workflows while still supporting multi-skill turns.
        //
        // Fallback: If no active skill is detected via tool matching, ALL loaded skills
        // get their full manuals (current behavior) — we never silently degrade context.
        if !self.session.loaded_skills.is_empty() {
            // Step 1: Collect tool server prefixes used in the last 5 messages.
            let recently_used_servers: std::collections::HashSet<String> = self
                .session
                .messages
                .iter()
                .rev()
                .take(5)
                .filter_map(|m| {
                    if let MessageContent::ToolCall(tc) = &m.content {
                        Some(tc.server_name.clone())
                    } else {
                        None
                    }
                })
                .collect();

            // Step 2: Identify which loaded skills have tools on those servers.
            // A skill is "turn-active" if any of its resolved_tools match a recently
            // used server prefix.
            let active_skill_names: std::collections::HashSet<String> = self
                .session
                .loaded_skills
                .iter()
                .filter_map(|(skill_name, payload_json)| {
                    let payload = serde_json::from_str::<
                        crate::components::shared::CapabilityContextPayload,
                    >(payload_json)
                    .ok()?;
                    let is_active = payload.resolved_tools.values().any(|tool_str| {
                        recently_used_servers.iter().any(|server| {
                            tool_str.contains(server.as_str())
                        })
                    });
                    if is_active { Some(skill_name.clone()) } else { None }
                })
                .collect();

            // Step 3: Build the loaded_skills array for the system context map.
            // Full manual → turn-active skills; stub only → inactive skills.
            // If no active skills were detected, fall back to injecting all skills fully.
            let use_lazy = !active_skill_names.is_empty();
            let skills_array: Vec<serde_json::Value> = self
                .session
                .loaded_skills
                .iter()
                .filter_map(|(skill_name, payload_json)| {
                    let payload = serde_json::from_str::<
                        crate::components::shared::CapabilityContextPayload,
                    >(payload_json)
                    .ok()?;
                    let tool_mappings: Vec<serde_json::Value> = payload
                        .resolved_tools
                        .iter()
                        .map(|(cap, tool)| json!({"capability": cap, "use_tool": tool}))
                        .collect();

                    let is_active = !use_lazy || active_skill_names.contains(skill_name);
                    if is_active {
                        tracing::debug!(
                            "Loaded skill '{}': full instruction_manual injected ({} chars)",
                            skill_name, payload.instruction_manual.len()
                        );
                        Some(json!({
                            "name": skill_name,
                            "instruction_manual": payload.instruction_manual,
                            "resolved_tools": tool_mappings,
                            "warnings": payload.warnings,
                        }))
                    } else {
                        tracing::debug!(
                            "Loaded skill '{}': stub only (not active this turn)",
                            skill_name
                        );
                        Some(json!({
                            "name": skill_name,
                            "resolved_tools": tool_mappings,
                            "note": "Skill loaded but not active this turn. Full instructions available if needed.",
                        }))
                    }
                })
                .collect();

            if !skills_array.is_empty() {
                system_context_map.insert(
                    "loaded_skills".to_string(),
                    serde_json::Value::Array(skills_array),
                );
                tracing::info!(
                    "Loaded skills injected: {} total, {} active (lazy={})",
                    self.session.loaded_skills.len(),
                    active_skill_names.len(),
                    use_lazy
                );
            }
        }

        // Inject the AI-authored scratchpad as a Tier 1 core payload.
        // This is never trimmed by compose_system_for_budget — it is the AI's
        // persistent working memory for the current session, written via
        // HOBBES_UPDATE_SCRATCHPAD and immune to history scrolling and compression.
        if !self.session.scratchpad.is_empty() {
            system_context_map.insert(
                "scratchpad".to_string(),
                json!({
                    "content": self.session.scratchpad,
                    "instruction": "This is your persistent working memory for this session. You wrote it and may update it at any time by calling HOBBES_UPDATE_SCRATCHPAD. Refer to it for key facts that should not be forgotten."
                }),
            );
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

        // Apply system context composition for finite context windows.
        // This compresses/omits low-priority sections to fit the system prompt
        // within ~20% of the provider's context budget.
        let provider_context = self.settings.resolve_context_window();
        Self::compose_system_for_budget(&mut system_context_map, &persona, provider_context, &tuning);

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
        // Track positions of ToolResult messages for Pass 2 budget allocation
        let mut tool_result_positions: Vec<ToolResultPosition> = Vec::new();
        // Pages to store in PageQueue (for both historical stashes and active pagination)
        let mut pages_to_store: Vec<(String, crate::session::PagedResult)> = Vec::new();

        let history_len = tuning.chat_history_length;
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
                        
                        // Why clone here?
                        // We clone `tc.response` explicitly so we can safely perform non-destructive 
                        // truncation, compression (json2markdown), or pagination on the *prompt copy* 
                        // exclusively for the LLM's consumption during this specific turn.
                        // By leaving the historical `SessionState::tc.response` entirely untouched, 
                        // we guarantee the original (potentially massive) tool payload is perpetually 
                        // preserved in memory. This ensures subsequent turns can dynamically re-paginate 
                        // or re-compress the full data as the conversation window shifts.
                        let result_string = tc.response.clone();

                        // Resolve whether to apply compact markdown conversion
                        // (uses `tuning` already resolved at line 176 for the entire build)
                        let use_compact = tuning.compact_tool_results;
                        // Tracks whether Pass 1 TOON condensation already stashed this result.
                        // When true, Pass 2 will skip re-paginating to avoid corrupting the
                        // HOBBES_PAGE_RESULT footer that Pass 1 already embedded.
                        let mut already_condensed_by_pass1 = false;

                        let result_value: serde_json::Value = if use_compact {
                            // Parse to JSON for markdown conversion
                            let mut json_val: serde_json::Value =
                                serde_json::from_str(&result_string).unwrap_or(json!(result_string));
                            // Recursively unwrap stringified JSON (e.g. Composio's result.text)
                            // so TOON can render the full structure efficiently.
                            unwrap_json_strings(&mut json_val);
                            let md = crate::formatters::toon::to_toon(&json_val);

                            if !is_active_tool_call {
                                // Historical result: condense for context, stash full data
                                let budget = tuning.max_tool_output_length;
                                if md.len() > budget {
                                    // Stash full markdown in PageQueue for retrieval
                                    let stash_id = format!(
                                        "hist-{}-{}",
                                        sanitized_tool_name,
                                        &tc.execution_id[..8.min(tc.execution_id.len())]
                                    );
                                    pages_to_store.push((
                                        stash_id.clone(),
                                        crate::session::PagedResult {
                                            remaining_content: md.clone(),
                                            tool_name: sanitized_tool_name.clone(),
                                        },
                                    ));

                                    // Truncate for in-context display
                                    let mut condensed = md;
                                    let trunc_len = floor_char_boundary(&condensed, budget);
                                    condensed.truncate(trunc_len);
                                    condensed.push_str(&format!(
                                        "\n... [Result condensed. Full data available: call HOBBES_PAGE_RESULT with tool_call_id \"{}\"]",
                                        stash_id
                                    ));
                                    tracing::debug!(
                                        "Condensed historical result for {} ({} chars → {} chars, stash_id={})",
                                        sanitized_tool_name, result_string.len(), condensed.len(), stash_id
                                    );
                                    // Mark as already_condensed so Pass 2 doesn't re-paginate
                                    // and corrupt the HOBBES_PAGE_RESULT footer we just added.
                                    already_condensed_by_pass1 = true;
                                    json!(condensed)
                                } else {
                                    // Fits within budget — use full markdown
                                    tracing::debug!(
                                        "Historical result {} converted to markdown ({} chars → {} chars)",
                                        sanitized_tool_name, result_string.len(), md.len()
                                    );
                                    json!(md)
                                }
                            } else {
                                // Active result: full markdown, Pass 2 handles budget
                                tracing::debug!(
                                    "Active result {} converted to markdown ({} chars → {} chars)",
                                    sanitized_tool_name, result_string.len(), md.len()
                                );
                                json!(md)
                            }
                        } else {
                            // compact_tool_results OFF: original JSON behavior.
                            // Oversized results are paginated (not silently truncated) so the
                            // model can fetch the remainder via HOBBES_PAGE_RESULT.
                            let rs = result_string;
                            let max_len = if is_active_tool_call {
                                self.effective_tool_result_limit(&tuning)
                            } else {
                                tuning.max_tool_output_length
                            };

                            if rs.len() > max_len {
                                let split_at = find_split_point(&rs, max_len);
                                if split_at < rs.len() {
                                    // Stash the remainder so the model can page through it.
                                    let stash_id = format!(
                                        "raw-{}-{}",
                                        sanitized_tool_name,
                                        &tc.execution_id[..8.min(tc.execution_id.len())]
                                    );
                                    pages_to_store.push((
                                        stash_id.clone(),
                                        crate::session::PagedResult {
                                            remaining_content: rs[split_at..].to_string(),
                                            tool_name: sanitized_tool_name.clone(),
                                        },
                                    ));
                                    let page1 = format!(
                                        "{}\n\n[Result truncated. Full data available: call HOBBES_PAGE_RESULT with tool_call_id \"{}\"]",
                                        &rs[..split_at], stash_id
                                    );
                                    already_condensed_by_pass1 = true;
                                    tracing::debug!(
                                        "compact=OFF: stashed raw result for '{}' ({} chars, stash_id={})",
                                        sanitized_tool_name, rs.len(), stash_id
                                    );
                                    match serde_json::from_str::<serde_json::Value>(&page1) {
                                        Ok(val) => val,
                                        Err(_) => json!(page1),
                                    }
                                } else {
                                    // split_at == rs.len(): entire content fits at split boundary
                                    match serde_json::from_str::<serde_json::Value>(&rs) {
                                        Ok(val) => val,
                                        Err(_) => json!(rs),
                                    }
                                }
                            } else {
                                match serde_json::from_str::<serde_json::Value>(&rs) {
                                    Ok(val) => val,
                                    Err(_) => json!(rs),
                                }
                            }
                        };

                        messages.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: vec![ContentBlock::ToolResult {
                                call_id: tc.execution_id.to_string(),
                                name: sanitized_tool_name.clone(),
                                content: result_value,
                            }],
                        });

                        // Capture the ToolResult index BEFORE any vision message is pushed
                        // so Pass 2 budget allocation always targets the correct message.
                        let tool_result_msg_idx = messages.len() - 1;

                        // ── Vision injection ─────────────────────────────────────────────
                        // If this tool call produced a screenshot, inject it as a vision
                        // message so the model can see the image on the next turn.
                        // The image was already resized to max 768px and saved as JPEG by
                        // stream_manager::save_tool_image — we just need to load and re-encode.
                        if let Some(ref img_path) = tc.cached_image_path {
                            let fs_path = img_path.strip_prefix("file://").unwrap_or(img_path);
                            match std::fs::read(fs_path) {
                                Ok(bytes) => {
                                    use base64::Engine;
                                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    messages.push(ChatMessage {
                                        role: ChatRole::User,
                                        content: vec![
                                            ContentBlock::Text {
                                                text: "[Vision context from previous tool result — screenshot captured by browser:]".to_string(),
                                            },
                                            ContentBlock::Image {
                                                mime_type: "image/jpeg".to_string(),
                                                data,
                                            },
                                        ],
                                    });
                                    tracing::debug!(
                                        "Injected cached screenshot '{}' as vision context for '{}'",
                                        fs_path, tc.tool_name
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Could not load cached tool image '{}': {}",
                                        fs_path, e
                                    );
                                }
                            }
                        }

                        // Track position for Pass 2 budget allocation.
                        // Uses the pre-captured index so vision messages don't shift it.
                        // The bool signals whether Pass 1 TOON condensation already handled
                        // pagination — if so, Pass 2 skips this entry.
                        tool_result_positions.push(ToolResultPosition {
                            msg_idx: tool_result_msg_idx,
                            tool_name: sanitized_tool_name,
                            execution_id: tc.execution_id.to_string(),
                            is_active: is_active_tool_call,
                            already_condensed: already_condensed_by_pass1,
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

        // ── Pass 2: Dynamic tool result budget & pagination ──
        // For providers with finite context windows, compute per-result budgets
        // and paginate any results that exceed them.


        if !tool_result_positions.is_empty() {
            let system_chars = system.len();
            let tool_def_chars: usize = tools.iter()
                .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
                .sum();

            let non_result_chars: usize = messages.iter()
                .enumerate()
                .filter(|(idx, _)| !tool_result_positions.iter().any(|trp| trp.msg_idx == *idx))
                .map(|(_, m)| m.content.iter().map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::Thinking { text, .. } => text.len(),
                    ContentBlock::ToolCall { name, arguments, .. } => name.len() + arguments.to_string().len(),
                    ContentBlock::ToolResult { content, .. } => content.to_string().len(),
                    _ => 0, // Image, etc.
                }).sum::<usize>())
                .sum();

            let active_idx = tool_result_positions.iter().position(|trp| trp.is_active);

            if let Some(budgets) = Self::compute_tool_result_budget(
                system_chars,
                tool_def_chars,
                non_result_chars,
                tool_result_positions.len(),
                active_idx,
                provider_context,
                &tuning,
            ) {
                // Apply budgets: paginate results that exceed their allocation.
                // Uses find_split_point directly (instead of the old segment_into_pages)
                // so all content beyond page 1 is stored in the PageQueue as a raw
                // string for dynamic re-segmentation at delivery time. This guarantees
                // the model can always fetch content via HOBBES_PAGE_RESULT rather than
                // hitting a silent hard truncation.
                for (pos_idx, trp) in tool_result_positions.iter().enumerate() {
                    let budget_chars = budgets[pos_idx];
                    let ToolResultPosition { msg_idx, tool_name, execution_id, already_condensed, .. } = trp;

                    // Skip entries already condensed by Pass 1 TOON stashing.
                    // Their HOBBES_PAGE_RESULT footer must not be overwritten.
                    if *already_condensed {
                        tracing::debug!(
                            "Pass 2 skipping '{}' — already condensed by Pass 1",
                            tool_name
                        );
                        continue;
                    }

                    if let Some(msg) = messages.get_mut(*msg_idx) {
                        if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first() {
                            // Extract raw content for pagination. If the value is a string
                            // (e.g., TOON markdown from compact_tool_results), use it directly
                            // to avoid JSON-escaping (\n → \\n). For objects/arrays, fall back
                            // to JSON serialization which preserves structure.
                            let serialized = content.as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string()));

                            if serialized.len() > budget_chars {
                                // Find where page 1 ends using a single smart split.
                                // Store everything after it as a raw string so delivery
                                // can re-split dynamically at the budget of each future turn.
                                let split_at = find_split_point(&serialized, budget_chars);

                                if split_at < serialized.len() {
                                    let short_suffix: String = execution_id.chars()
                                        .filter(|c| c.is_alphanumeric())
                                        .take(6)
                                        .collect();
                                    let tool_call_id = format!("page-{}-{}", tool_name, short_suffix);
                                    pages_to_store.push((tool_call_id.clone(), crate::session::PagedResult {
                                        remaining_content: serialized[split_at..].to_string(),
                                        tool_name: tool_name.clone(),
                                    }));
                                    let page1_with_footer = format!(
                                        "{}\n\n[More content available. To view the next page, use the HOBBES_PAGE_RESULT tool with tool_call_id \"{}\"]",
                                        &serialized[..split_at], tool_call_id
                                    );
                                    tracing::info!(
                                        "Pass 2: paginated '{}' ({} bytes → {} chars budget, id={})",
                                        tool_name, serialized.len(), budget_chars, tool_call_id
                                    );
                                    if let Some(ContentBlock::ToolResult { content, .. }) = msg.content.first_mut() {
                                        *content = json!(page1_with_footer);
                                    }
                                }
                                // else: split_at == serialized.len() means find_split_point
                                // found no good boundary and returned the full length —
                                // content fits as-is, no truncation needed.
                            }
                        }
                    }
                }
            }
            // else: compute_tool_result_budget returned None → no context limit, skip
        }

        // Inject HOBBES_PAGE_RESULT tool only when paginated results exist.
        // This prevents the model from speculatively calling it with fabricated IDs.
        if !pages_to_store.is_empty() {
            tools.push(ToolDefinition {
                name: "HOBBES_PAGE_RESULT".to_string(),
                description: "Fetch the next page of a paginated tool result. Use the exact tool_call_id from the pagination footer.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool_call_id": {
                            "type": "string",
                            "description": "The exact tool_call_id from the [Page X/Y] footer"
                        }
                    },
                    "required": ["tool_call_id"]
                }),
                server_name: "hobbes-local-meta".to_string(),
            });
            tracing::info!(
                "Injected HOBBES_PAGE_RESULT tool — {} paginated result(s) available",
                pages_to_store.len()
            );
        }

        // Inject HOBBES_UPDATE_SCRATCHPAD — always present so the AI can
        // save key facts to its persistent session memory at any time.
        // This is a Tier 1 meta-tool; its content is never compressed or trimmed.
        tools.push(ToolDefinition {
            name: "HOBBES_UPDATE_SCRATCHPAD".to_string(),
            description: "Write important facts, decisions, or discoveries to your persistent session scratchpad. The scratchpad survives context compression and history scrolling. OVERWRITE: include all information you want to retain — anything not in this call is lost.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The full new scratchpad content. Be concise but complete — include any facts from the previous scratchpad you want to keep."
                    }
                },
                "required": ["content"]
            }),
            server_name: "hobbes-local-meta".to_string(),
        });

        // For small models (Qwen 3.5, etc.), re-injected <thinking> tags from
        // previous turns can consume 30-50% of the context budget. We keep
        // thinking only on the most recent assistant message and strip it
        // from all earlier turns.
        if provider_context.is_some() {
            Self::strip_historical_thinking(&mut messages);
        }

        PromptBuildResult {
            prompt: NeutralLlmPrompt {
                system: Some(system),
                messages,
                tools,
            },
            pages_to_store,
        }
    }

    /// Strip `ContentBlock::Thinking` blocks from all assistant messages except
    /// the most recent one. This dramatically reduces context usage for models
    /// with finite context windows — thinking content from historical turns is
    /// the single largest contributor to context exhaustion on small models.
    ///
    /// Walks messages in reverse: the first `ChatRole::Assistant` message encountered
    /// keeps its thinking blocks; all earlier assistant messages have them removed.
    fn strip_historical_thinking(messages: &mut [ChatMessage]) {
        let mut found_latest = false;
        for msg in messages.iter_mut().rev() {
            if msg.role == ChatRole::Assistant {
                if found_latest {
                    // Strip thinking from older assistant messages
                    let before = msg.content.len();
                    msg.content.retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
                    if msg.content.len() < before {
                        tracing::debug!(
                            "Stripped {} thinking block(s) from historical assistant message",
                            before - msg.content.len()
                        );
                    }
                } else {
                    found_latest = true;
                    // Keep thinking on the most recent assistant message
                }
            }
        }
    }

    /// Compute per-tool-result budgets for fitting results within the context window.
    /// Returns `None` if the provider has unlimited context.
    ///
    /// Budget split: the active tool result receives 60% of the remaining budget,
    /// historical results share the remaining 40% equally.
    fn compute_tool_result_budget(
        system_chars: usize,
        tool_def_chars: usize,
        non_result_message_chars: usize,
        num_tool_results: usize,
        active_index: Option<usize>,
        max_context_tokens: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
    ) -> Option<Vec<usize>> {
        let max_tokens = max_context_tokens?;

        // Convert tokens to chars using the configurable ratio
        let total_chars = (max_tokens as f64 * tuning.chars_per_token * (1.0 - tuning.context_safety_margin)) as usize;
        let overhead = system_chars + tool_def_chars + non_result_message_chars;

        if overhead >= total_chars {
            // No room for tool results at all
            return Some(vec![1024; num_tool_results]); // minimal fallback
        }

        let remaining = total_chars - overhead;

        if num_tool_results == 0 {
            return Some(vec![]);
        }

        let mut budgets = Vec::with_capacity(num_tool_results);

        if let Some(active_idx) = active_index {
            let active_budget = (remaining as f64 * tuning.active_result_budget_ratio) as usize;
            let historical_count = num_tool_results - 1;
            let historical_per = if historical_count > 0 {
                ((remaining as f64 * (1.0 - tuning.active_result_budget_ratio)) / historical_count as f64) as usize
            } else {
                0
            };

            for i in 0..num_tool_results {
                if i == active_idx {
                    budgets.push(active_budget);
                } else {
                    budgets.push(historical_per);
                }
            }
        } else {
            // No active result identified → split equally
            let per_result = remaining / num_tool_results;
            budgets.resize(num_tool_results, per_result);
        }

        tracing::debug!(
            "Tool result budgets: {} chars remaining, {} results, active_idx={:?}, budgets={:?}",
            remaining, num_tool_results, active_index, budgets
        );

        Some(budgets)
    }

    /// Split `content` into a `Vec<String>` of pages that each fit within `page_size` chars.
    ///
    /// **Note:** Production code no longer calls this function — Pass 2 uses
    /// `crate::str_utils::find_split_point` directly so page 1 is served inline and
    /// the remainder is stored in the `PageQueue` for dynamic re-segmentation at
    /// delivery time. This function is preserved for unit-test coverage only.
    ///
    /// The split strategy (JSON boundaries → paragraph → line → char fallback) matches
    /// `str_utils::find_split_point` exactly because it delegates to it.
    #[cfg(test)]
    fn segment_into_pages(content: &str, page_size: usize) -> Vec<String> {
        if content.len() <= page_size {
            return vec![content.to_string()];
        }

        let mut pages = Vec::new();
        let mut remaining = content;

        while !remaining.is_empty() {
            if remaining.len() <= page_size {
                pages.push(remaining.to_string());
                break;
            }
            let split_at = crate::str_utils::find_split_point(remaining, page_size);
            pages.push(remaining[..split_at].to_string());
            remaining = &remaining[split_at..];
        }

        pages
    }

    /// Calculate a context-aware cap for tool result length.
    /// For providers with finite context windows (OpenAI-compat, Claude),
    /// caps to ~30% of the context budget (in chars, assuming ~4 chars/token).
    /// For Gemini or unconfigured providers, falls back to the resolved setting.
    fn effective_tool_result_limit(&self, tuning: &crate::settings::ResolvedContextTuning) -> usize {
        let user_max = tuning.max_active_tool_output_length;

        let provider_context_tokens = self.settings.resolve_context_window();

        if let Some(max_tokens) = provider_context_tokens {
            let ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(tuning.tool_result_budget_ratio);
            let context_cap = (max_tokens as f64 * ratio * tuning.chars_per_token) as usize;
            let effective = context_cap.min(user_max);
            tracing::debug!(
                "Tool result limit: {} chars (ratio: {:.0}%, provider context: {} tokens, user max: {})",
                effective, ratio * 100.0, max_tokens, user_max
            );
            effective
        } else {
            user_max
        }
    }

    /// Compress system context map to fit within a budget when the provider has
    /// a finite context window. Uses a 4-tier priority system:
    /// 1. Core (never omit): system_persona, user_name, current_time, scratchpad
    /// 2. Skill (compress before omit): loaded_skills instruction_manuals
    /// 3. Context (trim): conversation_summary, entities
    /// 4. Enrichment (omit first): composio_context, mcp_servers, user_instruction
    fn compose_system_for_budget(
        system_context_map: &mut serde_json::Map<String, serde_json::Value>,
        persona: &str,
        max_context_tokens: Option<usize>,
        tuning: &crate::settings::ResolvedContextTuning,
    ) {
        let Some(max_tokens) = max_context_tokens else { return };

        // System prompt budget is based on tuning configuration
        let system_budget_chars = (max_tokens as f64 * tuning.system_prompt_budget_ratio * tuning.chars_per_token) as usize;

        // Serialize once; track size delta incrementally (avoids cloning the map per check)
        let map_size = serde_json::to_string(&serde_json::Value::Object(system_context_map.clone()))
            .map(|s| s.len())
            .unwrap_or(0);
        let mut running_size = persona.len() + map_size;

        if running_size <= system_budget_chars { return; }

        // Warn if the scratchpad alone consumes a large share of the system budget.
        // The scratchpad is Tier 1 protected — we never remove it — but this log
        // helps diagnose why system context is being aggressively compressed.
        if let Some(scratch) = system_context_map.get("scratchpad") {
            let scratch_chars = serde_json::to_string(scratch).map(|s| s.len()).unwrap_or(0);
            if scratch_chars > system_budget_chars / 2 {
                tracing::warn!(
                    "Scratchpad ({} chars) exceeds 50% of system budget ({} chars). \
                     Consider calling HOBBES_UPDATE_SCRATCHPAD to condense it.",
                    scratch_chars, system_budget_chars
                );
            }
        }

        tracing::info!(
            "System context composition: {} chars vs {} budget ({}K model). Compressing.",
            running_size, system_budget_chars, max_tokens / 1000
        );

        // Helper: measure serialized size of a single value (for delta tracking)
        let value_size = |v: &serde_json::Value| -> usize {
            serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
        };

        // Tier 1 (protected — never touched): system_persona, user_name, current_time, scratchpad
        // These keys are NEVER removed or modified by any compression tier.
        // scratchpad is the AI's persistent session memory — immune to all compression.

        // Tier 4: Drop enrichment sections
        for key in ["composio_context", "mcp_servers", "user_instruction"] {
            if running_size <= system_budget_chars { return; }
            if let Some(removed) = system_context_map.remove(key) {
                // Account for key + value + quotes/colon/comma overhead
                let delta = key.len() + value_size(&removed) + 6;
                running_size = running_size.saturating_sub(delta);
                tracing::debug!("Context composition: dropped '{}' (-{} chars)", key, delta);
            }
        }

        // Tier 3: Truncate conversation summary
        if running_size > system_budget_chars {
            if let Some(summary) = system_context_map.get_mut("conversation_summary")
                .and_then(|v| v.as_object_mut())
            {
                let target = system_budget_chars / 6;
                if let Some(s) = summary.get("summary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                {
                    if s.len() > target {
                        let trunc = floor_char_boundary(&s, target);
                        let truncated = format!("{}... [truncated for context budget]", &s[..trunc]);
                        let delta = s.len().saturating_sub(truncated.len());
                        summary.insert("summary".to_string(), json!(truncated));
                        running_size = running_size.saturating_sub(delta);
                        tracing::debug!("Context composition: truncated summary (-{} chars)", delta);
                    }
                }
                // Aggressively prune entities for small models
                if let Some(entities) = summary.get_mut("entities")
                    .and_then(|v| v.as_object_mut())
                {
                    let keep_keys: Vec<String> = entities.keys().take(5).cloned().collect();
                    let before = entities.len();
                    let size_before = value_size(&serde_json::Value::Object(entities.clone()));
                    entities.retain(|k, _| keep_keys.contains(k) || k == "user_name");
                    if entities.len() < before {
                        let size_after = value_size(&serde_json::Value::Object(entities.clone()));
                        running_size = running_size.saturating_sub(size_before.saturating_sub(size_after));
                        tracing::debug!("Context composition: pruned entities from {} to {}", before, entities.len());
                    }
                }
            }
        }

        // Tier 2: Compress skill instruction_manual (keep resolved_tools intact)
        if running_size > system_budget_chars {
            if let Some(skills) = system_context_map.get_mut("loaded_skills")
                .and_then(|v| v.as_array_mut())
            {
                let per_skill_budget = system_budget_chars / (4 * skills.len().max(1));
                for skill in skills.iter_mut() {
                    if let Some(manual) = skill.get("instruction_manual")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                    {
                        if manual.len() > per_skill_budget {
                            let trunc = floor_char_boundary(&manual, per_skill_budget);
                            let truncated = format!(
                                "{}... [instruction truncated from {} chars to fit context]",
                                &manual[..trunc], manual.len()
                            );
                            let delta = manual.len().saturating_sub(truncated.len());
                            if let Some(obj) = skill.as_object_mut() {
                                obj.insert("instruction_manual".to_string(), json!(truncated));
                            }
                            running_size = running_size.saturating_sub(delta);
                            tracing::debug!(
                                "Context composition: truncated skill instruction from {} to {} chars",
                                manual.len(), trunc
                            );
                        }
                    }
                }
            }
        }

        tracing::info!(
            "System context composition complete: ~{} chars (budget: {})",
            running_size, system_budget_chars
        );
    }
}
