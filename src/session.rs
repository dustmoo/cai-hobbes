use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

use crate::mcp::manager::McpContext;
use serde_json::Value;

/// Deserialize a String that may be `null` in JSON (from LLM responses).
/// `#[serde(default)]` only handles missing keys; this also handles explicit `null`.
fn null_to_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummaryEntities {
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub user_name: String,
    #[serde(flatten)]
    pub other_entities: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummary {
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub sentiment: String,
    /// The active task or goal the user is currently pursuing.
    /// Populated automatically by the summarizer on each turn — no model tool call needed.
    /// Injected as part of `conversation_summary` in the system context (Tier 3).
    /// For small-context models this acts as a goal anchor so the task survives history scrolling.
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub current_task: String,
    #[serde(default)]
    pub entities: ConversationSummaryEntities,
}

impl ConversationSummary {
    /// Truncate summary to max_chars, appending truncation notice if needed
    pub fn truncate_summary(&mut self, max_chars: usize) {
        if max_chars > 0 && self.summary.len() > max_chars {
            let mut truncated_len = max_chars.saturating_sub(20); // Leave room for notice
            while truncated_len > 0 && !self.summary.is_char_boundary(truncated_len) {
                truncated_len -= 1;
            }
            self.summary.truncate(truncated_len);
            self.summary.push_str("... [truncated]");
        }
    }
}

impl ConversationSummaryEntities {
    /// Prune entities to max_count
    pub fn prune_entities(&mut self, max_count: usize) {
        if max_count > 0 && self.other_entities.len() > max_count {
            while self.other_entities.len() > max_count {
                if let Some(key) = self.other_entities.keys().next().cloned() {
                    self.other_entities.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub function_declarations: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolWrapper {
    pub tool: Tool,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ActiveContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_persona: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_tool_use_instruction: Option<String>,
    pub conversation_summary: ConversationSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tools: Option<McpContext>, // Fixed: Restored field as it is still used in chat.rs/prompt_builder.rs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolWrapper>>,
    /// Dynamically discovered Composio tools (populated by COMPOSIO_GET_APP_TOOLS).
    /// These are merged into the Gemini FunctionDeclarations on the next turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_composio_tools: Vec<rmcp::model::Tool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<super::components::chat::Message>,
    pub active_context: ActiveContext,
    pub last_updated: DateTime<Utc>,
    #[serde(default)]
    pub accumulated_cost: f64,
    #[serde(default)]
    pub accumulated_tokens: i32,
    #[serde(default)]
    pub accumulated_turns: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_optimization_summary: Option<String>,
    /// The specific Composio profile bound to this session.
    /// Acts as the live authority for tool-calling/MCP context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composio_profile: Option<String>,
    /// Per-session LLM provider override. None → follow global `Settings::active_llm`.
    /// Set together with `chat_model` by the chat-bar pickers so the pair stays consistent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_provider: Option<crate::settings::LlmProvider>,
    /// Per-session chat model override. None → the effective provider's configured model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_model: Option<String>,
    /// Skills actively loaded into this session's context.
    /// Maps skill_name → CapabilityContextPayload JSON (the response from execute_skill).
    /// Skills persist here until explicitly unloaded via /unload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub loaded_skills: HashMap<String, String>,
    /// AI-authored persistent scratchpad.
    /// Written by the AI via HOBBES_UPDATE_SCRATCHPAD (overwrite semantics).
    /// Injected as a Tier 1 core payload — never trimmed by context compression.
    /// Survives history scrolling and all 4 compression tiers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scratchpad: String,
    /// Track the number of automated AI turns in a single loop.
    /// Scoped per-session to prevent cross-tab interference and reset-bypasses.
    #[serde(default)]
    pub current_ai_turn_count: u32,
    /// Track how many times watch words have triggered auto-recovery in the
    /// current user turn. Resets when the user sends a new message.
    /// Prevents infinite recovery loops when the model keeps stalling.
    #[serde(default)]
    pub watch_word_recovery_count: u32,
    /// AI-scheduled timers/reminders for this session (HOBBES_SET_TIMER).
    /// Persisted so pending timers survive a restart and can be re-scheduled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_timers: Vec<crate::timers::ScheduledTimer>,
}

impl Session {
    pub fn increment_turn_count(&mut self) {
        self.current_ai_turn_count += 1;
    }

    pub fn reset_turn_count(&mut self) {
        self.current_ai_turn_count = 0;
        self.watch_word_recovery_count = 0;
    }

    pub fn delete_message_and_after(&mut self, message_id: &str) -> usize {
        if let Ok(uuid) = uuid::Uuid::parse_str(message_id) {
            if let Some(index) = self.messages.iter().position(|m| m.id == uuid) {
                let count = self.messages.len() - index;
                // Harvest cost/token data from the messages being deleted
                // so the session totals never drop when messages are pruned.
                // Also collect execution_ids from ToolCall messages so we can
                // remove their stale tool_snapshot entries from active_context.
                let mut deleted_execution_ids: Vec<String> = Vec::new();
                for msg in &self.messages[index..] {
                    if let Some(usage) = &msg.usage {
                        if let Some(cost) = usage.cost {
                            self.accumulated_cost += cost;
                        }
                        self.accumulated_tokens += usage.total_tokens;
                    }
                    if let crate::components::shared::MessageContent::ToolCall(tc) = &msg.content {
                        deleted_execution_ids.push(tc.execution_id.clone());
                    }
                }
                self.messages.truncate(index);

                // Remove tool_snapshot_* entries for deleted tool calls.
                // These are inserted by ToolCallSummarizer at end-of-turn and
                // serialized into SYSTEM_CONTEXT via #[serde(flatten)]. Without
                // this cleanup, the LLM sees phantom tool results from undone
                // turns, causing it to loop on stale context.
                for exec_id in &deleted_execution_ids {
                    let key = format!("tool_snapshot_{}", exec_id);
                    if self.active_context.extra.remove(&key).is_some() {
                        tracing::debug!("Removed stale tool snapshot after undo: {}", key);
                    }
                }

                // Reset conversation_summary to prevent stale context from
                // deleted turns leaking into future prompts. The summarizer
                // will re-populate it on the next turn's completion.
                if count > 0 {
                    self.active_context.conversation_summary = Default::default();
                    tracing::debug!("Reset conversation_summary after undo ({} messages removed)", count);
                }

                self.last_updated = Utc::now();
                count
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Calculate total cost for all messages in this session.
    /// Includes `accumulated_cost` from deleted messages so the counter
    /// never drops when the user prunes conversation history.
    pub fn total_cost(&self) -> f64 {
        let message_cost: f64 = self.messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .filter_map(|u| u.cost)
            .sum();
        self.accumulated_cost + message_cost
    }

    /// Calculate total tokens for all messages in this session.
    /// Includes `accumulated_tokens` from deleted messages so the counter
    /// never drops when the user prunes conversation history.
    pub fn total_tokens(&self) -> i32 {
        let message_tokens: i32 = self.messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| u.total_tokens)
            .sum();
        self.accumulated_tokens + message_tokens
    }

    /// Calculate average tokens per turn
    /// Always calculates from message-level usage data, which is the source of truth.
    pub fn average_tokens_per_turn(&self) -> f64 {
        let tokens = self
            .messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| u.total_tokens)
            .sum::<i32>() as f64;
        let turns = self
            .messages
            .iter()
            .filter(|m| m.author == "Hobbes")
            .count() as f64;

        if turns > 0.0 {
            tokens / turns
        } else {
            0.0
        }
    }
}

/// A large tool result that has been split into pages for small context windows.
/// Stored ephemerally in SessionState — not persisted to disk.
///
/// Pages are **not** pre-segmented. Instead, the full remaining content is stored
/// and dynamically sliced at delivery time using the model's current context budget.
/// This ensures page sizes adapt to context pressure changes between turns.
#[derive(Clone, Debug, PartialEq)]
pub struct PagedResult {
    /// Full remaining content. Consumed incrementally from the front
    /// using a dynamic `page_budget` supplied at delivery time.
    pub remaining_content: String,
    pub tool_name: String,
}

/// Session-scoped store for paginated tool results.
/// Keys are tool_call_id (execution_id). Cleared on session switch.
pub type PageQueue = HashMap<String, PagedResult>;

// floor_char_boundary and find_split_point live in crate::str_utils
// to avoid duplication with context/prompt_builder.rs.

/// Compute the dynamic page delivery budget based on the current model's context window.
/// Returns a character-count budget for a single page of HOBBES_PAGE_RESULT.
///
/// Uses the same provider-context resolution as `effective_tool_result_limit` in
/// prompt_builder.rs, ensuring page sizes scale proportionally with the model.
pub fn compute_page_budget(
    settings: &crate::settings::Settings,
    session: Option<&Session>,
) -> usize {
    let (provider, model) = match session {
        Some(s) => (
            settings.provider_for_session(s),
            settings.chat_model_for_session(s),
        ),
        None => (settings.active_llm, settings.active_chat_model()),
    };
    let tuning = settings.effective_context_tuning_for(provider);
    let provider_context_tokens = settings.resolve_context_window_for(provider, &model);

    if let Some(max_tokens) = provider_context_tokens {
        let ratio = crate::llm::config::ContextTuningPreset::clamp_budget_ratio(
            tuning.tool_result_budget_ratio,
        );
        let budget = (max_tokens as f64 * ratio * tuning.chars_per_token) as usize;
        tracing::debug!(
            "HOBBES_PAGE_RESULT budget: {} chars (ratio: {:.0}%, provider: {} tokens)",
            budget,
            ratio * 100.0,
            max_tokens
        );
        budget
    } else {
        tuning.max_active_tool_output_length
    }
}

/// Schema version for SessionState persistence.
/// Bump this when adding new migrations to `load()`.
/// Existing files without this field default to 0 via `#[serde(default)]`.
pub const CURRENT_SESSION_SCHEMA_VERSION: u32 = 2;

/// Fingerprint-map key for the tool-call-history meta blob (see `collect_meta_kv`).
const TCH_FP_KEY: &str = "\0meta:tool_call_history";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    /// Schema version for forward-compatible migrations.
    /// Files without this field (pre-versioning) default to 0.
    #[serde(default)]
    pub schema_version: u32,
    pub sessions: HashMap<String, Session>,
    pub active_session_id: String,
    pub window_width: f64,
    pub window_height: f64,
    #[serde(default)]
    pub tool_call_history: Vec<crate::components::shared::ToolCallRecord>,
    /// All-time cumulative cost in USD across all sessions (including deleted ones).
    #[serde(default)]
    pub lifetime_cost: f64,
    /// All-time cumulative token count across all sessions (including deleted ones).
    #[serde(default)]
    pub lifetime_tokens: i64,
    /// When true, save() will no-op to protect backup data after load failure
    #[serde(skip)]
    pub save_disabled: bool,
    /// Ephemeral store for paginated tool results — intentionally NOT persisted.
    /// Pages are keyed by `tool_call_id` and become stale on app restart because
    /// the model has no memory of which call IDs it was iterating.  Worst-case
    /// memory is ~2 MB (50 entries × 10 pages × 4 KB each) and is bounded by
    /// `MAX_PAGE_QUEUE_SIZE` in `handle_page_result`.
    #[serde(skip)]
    pub page_queue: PageQueue,
}

impl SessionState {
    /// Fetch the next page of a paginated tool result.
    ///
    /// `page_budget` controls how many characters to include in this page,
    /// allowing dynamic sizing based on the current model's context window.
    /// Returns `(page_content, tool_name, estimated_remaining)` or `None`
    /// if the tool_call_id is not found. Automatically cleans up the entry
    /// when all content has been consumed.
    pub fn fetch_next_page(
        &mut self,
        tool_call_id: &str,
        page_budget: usize,
    ) -> Option<(String, String, usize)> {
        // Remove the entry to avoid borrow conflicts during splitting.
        // Re-insert after if content remains.
        let mut entry = self.page_queue.remove(tool_call_id)?;

        if entry.remaining_content.is_empty() {
            return None;
        }

        let tool_name = entry.tool_name.clone();

        if entry.remaining_content.len() <= page_budget {
            // Last page — return everything, don't re-insert
            return Some((entry.remaining_content, tool_name, 0));
        }

        // Dynamically split at the current budget
        let split_at = crate::str_utils::find_split_point(&entry.remaining_content, page_budget);
        let page = entry.remaining_content[..split_at].to_string();
        entry.remaining_content = entry.remaining_content[split_at..].to_string();

        // Remaining page count is an ESTIMATE based on current budget.
        // Limitations:
        //  1. The budget may change between calls (model switch, context pressure)
        //     so the actual number of remaining pages can drift from this estimate.
        //  2. Assumes roughly uniform content density; highly variable content
        //     (e.g., large JSON array followed by short metadata) will skew the prediction.
        //  3. Smart splitting at semantic boundaries means actual page sizes are
        //     slightly smaller than `page_budget`, so remaining count may underestimate.
        let remaining_estimate = if entry.remaining_content.is_empty() {
            0
        } else {
            ((entry.remaining_content.len() as f64) / (page_budget as f64)).ceil() as usize
        };

        // Re-insert if there's remaining content
        if !entry.remaining_content.is_empty() {
            self.page_queue
                .insert(tool_call_id.to_string(), entry);
        }

        Some((page, tool_name, remaining_estimate))
    }

    /// Handle a HOBBES_PAGE_RESULT tool call. Extracts `tool_call_id` from
    /// `args_json`, fetches the next page, and returns `(status, response_string)`.
    /// Single authority for all HOBBES_PAGE_RESULT dispatch sites.
    ///
    /// `page_budget` controls how many characters to include in this page,
    /// computed by the caller using `compute_page_budget(&settings)` to
    /// dynamically match the model's current context window.
    pub fn handle_page_result(
        &mut self,
        args_json: &serde_json::Value,
        tool_call_id_arg: &str,
        page_budget: usize,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        let tool_call_id = if tool_call_id_arg.is_empty() {
            args_json
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            tool_call_id_arg.to_string()
        };

        if tool_call_id.is_empty() {
            return (
                crate::components::shared::ToolCallStatus::Error,
                "Missing 'tool_call_id' argument".to_string(),
            );
        }

        match self.fetch_next_page(&tool_call_id, page_budget) {
            Some((content, tool_name, remaining)) => {
                // NOTE: remaining count is an estimate (prefixed with ~) because
                // dynamic page sizing means the budget may differ on each call.
                // The actual number of pages may vary as context pressure changes.
                let footer = if remaining > 0 {
                    format!(
                        "\n\n[~{} more page(s) remaining. Call HOBBES_PAGE_RESULT with tool_call_id=\"{}\" to see the next page.]",
                        remaining, tool_call_id
                    )
                } else {
                    "\n\n[All pages delivered.]".to_string()
                };
                tracing::info!(
                    "HOBBES_PAGE_RESULT: Delivered page for '{}' (tool_call_id={}, remaining=~{})",
                    tool_name, tool_call_id, remaining
                );
                (
                    crate::components::shared::ToolCallStatus::Completed,
                    format!("{}{}", content, footer),
                )
            }
            None => (
                crate::components::shared::ToolCallStatus::Error,
                format!(
                    "No paginated results found for tool_call_id '{}'. The pages may have expired.",
                    tool_call_id
                ),
            ),
        }
    }

    /// Handle a HOBBES_UPDATE_SCRATCHPAD tool call.
    /// Overwrites the active session's `scratchpad` field with the provided content.
    /// Returns `(status, confirmation_string)` matching the pattern of `handle_page_result`.
    pub fn handle_scratchpad_update(
        &mut self,
        args_json: &serde_json::Value,
        session_id: &str,
        settings: &crate::settings::Settings,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        // Limit scratchpad to 2% of the context window in chars.
        // e.g. 128K tokens × 4 chars/token × 2% ≈ 10,240 chars.
        // Floor at 4K so small/unconfigured models still get meaningful space.
        // Cap at 32K so even enormous context windows don't allow bloat.
        let session = self.sessions.get(session_id);
        let (provider, model) = match session {
            Some(s) => (
                settings.provider_for_session(s),
                settings.chat_model_for_session(s),
            ),
            None => (settings.active_llm, settings.active_chat_model()),
        };
        let tuning = settings.effective_context_tuning_for(provider);
        let max_scratchpad_chars: usize = settings
            .resolve_context_window_for(provider, &model)
            .map(|tokens| {
                let chars = (tokens as f64 * tuning.chars_per_token * 0.02) as usize;
                chars.clamp(4_000, 32_000)
            })
            .unwrap_or(16_000);

        let content = args_json
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if content.is_empty() {
            return (
                crate::components::shared::ToolCallStatus::Error,
                "Missing 'content' argument for HOBBES_UPDATE_SCRATCHPAD".to_string(),
            );
        }

        let char_count = content.chars().count();

        if char_count > max_scratchpad_chars {
            return (
                crate::components::shared::ToolCallStatus::Error,
                format!(
                    "Scratchpad too large ({} chars, max {} for this model). Please condense.",
                    char_count, max_scratchpad_chars
                ),
            );
        }

        if let Some(session) = self.sessions.get_mut(session_id) {
            session.scratchpad = content;
            tracing::info!(
                "HOBBES_UPDATE_SCRATCHPAD: Scratchpad updated ({} chars, limit {})",
                char_count, max_scratchpad_chars
            );
            (
                crate::components::shared::ToolCallStatus::Completed,
                format!("Scratchpad updated ({} chars). Content is now persistent for this session.", char_count),
            )
        } else {
            tracing::warn!("HOBBES_UPDATE_SCRATCHPAD: Session '{}' not found", session_id);
            (
                crate::components::shared::ToolCallStatus::Error,
                format!("Session '{}' not found", session_id),
            )
        }
    }

    /// HOBBES_SET_TIMER: schedule a timer/reminder for this session.
    pub fn handle_set_timer(
        &mut self,
        args_json: &serde_json::Value,
        session_id: &str,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        use crate::components::shared::ToolCallStatus;
        use crate::timers::{
            parse_duration_secs, ScheduledTimer, TimerMode, TimerStatus, MAX_DELAY_SECS,
            MIN_DELAY_SECS,
        };

        // Delay accepts either `delay_secs` (integer) or `delay` ("10m", "1h30m").
        let delay_secs = args_json
            .get("delay_secs")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                args_json
                    .get("delay")
                    .and_then(|v| v.as_str())
                    .and_then(parse_duration_secs)
            });
        let delay_secs = match delay_secs {
            Some(n) => n,
            None => {
                return (
                    ToolCallStatus::Error,
                    "Provide 'delay_secs' (integer seconds) or 'delay' (e.g. \"10m\", \"1h30m\")."
                        .to_string(),
                )
            }
        };
        if delay_secs < MIN_DELAY_SECS {
            return (
                ToolCallStatus::Error,
                format!("Delay too short (minimum {}s).", MIN_DELAY_SECS),
            );
        }
        if delay_secs > MAX_DELAY_SECS {
            return (
                ToolCallStatus::Error,
                "Delay too long (maximum 7 days).".to_string(),
            );
        }

        let mode = match args_json.get("mode").and_then(|v| v.as_str()) {
            Some("prompt") => TimerMode::Prompt,
            Some("notify") | None => TimerMode::Notify,
            Some(other) => {
                return (
                    ToolCallStatus::Error,
                    format!("Unknown mode '{}'. Use 'notify' or 'prompt'.", other),
                )
            }
        };
        let prompt = args_json
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let label = args_json
            .get("label")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if mode == TimerMode::Prompt
            && prompt.as_deref().map(str::trim).unwrap_or("").is_empty()
        {
            return (
                ToolCallStatus::Error,
                "mode 'prompt' requires a non-empty 'prompt' to run when the timer fires."
                    .to_string(),
            );
        }

        let now = Utc::now();
        let timer = ScheduledTimer {
            id: format!("tmr_{}", &uuid::Uuid::new_v4().to_string()[..8]),
            session_id: session_id.to_string(),
            created_at: now,
            fire_at: now + chrono::Duration::seconds(delay_secs),
            mode,
            label,
            prompt,
            status: TimerStatus::Pending,
        };
        let summary = timer.summary();

        {
            let Some(session) = self.sessions.get_mut(session_id) else {
                return (
                    ToolCallStatus::Error,
                    format!("Session '{}' not found", session_id),
                );
            };
            session.scheduled_timers.push(timer);
        }
        Self::save_async(self, None);

        let human = {
            let (m, s) = (delay_secs / 60, delay_secs % 60);
            if m > 0 {
                format!("{}m{:02}s", m, s)
            } else {
                format!("{}s", s)
            }
        };
        (
            ToolCallStatus::Completed,
            format!("Timer set — fires in {}. {}", human, summary),
        )
    }

    /// HOBBES_LIST_TIMERS: list this session's pending timers.
    pub fn handle_list_timers(
        &self,
        session_id: &str,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        use crate::components::shared::ToolCallStatus;
        use crate::timers::TimerStatus;

        let pending: Vec<String> = self
            .sessions
            .get(session_id)
            .map(|s| {
                s.scheduled_timers
                    .iter()
                    .filter(|t| t.status == TimerStatus::Pending)
                    .map(|t| t.summary())
                    .collect()
            })
            .unwrap_or_default();

        if pending.is_empty() {
            (ToolCallStatus::Completed, "No pending timers.".to_string())
        } else {
            (
                ToolCallStatus::Completed,
                format!("Pending timers:\n{}", pending.join("\n")),
            )
        }
    }

    /// HOBBES_CANCEL_TIMER: cancel a pending timer by id.
    pub fn handle_cancel_timer(
        &mut self,
        args_json: &serde_json::Value,
        session_id: &str,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        use crate::components::shared::ToolCallStatus;
        use crate::timers::TimerStatus;

        let timer_id = args_json
            .get("timer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if timer_id.is_empty() {
            return (
                ToolCallStatus::Error,
                "Missing 'timer_id' (from the timer's confirmation or HOBBES_LIST_TIMERS).".to_string(),
            );
        }

        let cancelled = {
            match self.sessions.get_mut(session_id) {
                Some(session) => session
                    .scheduled_timers
                    .iter_mut()
                    .find(|t| t.id == timer_id && t.status == TimerStatus::Pending)
                    .map(|t| {
                        t.status = TimerStatus::Cancelled;
                        true
                    })
                    .unwrap_or(false),
                None => false,
            }
        };

        if cancelled {
            Self::save_async(self, None);
            (
                ToolCallStatus::Completed,
                format!("Cancelled timer {}.", timer_id),
            )
        } else {
            (
                ToolCallStatus::Error,
                format!("No pending timer with id '{}'.", timer_id),
            )
        }
    }

    /// Store newly-generated paginated pages into the queue.
    /// Call this after `PromptBuilder::build_prompt` in a separate write scope.
    ///
    /// **Critical**: Skips entries that already exist in the queue.
    /// Each continuation turn re-runs `build_prompt()`, which re-generates
    /// pages from the same tool results. Without this guard, `HashMap::insert`
    /// would overwrite partially-consumed page state, causing the model to
    /// always see page 1 again instead of the next page.
    pub fn store_pages(&mut self, pages: Vec<(String, PagedResult)>) {
        let mut inserted_ids = std::collections::HashSet::new();
        
        for (id, paged) in pages {
            if self.page_queue.contains_key(&id) {
                tracing::debug!(
                    "Skipping page queue entry (already exists): id={} (remaining={} bytes)",
                    id,
                    self.page_queue.get(&id).map_or(0, |p| p.remaining_content.len())
                );
            } else {
                tracing::debug!("Storing page queue entry: id={}", id);
                self.page_queue.insert(id.clone(), paged);
                inserted_ids.insert(id);
            }
        }

        // Keep memory bounded: cap `page_queue` size to 50 active paginated tools per session.
        const MAX_PAGE_QUEUE_SIZE: usize = 50;
        if self.page_queue.len() > MAX_PAGE_QUEUE_SIZE {
            let excess = self.page_queue.len() - MAX_PAGE_QUEUE_SIZE;
            
            // Collect oldest/random keys EXCEPT those we just inserted
            let keys_to_remove: Vec<String> = self.page_queue.keys()
                .filter(|k| !inserted_ids.contains(*k))
                .take(excess)
                .cloned()
                .collect();
                
            for key in keys_to_remove {
                self.page_queue.remove(&key);
                tracing::debug!("Pruned expired page queue entry to save memory: {}", key);
            }
        }
    }

    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load session state from the SQLite store, hydrating only the sessions
    /// that need to be in memory: open tabs, the active session, and any
    /// session with scheduled timers. Everything else stays on disk and is
    /// hydrated on demand (history click / timer fire).
    pub fn load(open_tab_ids: &[String]) -> Result<Self, std::io::Error> {
        use crate::session_store as store;

        if !store::is_available() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session store not initialized",
            ));
        }

        let schema_version = store::meta_get(store::META_SCHEMA_VERSION)
            .and_then(|v| v.parse().ok())
            .unwrap_or(CURRENT_SESSION_SCHEMA_VERSION);
        let mut active_session_id =
            store::meta_get(store::META_ACTIVE_SESSION).unwrap_or_default();
        let default = SessionState::default();
        let window_width = store::meta_get(store::META_WINDOW_WIDTH)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default.window_width);
        let window_height = store::meta_get(store::META_WINDOW_HEIGHT)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default.window_height);
        let lifetime_cost = store::meta_get(store::META_LIFETIME_COST)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        let lifetime_tokens = store::meta_get(store::META_LIFETIME_TOKENS)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let tool_call_history = store::meta_get(store::META_TOOL_CALL_HISTORY)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();

        // Hydration set: open tabs, the active session, sessions with timers.
        let mut ids: Vec<String> = open_tab_ids.to_vec();
        if !active_session_id.is_empty() && !ids.contains(&active_session_id) {
            ids.push(active_session_id.clone());
        }
        for id in store::session_ids_with_timers() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let mut sessions = store::load_sessions(&ids);

        // Validate active_session_id against the full store, not just memory.
        if !sessions.contains_key(&active_session_id) {
            if let Some(recent) = store::most_recent_session_id() {
                if let Some(session) = store::load_session(&recent) {
                    sessions.insert(recent.clone(), session);
                }
                active_session_id = recent;
            } else {
                active_session_id.clear();
            }
        }

        tracing::info!(
            "Loaded session store: {} of {} sessions hydrated (schema_version={}).",
            sessions.len(),
            store::session_count(),
            schema_version
        );

        Ok(SessionState {
            schema_version,
            sessions,
            active_session_id,
            window_width,
            window_height,
            tool_call_history,
            lifetime_cost,
            lifetime_tokens,
            save_disabled: false,
            page_queue: PageQueue::default(),
        })
    }

    /// Read just the persisted window dimensions (pre-UI startup path).
    pub fn load_window_dims() -> (f64, f64) {
        use crate::session_store as store;
        let default = SessionState::default();
        let w = store::meta_get(store::META_WINDOW_WIDTH)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default.window_width);
        let h = store::meta_get(store::META_WINDOW_HEIGHT)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default.window_height);
        (w, h)
    }

    /// Parse a legacy `sessions.json` file (including pre-v2 formats) without
    /// writing anything back. Used only by the one-time JSON→SQLite import.
    pub(crate) fn load_from_json_file(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let data = fs::read_to_string(path).map_err(|e| {
            tracing::error!("Failed to read session file at {:?}: {}", path, e);
            e
        })?;

        if let Ok(mut state) = serde_json::from_str::<Self>(&data) {
            if !state.sessions.contains_key(&state.active_session_id) {
                state.active_session_id = state
                    .sessions
                    .values()
                    .max_by_key(|s| s.last_updated)
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
            }
            state.schema_version = CURRENT_SESSION_SCHEMA_VERSION;
            return Ok(state);
        }

        tracing::warn!("Legacy sessions.json is not in current format, running migrations...");
        let mut state = Self::migrate_from_raw_json(&data)?;
        state.schema_version = CURRENT_SESSION_SCHEMA_VERSION;
        Ok(state)
    }

    /// Migrate raw JSON data from an older format into a [`SessionState`].
    ///
    /// This is the fallback path when direct deserialization fails — it parses
    /// the data as a generic `serde_json::Value`, applies all known field/format
    /// migrations, and then deserializes the result into the current struct shape.
    ///
    /// Extracted from `load()` so it can be exercised by unit tests without
    /// touching the filesystem.
    pub(crate) fn migrate_from_raw_json(data: &str) -> Result<Self, std::io::Error> {
        let mut state = SessionState::default();
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data) {
            // Migrate MessageContent::Text from old tuple format to new struct format
            if let Some(sessions_obj) = value.get_mut("sessions").and_then(|v| v.as_object_mut()) {
                for (_session_id, session_val) in sessions_obj.iter_mut() {
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        for message in messages.iter_mut() {
                            if let Some(content) = message.get_mut("content") {
                                // Check if this is the old Text format: {"Text": "string"}
                                if let Some(text_str) = content.get("Text").and_then(|v| v.as_str())
                                {
                                    // Convert to new format: {"Text": {"content": "string", "thought_signature": null}}
                                    *content = serde_json::json!({
                                        "Text": {
                                            "content": text_str,
                                            "thought_signature": null,
                                            "thought_summary": null
                                        }
                                    });
                                    tracing::debug!("Migrated MessageContent::Text for message");
                                }
                            }
                        }
                    }

                    // Migrate messages without created_at timestamps
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        let base_time = chrono::Utc::now() - chrono::Duration::hours(1);
                        for (index, message) in messages.iter_mut().enumerate() {
                            if message.get("created_at").is_none() {
                                let timestamp =
                                    base_time + chrono::Duration::milliseconds(index as i64);
                                message
                                    .as_object_mut()
                                    .expect(
                                        "message migration: message value must be a JSON object",
                                    )
                                    .insert(
                                        "created_at".to_string(),
                                        serde_json::json!(timestamp.to_rfc3339()),
                                    );
                                tracing::debug!("Migrated message {} with timestamp", index);
                            }
                        }
                    }

                    // Migrate ToolCall/SkillCall fields
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        for message in messages.iter_mut() {
                            if let Some(content) = message.get_mut("content") {
                                let migrate_tool_fields = |obj: &mut serde_json::Map<
                                    String,
                                    serde_json::Value,
                                >| {
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                        }
                                    }
                                    if obj.contains_key("name") && !obj.contains_key("tool_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("tool_name".to_string(), val);
                                        }
                                    }
                                };

                                let migrate_skill_fields = |obj: &mut serde_json::Map<
                                    String,
                                    serde_json::Value,
                                >| {
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                        }
                                    }
                                    if obj.contains_key("name") && !obj.contains_key("skill_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("skill_name".to_string(), val);
                                        }
                                    }
                                };

                                if let Some(tool_call) =
                                    content.get_mut("ToolCall").and_then(|v| v.as_object_mut())
                                {
                                    migrate_tool_fields(tool_call);
                                }
                                if let Some(perm_req) = content
                                    .get_mut("PermissionRequest")
                                    .and_then(|v| v.as_object_mut())
                                {
                                    migrate_tool_fields(perm_req);
                                }
                                if let Some(skill_call) =
                                    content.get_mut("SkillCall").and_then(|v| v.as_object_mut())
                                {
                                    migrate_skill_fields(skill_call);
                                }
                                if let Some(skill_perm) = content
                                    .get_mut("SkillPermissionRequest")
                                    .and_then(|v| v.as_object_mut())
                                {
                                    migrate_skill_fields(skill_perm);
                                }
                            }
                        }
                    }
                }
            }

            // Now deserialize the migrated value
            if let Some(sessions_val) = value.get("sessions") {
                match serde_json::from_value(sessions_val.clone()) {
                    Ok(sessions) => {
                        state.sessions = sessions;
                        tracing::info!("Migration recovered {} sessions", state.sessions.len());
                    }
                    Err(e) => {
                        tracing::error!(
                            "Migration failed to deserialize sessions: {}. NOT overwriting backup.",
                            e
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Migration failed: {}. Your data backup is at sessions.json.bak",
                                e
                            ),
                        ));
                    }
                }
            }
            if let Some(active_id) = value.get("active_session_id").and_then(|v| v.as_str()) {
                state.active_session_id = active_id.to_string();
            }
            if let Some(width) = value.get("window_width").and_then(|v| v.as_f64()) {
                state.window_width = width;
            }
            if let Some(height) = value.get("window_height").and_then(|v| v.as_f64()) {
                state.window_height = height;
            }
            if let Some(history_val) = value.get("tool_call_history") {
                if let Ok(history) = serde_json::from_value(history_val.clone()) {
                    state.tool_call_history = history;
                }
            }
        }
        Ok(state)
    }

    /// Synchronously persist dirty sessions + meta to the SQLite store.
    /// Only sessions whose serialized bytes changed since the last successful
    /// write are upserted. App code uses `save_async`; this sync variant is
    /// kept for tests and non-UI callers.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<(), std::io::Error> {
        // If save is disabled (due to load failure), protect the stored data
        if self.save_disabled {
            tracing::warn!("Save disabled due to prior load failure - protecting stored data");
            return Ok(());
        }

        use crate::session_store as store;
        let rows = store::collect_dirty_rows(&self.sessions)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let (meta, tch_mark) = self.collect_meta_kv();
        store::write_rows_and_meta(&rows, &meta)
            .map_err(std::io::Error::other)?;
        store::mark_rows_saved(&rows);
        if let Some(tch) = tch_mark {
            store::mark_blob_saved(TCH_FP_KEY, &tch);
        }
        Ok(())
    }

    /// Meta key/value pairs to persist alongside session rows. The
    /// tool-call history blob is fingerprinted and only included when it
    /// changed; the second return value is the blob to mark saved on success.
    fn collect_meta_kv(&self) -> (Vec<(String, String)>, Option<String>) {
        use crate::session_store as store;
        let mut kv = vec![
            (store::META_SCHEMA_VERSION.to_string(), self.schema_version.to_string()),
            (store::META_ACTIVE_SESSION.to_string(), self.active_session_id.clone()),
            (store::META_WINDOW_WIDTH.to_string(), self.window_width.to_string()),
            (store::META_WINDOW_HEIGHT.to_string(), self.window_height.to_string()),
            (store::META_LIFETIME_COST.to_string(), self.lifetime_cost.to_string()),
            (store::META_LIFETIME_TOKENS.to_string(), self.lifetime_tokens.to_string()),
        ];
        let tch = serde_json::to_string(&self.tool_call_history).unwrap_or_else(|_| "[]".into());
        let mark = if store::blob_changed(TCH_FP_KEY, &tch) {
            kv.push((store::META_TOOL_CALL_HISTORY.to_string(), tch.clone()));
            Some(tch)
        } else {
            None
        };
        (kv, mark)
    }

    pub fn create_session_raw(&mut self, initial_profile: Option<String>) -> String {
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Local::now();
        let new_session = Session {
            id: new_id.clone(),
            name: now.format("%b %d - %I:%M %p").to_string(),
            messages: vec![],
            active_context: ActiveContext::default(),
            last_updated: Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: initial_profile,
            llm_provider: None,
            chat_model: None,
            loaded_skills: HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
        };
        self.sessions.insert(new_id.clone(), new_session);
        self.active_session_id = new_id.clone();
        new_id
    }

    pub fn create_session(&mut self, initial_profile: Option<String>) -> String {
        let new_id = self.create_session_raw(initial_profile);
        Self::save_async(self, None);
        new_id
    }

    pub fn delete_session_raw(&mut self, id: &str) {
        // Harvest cost/token data before destruction so lifetime counters survive deletion
        if let Some(session) = self.sessions.get(id) {
            self.lifetime_cost += session.total_cost();
            self.lifetime_tokens += session.total_tokens() as i64;
            self.sessions.remove(id);
            // The in-memory copy was authoritative; ignore the (possibly
            // stale) stored totals to avoid double counting.
            crate::session_store::delete_session(id);
        } else if let Some((cost, tokens)) = crate::session_store::delete_session(id) {
            // Not hydrated — harvest from the stored row's totals.
            self.lifetime_cost += cost;
            self.lifetime_tokens += tokens;
        }

        if self.active_session_id == id {
            // The active session was deleted. Prefer the most recent hydrated
            // session, then fall back to the most recent stored one.
            let mut next = self
                .sessions
                .values()
                .max_by_key(|s| s.last_updated)
                .map(|s| s.id.clone());
            if next.is_none() {
                if let Some(recent) = crate::session_store::most_recent_session_id() {
                    if let Some(session) = crate::session_store::load_session(&recent) {
                        self.sessions.insert(recent.clone(), session);
                    }
                    next = Some(recent);
                }
            }
            self.active_session_id = next.unwrap_or_default();
        } else if self.sessions.is_empty() {
            self.active_session_id = String::new();
        }
    }

    pub fn delete_session(&mut self, id: &str) {
        self.delete_session_raw(id);
        Self::save_async(self, None);
    }

    pub fn get_active_session(&self) -> Option<&Session> {
        self.sessions.get(&self.active_session_id)
    }

    pub fn get_active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(&self.active_session_id)
    }

    /// Touch (update `last_updated`) on a specific session by ID.
    /// Used by stream_manager to target the originating session after a tab switch.
    pub fn touch_session(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_updated = Utc::now();
        }
    }

    /// Look up a message by UUID within a specific session (not the active one).
    /// Used by stream_manager to write streaming data to the originating session.
    pub fn get_message_mut_in_session(
        &mut self,
        session_id: &str,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.sessions
            .get_mut(session_id)
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
    }

    /// Remove a message from a specific session by ID.
    /// Used by cancel_stream to target the originating session after a tab switch.
    pub fn remove_message_in_session(&mut self, session_id: &str, message_id: &uuid::Uuid) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if let Some(index) = session.messages.iter().position(|m| m.id == *message_id) {
                session.messages.remove(index);
                tracing::info!(message_id = %message_id, session_id = %session_id, "Removed message from target session.");
            }
        }
    }

    pub fn update_window_size(&mut self, width: f64, height: f64) {
        self.window_width = width;
        self.window_height = height;
        // Note: Save is now handled asynchronously by the caller to avoid UI hang.
        // The caller should invoke save_async() after updating window size.
    }

    /// Write pre-serialized bytes to the session file using the same atomic
    /// tempfile pattern as `save()`. Used by `save_async` to avoid cloning.
    /// Saves dirty sessions to the SQLite store on a background thread.
    /// This prevents blocking the main UI thread during DB I/O.
    /// If `error_signal` is provided, save failures will be surfaced to the UI.
    ///
    /// **Design Note (Serialize-Then-Move, P-009):** Each hydrated session is
    /// serialized on the calling thread via a borrow of `&SessionState`;
    /// sessions whose bytes match the last successful write are skipped
    /// entirely. Only the owned row buffers move to the background thread —
    /// the state is never cloned, and a one-message append writes one row.
    /// Fingerprints commit only after the write succeeds, so a failed write
    /// is retried on the next save.
    pub fn save_async(
        state: &SessionState,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        // Guard: skip if saves are disabled (backup protection)
        if state.save_disabled {
            tracing::warn!(
                "save_async: Save disabled due to prior load failure - protecting stored data"
            );
            return;
        }

        use crate::session_store as store;
        let rows = match store::collect_dirty_rows(&state.sessions) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to serialize session state: {}", e);
                if let Some(mut sig) = error_signal {
                    use dioxus::prelude::Writable;
                    *sig.write() = Some(format!("Failed to serialize session state: {}", e));
                }
                return;
            }
        };
        let (meta, tch_mark) = state.collect_meta_kv();

        // Move only the prepared row/meta buffers to the background thread
        dioxus::prelude::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                store::write_rows_and_meta(&rows, &meta).map(|_| (rows, tch_mark))
            })
            .await;

            // Back on the Dioxus runtime — safe to touch Signals
            match result {
                Ok(Ok((rows, tch_mark))) => {
                    store::mark_rows_saved(&rows);
                    if let Some(tch) = tch_mark {
                        store::mark_blob_saved(TCH_FP_KEY, &tch);
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to save session state: {}", e);
                    if let Some(mut sig) = error_signal {
                        use dioxus::prelude::Writable;
                        *sig.write() = Some(format!("Failed to save session state: {}", e));
                    }
                }
                Err(e) => {
                    tracing::error!("spawn_blocking panicked during save: {}", e);
                }
            }
        });
    }

    /// Convenience: save the session state from a Dioxus Signal after releasing a write guard.
    /// This encapsulates the common `drop(guard); save_async(&signal.read(), ...)` pattern
    /// to avoid borrow conflicts between write guards and the read borrow needed for serialization.
    pub fn save_signal(
        signal: &dioxus::prelude::Signal<SessionState>,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        use dioxus::prelude::Readable;
        Self::save_async(&signal.read(), error_signal);
    }

    pub fn update_session_name_raw(&mut self, id: &str, new_name: String) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.name = new_name;
        } else {
            // Not hydrated (renamed from History) — update the stored row directly.
            crate::session_store::rename_session(id, &new_name);
        }
    }

    pub fn update_session_name(&mut self, id: &str, new_name: String) {
        self.update_session_name_raw(id, new_name);
        Self::save_async(self, None);
    }
    pub fn get_message_mut(
        &mut self,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut()
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
    }

    pub fn get_message_mut_by_execution_id(
        &mut self,
        execution_id: &str,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut().and_then(|session| {
            session.messages.iter_mut().find(|m| match &m.content {
                super::components::shared::MessageContent::ToolCall(tc) => {
                    tc.execution_id == execution_id
                }
                super::components::shared::MessageContent::PermissionRequest(tc) => {
                    tc.execution_id == execution_id
                }
                _ => false,
            })
        })
    }

    /// Migrate session composio_profile from name-based to ID-based.
    /// Any session whose composio_profile matches a profile name (but not an ID)
    /// gets updated to the corresponding profile ID.
    pub fn migrate_session_profiles_to_ids(
        &mut self,
        settings: &crate::settings::Settings,
    ) -> bool {
        let mut migrated = false;
        for session in self.sessions.values_mut() {
            if let Some(ref value) = session.composio_profile {
                // Already an ID — skip
                if settings.composio_profiles.iter().any(|p| &p.id == value) {
                    continue;
                }
                // Match by name → replace with ID
                if let Some(profile) = settings.composio_profiles.iter().find(|p| &p.name == value)
                {
                    tracing::info!(
                        "Migrating session '{}' composio_profile from name '{}' to id '{}'",
                        session.id,
                        value,
                        profile.id
                    );
                    session.composio_profile = Some(profile.id.clone());
                    migrated = true;
                }
            }
        }
        migrated
    }
}
impl Default for SessionState {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SESSION_SCHEMA_VERSION,
            sessions: HashMap::new(),
            active_session_id: String::new(),
            window_width: 1440.0, // 16:9 ratio default
            window_height: 810.0,
            tool_call_history: Vec::new(),
            lifetime_cost: 0.0,
            lifetime_tokens: 0,
            save_disabled: false,
            page_queue: PageQueue::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // === Migration Tests ===

