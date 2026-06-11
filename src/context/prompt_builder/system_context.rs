use crate::components::shared::MessageContent;
use crate::llm::types::ToolDefinition;
use chrono::Local;
use serde_json::json;
use super::PromptBuilder;

/// Result of system context assembly.
pub(crate) struct SystemContextResult {
    /// The complete system instruction string (persona + SYSTEM_CONTEXT XML block).
    pub system: String,
    /// The tools list, potentially filtered by active skill scope.
    pub tools: Vec<ToolDefinition>,
    /// The resolved context tuning for this prompt build.
    pub tuning: crate::settings::ResolvedContextTuning,
    /// The persona string (extracted for use by context_compression).
    pub provider_context: Option<usize>,
    /// Whether the last message is a continuation placeholder.
    pub is_continuation_placeholder: bool,
}

impl<'a> PromptBuilder<'a> {
    /// Build the system instruction and tool list from session context.
    ///
    /// This assembles the persona, injects contextual metadata (Composio, skills,
    /// scratchpad, entities, MCP servers, user name), applies skill-scoped tool
    /// filtering, and compresses the system context for finite context windows.
    pub(crate) fn build_system_context(&self) -> SystemContextResult {
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
        // (session provider override → global settings)
        let tuning = self.effective_tuning();

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

        // Build the user message from the last message if it's a user text message
        let user_message_is_empty = {
            // We check this indirectly — the caller passes user_message,
            // but we need to detect continuation/recovery mode here.
            // Use the placeholder detection as a signal.
            is_continuation_placeholder || last_message.is_some_and(|m| {
                matches!(&m.content, MessageContent::ToolCall(_))
            })
        };

        if user_message_is_empty {
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
        let provider_context = self.effective_context_window();
        Self::compose_system_for_budget(&mut system_context_map, &persona, provider_context, &tuning);

        let context_json = serde_json::to_string_pretty(&system_context_map).unwrap_or_default();

        let mut system = persona;
        if !context_json.is_empty() && context_json != "{}" {
            system.push_str(&format!(
                "\n\n<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>",
                context_json
            ));
        }

        SystemContextResult {
            system,
            tools,
            tuning,
            provider_context,
            is_continuation_placeholder,
        }
    }
}
