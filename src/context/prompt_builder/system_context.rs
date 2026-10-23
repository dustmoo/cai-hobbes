use crate::components::shared::MessageContent;
use crate::llm::types::ToolDefinition;
use serde_json::json;
use super::PromptBuilder;

/// Result of system context assembly.
pub(crate) struct SystemContextResult {
    /// The system instruction string: persona + `<SYSTEM_CONTEXT>`. It holds
    /// only stable content — everything that changes during a session goes
    /// through the turn context ledger (`ledger.rs`) as messages.
    pub system: String,
    /// The composed per-session live context (scratchpad, planner_today,
    /// loaded_skills, conversation_summary, tool snapshots, other extras),
    /// keyed like the old `<TURN_CONTEXT>` block. The ledger decides which of
    /// these reach the model and when.
    pub volatile: serde_json::Map<String, serde_json::Value>,
    /// Per-request turn instructions (TOOL COMPLETION / CONTINUATION /
    /// RECOVERY), without leading blank lines. Rendered into the request tail.
    pub turn_instructions: String,
    /// The tools list, potentially filtered by active skill scope.
    pub tools: Vec<ToolDefinition>,
    /// The resolved context tuning for this prompt build.
    pub tuning: crate::settings::ResolvedContextTuning,
    /// The persona string (extracted for use by context_compression).
    pub provider_context: Option<usize>,
    /// Whether the last message is a continuation placeholder.
    pub is_continuation_placeholder: bool,
}

/// Default of `Settings.skill_idle_turns` (user turns a loaded skill may go
/// unused before its full manual is replaced by the idle stub); used by tests.
#[cfg(test)]
pub(crate) const SKILL_IDLE_USER_TURNS: usize = 6;

/// Loaded skills that have gone `idle_after_user_turns` user turns without use.
///
/// "Use" is the latest message that either calls one of the skill's resolved
/// tools (same server-prefix match as the turn-active check), re-invokes the
/// skill by name (`HOBBES_INVOKE_SKILL`), or is the user's `/skill` run of it.
/// Idle = that many user text messages have followed the last use. A skill with
/// no use found in history is treated as fresh, never idle — we don't silently
/// drop a manual we can't account for.
///
/// `idle_after_user_turns == 0` disables stubbing entirely.
///
/// Pure over `messages` + `loaded_skills`, so session replay reproduces the
/// same decision the live session made.
pub(crate) fn idle_loaded_skills(
    messages: &[crate::components::chat::Message],
    loaded_skills: &std::collections::HashMap<String, String>,
    idle_after_user_turns: usize,
) -> std::collections::HashSet<String> {
    if idle_after_user_turns == 0 {
        return std::collections::HashSet::new();
    }
    loaded_skills
        .iter()
        .filter_map(|(name, payload_json)| {
            let payload = serde_json::from_str::<
                crate::components::shared::CapabilityContextPayload,
            >(payload_json)
            .ok()?;
            let last_use = messages
                .iter()
                .rposition(|m| message_uses_skill(m, name, &payload))?;
            let user_turns_since = messages[last_use + 1..]
                .iter()
                .filter(|m| {
                    m.author == "User" && matches!(m.content, MessageContent::Text { .. })
                })
                .count();
            (user_turns_since >= idle_after_user_turns).then(|| name.clone())
        })
        .collect()
}