    /// Helper: wrap a single session with messages into a full SessionState JSON string.
    /// Uses the old format WITHOUT schema_version so it will fail direct deser and hit migrate_from_raw_json.
    fn make_old_session_json(messages_json: &str) -> String {
        format!(
            r#"{{
                "sessions": {{
                    "test-session-1": {{
                        "id": "test-session-1",
                        "name": "Test Session",
                        "messages": [{messages_json}],
                        "active_context": {{}},
                        "last_updated": "2026-01-01T00:00:00Z"
                    }}
                }},
                "active_session_id": "test-session-1",
                "window_width": 800.0,
                "window_height": 600.0,
                "tool_call_history": []
            }}"#
        )
    }

    /// Test: old {"Text": "string"} format is migrated to {"Text": {"content": "string", ...}}
    #[test]
    fn test_migration_text_format() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": "Hello world"},
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 1);

        match &session.messages[0].content {
            crate::components::shared::MessageContent::Text {
                content,
                thought_signature,
                thought_summary,
            } => {
                assert_eq!(content, "Hello world");
                assert!(thought_signature.is_none());
                assert!(thought_summary.is_none());
            }
            other => panic!("Expected Text variant, got {:?}", other),
        }
    }

    /// Test: messages without created_at get timestamps backfilled in order.
    #[test]
    fn test_migration_timestamps() {
        let old_messages = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": {"content": "First", "thought_signature": null, "thought_summary": null}},
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000002",
                "author": "Hobbes",
                "content": {"Text": {"content": "Second", "thought_signature": null, "thought_summary": null}},
                "attachments": [],
                "comments": []
            }
        "#;
        let json = make_old_session_json(old_messages);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 2);

        // Both should have created_at, and the second should be after the first
        let t0 = session.messages[0].created_at;
        let t1 = session.messages[1].created_at;
        assert!(
            t1 > t0,
            "Second message timestamp should be after first: {:?} vs {:?}",
            t0,
            t1
        );
    }

    /// Test: ToolCall/PermissionRequest field renames (id -> execution_id, name -> tool_name)
    #[test]
    fn test_migration_toolcall_renames() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "Hobbes",
                "content": {
                    "ToolCall": {
                        "id": "old-exec-id",
                        "server_name": "test-server",
                        "name": "old-tool-name",
                        "arguments": "{}",
                        "status": "Completed",
                        "response": "ok",
                        "thought_signature": null,
                        "thought_summary": null
                    }
                },
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        match &session.messages[0].content {
            crate::components::shared::MessageContent::ToolCall(tc) => {
                assert_eq!(
                    tc.execution_id, "old-exec-id",
                    "id should have migrated to execution_id"
                );
                assert_eq!(
                    tc.tool_name, "old-tool-name",
                    "name should have migrated to tool_name"
                );
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    /// Test: SkillCall/SkillPermissionRequest field renames (id -> execution_id, name -> skill_name)
    #[test]
    fn test_migration_skillcall_renames() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "Hobbes",
                "content": {
                    "SkillCall": {
                        "id": "old-skill-exec-id",
                        "name": "old-skill-name",
                        "arguments": "{}",
                        "status": "Completed",
                        "response": "ok",
                        "instructions": "do stuff"
                    }
                },
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        match &session.messages[0].content {
            crate::components::shared::MessageContent::SkillCall(sc) => {
                assert_eq!(
                    sc.execution_id, "old-skill-exec-id",
                    "id should have migrated to execution_id"
                );
                assert_eq!(
                    sc.skill_name, "old-skill-name",
                    "name should have migrated to skill_name"
                );
            }
            other => panic!("Expected SkillCall, got {:?}", other),
        }
    }

    /// Test: Full round-trip — old format with ALL migration triggers, migrated, serialized, re-deserialized.
    #[test]
    fn test_migration_full_roundtrip() {
        // Build a fixture with every migration trigger active simultaneously:
        // 1. Old Text format ({"Text": "string"})
        // 2. Missing created_at
        // 3. Old ToolCall field names
        let old_messages = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": "Hello from old format"},
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000002",
                "author": "Hobbes",
                "content": {
                    "ToolCall": {
                        "id": "tc-1",
                        "server_name": "mcp-server",
                        "name": "read_file",
                        "arguments": "{\"path\":\"/tmp/test\"}",
                        "status": "Completed",
                        "response": "file contents",
                        "thought_signature": null,
                        "thought_summary": null
                    }
                },
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000003",
                "author": "Hobbes",
                "content": {"Text": "Here is the result"},
                "attachments": [],
                "comments": []
            }
        "#;
        let json = make_old_session_json(old_messages);

        // Step 1: Migrate from old format
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 3);

        // Verify Text migration worked
        match &session.messages[0].content {
            crate::components::shared::MessageContent::Text { content, .. } => {
                assert_eq!(content, "Hello from old format");
            }
            other => panic!("Message 0: expected Text, got {:?}", other),
        }

        // Verify ToolCall migration worked
        match &session.messages[1].content {
            crate::components::shared::MessageContent::ToolCall(tc) => {
                assert_eq!(tc.execution_id, "tc-1");
                assert_eq!(tc.tool_name, "read_file");
            }
            other => panic!("Message 1: expected ToolCall, got {:?}", other),
        }

        // Verify timestamps were backfilled
        for msg in &session.messages {
            // created_at should be set (non-zero)
            assert!(
                msg.created_at.timestamp() > 0,
                "created_at should be set for all messages"
            );
        }
        // Timestamps should be in order
        assert!(session.messages[1].created_at > session.messages[0].created_at);
        assert!(session.messages[2].created_at > session.messages[1].created_at);

        // Verify window dimensions preserved
        assert_eq!(state.window_width, 800.0);
        assert_eq!(state.window_height, 600.0);

        // Step 2: Serialize the migrated state and re-deserialize — "round trip"
        let serialized =
            serde_json::to_string_pretty(&state).expect("Serialization should succeed");
        let reloaded: SessionState = serde_json::from_str(&serialized)
            .expect("Round-trip deserialization should succeed without needing migration");

        assert_eq!(state.sessions.len(), reloaded.sessions.len());
        assert_eq!(state.active_session_id, reloaded.active_session_id);
        assert_eq!(state.window_width, reloaded.window_width);
        let reloaded_session = reloaded
            .sessions
            .get("test-session-1")
            .expect("Session missing on reload");
        assert_eq!(reloaded_session.messages.len(), 3);
    }

    /// Test: save() is a no-op when save_disabled is true.
    #[test]
    fn test_save_disabled_guard() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let test_path = temp_dir.path().join("disabled_sessions.json");

        // Create initial content
        fs::write(&test_path, "original content").expect("Failed to write initial file");

        let state = SessionState {
            save_disabled: true,
            window_width: 999.0,
            ..Default::default()
        };

        // save() should return Ok but NOT touch the file
        // We can't directly test with the hardcoded path, but we can verify the guard logic
        let result = state.save();
        // save() will try to use get_sessions_path(), but the important thing is
        // the save_disabled check returns early with Ok(())
        assert!(result.is_ok(), "save() with save_disabled should return Ok");
    }

    /// Test: deleting a session harvests its cost and tokens into lifetime counters.
    #[test]
    fn test_delete_session_harvests_cost() {
        let mut state = SessionState::default();
        let session_id = state.create_session_raw(None);

        // Add a message with usage data
        if let Some(session) = state.sessions.get_mut(&session_id) {
            session.messages.push(crate::components::chat::Message {
                id: uuid::Uuid::new_v4(),
                author: "Hobbes".to_string(),
                content: crate::components::shared::MessageContent::Text {
                    content: "Hello".to_string(),
                    thought_signature: None,
                    thought_summary: None,
                },
                attachments: Vec::new(),
                comments: Vec::new(),
                created_at: chrono::Utc::now(),
                usage: Some(crate::components::shared::UsageData {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    thoughts_tokens: None,
                    cached_content_tokens: None,
                    cost: Some(0.0042),
                }),
            });
        }

        // Verify session cost before deletion
        assert!(
            (state.sessions.get(&session_id).unwrap().total_cost() - 0.0042).abs() < 0.00001
        );
        assert_eq!(state.lifetime_cost, 0.0);
        assert_eq!(state.lifetime_tokens, 0);

        // Delete the session
        state.delete_session_raw(&session_id);

        // Session should be gone, but lifetime counters should be updated
        assert!(state.sessions.get(&session_id).is_none());
        assert!((state.lifetime_cost - 0.0042).abs() < 0.00001);
        assert_eq!(state.lifetime_tokens, 150);
    }

    /// Test: lifetime counters accumulate across multiple session deletions.
    #[test]
    fn test_lifetime_counters_accumulate() {
        let mut state = SessionState::default();

        for cost in [0.01, 0.02, 0.03] {
            let sid = state.create_session_raw(None);
            if let Some(session) = state.sessions.get_mut(&sid) {
                session.messages.push(crate::components::chat::Message {
                    id: uuid::Uuid::new_v4(),
                    author: "Hobbes".to_string(),
                    content: crate::components::shared::MessageContent::Text {
                        content: "test".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                    },
                    attachments: Vec::new(),
                    comments: Vec::new(),
                    created_at: chrono::Utc::now(),
                    usage: Some(crate::components::shared::UsageData {
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        total_tokens: 150,
                        thoughts_tokens: None,
                        cached_content_tokens: None,
                        cost: Some(cost),
                    }),
                });
            }
            state.delete_session_raw(&sid);
        }

        assert!((state.lifetime_cost - 0.06).abs() < 0.00001);
        assert_eq!(state.lifetime_tokens, 450);
    }

    /// Test: lifetime counters survive JSON serialization roundtrip.
    #[test]
    fn test_lifetime_counters_serialization() {
        let mut state = SessionState::default();
        state.lifetime_cost = 1.2345;
        state.lifetime_tokens = 999_999;

        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let loaded: SessionState = serde_json::from_str(&json).expect("deserialize");

        assert!((loaded.lifetime_cost - 1.2345).abs() < 0.00001);
        assert_eq!(loaded.lifetime_tokens, 999_999);
    }

    /// Test: old session files without lifetime fields deserialize with defaults.
    #[test]
    fn test_backward_compat_no_lifetime_fields() {
        let old_json = r#"{
            "schema_version": 1,
            "sessions": {},
            "active_session_id": "",
            "window_width": 800.0,
            "window_height": 600.0,
            "tool_call_history": []
        }"#;

        let state: SessionState = serde_json::from_str(old_json).expect("deserialize old format");
        assert_eq!(state.lifetime_cost, 0.0);
        assert_eq!(state.lifetime_tokens, 0);
    }

    /// Test: store_pages does NOT overwrite partially-consumed entries.
    /// This is a critical correctness invariant — each continuation turn re-runs
    /// `build_prompt()`, which re-generates pages from the same tool results.
    /// Without the idempotency guard, `HashMap::insert` would reset already-consumed
    /// page state, causing the model to always see page 1 again.
    #[test]
    fn test_store_pages_idempotency() {
        let mut state = SessionState::default();

        // Store initial content: ID "page-A" with ~45 chars of remaining content
        let initial_pages = vec![
            ("page-A".to_string(), PagedResult {
                remaining_content: "Page 1 content\nPage 2 content\nPage 3 content".to_string(),
                tool_name: "test_tool".to_string(),
            }),
        ];
        state.store_pages(initial_pages);
        assert_eq!(
            state.page_queue.get("page-A").unwrap().remaining_content.len(),
            45 - 1  // "Page 1 content\nPage 2 content\nPage 3 content" = 44 chars
        );

        // Consume one page via fetch_next_page with a 15-char budget
        let (content, name, remaining) = state.fetch_next_page("page-A", 15).unwrap();
        assert_eq!(name, "test_tool");
        assert!(!content.is_empty(), "Should return a non-empty page");
        assert!(content.len() <= 15, "Page should respect budget");
        assert!(remaining > 0, "Should have remaining pages");

        // Record how much content remains after consuming one page
        let remaining_after_first = state.page_queue.get("page-A")
            .unwrap()
            .remaining_content
            .len();

        // Simulate continuation turn: store_pages called again with same ID but fresh content
        let re_generated = vec![
            ("page-A".to_string(), PagedResult {
                remaining_content: "Page 1 content\nPage 2 content\nPage 3 content".to_string(),
                tool_name: "test_tool".to_string(),
            }),
        ];
        state.store_pages(re_generated);

        // CRITICAL: the queue should still have the partially-consumed content (not reset)
        assert_eq!(
            state.page_queue.get("page-A").unwrap().remaining_content.len(),
            remaining_after_first,
            "store_pages must NOT overwrite partially-consumed entries"
        );

        // Verify we get the next portion of content (not starting over)
        let (content2, _, _remaining2) = state.fetch_next_page("page-A", 15).unwrap();
        assert!(!content2.is_empty(), "Should return next page content");
        assert!(content2.len() <= 15, "Second page should also respect budget");
    }

    /// Test: fetch_next_page dynamically sizes pages based on page_budget.
    /// Verifies that the same content produces differently-sized pages
    /// when called with different budgets.
    #[test]
    fn test_dynamic_page_sizing() {
        let mut state = SessionState::default();

        // Store content (~100 chars)
        let content = "Line one of the result.\nLine two of the result.\nLine three of the result.\nLine four of the result.";
        state.store_pages(vec![
            ("page-dynamic".to_string(), PagedResult {
                remaining_content: content.to_string(),
                tool_name: "dynamic_tool".to_string(),
            }),
        ]);

        // Fetch with a small budget (30 chars)
        let (page1, _, remaining1) = state.fetch_next_page("page-dynamic", 30).unwrap();
        assert!(page1.len() <= 30, "Small budget: page should be ≤30 chars, got {}", page1.len());
        assert!(remaining1 > 0, "Should have remaining content");

        // Fetch with a large budget (remaining content should fit in one page)
        let (page2, _, remaining2) = state.fetch_next_page("page-dynamic", 10000).unwrap();
        assert!(page2.len() > 30, "Large budget: page should be larger than small-budget page");
        assert_eq!(remaining2, 0, "Large budget should consume all remaining content");

        // All content consumed — entry should be cleaned up
        assert!(
            state.page_queue.get("page-dynamic").is_none(),
            "Entry should be removed after all content consumed"
        );

        // Concatenated pages should reproduce original content
        let reconstructed = format!("{}{}", page1, page2);
        assert_eq!(reconstructed, content, "Pages must reconstruct original content");
    }

    /// Test that delete_message_and_after cleans up stale tool_snapshot entries
    /// and resets conversation_summary to prevent undo-loop bugs.
    #[test]
    fn test_delete_message_and_after_cleans_up_turn_state() {
        use crate::components::chat::Message;
        use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus};

        let exec_id_1 = "exec-aaaa-1111";
        let exec_id_2 = "exec-bbbb-2222";
        let msg1_id = uuid::Uuid::new_v4();
        let msg2_id = uuid::Uuid::new_v4(); // user message
        let msg3_id = uuid::Uuid::new_v4(); // tool call message (will be deleted)
        let msg4_id = uuid::Uuid::new_v4(); // another tool call (will be deleted)

        let mut session = Session {
            id: "test-session".to_string(),
            name: "Test".to_string(),
            messages: vec![
                Message {
                    id: msg1_id,
                    author: "User".to_string(),
                    content: MessageContent::Text {
                        content: "Hello".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                    },
                    attachments: vec![],
                    comments: vec![],
                    created_at: chrono::Utc::now(),
                    usage: None,
                },
                Message {
                    id: msg2_id,
                    author: "User".to_string(),
                    content: MessageContent::Text {
                        content: "Do something".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                    },
                    attachments: vec![],
                    comments: vec![],
                    created_at: chrono::Utc::now(),
                    usage: None,
                },
                Message {
                    id: msg3_id,
                    author: "Hobbes".to_string(),
                    content: MessageContent::ToolCall(ToolCall {
                        execution_id: exec_id_1.to_string(),
                        server_name: "test-server".to_string(),
                        tool_name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                        status: ToolCallStatus::Completed,
                        response: "file contents".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                        cached_image_path: None,
                        result_summary: None,
                    }),
                    attachments: vec![],
                    comments: vec![],
                    created_at: chrono::Utc::now(),
                    usage: None,
                },
                Message {
                    id: msg4_id,
                    author: "Hobbes".to_string(),
                    content: MessageContent::ToolCall(ToolCall {
                        execution_id: exec_id_2.to_string(),
                        server_name: "test-server".to_string(),
                        tool_name: "write_file".to_string(),
                        arguments: "{}".to_string(),
                        status: ToolCallStatus::Completed,
                        response: "ok".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                        cached_image_path: None,
                        result_summary: None,
                    }),
                    attachments: vec![],
                    comments: vec![],
                    created_at: chrono::Utc::now(),
                    usage: None,
                },
            ],
            active_context: ActiveContext {
                conversation_summary: ConversationSummary {
                    summary: "User asked to read and write files.".to_string(),
                    sentiment: "neutral".to_string(),
                    current_task: String::new(),
                    entities: ConversationSummaryEntities::default(),
                },
                ..Default::default()
            },
            last_updated: chrono::Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: None,
            llm_provider: None,
            chat_model: None,
            loaded_skills: HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
        };

        // Simulate ToolCallSummarizer having inserted snapshots
        session.active_context.extra.insert(
            format!("tool_snapshot_{}", exec_id_1),
            serde_json::json!({"tool_name": "read_file", "result_summary": "completed"}),
        );
        session.active_context.extra.insert(
            format!("tool_snapshot_{}", exec_id_2),
            serde_json::json!({"tool_name": "write_file", "result_summary": "completed"}),
        );
        // Also add an unrelated extra entry that should NOT be removed
        session.active_context.extra.insert(
            "unrelated_key".to_string(),
            serde_json::json!("should survive"),
        );

        assert_eq!(session.active_context.extra.len(), 3);
        assert!(!session.active_context.conversation_summary.summary.is_empty());

        // Undo: delete msg2 and everything after (msg2, msg3, msg4)
        let deleted = session.delete_message_and_after(&msg2_id.to_string());
        assert_eq!(deleted, 3, "Should have deleted 3 messages");
        assert_eq!(session.messages.len(), 1, "Only first message should remain");

        // Tool snapshots for the deleted tool calls should be gone
        assert!(
            !session.active_context.extra.contains_key(&format!("tool_snapshot_{}", exec_id_1)),
            "tool_snapshot for exec_id_1 should be removed"
        );
        assert!(
            !session.active_context.extra.contains_key(&format!("tool_snapshot_{}", exec_id_2)),
            "tool_snapshot for exec_id_2 should be removed"
        );

        // Unrelated extra entries should survive
        assert!(
            session.active_context.extra.contains_key("unrelated_key"),
            "Unrelated extra entries must not be removed"
        );

        // Conversation summary should be reset
        assert!(
            session.active_context.conversation_summary.summary.is_empty(),
            "Conversation summary should be cleared after undo"
        );
    }

    #[test]
    fn test_store_upsert_roundtrip() {
        use crate::session_store::test_support as ts;
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        state.sessions.get_mut(&id).unwrap().name = "Roundtrip".to_string();

        ts::with_test_db(|conn| {
            ts::upsert(conn, state.sessions.get(&id).unwrap());
            assert_eq!(ts::get_row_name(conn, &id).as_deref(), Some("Roundtrip"));
            let data = ts::get_row_data(conn, &id).unwrap();
            let loaded: Session = serde_json::from_str(&data).unwrap();
            assert_eq!(loaded.id, id);
            assert_eq!(loaded.name, "Roundtrip");
        });
    }

    /// Out-of-order async saves must not clobber newer rows with older data.
    #[test]
    fn test_store_seq_guard_rejects_stale_writes() {
        use crate::session_store::test_support as ts;
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);

        ts::with_test_db(|conn| {
            state.sessions.get_mut(&id).unwrap().name = "Newer".to_string();
            ts::upsert_with_seq(conn, state.sessions.get(&id).unwrap(), 100);

            state.sessions.get_mut(&id).unwrap().name = "Stale".to_string();
            ts::upsert_with_seq(conn, state.sessions.get(&id).unwrap(), 50);

            assert_eq!(
                ts::get_row_name(conn, &id).as_deref(),
                Some("Newer"),
                "A write with a lower seq must not overwrite a newer row"
            );
        });
    }

    /// One-time JSON→SQLite import: live sessions win over archive lines on
    /// id collision, meta carries over, and a second run is a no-op.
    #[test]
    fn test_json_to_sqlite_import() {
        use crate::session_store::test_support as ts;
        let dir = tempfile::tempdir().unwrap();

        // Live state: two sessions
        let mut live = SessionState::default();
        let id_a = live.create_session_raw(None);
        let id_b = live.create_session_raw(None);
        live.sessions.get_mut(&id_a).unwrap().name = "Live A".into();
        live.sessions.get_mut(&id_b).unwrap().name = "Live B".into();
        live.lifetime_cost = 1.25;
        live.schema_version = CURRENT_SESSION_SCHEMA_VERSION;
        let json_path = dir.path().join("sessions.json");
        fs::write(&json_path, serde_json::to_string(&live).unwrap()).unwrap();

        // Archive: an old copy of session A (must lose) + a unique session C
        let mut old_a = live.sessions.get(&id_a).unwrap().clone();
        old_a.name = "Stale A".into();
        let mut state_c = SessionState::default();
        let id_c = state_c.create_session_raw(None);
        state_c.sessions.get_mut(&id_c).unwrap().name = "Archived C".into();
        let archive_path = dir.path().join("sessions-archive.jsonl");
        fs::write(
            &archive_path,
            format!(
                "{}\n{}\nnot-json\n",
                serde_json::to_string(&old_a).unwrap(),
                serde_json::to_string(state_c.sessions.get(&id_c).unwrap()).unwrap()
            ),
        )
        .unwrap();

        ts::with_test_db(|conn| {
            let (live_n, archive_n) =
                ts::run_import(conn, Some(json_path.clone()), Some(archive_path.clone())).unwrap();
            assert_eq!(live_n, 2);
            assert_eq!(archive_n, 1, "collision + junk line must be skipped");
            assert_eq!(ts::count(conn), 3);
            assert_eq!(ts::get_row_name(conn, &id_a).as_deref(), Some("Live A"));
            assert_eq!(ts::get_row_name(conn, &id_c).as_deref(), Some("Archived C"));
            assert_eq!(
                ts::meta(conn, crate::session_store::META_LIFETIME_COST).as_deref(),
                Some("1.25")
            );

            // Second run must be a no-op (migration marker set)
            let (l2, a2) = ts::run_import(conn, Some(json_path), Some(archive_path)).unwrap();
            assert_eq!((l2, a2), (0, 0));
            assert_eq!(ts::count(conn), 3);
        });
    }

    /// Real-data migration rehearsal. Skipped unless the env vars point at
    /// copies of a real sessions.json / sessions-archive.jsonl:
    ///   HOBBES_IMPORT_TEST_JSON=... HOBBES_IMPORT_TEST_ARCHIVE=... \
    ///     cargo test --release real_data_import -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_data_import() {
        use crate::session_store::test_support as ts;
        let json = std::env::var("HOBBES_IMPORT_TEST_JSON").ok().map(std::path::PathBuf::from);
        let archive = std::env::var("HOBBES_IMPORT_TEST_ARCHIVE").ok().map(std::path::PathBuf::from);
        if json.is_none() && archive.is_none() {
            eprintln!("real_data_import: no env vars set, skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sessions.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        crate::session_store::create_schema(&conn).unwrap();
        let start = std::time::Instant::now();
        let (live_n, archive_n) = ts::run_import(&conn, json, archive).unwrap();
        let db_size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "real_data_import: {live_n} live + {archive_n} archived sessions in {:.1}s, db size {:.1} MB",
            start.elapsed().as_secs_f64(),
            db_size as f64 / 1e6
        );
        assert!(ts::count(&conn) as usize >= live_n + archive_n);
    }

    /// Legacy archive lines (pre-v2 message formats) must import via the
    /// raw-JSON migration fallback.
    #[test]
    fn test_legacy_session_import_fallback() {
        let old_format = make_old_session_json(r#"{
            "id": "0b0e9e2e-49e2-4a4d-bd58-9d1b4a55b573",
            "author": "User",
            "content": { "Text": "hello from the past" },
            "attachments": [],
            "comments": [],
            "created_at": "2026-01-01T00:00:00Z"
        }"#);
        let state = SessionState::migrate_from_raw_json(&old_format).unwrap();
        let session = state.sessions.get("test-session-1").unwrap();
        assert_eq!(session.messages.len(), 1);
    }
}
