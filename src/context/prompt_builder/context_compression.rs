use crate::str_utils::floor_char_boundary;
use serde_json::json;
use super::PromptBuilder;

impl<'a> PromptBuilder<'a> {
    /// Compress system context map to fit within a budget when the provider has
    /// a finite context window. Uses a 4-tier priority system:
    /// 1. Core (never omit): system_persona, user_name, current_time, scratchpad
    /// 2. Skill (compress before omit): loaded_skills instruction_manuals
    /// 3. Context (trim): conversation_summary, entities
    /// 4. Enrichment (omit first): composio_context, mcp_servers, user_instruction
    pub(crate) fn compose_system_for_budget(
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