fn message_uses_skill(
    message: &crate::components::chat::Message,
    skill_name: &str,
    payload: &crate::components::shared::CapabilityContextPayload,
) -> bool {
    match &message.content {
        MessageContent::ToolCall(tc) if tc.tool_name == "HOBBES_INVOKE_SKILL" => {
            serde_json::from_str::<serde_json::Value>(&tc.arguments)
                .ok()
                .and_then(|a| a.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .as_deref()
                == Some(skill_name)
        }
        MessageContent::ToolCall(tc) => {
            !tc.server_name.is_empty()
                && payload
                    .resolved_tools
                    .values()
                    .any(|tool_str| tool_str.contains(tc.server_name.as_str()))
        }
        MessageContent::SkillCall(sc) => sc.skill_name == skill_name,
        _ => false,
    }
}

/// Whether a registry skill belongs in the `available_skills` advert.
///
/// - Not loaded: advertised unless the user explicitly turned it off (`false`).
///   Unlisted skills stay advertised — the model may ask, and the user answers
///   the approval bubble.
/// - Loaded and fresh: already in context, not advertised.
/// - Loaded but idle-stubbed: advertised so the model can reload the manual
///   (a loaded skill needs no fresh approval to be re-invoked).
pub(crate) fn skill_is_advertised(
    name: &str,
    loaded: bool,
    idle: bool,
    permissions: &crate::context::permissions::PermissionSettings,
) -> bool {
    if loaded {
        return idle;
    }
    permissions.skill_permissions.get(name) != Some(&false)
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
            tools = mcp_context
                .servers
                .iter()
                .flat_map(|server| {
                    server
                        .tools
                        .iter()
                        .map(|tool| ToolDefinition::from_mcp(tool, &server.name))
                })
                .collect();
        }
        // Agent time: HOBBES_AGENT_TIME exists only in the leased agent-time
        // session. Stripped from every other prompt (whatever the synced MCP
        // context holds), added for that one. Added before the sort, so its
        // place in the cached tool prefix is stable for the whole run.
        tools.retain(|t: &ToolDefinition| {
            !crate::components::builtin_tools::is_agent_time_tool(&t.name)
        });
        // Release gate: with Agent time / Decisions hidden, neither tool is
        // offered anywhere — not even to a (stale) leased session.
        let agent_time_on = self.settings.agent_time_enabled();
        if !agent_time_on {
            tools.retain(|t: &ToolDefinition| {
                !crate::components::builtin_tools::is_experimental_agent_tool(&t.name)
            });
        }
        if self.agent_time_tools && agent_time_on {
            tools.extend(
                crate::mcp::planner_client::PlannerClient::agent_time_tools()
                    .iter()
                    .map(|tool| {
                        ToolDefinition::from_mcp(tool, crate::mcp::manager::HOBBES_PLANNER_SERVER)
                    }),
            );
        }
        let mut tools = sort_and_dedupe_tools(tools);
        // HOBBES_DISPATCH names the models the user allows for a chat worker.
        // Derived from settings only (sorted), so the cached prefix holds.
        crate::mcp::planner_client::annotate_dispatch_models(&mut tools, self.settings);

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
            persona = format!("{}\n\nTOOL POLICY: {}", persona, instruction);
        }

        // Instructions that depend on how the conversation currently ends. They
        // are per-request, so they render in the request tail (see
        // `ledger.rs`), never inside the persona (the stable, cacheable part).
        let mut turn_instructions = String::new();

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
            // A conversation ending on the assistant's own prose is a
            // continuation too (the instruction used to ride on a separate
            // nudge message; the request tail carries it now).
            is_continuation_placeholder || last_message.is_some_and(|m| {
                matches!(&m.content, MessageContent::ToolCall(_))
                    || (m.author == "Hobbes" && matches!(&m.content, MessageContent::Text { .. }))
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
                let tool_completion_instruction = "\n\nTOOL COMPLETION INSTRUCTION: The tool results are above. If finishing the user's request needs more tool calls, make them now; otherwise answer from the results. Do not ask the user for the tool output again. When reporting specific values from tool outputs (like dates, IDs, or file paths), present them exactly as returned. Do not transform, reformat, or convert them (e.g. date conversion) unless explicitly requested by the user.";
                turn_instructions.push_str(tool_completion_instruction);
            } else {
                let continuation_instruction = "\n\nCONTINUATION INSTRUCTION: You were the last one to speak. The user has not replied. Continue the conversation based on the existing context. Do not repeat yourself. Provide new information or ask a clarifying question.";
                turn_instructions.push_str(continuation_instruction);
            }
        }

        // Only the round the model is about to respond to matters: once a later
        // round succeeds, an earlier failure is already handled.
        let failed_tools = latest_round_failed_tools(&self.session.messages);
        if !failed_tools.is_empty() {
            turn_instructions.push_str(&format!(
                "\n\nRECOVERY NOTE: The last tool call(s) failed ({}). Read the error, then try a different approach rather than repeating the identical call.",
                failed_tools.join(", ")
            ));
        }

        active_context.system_persona = Some(persona);

        // Extract MCP server info BEFORE nulling mcp_tools - so LLM knows what each server is
        // Sorted by name: `McpManager.servers` is a HashMap, and this array sits
        // in the stable (cacheable) part of the system prompt.
        let mcp_servers_info: Option<Vec<serde_json::Value>> =
            active_context.mcp_tools.as_ref().map(|ctx| {
                let mut servers: Vec<_> = ctx.servers.iter().collect();
                servers.sort_by(|a, b| a.name.cmp(&b.name));
                servers
                    .into_iter()
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

        // How the live context reaches the model (see `ledger.rs`). Static
        // text, so it lives in the stable part.
        system_context_map.insert(
            "context_protocol".to_string(),
            json!(CONTEXT_PROTOCOL_NOTE),
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
                        let tool_mappings = sorted_tool_mappings(&payload.resolved_tools);

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
        //
        // The turn-active set is also reused further down to gate `available_skills`:
        // while a skill is mid-execution, no other skills are advertised.
        let mut turn_active_skill_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Loaded skills currently shown as idle stubs; also drives re-advertising
        // them below so the model can reload the manual.
        let mut idle_skill_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
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
            turn_active_skill_names = self
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
            // Every loaded skill carries its full manual unless it has gone
            // idle for `settings.skill_idle_turns` user turns (see
            // `idle_loaded_skills`), so a one-off load stops costing its manual
            // on later turns. The value reaches the model through the context
            // ledger, which appends it once per change: a per-request
            // turn-active stub/manual toggle would re-append the manual each
            // time recent tool use shifted, so it is not applied here.
            idle_skill_names = idle_loaded_skills(
                &self.session.messages,
                &self.session.loaded_skills,
                self.settings.skill_idle_turns,
            );
            let mut skills_array: Vec<serde_json::Value> = self
                .session
                .loaded_skills
                .iter()
                .filter_map(|(skill_name, payload_json)| {
                    let payload = serde_json::from_str::<
                        crate::components::shared::CapabilityContextPayload,
                    >(payload_json)
                    .ok()?;
                    let tool_mappings = sorted_tool_mappings(&payload.resolved_tools);

                    if idle_skill_names.contains(skill_name) {
                        tracing::debug!(
                            "Loaded skill '{}': idle stub (unused for {}+ user turns)",
                            skill_name, self.settings.skill_idle_turns
                        );
                        return Some(json!({
                            "name": skill_name,
                            "resolved_tools": tool_mappings,
                            "note": format!(
                                "Skill idle — its full manual was dropped after {} user turns without use. Call HOBBES_INVOKE_SKILL with this name to reload the full manual, or HOBBES_UNLOAD_SKILL to drop it.",
                                self.settings.skill_idle_turns
                            ),
                        }));
                    }

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
                })
                .collect();

            // `loaded_skills` is a HashMap; order the array by skill name.
            skills_array.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if !skills_array.is_empty() {
                system_context_map.insert(
                    "loaded_skills".to_string(),
                    serde_json::Value::Array(skills_array),
                );
                tracing::debug!(
                    "Loaded skills injected: {} total, {} turn-active, {} idle",
                    self.session.loaded_skills.len(),
                    turn_active_skill_names.len(),
                    idle_skill_names.len()
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

        // Inject today's plan so the model sees the day without calling a tool
        // for it. Prebuilt at the call site (todo::handlers::planner_today_context)
        // and already hard-capped there; None when the planner or the injection
        // is disabled. Tier 4 in context_compression — the last enrichment
        // dropped under budget pressure.
        if let Some(planner_today) = &self.planner_today {
            system_context_map.insert("planner_today".to_string(), planner_today.clone());
        }

        // When the planner is disabled its tools must disappear from the prompt
        // entirely, not just fail on call — same precedent as the
        // HOBBES_INVOKE_SKILL withholding below.
        if !self.settings.planner_enabled {
            tools.retain(|t| !crate::components::builtin_tools::is_planner_tool(&t.name));
        }

        // Fleet tools follow the fleet's own gate (not the planner's).
        if !(self.settings.fleet_enabled && crate::entitlement::pro_active()) {
            tools.retain(|t| !crate::components::builtin_tools::is_fleet_tool(&t.name));
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
                tracing::debug!(
                    "Skill-scoped tool filter (last turn): {} → {} tools (skill resolved: {:?})",
                    before,
                    tools.len(),
                    skill_tools
                );
            }
        }

        // Advertise invocable skills so the model can load one on its own —
        // the same on-demand mechanic as MCP_LOAD_SERVER_TOOLS: compact metadata
        // in the system context plus a loader tool; the full instruction manual
        // is only injected once a skill is actually loaded.
        //
        // Gated on skill execution: while a skill is running this turn (a skill
        // call is the current turn, a loaded skill's tools were used recently,
        // or the model just loaded one), no other skills are advertised and the
        // loader tool is withheld entirely. The only skill in context is the one
        // being run — advertising alternatives mid-skill derails small local
        // models into re-execution loops.
        {
            let skill_running = skill_tool_names.is_some()
                || !turn_active_skill_names.is_empty()
                || last_meaningful_msg.is_some_and(|m| {
                    matches!(
                        &m.content,
                        MessageContent::ToolCall(tc) if tc.tool_name == "HOBBES_INVOKE_SKILL"
                    )
                });
            let available: Vec<serde_json::Value> = if skill_running {
                Vec::new()
            } else {
                crate::skills::registry::available_skills_snapshot()
                    .into_iter()
                    .filter(|s| !s.disable_model_invocation)
                    .filter(|s| {
                        skill_is_advertised(
                            &s.name,
                            self.session.loaded_skills.contains_key(&s.name),
                            idle_skill_names.contains(&s.name),
                            &self.settings.permission_settings,
                        )
                    })
                    .map(|s| {
                        let mut entry = json!({
                            "name": s.name,
                            "description": s.description,
                        });
                        if let Some(hint) = s.argument_hint {
                            entry["argument_hint"] = json!(hint);
                        }
                        entry
                    })
                    .collect()
            };
            if available.is_empty() {
                // Nothing to advertise this turn — withhold the loader tool the
                // same way an on-demand server withholds its tools until loaded.
                tools.retain(|t| t.name != "HOBBES_INVOKE_SKILL");
                if skill_running {
                    tracing::debug!(
                        "Skill running this turn — available_skills and HOBBES_INVOKE_SKILL withheld"
                    );
                }
            } else {
                system_context_map.insert(
                    "available_skills".to_string(),
                    json!({
                        "skills": available,
                        "instruction": "On-demand skills. Load one by calling the HOBBES_INVOKE_SKILL tool with {\"name\": \"<skill name>\", \"arguments\": \"<optional args>\"}; it returns the skill's full instruction manual. Only load a skill that clearly matches the user's current request."
                    }),
                );
            }
        }

        // Nothing to unload → don't spend a tool definition on it.
        if self.session.loaded_skills.is_empty() {
            tools.retain(|t| t.name != "HOBBES_UNLOAD_SKILL");
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
                    if crate::log_utils::first_time(&format!("oversized_entity:{}", key)) {
                        tracing::warn!("Stripped oversized entity '{}' from SYSTEM_CONTEXT", key);
                    } else {
                        tracing::debug!("Stripped oversized entity '{}' from SYSTEM_CONTEXT", key);
                    }
                }
            }
        }

        // Apply system context composition for finite context windows.
        // This compresses/omits low-priority sections to fit the system prompt
        // within ~20% of the provider's context budget.
        let provider_context = self.effective_context_window();
        // The budget runs over the combined map, before the stable/live
        // split, so its priority tiers apply across both parts; the free text
        // (persona + turn instructions + the tail header) counts too.
        Self::compose_system_for_budget(
            &mut system_context_map,
            persona.len() + turn_instructions.len() + super::ledger::TAIL_HEADER.len(),
            provider_context,
            &tuning,
        );

        // Layout: <persona>\n\n<SYSTEM_CONTEXT>{stable keys}</SYSTEM_CONTEXT>.
        // The live keys go to the context ledger (`ledger.rs`) and the turn
        // instructions to the request tail, both as messages, so the system
        // string is byte-stable across the session.
        let (stable_map, volatile) = split_context_map(system_context_map);

        let mut system = persona;
        if let Some(block) = render_context_block("SYSTEM_CONTEXT", None, stable_map) {
            system.push_str("\n\n");
            system.push_str(&block);
        }

        SystemContextResult {
            system,
            volatile,
            turn_instructions: turn_instructions.trim_start().to_string(),
            tools,
            tuning,
            provider_context,
            is_continuation_placeholder,
        }
    }
}

/// `SYSTEM_CONTEXT` keys that belong in the stable (prompt-cacheable) part of
/// the system prompt. A key is stable only if it changes at most a handful of
/// times per session and only on an explicit event (settings edit, MCP server
/// (un)load, Composio profile switch, `/skill` run, skill catalog change).
/// Every other key — including unknown `ActiveContext.extra` keys — goes to
/// the context ledger as its own key, so a new key defaults to the safe side
/// (recorded when it changes, at a user-turn boundary).
const STABLE_CONTEXT_KEYS: &[&str] = &[
    "user_name",
    "user_instruction",
    "force_tool_use_instruction",
    "tools",
    "mcp_servers",
    "composio_context",
    "active_skill",
    "available_skills",
    "context_protocol",
];

/// Explains the ledger and tail messages to the model (stable).
const CONTEXT_PROTOCOL_NOTE: &str = "Live state is not in this block. It arrives inside the conversation with the same authority as SYSTEM_CONTEXT: a message starting with [Context update: <key>] gives the current value of that key (working memory, today's plan, loaded skills) and replaces any earlier value of the same key; [History trimmed] marks omitted earlier messages and restates the current values; [Turn context] carries the current time and instructions for that request only.";

/// Recursively sort every JSON object's keys. The app's `serde_json` has
/// `preserve_order` enabled (pulled in transitively), so a `Map` keeps
/// insertion order — and objects built from a `HashMap` (`ActiveContext.extra`,
/// summary entities) would iterate in a per-process random order. Sorting
/// makes the rendered bytes a pure function of the content.
pub(crate) fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, mut v) in entries {
                canonicalize_json(&mut v);
                map.insert(k, v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(canonicalize_json),
        _ => {}
    }
}

/// Sort `{capability, use_tool}` mappings built from a `HashMap` (a skill
/// payload's `resolved_tools` is re-deserialized — with a fresh random hasher —
/// on every build, so its iteration order differs build to build).
fn sorted_tool_mappings(
    resolved_tools: &std::collections::HashMap<String, String>,
) -> Vec<serde_json::Value> {
    let mut pairs: Vec<(&String, &String)> = resolved_tools.iter().collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(capability, tool)| json!({"capability": capability, "use_tool": tool}))
        .collect()
}

/// Render a context map as a tagged pretty-JSON block (empty map → `None`).
pub(crate) fn render_context_block(
    tag: &str,
    header: Option<&str>,
    mut map: serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let mut value = serde_json::Value::Object(std::mem::take(&mut map));
    canonicalize_json(&mut value);
    let json = serde_json::to_string_pretty(&value).unwrap_or_default();
    let header = header.map(|h| format!("{h}\n")).unwrap_or_default();
    Some(format!("<{tag}>\n{header}{json}\n</{tag}>"))
}

/// Split the composed context map into (stable, volatile) by
/// `STABLE_CONTEXT_KEYS`.
fn split_context_map(
    map: serde_json::Map<String, serde_json::Value>,
) -> (
    serde_json::Map<String, serde_json::Value>,
    serde_json::Map<String, serde_json::Value>,
) {
    let mut stable = serde_json::Map::new();
    let mut volatile = serde_json::Map::new();
    for (k, v) in map {
        if STABLE_CONTEXT_KEYS.contains(&k.as_str()) {
            stable.insert(k, v);
        } else {
            volatile.insert(k, v);
        }
    }
    (stable, volatile)
}

/// Order tools by provider-facing wire name (ties broken by server name), then
/// drop wire-name duplicates. Tools render first in every request, so their
/// order must not follow `McpManager.servers` (a `HashMap`) — a reorder would
/// miss the provider's prompt cache for the whole request. Sorting before the
/// dedupe also makes the kept duplicate deterministic.
fn sort_and_dedupe_tools(mut tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    tools.sort_by_cached_key(|t| {
        (
            crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name),
            t.server_name.clone(),
        )
    });
    dedupe_by_wire_name(tools)
}

/// Drop tools whose provider-facing name repeats an earlier one, keeping the
/// first. Every connector names a tool `get_prefixed_tool_name(server, tool)`,
/// which folds all `composio-native:{profile}` servers into one
/// `composio-native_` prefix; if two profiles' Composio servers ever reach one
/// context, their meta-tools collide and the provider rejects the whole
/// request (Gemini: "Duplicate function declaration found").
fn dedupe_by_wire_name(tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    let mut seen = std::collections::HashSet::new();
    tools
        .into_iter()
        .filter(|t| {
            let wire = crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);
            let first = seen.insert(wire.clone());
            if !first {
                if crate::log_utils::first_time(&format!("duplicate_tool:{}", wire)) {
                    tracing::warn!(
                        tool = %wire,
                        server = %t.server_name,
                        "Dropping duplicate tool declaration (same provider-facing name)"
                    );
                } else {
                    tracing::debug!(tool = %wire, "Dropping duplicate tool declaration");
                }
            }
            first
        })
        .collect()
}

/// Names of the tools that errored in the latest tool round — the trailing
/// tool-call messages (ignoring an empty continuation placeholder) that share
/// the last call's `round_id`, i.e. the results the model is about to respond
/// to. Empty when the conversation doesn't end in a tool round or every call
/// in it succeeded.
fn latest_round_failed_tools(messages: &[crate::components::chat::Message]) -> Vec<String> {
    let mut failed = Vec::new();
    let mut round: Option<&Option<String>> = None;
    for m in messages.iter().rev() {
        let tc = match &m.content {
            MessageContent::Text { content, .. }
                if round.is_none() && m.author == "Hobbes" && content.is_empty() =>
            {
                continue
            }
            MessageContent::ToolCall(tc) | MessageContent::PermissionRequest(tc) => tc,
            _ => break,
        };
        match round {
            None => round = Some(&tc.round_id),
            Some(r) if *r != tc.round_id => break,
            Some(_) => {}
        }
        if matches!(m.content, MessageContent::ToolCall(_))
            && tc.status == crate::components::shared::ToolCallStatus::Error
            && !failed.contains(&tc.tool_name)
        {
            failed.push(tc.tool_name.clone());
        }
    }
    failed.reverse();
    failed
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    fn tool(server: &str, name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            server_name: server.to_string(),
            description: String::new(),
            parameters: json!({"type": "object"}),
        }
    }

    #[test]
    fn two_composio_profiles_collapse_to_one_declaration() {
        let tools = vec![
            tool("composio-native:profile-a", "COMPOSIO_DISCOVER_APPS"),
            tool("composio-native:profile-b", "COMPOSIO_DISCOVER_APPS"),
            tool("composio-native:profile-b", "COMPOSIO_EXECUTE_TOOL"),
            tool("fs", "list_files"),
        ];
        let kept = dedupe_by_wire_name(tools);
        let names: Vec<(&str, &str)> = kept
            .iter()
            .map(|t| (t.server_name.as_str(), t.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("composio-native:profile-a", "COMPOSIO_DISCOVER_APPS"),
                ("composio-native:profile-b", "COMPOSIO_EXECUTE_TOOL"),
                ("fs", "list_files"),
            ]
        );
    }
}

#[cfg(test)]
mod recovery_note_tests {
    use crate::components::chat::Message;
    use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus};
    use crate::context::prompt_builder::PromptBuilder;
    use crate::session::SessionState;
    use crate::settings::Settings;

    fn msg(author: &str, content: MessageContent) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.to_string(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: chrono::Utc::now(),
            usage: None,
        }
    }

    fn text(author: &str, content: &str) -> Message {
        msg(
            author,
            MessageContent::Text {
                content: content.to_string(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            },
        )
    }

    fn call(round: &str, tool: &str, status: ToolCallStatus) -> Message {
        msg(
            "Hobbes",
            MessageContent::ToolCall(ToolCall {
                server_name: "terminal".to_string(),
                tool_name: tool.to_string(),
                arguments: "{}".to_string(),
                status,
                response: "result".to_string(),
                round_id: Some(round.to_string()),
                ..Default::default()
            }),
        )
    }

    fn system_prompt(messages: Vec<Message>) -> String {
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        let session = state.sessions.get_mut(&id).unwrap();
        session.messages = messages;
        let settings = Settings::default();
        PromptBuilder::new(session, &settings)
            .build_system_context()
            .turn_instructions
    }

    #[test]
    fn error_in_latest_round_names_the_failing_tool() {
        let system = system_prompt(vec![
            text("User", "run it"),
            call("r1", "run_command", ToolCallStatus::Completed),
            call("r2", "read_file", ToolCallStatus::Completed),
            call("r2", "run_command", ToolCallStatus::Error),
            text("Hobbes", ""),
        ]);
        assert!(system.contains("RECOVERY NOTE"), "{system}");
        assert!(system.contains("failed (run_command)"), "{system}");
        assert!(!system.contains("CRITICAL RECOVERY INSTRUCTION"));
    }

    #[test]
    fn earlier_round_error_is_forgotten_once_latest_round_succeeds() {
        let system = system_prompt(vec![
            text("User", "run it"),
            call("r1", "run_command", ToolCallStatus::Error),
            call("r2", "reset_terminal", ToolCallStatus::Completed),
            call("r3", "run_command", ToolCallStatus::Completed),
            text("Hobbes", ""),
        ]);
        assert!(!system.contains("RECOVERY NOTE"), "{system}");
    }

    #[test]
    fn no_errors_means_no_recovery_note() {
        let system = system_prompt(vec![
            text("User", "run it"),
            call("r1", "run_command", ToolCallStatus::Completed),
            text("Hobbes", ""),
        ]);
        assert!(!system.contains("RECOVERY NOTE"), "{system}");
    }

    #[test]
    fn failure_does_not_leak_past_a_user_message() {
        let system = system_prompt(vec![
            text("User", "run it"),
            call("r1", "run_command", ToolCallStatus::Error),
            text("User", "never mind"),
        ]);
        assert!(!system.contains("RECOVERY NOTE"), "{system}");
    }
}
