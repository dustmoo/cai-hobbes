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
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_to_empty"
    )]
    pub user_name: String,
    #[serde(flatten)]
    pub other_entities: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummary {
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_to_empty"
    )]
    pub summary: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_to_empty"
    )]
    pub sentiment: String,
    /// The active task or goal the user is currently pursuing.
    /// Populated automatically by the summarizer on each turn — no model tool call needed.
    /// Injected as part of `conversation_summary` in the system context (Tier 3).
    /// For small-context models this acts as a goal anchor so the task survives history scrolling.
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_to_empty"
    )]
    pub current_task: String,
    #[serde(default)]
    pub entities: ConversationSummaryEntities,
    /// AI-proposed 2-4 word tab topic, alongside the rolling summary.
    /// Validated/normalized (`normalize_tab_title`) before being applied to
    /// `Session.tab_title` — never applied raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_title: Option<String>,
    /// AI-proposed single-emoji tab icon, alongside the rolling summary.
    /// Validated/normalized (`normalize_tab_emoji`) before being applied to
    /// `Session.tab_emoji` — never applied raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_emoji: Option<String>,
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

/// Fallback tab emoji used whenever the summarizer's proposal is missing or
/// fails validation.
pub const FALLBACK_TAB_EMOJI: &str = "⭐";

/// Validate an AI-proposed tab emoji: exactly one extended grapheme cluster
/// (so multi-codepoint sequences like `🏗️` — base + variation selector —
/// still count as one "emoji"), short, and not plain ASCII text. Falls back
/// to [`FALLBACK_TAB_EMOJI`] on anything else (missing, empty, multiple
/// clusters, or ASCII).
pub fn normalize_tab_emoji(raw: Option<&str>) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return FALLBACK_TAB_EMOJI.to_string();
    };
    let mut graphemes = raw.graphemes(true);
    let Some(first) = graphemes.next() else {
        return FALLBACK_TAB_EMOJI.to_string();
    };
    if graphemes.next().is_some() {
        return FALLBACK_TAB_EMOJI.to_string(); // more than one grapheme cluster
    }
    if first.len() > 8 || first.is_ascii() {
        return FALLBACK_TAB_EMOJI.to_string();
    }
    first.to_string()
}

/// Normalize an AI-proposed tab topic: trim, strip wrapping quotes, cap at
/// 40 chars. Returns `None` for a missing/empty/quotes-only proposal.
pub fn normalize_tab_title(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let stripped = raw
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '\u{201c}' | '\u{201d}'))
        .trim();
    if stripped.is_empty() {
        return None;
    }
    let truncated: String = stripped.chars().take(40).collect();
    let truncated = truncated.trim_end().to_string();
    if truncated.is_empty() {
        None
    } else {
        Some(truncated)
    }
}

/// User messages a tab needs before its title stops being provisional. The
/// first title lands early (after the first real exchange) so every tab —
/// including background/dispatched ones — gets an identity quickly; the first
/// summary once the conversation reaches this many user messages may replace
/// it once, and then the title locks for good.
pub const TAB_TITLE_MATURE_USER_MESSAGES: usize = 3;

/// Applies an AI-proposed tab emoji/topic from a freshly computed
/// [`ConversationSummary`] to `session.tab_title`/`tab_emoji`, normalizing
/// both first (never applied raw) and honoring the `name_user_set` guard — a
/// user rename always wins, forever.
///
/// Nothing is written until the tab has at least one user message: a summary
/// of an empty tab (e.g. the profile-switch `ForceRefresh`, which sees only a
/// "current Composio profile" system note) otherwise names the tab after that
/// note ("Profile Setup") before the user has said anything.
///
/// The TITLE is written at most twice per tab:
/// 1. A provisional title from the first summary that proposes one
///    (`tab_title_provisional = true` while the tab is still short).
/// 2. One retitle from the first summary after the tab reaches
///    [`TAB_TITLE_MATURE_USER_MESSAGES`] user messages — an opening exchange
///    is often too thin to name the conversation it turns into. The title
///    then locks (`tab_title_provisional = false`).
///
/// After that, later summaries (each idle pause, each tool-call turn) are a
/// no-op. Without the lock the tab would keep re-proposing a title as the
/// conversation's topic drifts — a tab you'd already learned to recognize
/// would relabel itself out from under you while switching between a
/// handful of tabs, which is more disorienting than a picked title going
/// slightly stale.
///
/// The lock is keyed on `tab_title` specifically, NOT `tab_emoji` —
/// `normalize_tab_emoji` always returns a fallback glyph even when the raw
/// proposal was empty/invalid (it never produces `None`), so an early,
/// low-quality summary reliably writes SOME emoji while leaving `tab_title`
/// empty. Locking on emoji too would freeze that tab on a bare fallback
/// emoji + date forever. Emoji may keep refreshing until a real title
/// arrives; on the mature retitle, emoji is only replaced together with a
/// title so the pair stays consistent.
///
/// Every caller that computes a `ConversationSummary` (the proactive
/// tool-call-turn summarizer in `stream_manager.rs`, and the idle-inactivity
/// scheduler in `summarization_scheduler.rs`) must call this, or a tab's
/// emoji/topic silently never gets set — this was missed on the scheduler's
/// two call sites when the feature first shipped (2026-09-16): they wrote
/// `session.active_context.conversation_summary` but never read
/// `summary.tab_title`/`tab_emoji` off of it, so ordinary new tabs (no tool
/// calls, first summarized via the idle path) never got titled at all.
///
/// Returns `Some((emoji, title))` when it actually wrote something, for the
/// caller to journal as `SessionEvent::TabTitled`; `None` when the summary
/// carried no proposal, the tab's identity is locked (user-set or a final AI
/// title), the tab is still too short to retitle, or it has no user message.
pub fn apply_tab_identity_from_summary(
    session: &mut Session,
    summary: &ConversationSummary,
) -> Option<(Option<String>, Option<String>)> {
    if session.name_user_set {
        return None;
    }
    let user_messages = session.user_message_count();
    if user_messages == 0 {
        return None;
    }
    let mature = user_messages >= TAB_TITLE_MATURE_USER_MESSAGES;
    let retitling = session.tab_title.is_some();
    if retitling && !(session.tab_title_provisional && mature) {
        return None;
    }
    let raw_title = summary.tab_title.as_deref();
    let raw_emoji = summary.tab_emoji.as_deref();
    if raw_title.is_none() && raw_emoji.is_none() {
        return None;
    }
    let title = normalize_tab_title(raw_title);
    if retitling && title.is_none() {
        // Keep the provisional title; a later summary gets another try.
        return None;
    }
    let emoji = normalize_tab_emoji(raw_emoji);
    if let Some(title) = &title {
        session.tab_title = Some(title.clone());
        session.tab_title_provisional = !mature;
    }
    session.tab_emoji = Some(emoji.clone());
    Some((Some(emoji), title))
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
    /// Per-session LLM connector override (stable connector ID).
    /// None → fall back to `llm_provider` kind match, then the global active connector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_connector_id: Option<String>,
    /// Per-session LLM provider-kind override. Legacy pin (pre-multi-connector)
    /// and downgrade-safety mirror of `llm_connector_id`'s kind — the pickers
    /// set both. None → follow the global active connector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_provider: Option<crate::settings::LlmProvider>,
    /// Per-session chat model override. None → the effective provider's configured model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_model: Option<String>,
    /// Planner project this chat belongs to (auto-tagged by the AI from
    /// conversation content, or set by the user via the chat-bar chip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// True after any manual set/clear of the project tag — the auto-tagger
    /// never touches a session the user has decided about.
    #[serde(default)]
    pub project_tag_user_set: bool,
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
    /// The model's latest note to its future self (HOBBES_WRITE_HANDOFF).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<crate::handoff::HandoffNote>,
    /// Tool calls executed during the current turn of THIS session.
    /// Per-session (never global): two tabs streaming concurrently must not
    /// see each other's tool calls, and cancelling one tab must not wipe the
    /// other's in-flight history. Cleared on send and on cancel; drained by
    /// `ToolCallSummarizer` at turn end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_history: Vec<crate::components::shared::ToolCallRecord>,
    /// AI-derived one-emoji "icon" for this chat's tab, set by the rolling
    /// summarizer (or programmatically for dispatch review tabs). `None`
    /// until the first summary lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_emoji: Option<String>,
    /// AI-derived 2-4 word topic for this chat's tab. Overridden forever by
    /// a user rename (see `name_user_set`); `None` until the first summary
    /// lands (or set programmatically for dispatch review tabs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_title: Option<String>,
    /// True once the user has explicitly renamed this tab (from the tab bar
    /// or History). A user rename wins forever: the summarizer never
    /// overwrites `name`/`tab_title` again once this is set.
    #[serde(default)]
    pub name_user_set: bool,
    /// True while `tab_title` is an early AI guess that the first summary
    /// after [`TAB_TITLE_MATURE_USER_MESSAGES`] user messages may replace
    /// once. False for locked titles, programmatic titles, and every tab
    /// saved before this field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tab_title_provisional: bool,
    /// Forward-only history trim anchor: the id of the first non-pinned
    /// message the prompt builder sends (everything before it, except the
    /// pinned first user message, is left out). It only ever moves forward,
    /// in whole chunks, to a user text message (a turn boundary), so the
    /// outgoing message prefix stays byte-stable between requests and a
    /// provider prompt cache keeps hitting. Set from
    /// `PromptBuildResult::history_anchor` after each build; `None` (or an id
    /// that no longer exists) means "recompute from the start of the chat".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_anchor: Option<uuid::Uuid>,
    /// Turn context ledger: live context (today's plan, scratchpad, loaded
    /// skills, ...) and each request's turn-scoped tail, recorded as entries
    /// anchored after the message they follow. The prompt builder splices
    /// them into the outgoing history as `ChatRole::System` messages, so that
    /// state reaches the model as an append-only part of the conversation
    /// instead of a per-request system block. Written only from
    /// `PromptBuildResult::ledger_entries`; entries whose anchor message is
    /// gone are dropped by `prune_orphaned_ledger`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_ledger: Vec<LedgerEntry>,
    /// Dispatch relay, worker side: the chat that dispatched this tab and the
    /// to-do it works on (`todo::relay`). Messages travel only along this
    /// edge. `None` for every tab not dispatched from a chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_link: Option<crate::todo::relay::DispatchLink>,
    /// Dispatch relay, parent side: worker tabs dispatched from this chat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatched_workers: Vec<String>,
}

/// What a [`LedgerEntry`] records.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A context value that changed at a user-turn boundary (empty body =
    /// the value was cleared).
    Ledger,
    /// One request's turn-scoped tail: the clock plus turn instructions.
    Tail,
    /// Part of the snapshot written when the history window was trimmed;
    /// rendered after the pinned first message while `trim_anchor` is the
    /// session's history anchor.
    Trim,
}

/// One entry of `Session.context_ledger`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LedgerEntry {
    pub id: uuid::Uuid,
    /// The message this entry renders after.
    pub after_message_id: uuid::Uuid,
    pub kind: LedgerKind,
    /// Context key (`planner_today`, `scratchpad`, ...); `tail` for tails.
    pub key: String,
    /// Canonical body. Empty on a `Ledger` entry means "cleared".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// `Trim` entries only: the history anchor whose window they belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_anchor: Option<uuid::Uuid>,
}

/// Stamp `created_at` on a message that is already in `messages`, without
/// breaking the vec's `created_at`-monotonic ordering.
///
/// Ordering is load-bearing in two places that must agree: the prompt builder
/// walks `session.messages` in vec order, while the journal projector reinserts
/// messages at their `created_at`-sorted position
/// (`session_events::upsert_message`). A vec whose order disagrees with its
/// own timestamps therefore renders and replays differently — so a restamp is
/// clamped into the window its neighbours leave open, and skipped entirely
/// when they leave none (legacy rows that are already out of order).
///
/// Returns the stamp actually applied, or `None` when the message isn't there.
pub fn restamp_message(
    messages: &mut [crate::components::chat::Message],
    message_id: &uuid::Uuid,
    ts: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let idx = messages.iter().position(|m| m.id == *message_id)?;
    let lower = idx.checked_sub(1).map(|i| messages[i].created_at);
    let upper = messages.get(idx + 1).map(|m| m.created_at);
    if let (Some(lo), Some(hi)) = (lower, upper) {
        if lo > hi {
            return Some(messages[idx].created_at);
        }
    }
    let mut ts = ts;
    if let Some(lo) = lower {
        ts = ts.max(lo);
    }
    if let Some(hi) = upper {
        ts = ts.min(hi);
    }
    messages[idx].created_at = ts;
    Some(ts)
}

/// Move a message to the end of `messages` and stamp it at `ts` (never earlier
/// than the message it now follows), returning the moved message.
///
/// Used when a turn's prose reply is finalized *after* the tool calls it
/// follows: the reply's placeholder is pushed at turn start, so leaving it in
/// place renders the turn's work BELOW the answer that work produced. Moving
/// the message rather than only restamping it keeps the vec order and the
/// timestamps in agreement, which is what makes `session_events::project`
/// replay the same layout the live session renders.
pub fn move_message_to_end(
    messages: &mut Vec<crate::components::chat::Message>,
    message_id: &uuid::Uuid,
    ts: DateTime<Utc>,
) -> Option<crate::components::chat::Message> {
    let idx = messages.iter().position(|m| m.id == *message_id)?;
    if idx + 1 == messages.len() {
        restamp_message(messages, message_id, ts);
        return messages.last().cloned();
    }
    let mut msg = messages.remove(idx);
    msg.created_at = match messages.last() {
        Some(last) => ts.max(last.created_at),
        None => ts,
    };
    messages.push(msg.clone());
    Some(msg)
}

impl Session {
    /// Number of messages the user wrote in this tab.
    pub fn user_message_count(&self) -> usize {
        self.messages.iter().filter(|m| m.author == "User").count()
    }

    /// True when a summary could still set or refine this tab's title: it has
    /// a user message, no user rename, and either no title yet or a
    /// provisional one that is now old enough to be replaced. The turn-end
    /// summarizer uses this to summarize tabs the idle scheduler never
    /// reaches (e.g. background tabs from HOBBES_DISPATCH).
    pub fn wants_tab_identity(&self) -> bool {
        if self.name_user_set {
            return false;
        }
        let user_messages = self.user_message_count();
        if user_messages == 0 {
            return false;
        }
        self.tab_title.is_none()
            || (self.tab_title_provisional
                && user_messages >= TAB_TITLE_MATURE_USER_MESSAGES)
    }

    /// The tab's headline label: the user's own name wins forever, then the
    /// AI-derived topic, then the creation-date placeholder (`name`, before
    /// any rename or summary).
    pub fn display_title(&self) -> String {
        if self.name_user_set {
            self.name.clone()
        } else if let Some(title) = &self.tab_title {
            title.clone()
        } else {
            self.name.clone()
        }
    }

    /// The AI-derived topic layer for the tab strip — `None` until the first
    /// summary lands. A user rename replaces it forever (keeping the emoji).
    pub fn tab_topic(&self) -> Option<String> {
        if self.name_user_set {
            Some(self.name.clone())
        } else {
            self.tab_title.clone()
        }
    }

    /// Short creation date ("Sep 16") for the tab strip. `Session` has no
    /// dedicated `created_at` field, so this parses it out of `name`, which
    /// is stamped as `"%b %d - %I:%M %p"` at creation (`create_session_raw`)
    /// and is never touched again except by a user rename. Once renamed,
    /// that original stamp is gone, so this falls back to `last_updated`
    /// (the closest thing to a creation date this struct still carries),
    /// shown in local time like the stamp.
    pub fn short_date(&self) -> String {
        match self.name.split_once(" - ") {
            Some((date_part, _)) => date_part.to_string(),
            None => self.last_updated.with_timezone(&chrono::Local).format("%b %d").to_string(),
        }
    }

    /// See [`restamp_message`].
    pub fn restamp_message(
        &mut self,
        message_id: &uuid::Uuid,
        ts: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        restamp_message(&mut self.messages, message_id, ts)
    }

    /// See [`move_message_to_end`].
    pub fn move_message_to_end(
        &mut self,
        message_id: &uuid::Uuid,
        ts: DateTime<Utc>,
    ) -> Option<crate::components::chat::Message> {
        move_message_to_end(&mut self.messages, message_id, ts)
    }

    /// When this tab's earliest pending timer is due, if it has one.
    /// See also [`SessionState::timer_waits`].
    pub fn next_pending_timer(&self) -> Option<DateTime<Utc>> {
        self.scheduled_timers
            .iter()
            .filter(|t| t.status == crate::timers::TimerStatus::Pending)
            .map(|t| t.fire_at)
            .min()
    }

    pub fn increment_turn_count(&mut self) {
        self.current_ai_turn_count += 1;
    }

    pub fn reset_turn_count(&mut self) {
        self.current_ai_turn_count = 0;
        self.watch_word_recovery_count = 0;
    }

    /// Truncate history from `message_id` (inclusive) to the end, applying the
    /// derived undo effects. This is the single home of the rewind rules —
    /// shared by the legacy in-place undo (`delete_message_and_after`) and by
    /// the journal projector (`session_events::project` on `RewoundTo`):
    ///
    /// - Harvest cost/token usage from the deleted messages into
    ///   `accumulated_cost`/`accumulated_tokens` so session totals never drop.
    /// - Remove `tool_snapshot_*` extras for deleted tool calls (inserted by
    ///   ToolCallSummarizer, serialized into SYSTEM_CONTEXT via flatten —
    ///   without this the LLM sees phantom results and loops on stale context).
    /// - Reset `conversation_summary` so stale context from deleted turns
    ///   can't leak into future prompts (re-populated on the next turn).
    ///
    /// Pure with respect to clocks and the journal: no `Utc::now()`, no event
    /// logging — callers own those side effects.
    pub fn truncate_from_message(&mut self, message_id: &str) -> usize {
        let Ok(uuid) = uuid::Uuid::parse_str(message_id) else {
            return 0;
        };
        let Some(index) = self.messages.iter().position(|m| m.id == uuid) else {
            return 0;
        };
        let count = self.messages.len() - index;
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
        self.prune_orphaned_ledger();

        for exec_id in &deleted_execution_ids {
            let key = format!("tool_snapshot_{}", exec_id);
            if self.active_context.extra.remove(&key).is_some() {
                tracing::debug!("Removed stale tool snapshot after undo: {}", key);
            }
        }

        if count > 0 {
            self.active_context.conversation_summary = Default::default();
            tracing::debug!(
                "Reset conversation_summary after undo ({} messages removed)",
                count
            );
        }
        count
    }

    /// End every tool call still `Running` in this session with `reason`
    /// ([`ToolCall::mark_cancelled`](crate::components::shared::ToolCall::mark_cancelled)).
    /// Calls held for approval are `PermissionRequest`s and stay held.
    /// Returns the changed messages, for the caller to journal as
    /// `ToolResult`s.
    pub fn cancel_running_calls(&mut self, reason: &str, now: DateTime<Utc>) -> Vec<crate::components::chat::Message> {
        use crate::components::shared::{MessageContent, ToolCallStatus};
        let mut changed = Vec::new();
        for m in &mut self.messages {
            if let MessageContent::ToolCall(tc) = &mut m.content {
                if tc.status == ToolCallStatus::Running {
                    tc.mark_cancelled(reason, now);
                    changed.push(m.clone());
                }
            }
        }
        changed
    }

    /// Drop ledger entries whose anchor message is gone (rewind, cancel,
    /// message removal), and trim snapshots whose window anchor is gone.
    pub fn prune_orphaned_ledger(&mut self) {
        if self.context_ledger.is_empty() {
            return;
        }
        let ids: std::collections::HashSet<uuid::Uuid> =
            self.messages.iter().map(|m| m.id).collect();
        self.context_ledger.retain(|e| {
            ids.contains(&e.after_message_id)
                && e.trim_anchor.is_none_or(|a| ids.contains(&a))
        });
    }

    /// Legacy in-place undo. Journal-complete sessions should go through
    /// `session_events::rewind_session_state` instead (rewind-as-replay);
    /// this path remains for pre-journal sessions and journals its own
    /// `RewoundTo` so the history is recorded either way.
    pub fn delete_message_and_after(&mut self, message_id: &str) -> usize {
        let count = self.truncate_from_message(message_id);
        if count > 0 {
            // The anchor is the seq of the first journaled event referencing
            // the deleted message; 0 when it predates the journal.
            let rewind_seq =
                crate::session_store::first_event_seq_for_message(&self.id, message_id)
                    .unwrap_or(0);
            crate::session_events::log_event(
                &self.id,
                crate::session_events::SessionEvent::RewoundTo {
                    seq: rewind_seq,
                    message_id: message_id.to_string(),
                },
            );
            self.last_updated = Utc::now();
        }
        count
    }

    /// Calculate total cost for all messages in this session.
    /// Includes `accumulated_cost` from deleted messages so the counter
    /// never drops when the user prunes conversation history.
    pub fn total_cost(&self) -> f64 {
        let message_cost: f64 = self
            .messages
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
        let message_tokens: i32 = self
            .messages
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

        if turns > 0.0 { tokens / turns } else { 0.0 }
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
    let instance = match session {
        Some(s) => settings.connector_for_session(s),
        None => settings.active_connector(),
    };
    let model = match session {
        Some(s) => settings.chat_model_for_session(s),
        None => settings.active_chat_model(),
    };
    let (tuning, provider_context_tokens) = settings.tuning_and_window(instance, &model);

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
            self.page_queue.insert(tool_call_id.to_string(), entry);
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
                    tool_name,
                    tool_call_id,
                    remaining
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
        let instance = match session {
            Some(s) => settings.connector_for_session(s),
            None => settings.active_connector(),
        };
        let model = match session {
            Some(s) => settings.chat_model_for_session(s),
            None => settings.active_chat_model(),
        };
        let (tuning, context_tokens) = settings.tuning_and_window(instance, &model);
        let max_scratchpad_chars: usize = context_tokens
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
            session.scratchpad = content.clone();
            crate::session_events::log_event(
                session_id,
                crate::session_events::SessionEvent::ScratchpadSet { content },
            );
            tracing::info!(
                "HOBBES_UPDATE_SCRATCHPAD: Scratchpad updated ({} chars, limit {})",
                char_count,
                max_scratchpad_chars
            );
            (
                crate::components::shared::ToolCallStatus::Completed,
                format!(
                    "Scratchpad updated ({} chars). Content is now persistent for this session.",
                    char_count
                ),
            )
        } else {
            tracing::warn!(
                "HOBBES_UPDATE_SCRATCHPAD: Session '{}' not found",
                session_id
            );
            (
                crate::components::shared::ToolCallStatus::Error,
                format!("Session '{}' not found", session_id),
            )
        }
    }

    /// HOBBES_SET_TIMER: schedule a timer/reminder for this session.
    ///
    /// `until_worker_done` makes it an intention (fire when that to-do's
    /// worker finishes, or at the deadline). Whether the to-do can be waited
    /// on is checked by the dispatcher, which holds the planner
    /// (`timers::validate_worker_target`); `todo_title` only names it in the
    /// confirmation.
    pub fn handle_set_timer(
        &mut self,
        args_json: &serde_json::Value,
        session_id: &str,
        todo_title: impl Fn(&str) -> Option<String>,
    ) -> (crate::components::shared::ToolCallStatus, String) {
        use crate::components::shared::ToolCallStatus;
        use crate::timers::{
            MAX_DELAY_SECS, MIN_DELAY_SECS, ScheduledTimer, TimerMode, TimerStatus, WakeCondition,
            clean_reason, parse_duration_secs,
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
        let delay_secs =
            match delay_secs {
                Some(n) => n,
                None => return (
                    ToolCallStatus::Error,
                    "Provide 'delay_secs' (integer seconds) or 'delay' (e.g. \"10m\", \"1h30m\")."
                        .to_string(),
                ),
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
                );
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
        let reason = clean_reason(args_json.get("reason").and_then(|v| v.as_str()));
        let until = args_json
            .get("until_worker_done")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| WakeCondition::WorkerDone { todo_id: id.to_string() });

        if mode == TimerMode::Prompt && prompt.as_deref().map(str::trim).unwrap_or("").is_empty() {
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
            reason,
            until,
            fired_at: None,
            fire_cause: None,
        };
        let summary = timer.summary_with(todo_title);
        let deadline_note = if timer.until.is_some() { " at the latest" } else { "" };

        {
            let Some(session) = self.sessions.get_mut(session_id) else {
                return (
                    ToolCallStatus::Error,
                    format!("Session '{}' not found", session_id),
                );
            };
            session.scheduled_timers.push(timer.clone());
        }
        crate::session_events::log_event(
            session_id,
            crate::session_events::SessionEvent::TimerCreated { timer },
        );
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
            format!("Timer set — fires in {}{}. {}", human, deadline_note, summary),
        )
    }

    /// HOBBES_LIST_TIMERS: list this session's pending timers.
    pub fn handle_list_timers(
        &self,
        session_id: &str,
        todo_title: impl Fn(&str) -> Option<String>,
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
                    .map(|t| t.summary_with(&todo_title))
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
                "Missing 'timer_id' (from the timer's confirmation or HOBBES_LIST_TIMERS)."
                    .to_string(),
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
            crate::session_events::log_event(
                session_id,
                crate::session_events::SessionEvent::TimerCancelled {
                    timer_id: timer_id.clone(),
                },
            );
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

    /// Persist the history trim anchor a prompt build resolved
    /// (`PromptBuildResult::history_anchor`) onto `session_id`.
    /// Call this after `PromptBuilder::build_prompt` in a separate write scope,
    /// like `store_pages`. Returns true when the stored anchor changed.
    pub fn set_history_anchor(&mut self, session_id: &str, anchor: Option<uuid::Uuid>) -> bool {
        match self.sessions.get_mut(session_id) {
            Some(session) if session.history_anchor != anchor => {
                session.history_anchor = anchor;
                true
            }
            _ => false,
        }
    }

    /// True when `anchor` differs from the one stored on `session_id`, so a
    /// caller can skip taking a write guard (and re-rendering) when nothing
    /// moved.
    pub fn history_anchor_changed(&self, session_id: &str, anchor: Option<uuid::Uuid>) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|s| s.history_anchor != anchor)
    }

    /// Append the ledger entries a prompt build produced
    /// (`PromptBuildResult::ledger_entries`) to `session_id` and journal each
    /// one. Call after `PromptBuilder::build_prompt` in a separate write
    /// scope, like `set_history_anchor`. Entries already present (same id)
    /// are skipped. Returns true when anything was appended.
    pub fn append_ledger_entries(&mut self, session_id: &str, entries: Vec<LedgerEntry>) -> bool {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        let fresh: Vec<LedgerEntry> = entries
            .into_iter()
            .filter(|e| !session.context_ledger.iter().any(|x| x.id == e.id))
            .collect();
        if fresh.is_empty() {
            return false;
        }
        session.context_ledger.extend(fresh.iter().cloned());
        crate::session_events::log_events(
            session_id,
            fresh
                .into_iter()
                .map(|entry| crate::session_events::SessionEvent::LedgerEntryAdded { entry })
                .collect(),
        );
        true
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
                    self.page_queue
                        .get(&id)
                        .map_or(0, |p| p.remaining_content.len())
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
            let keys_to_remove: Vec<String> = self
                .page_queue
                .keys()
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
        let mut active_session_id = store::meta_get(store::META_ACTIVE_SESSION).unwrap_or_default();
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
        // Legacy: tool-call history used to live in a single global meta
        // blob. It is now per-session (`Session.tool_call_history`), so the
        // old key is discarded on load and never written again; the stale
        // row is removed once, here.
        store::meta_delete(store::META_TOOL_CALL_HISTORY);

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
        // Unparseable data must be an error, not an empty state: the caller
        // (the one-time JSON→SQLite import) treats Ok as a successful import
        // and permanently stamps the migrated marker.
        let parsed = serde_json::from_str::<serde_json::Value>(data).map_err(|e| {
            tracing::error!(
                "Migration could not parse sessions.json as JSON: {e}. NOT treating as empty state."
            );
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("sessions.json is unparseable: {e}"),
            )
        })?;
        {
            let mut value = parsed;
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
            // Legacy global "tool_call_history" is intentionally dropped:
            // history is per-session now and is turn-local anyway.
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
        let meta = self.collect_meta_kv();
        store::write_rows_and_meta(&rows, &meta).map_err(std::io::Error::other)?;
        store::mark_rows_saved(&rows);
        Ok(())
    }

    /// Meta key/value pairs to persist alongside session rows.
    /// Seqs are drawn here, at collect time on the calling thread — not at
    /// write time on the blocking pool — so overlapping saves whose writes
    /// land out of order cannot overwrite newer meta with an older snapshot.
    fn collect_meta_kv(&self) -> Vec<(String, String, i64)> {
        use crate::session_store as store;
        vec![
            (
                store::META_SCHEMA_VERSION.to_string(),
                self.schema_version.to_string(),
                store::next_seq(),
            ),
            (
                store::META_ACTIVE_SESSION.to_string(),
                self.active_session_id.clone(),
                store::next_seq(),
            ),
            (
                store::META_WINDOW_WIDTH.to_string(),
                self.window_width.to_string(),
                store::next_seq(),
            ),
            (
                store::META_WINDOW_HEIGHT.to_string(),
                self.window_height.to_string(),
                store::next_seq(),
            ),
            (
                store::META_LIFETIME_COST.to_string(),
                self.lifetime_cost.to_string(),
                store::next_seq(),
            ),
            (
                store::META_LIFETIME_TOKENS.to_string(),
                self.lifetime_tokens.to_string(),
                store::next_seq(),
            ),
        ]
    }

    /// Hydrated tabs with a pending timer, and when the earliest is due.
    pub fn timer_waits(&self) -> std::collections::HashMap<String, DateTime<Utc>> {
        self.sessions
            .iter()
            .filter_map(|(id, s)| s.next_pending_timer().map(|at| (id.clone(), at)))
            .collect()
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
            llm_connector_id: None,
            llm_provider: None,
            chat_model: None,
            project_id: None,
            project_tag_user_set: false,
            loaded_skills: HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
            handoff: None,
            tool_call_history: Vec::new(),
            tab_emoji: None,
            tab_title: None,
            name_user_set: false,
            tab_title_provisional: false,
            history_anchor: None,
            context_ledger: Vec::new(),
            dispatch_link: None,
            dispatched_workers: Vec::new(),
        };
        // Birth event: a journal that starts with SessionCreated is
        // "journal-complete" and can be rebuilt from nothing via project().
        // The initial profile binding is journaled alongside so replay
        // reproduces it (create_session_raw is its assignment site).
        let mut birth_events = vec![crate::session_events::SessionEvent::SessionCreated {
            id: new_id.clone(),
            name: new_session.name.clone(),
        }];
        if let Some(profile) = &new_session.composio_profile {
            birth_events.push(crate::session_events::SessionEvent::ComposioProfileSet {
                profile: Some(profile.clone()),
            });
        }
        crate::session_events::log_events(&new_id, birth_events);

        self.sessions.insert(new_id.clone(), new_session);
        self.active_session_id = new_id.clone();
        new_id
    }

    /// Fork a journal-complete session: copy its journal up to `at_seq`
    /// (inclusive; `None` = whole journal) into a fresh session id, rebuild
    /// the hydrated `Session` via `project()`, and insert it into state as the
    /// active session. The caller persists (`save_async`) and opens the tab.
    ///
    /// Only journal-complete sources (journal starts with `SessionCreated`)
    /// can be forked — pre-journal sessions return a clear error.
    pub fn fork_session(&mut self, source_id: &str, at_seq: Option<i64>) -> Result<String, String> {
        let source_name = self
            .sessions
            .get(source_id)
            .map(|s| s.name.clone())
            .or_else(|| {
                crate::session_store::metadata_by_ids(&[source_id.to_string()])
                    .pop()
                    .map(|m| m.name)
            })
            .ok_or_else(|| format!("Session '{source_id}' not found"))?;

        let new_id = uuid::Uuid::new_v4().to_string();
        let new_name = format!("Fork of {source_name}");
        let events = crate::session_store::fork_events(source_id, at_seq, &new_id, &new_name)?;
        let session = crate::session_events::project(None, &events);
        self.sessions.insert(new_id.clone(), session);
        self.active_session_id = new_id.clone();
        Ok(new_id)
    }

    pub fn create_session(&mut self, initial_profile: Option<String>) -> String {
        let new_id = self.create_session_raw(initial_profile);
        Self::save_async(self, None);
        new_id
    }

    pub fn delete_session_raw(&mut self, id: &str) {
        crate::llm::claude::keepalive::cancel_session(id);
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
                session.prune_orphaned_ledger();
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
        let meta = state.collect_meta_kv();

        // Move only the prepared row/meta buffers to the background thread
        dioxus::prelude::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                store::write_rows_and_meta(&rows, &meta).map(|_| rows)
            })
            .await;

            // Back on the Dioxus runtime — safe to touch Signals
            match result {
                Ok(Ok(rows)) => {
                    store::mark_rows_saved(&rows);
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

    /// A rename from the tab bar or History — a user act. Wins forever: sets
    /// `name_user_set` so the summarizer never overwrites `name`/`tab_title`
    /// for this session again.
    pub fn update_session_name_raw(&mut self, id: &str, new_name: String) {
        crate::session_events::log_event(
            id,
            crate::session_events::SessionEvent::SessionRenamed {
                name: new_name.clone(),
            },
        );
        if let Some(session) = self.sessions.get_mut(id) {
            session.name = new_name;
            session.name_user_set = true;
        } else {
            // Not hydrated (renamed from History) — update the stored row directly.
            crate::session_store::rename_session(id, &new_name);
        }
    }

    pub fn update_session_name(&mut self, id: &str, new_name: String) {
        self.update_session_name_raw(id, new_name);
        Self::save_async(self, None);
    }

    /// Programmatic tab naming (e.g. a dispatch review tab: 🐯 + the todo
    /// title) — NOT a user act, so `name_user_set` is left untouched and the
    /// summarizer may still overwrite this later.
    pub fn set_session_title_programmatic(
        &mut self,
        id: &str,
        title: Option<String>,
        emoji: Option<String>,
    ) {
        crate::session_events::log_event(
            id,
            crate::session_events::SessionEvent::TabTitled {
                emoji: emoji.clone(),
                title: title.clone(),
            },
        );
        if let Some(session) = self.sessions.get_mut(id) {
            if emoji.is_some() {
                session.tab_emoji = emoji;
            }
            if title.is_some() {
                session.tab_title = title;
            }
        }
    }
    pub fn get_message_mut(
        &mut self,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut()
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
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

    #[test]
    fn cancel_running_calls_ends_only_calls_in_flight() {
        use crate::components::chat::Message;
        use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus};
        let mut state = SessionState { save_disabled: true, ..Default::default() };
        let id = state.create_session_raw(None);
        let s = state.sessions.get_mut(&id).unwrap();
        let call = |status| {
            let mut tc = ToolCall::new("hobbes-terminal".into(), "HOBBES_TERMINAL_EXEC".into(), serde_json::json!({}), None, None);
            tc.status = status;
            Message {
                id: uuid::Uuid::new_v4(),
                author: "Hobbes".into(),
                content: MessageContent::ToolCall(tc),
                attachments: Vec::new(),
                comments: Vec::new(),
                created_at: Utc::now(),
                usage: None,
            }
        };
        s.messages.push(call(ToolCallStatus::Completed));
        s.messages.push(call(ToolCallStatus::Running));
        let now = Utc::now();
        let changed = s.cancel_running_calls(crate::components::shared::CANCELLED_BY_RESTART, now);
        assert_eq!(changed.len(), 1);
        let statuses: Vec<_> = s
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::ToolCall(tc) => Some((tc.status, tc.cancelled.clone(), tc.completed_at)),
                _ => None,
            })
            .collect();
        assert_eq!(statuses[0], (ToolCallStatus::Completed, None, None), "finished calls untouched");
        assert_eq!(statuses[1], (ToolCallStatus::Error, Some("Hobbes closed while it ran".to_string()), Some(now)));
        assert!(s.cancel_running_calls("again", now).is_empty(), "nothing left running");
    }

    mod timer_tools {
        use super::*;
        use crate::components::shared::ToolCallStatus;
        use crate::timers::{TimerStatus, WakeCondition};

        fn state_with_session() -> (SessionState, String) {
            let mut state = SessionState { save_disabled: true, ..Default::default() };
            let id = state.create_session_raw(None);
            (state, id)
        }

        fn no_titles(_: &str) -> Option<String> {
            None
        }

        #[test]
        fn set_timer_trims_reason_and_parses_until() {
            let (mut state, sid) = state_with_session();
            let args = serde_json::json!({
                "delay": "10m",
                "label": "Check the suite",
                "reason": "  See whether the run passed, then report.  ",
                "until_worker_done": " tdo_1 ",
            });
            let (status, text) =
                state.handle_set_timer(&args, &sid, |id| (id == "tdo_1").then(|| "Run the suite".to_string()));
            assert_eq!(status, ToolCallStatus::Completed, "{text}");
            let t = &state.sessions[&sid].scheduled_timers[0];
            assert_eq!(t.reason.as_deref(), Some("See whether the run passed, then report."));
            assert_eq!(t.until, Some(WakeCondition::WorkerDone { todo_id: "tdo_1".into() }));
            assert_eq!(t.status, TimerStatus::Pending);
            assert!(text.contains("fires in 10m00s at the latest"), "{text}");
            assert!(text.contains("or sooner when the worker for 'Run the suite' finishes"), "{text}");
            assert!(text.contains("reason: See whether the run passed"), "{text}");
        }

        #[test]
        fn set_timer_blank_reason_and_until_are_none() {
            let (mut state, sid) = state_with_session();
            let args = serde_json::json!({ "delay_secs": 60, "reason": "   ", "until_worker_done": "" });
            let (status, text) = state.handle_set_timer(&args, &sid, no_titles);
            assert_eq!(status, ToolCallStatus::Completed);
            let t = &state.sessions[&sid].scheduled_timers[0];
            assert!(t.reason.is_none());
            assert!(t.until.is_none());
            assert!(!text.contains("at the latest"), "{text}");
            assert!(!text.contains("reason:"), "{text}");
        }

        #[test]
        fn set_timer_caps_long_reason() {
            let (mut state, sid) = state_with_session();
            let long = "x".repeat(2_000);
            let args = serde_json::json!({ "delay": "5m", "reason": long });
            state.handle_set_timer(&args, &sid, no_titles);
            let reason = state.sessions[&sid].scheduled_timers[0].reason.clone().unwrap();
            assert_eq!(reason.chars().count(), crate::timers::MAX_REASON_CHARS);
            assert!(reason.ends_with('…'));
        }

        #[test]
        fn set_timer_still_requires_a_deadline() {
            let (mut state, sid) = state_with_session();
            let args = serde_json::json!({ "reason": "wait for it", "until_worker_done": "tdo_1" });
            let (status, text) = state.handle_set_timer(&args, &sid, no_titles);
            assert_eq!(status, ToolCallStatus::Error);
            assert!(text.contains("delay"), "{text}");
            assert!(state.sessions[&sid].scheduled_timers.is_empty());
        }

        #[test]
        fn list_timers_shows_reason_and_condition() {
            let (mut state, sid) = state_with_session();
            let args = serde_json::json!({
                "delay": "1h",
                "reason": "Review the worker's output.",
                "until_worker_done": "tdo_9",
            });
            state.handle_set_timer(&args, &sid, no_titles);
            let (status, text) = state.handle_list_timers(&sid, no_titles);
            assert_eq!(status, ToolCallStatus::Completed);
            assert!(text.contains("or sooner when the worker for to-do tdo_9 finishes"), "{text}");
            // With the planner's titles, the to-do is named.
            let (_, named) = state.handle_list_timers(&sid, |_| Some("Run the suite".to_string()));
            assert!(named.contains("the worker for 'Run the suite' finishes"), "{named}");
            assert!(text.contains("reason: Review the worker's output."), "{text}");
        }
    }

    mod tab_identity {
        use super::*;

        fn session_with(name: &str, name_user_set: bool, tab_title: Option<&str>) -> Session {
            let mut s = create_test_session();
            s.name = name.to_string();
            s.name_user_set = name_user_set;
            s.tab_title = tab_title.map(str::to_string);
            add_user_messages(&mut s, 1);
            s
        }

        fn add_user_messages(s: &mut Session, n: usize) {
            for _ in 0..n {
                s.messages.push(crate::components::chat::Message {
                    id: uuid::Uuid::new_v4(),
                    author: "User".to_string(),
                    content: crate::components::shared::MessageContent::Text {
                        content: "hi".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                        signature_provider: None,
                    },
                    attachments: Vec::new(),
                    comments: Vec::new(),
                    created_at: Utc::now(),
                    usage: None,
                });
            }
        }

        fn proposal(title: &str, emoji: &str) -> ConversationSummary {
            ConversationSummary {
                tab_title: Some(title.to_string()),
                tab_emoji: Some(emoji.to_string()),
                ..Default::default()
            }
        }

        #[test]
        fn an_empty_tab_is_never_titled() {
            // A profile switch's ForceRefresh summarizes a fresh tab that
            // only carries a system note; that must not name the tab.
            let mut session = create_test_session();
            let result =
                apply_tab_identity_from_summary(&mut session, &proposal("Profile Setup", "🆔"));
            assert_eq!(result, None);
            assert_eq!(session.tab_title, None);
            assert_eq!(session.tab_emoji, None);
            assert!(!session.wants_tab_identity());
        }

        #[test]
        fn an_early_title_is_provisional_and_replaced_once_the_tab_matures() {
            let mut session = session_with("Sep 30 - 09:00 AM", false, None);
            assert!(session.wants_tab_identity());
            apply_tab_identity_from_summary(&mut session, &proposal("Quick question", "❓"))
                .expect("first title lands");
            assert!(session.tab_title_provisional);
            assert!(!session.wants_tab_identity());

            // Still short: later summaries don't touch the provisional title.
            add_user_messages(&mut session, 1);
            assert_eq!(
                apply_tab_identity_from_summary(&mut session, &proposal("Other", "🐛")),
                None
            );
            assert_eq!(session.tab_title.as_deref(), Some("Quick question"));

            // Mature: one retitle, then locked.
            add_user_messages(&mut session, TAB_TITLE_MATURE_USER_MESSAGES - 2);
            assert!(session.wants_tab_identity());
            apply_tab_identity_from_summary(&mut session, &proposal("Trust dispatch", "🎯"))
                .expect("mature retitle lands");
            assert_eq!(session.tab_title.as_deref(), Some("Trust dispatch"));
            assert_eq!(session.tab_emoji.as_deref(), Some("🎯"));
            assert!(!session.tab_title_provisional);
            assert!(!session.wants_tab_identity());

            add_user_messages(&mut session, 5);
            assert_eq!(
                apply_tab_identity_from_summary(&mut session, &proposal("Drifted", "🐛")),
                None
            );
            assert_eq!(session.tab_title.as_deref(), Some("Trust dispatch"));
        }

        #[test]
        fn a_mature_summary_without_a_title_keeps_the_provisional_one_open() {
            let mut session = session_with("Sep 30 - 09:00 AM", false, None);
            apply_tab_identity_from_summary(&mut session, &proposal("Quick question", "❓"));
            add_user_messages(&mut session, TAB_TITLE_MATURE_USER_MESSAGES);
            assert_eq!(
                apply_tab_identity_from_summary(&mut session, &proposal("", "🐛")),
                None
            );
            assert_eq!(session.tab_title.as_deref(), Some("Quick question"));
            assert_eq!(session.tab_emoji.as_deref(), Some("❓"));
            assert!(session.tab_title_provisional);
        }

        #[test]
        fn a_title_first_set_on_a_mature_tab_locks_immediately() {
            let mut session = session_with("Sep 30 - 09:00 AM", false, None);
            add_user_messages(&mut session, TAB_TITLE_MATURE_USER_MESSAGES);
            apply_tab_identity_from_summary(&mut session, &proposal("Trust dispatch", "🎯"));
            assert!(!session.tab_title_provisional);
            assert!(!session.wants_tab_identity());
        }

        #[test]
        fn a_user_renamed_tab_never_wants_identity() {
            let mut session = session_with("Mine", true, None);
            add_user_messages(&mut session, 5);
            assert!(!session.wants_tab_identity());
        }

        fn create_test_session() -> Session {
            Session {
                id: "s1".to_string(),
                name: "Sep 16 - 03:45 PM".to_string(),
                messages: Vec::new(),
                active_context: ActiveContext::default(),
                last_updated: Utc::now(),
                accumulated_cost: 0.0,
                accumulated_tokens: 0,
                accumulated_turns: 0,
                memory_optimization_summary: None,
                composio_profile: None,
                llm_connector_id: None,
                llm_provider: None,
                chat_model: None,
                project_id: None,
                project_tag_user_set: false,
                loaded_skills: HashMap::new(),
                scratchpad: String::new(),
                current_ai_turn_count: 0,
                watch_word_recovery_count: 0,
                scheduled_timers: Vec::new(),
                handoff: None,
                tool_call_history: Vec::new(),
                tab_emoji: None,
                tab_title: None,
                name_user_set: false,
            tab_title_provisional: false,
            history_anchor: None,
            context_ledger: Vec::new(),
            dispatch_link: None,
            dispatched_workers: Vec::new(),
            }
        }

        #[test]
        fn display_title_prefers_user_name_over_tab_title_over_date() {
            let user_named = session_with("My Renamed Chat", true, Some("AI Topic"));
            assert_eq!(user_named.display_title(), "My Renamed Chat");

            let ai_titled = session_with("Sep 16 - 03:45 PM", false, Some("AI Topic"));
            assert_eq!(ai_titled.display_title(), "AI Topic");

            let bare = session_with("Sep 16 - 03:45 PM", false, None);
            assert_eq!(bare.display_title(), "Sep 16 - 03:45 PM");
        }

        #[test]
        fn tab_topic_user_rename_replaces_ai_topic_forever() {
            let renamed = session_with("Renamed", true, Some("Old AI Topic"));
            assert_eq!(renamed.tab_topic().as_deref(), Some("Renamed"));

            let untouched = session_with("Sep 16 - 03:45 PM", false, Some("AI Topic"));
            assert_eq!(untouched.tab_topic().as_deref(), Some("AI Topic"));

            let no_summary_yet = session_with("Sep 16 - 03:45 PM", false, None);
            assert_eq!(no_summary_yet.tab_topic(), None);
        }

        #[test]
        fn short_date_parses_the_creation_stamp() {
            let s = session_with("Sep 16 - 03:45 PM", false, None);
            assert_eq!(s.short_date(), "Sep 16");
        }

        #[test]
        fn short_date_falls_back_to_last_updated_once_renamed() {
            let mut s = session_with("My Renamed Chat", true, None);
            s.last_updated =
                chrono::DateTime::parse_from_rfc3339("2026-03-01T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc);
            // Noon UTC is Mar 01 in every timezone from UTC-11 to UTC+11.
            assert_eq!(s.short_date(), "Mar 01");
        }

        #[test]
        fn emoji_validation_accepts_single_emoji_clusters() {
            assert_eq!(normalize_tab_emoji(Some("✅")), "✅");
            assert_eq!(normalize_tab_emoji(Some("🏗️")), "🏗️"); // base + VS16
            assert_eq!(normalize_tab_emoji(Some("🐯")), "🐯");
        }

        #[test]
        fn emoji_validation_falls_back_on_invalid_input() {
            assert_eq!(normalize_tab_emoji(Some("ab")), FALLBACK_TAB_EMOJI);
            assert_eq!(normalize_tab_emoji(Some("")), FALLBACK_TAB_EMOJI);
            assert_eq!(normalize_tab_emoji(None), FALLBACK_TAB_EMOJI);
            assert_eq!(normalize_tab_emoji(Some("😀😁")), FALLBACK_TAB_EMOJI);
        }

        #[test]
        fn title_normalization_trims_quotes_and_caps_length() {
            assert_eq!(
                normalize_tab_title(Some("  \"Ship the release\"  ")),
                Some("Ship the release".to_string())
            );
            let long = "a".repeat(60);
            assert_eq!(
                normalize_tab_title(Some(&long)).map(|s| s.len()),
                Some(40)
            );
            assert_eq!(normalize_tab_title(Some("   ")), None);
            assert_eq!(normalize_tab_title(None), None);
        }

        #[test]
        fn apply_tab_identity_writes_normalized_title_and_emoji() {
            let mut session = session_with("Sep 16 - 03:45 PM", false, None);
            let summary = ConversationSummary {
                tab_title: Some("  \"Trust dispatch\"  ".to_string()),
                tab_emoji: Some("🎯".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &summary);
            assert_eq!(
                result,
                Some((Some("🎯".to_string()), Some("Trust dispatch".to_string())))
            );
            assert_eq!(session.tab_title, Some("Trust dispatch".to_string()));
            assert_eq!(session.tab_emoji, Some("🎯".to_string()));
        }

        #[test]
        fn apply_tab_identity_never_overwrites_a_user_rename() {
            let mut session = session_with("My renamed tab", true, None);
            let summary = ConversationSummary {
                tab_title: Some("Trust dispatch".to_string()),
                tab_emoji: Some("🎯".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &summary);
            assert_eq!(result, None);
            assert_eq!(session.tab_title, None);
            assert_eq!(session.tab_emoji, None);
        }

        #[test]
        fn apply_tab_identity_is_a_noop_when_summary_carries_no_proposal() {
            let mut session = session_with("Sep 16 - 03:45 PM", false, None);
            let summary = ConversationSummary::default();
            let result = apply_tab_identity_from_summary(&mut session, &summary);
            assert_eq!(result, None);
            assert_eq!(session.tab_title, None);
            assert_eq!(session.tab_emoji, None);
        }

        #[test]
        fn apply_tab_identity_locks_after_the_first_write_and_never_relabels_again() {
            let mut session = session_with("Sep 16 - 03:45 PM", false, None);
            let first = ConversationSummary {
                tab_title: Some("Trust dispatch".to_string()),
                tab_emoji: Some("🎯".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &first);
            assert!(result.is_some());
            assert_eq!(session.tab_title, Some("Trust dispatch".to_string()));
            assert_eq!(session.tab_emoji, Some("🎯".to_string()));

            // A later summary reflecting where the conversation drifted to
            // must NOT relabel a tab the user has already learned to
            // recognize.
            let later = ConversationSummary {
                tab_title: Some("Totally different topic".to_string()),
                tab_emoji: Some("🐛".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &later);
            assert_eq!(result, None);
            assert_eq!(session.tab_title, Some("Trust dispatch".to_string()));
            assert_eq!(session.tab_emoji, Some("🎯".to_string()));
        }

        #[test]
        fn an_emoji_only_proposal_does_not_lock_out_a_later_real_title() {
            // normalize_tab_emoji always falls back to SOME emoji even for
            // empty/invalid input — it never produces None — so an early,
            // low-quality summary (e.g. seconds after a brand new tab opens)
            // reliably sets tab_emoji while leaving no title. That must not
            // permanently block the real title a later, more mature summary
            // proposes; only a title landing should lock the tab.
            let mut session = session_with("Sep 16 - 03:45 PM", false, None);
            let early = ConversationSummary {
                tab_title: Some("".to_string()),
                tab_emoji: Some("".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &early);
            assert!(result.is_some());
            assert_eq!(session.tab_title, None);
            assert_eq!(session.tab_emoji, Some(FALLBACK_TAB_EMOJI.to_string()));

            let later = ConversationSummary {
                tab_title: Some("Trust dispatch".to_string()),
                tab_emoji: Some("🎯".to_string()),
                ..Default::default()
            };
            let result = apply_tab_identity_from_summary(&mut session, &later);
            assert!(result.is_some());
            assert_eq!(session.tab_title, Some("Trust dispatch".to_string()));
            assert_eq!(session.tab_emoji, Some("🎯".to_string()));
        }
    }

    /// Both ordering helpers exist to keep one invariant: the vec's order and
    /// its `created_at` stamps agree, because the journal projector reinserts
    /// by timestamp while the prompt builder walks the vec.
    mod message_order {
        use super::*;

        fn msg(at_secs: i64) -> crate::components::chat::Message {
            crate::components::chat::Message {
                id: uuid::Uuid::new_v4(),
                author: "Hobbes".to_string(),
                content: crate::components::shared::MessageContent::Text {
                    content: String::new(),
                    thought_signature: None,
                    thought_summary: None,
                    signature_provider: None,
                },
                attachments: Vec::new(),
                comments: Vec::new(),
                created_at: Utc::now() + chrono::Duration::seconds(at_secs),
                usage: None,
            }
        }

        #[test]
        fn restamp_of_the_tail_takes_the_new_stamp() {
            let mut messages = vec![msg(0), msg(1)];
            let id = messages[1].id;
            let ts = messages[1].created_at + chrono::Duration::seconds(10);
            assert_eq!(restamp_message(&mut messages, &id, ts), Some(ts));
            assert_eq!(messages[1].created_at, ts);
        }

        #[test]
        fn restamp_is_clamped_by_its_neighbours() {
            let mut messages = vec![msg(0), msg(5), msg(6)];
            let id = messages[1].id;
            let upper = messages[2].created_at;
            // Way past the next message: clamped, so the vec stays sorted.
            let ts = upper + chrono::Duration::seconds(60);
            assert_eq!(restamp_message(&mut messages, &id, ts), Some(upper));
            let stamps: Vec<_> = messages.iter().map(|m| m.created_at).collect();
            assert!(stamps.windows(2).all(|w| w[0] <= w[1]));
        }

        #[test]
        fn restamp_leaves_already_unordered_neighbours_alone() {
            let mut messages = vec![msg(9), msg(5), msg(0)];
            let id = messages[1].id;
            let before = messages[1].created_at;
            let ts = Utc::now();
            assert_eq!(restamp_message(&mut messages, &id, ts), Some(before));
            assert_eq!(messages[1].created_at, before);
        }

        #[test]
        fn restamp_of_a_missing_message_is_none() {
            let mut messages = vec![msg(0)];
            assert!(restamp_message(&mut messages, &uuid::Uuid::new_v4(), Utc::now()).is_none());
        }

        #[test]
        fn move_to_end_reorders_and_stamps_last() {
            let mut messages = vec![msg(0), msg(1), msg(2)];
            let id = messages[1].id;
            let ts = messages[2].created_at + chrono::Duration::seconds(1);
            let moved = move_message_to_end(&mut messages, &id, ts).expect("present");
            assert_eq!(moved.id, id);
            assert_eq!(messages.last().unwrap().id, id);
            assert_eq!(messages.len(), 3);
            let stamps: Vec<_> = messages.iter().map(|m| m.created_at).collect();
            assert!(stamps.windows(2).all(|w| w[0] <= w[1]));
        }

        #[test]
        fn move_to_end_never_stamps_before_the_message_it_follows() {
            let mut messages = vec![msg(0), msg(60)];
            let id = messages[0].id;
            // A stamp earlier than the tail would break the invariant.
            let moved = move_message_to_end(&mut messages, &id, Utc::now()).expect("present");
            assert_eq!(moved.created_at, messages[0].created_at);
            assert_eq!(messages.last().unwrap().id, id);
            let stamps: Vec<_> = messages.iter().map(|m| m.created_at).collect();
            assert!(stamps.windows(2).all(|w| w[0] <= w[1]));
        }

        #[test]
        fn move_to_end_of_the_tail_only_restamps() {
            let mut messages = vec![msg(0), msg(1)];
            let id = messages[1].id;
            let ts = messages[1].created_at + chrono::Duration::seconds(5);
            let moved = move_message_to_end(&mut messages, &id, ts).expect("present");
            assert_eq!(moved.created_at, ts);
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[1].id, id);
        }
    }

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
                signature_provider,
            } => {
                assert_eq!(content, "Hello world");
                assert!(thought_signature.is_none());
                assert!(thought_summary.is_none());
                assert!(signature_provider.is_none());
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
                    signature_provider: None,
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
                    cache_creation_tokens: None,
                    cost: Some(0.0042),
                }),
            });
        }

        // Verify session cost before deletion
        assert!((state.sessions.get(&session_id).unwrap().total_cost() - 0.0042).abs() < 0.00001);
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
                        signature_provider: None,
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
                        cache_creation_tokens: None,
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
        let initial_pages = vec![(
            "page-A".to_string(),
            PagedResult {
                remaining_content: "Page 1 content\nPage 2 content\nPage 3 content".to_string(),
                tool_name: "test_tool".to_string(),
            },
        )];
        state.store_pages(initial_pages);
        assert_eq!(
            state
                .page_queue
                .get("page-A")
                .unwrap()
                .remaining_content
                .len(),
            45 - 1 // "Page 1 content\nPage 2 content\nPage 3 content" = 44 chars
        );

        // Consume one page via fetch_next_page with a 15-char budget
        let (content, name, remaining) = state.fetch_next_page("page-A", 15).unwrap();
        assert_eq!(name, "test_tool");
        assert!(!content.is_empty(), "Should return a non-empty page");
        assert!(content.len() <= 15, "Page should respect budget");
        assert!(remaining > 0, "Should have remaining pages");

        // Record how much content remains after consuming one page
        let remaining_after_first = state
            .page_queue
            .get("page-A")
            .unwrap()
            .remaining_content
            .len();

        // Simulate continuation turn: store_pages called again with same ID but fresh content
        let re_generated = vec![(
            "page-A".to_string(),
            PagedResult {
                remaining_content: "Page 1 content\nPage 2 content\nPage 3 content".to_string(),
                tool_name: "test_tool".to_string(),
            },
        )];
        state.store_pages(re_generated);

        // CRITICAL: the queue should still have the partially-consumed content (not reset)
        assert_eq!(
            state
                .page_queue
                .get("page-A")
                .unwrap()
                .remaining_content
                .len(),
            remaining_after_first,
            "store_pages must NOT overwrite partially-consumed entries"
        );

        // Verify we get the next portion of content (not starting over)
        let (content2, _, _remaining2) = state.fetch_next_page("page-A", 15).unwrap();
        assert!(!content2.is_empty(), "Should return next page content");
        assert!(
            content2.len() <= 15,
            "Second page should also respect budget"
        );
    }

    /// Test: fetch_next_page dynamically sizes pages based on page_budget.
    /// Verifies that the same content produces differently-sized pages
    /// when called with different budgets.
    #[test]
    fn test_dynamic_page_sizing() {
        let mut state = SessionState::default();

        // Store content (~100 chars)
        let content = "Line one of the result.\nLine two of the result.\nLine three of the result.\nLine four of the result.";
        state.store_pages(vec![(
            "page-dynamic".to_string(),
            PagedResult {
                remaining_content: content.to_string(),
                tool_name: "dynamic_tool".to_string(),
            },
        )]);

        // Fetch with a small budget (30 chars)
        let (page1, _, remaining1) = state.fetch_next_page("page-dynamic", 30).unwrap();
        assert!(
            page1.len() <= 30,
            "Small budget: page should be ≤30 chars, got {}",
            page1.len()
        );
        assert!(remaining1 > 0, "Should have remaining content");

        // Fetch with a large budget (remaining content should fit in one page)
        let (page2, _, remaining2) = state.fetch_next_page("page-dynamic", 10000).unwrap();
        assert!(
            page2.len() > 30,
            "Large budget: page should be larger than small-budget page"
        );
        assert_eq!(
            remaining2, 0,
            "Large budget should consume all remaining content"
        );

        // All content consumed — entry should be cleaned up
        assert!(
            state.page_queue.get("page-dynamic").is_none(),
            "Entry should be removed after all content consumed"
        );

        // Concatenated pages should reproduce original content
        let reconstructed = format!("{}{}", page1, page2);
        assert_eq!(
            reconstructed, content,
            "Pages must reconstruct original content"
        );
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
            tab_emoji: None,
            tab_title: None,
            name_user_set: false,
            tab_title_provisional: false,
            history_anchor: None,
            context_ledger: Vec::new(),
            dispatch_link: None,
            dispatched_workers: Vec::new(),
            messages: vec![
                Message {
                    id: msg1_id,
                    author: "User".to_string(),
                    content: MessageContent::Text {
                        content: "Hello".to_string(),
                        thought_signature: None,
                        thought_summary: None,
                        signature_provider: None,
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
                        signature_provider: None,
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
                        signature_provider: None,
                        cached_image_path: None,
                        result_summary: None,
                        completed_at: None,
                        profile_id: None,
                        connector_name: None,
                        model: None,
                        round_id: None,
                        cancelled: None,
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
                        signature_provider: None,
                        cached_image_path: None,
                        result_summary: None,
                        completed_at: None,
                        profile_id: None,
                        connector_name: None,
                        model: None,
                        round_id: None,
                        cancelled: None,
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
                    tab_title: None,
                    tab_emoji: None,
                },
                ..Default::default()
            },
            last_updated: chrono::Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: None,
            llm_connector_id: None,
            llm_provider: None,
            chat_model: None,
            project_id: None,
            project_tag_user_set: false,
            loaded_skills: HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
            handoff: None,
            tool_call_history: Vec::new(),
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
        assert!(
            !session
                .active_context
                .conversation_summary
                .summary
                .is_empty()
        );

        // Undo: delete msg2 and everything after (msg2, msg3, msg4)
        let deleted = session.delete_message_and_after(&msg2_id.to_string());
        assert_eq!(deleted, 3, "Should have deleted 3 messages");
        assert_eq!(
            session.messages.len(),
            1,
            "Only first message should remain"
        );

        // Tool snapshots for the deleted tool calls should be gone
        assert!(
            !session
                .active_context
                .extra
                .contains_key(&format!("tool_snapshot_{}", exec_id_1)),
            "tool_snapshot for exec_id_1 should be removed"
        );
        assert!(
            !session
                .active_context
                .extra
                .contains_key(&format!("tool_snapshot_{}", exec_id_2)),
            "tool_snapshot for exec_id_2 should be removed"
        );

        // Unrelated extra entries should survive
        assert!(
            session.active_context.extra.contains_key("unrelated_key"),
            "Unrelated extra entries must not be removed"
        );

        // Conversation summary should be reset
        assert!(
            session
                .active_context
                .conversation_summary
                .summary
                .is_empty(),
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

    /// The seq counter is process-local. A fresh process must seed it from the
    /// store's high-water mark, or rows/meta last written by a longer-lived
    /// earlier process silently reject every update (the July 12 data-loss bug:
    /// active_session_id and whole sessions frozen at their Friday state).
    #[test]
    fn test_store_seq_seeded_from_high_water_mark() {
        use crate::session_store::test_support as ts;
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);

        ts::with_test_db(|conn| {
            // Simulate a previous long-lived process: row + meta at a high seq.
            state.sessions.get_mut(&id).unwrap().name = "From old process".to_string();
            ts::upsert_with_seq(conn, state.sessions.get(&id).unwrap(), 50_000);
            ts::meta_set_with_seq(conn, "active_session_id", &id, 50_001);

            // A fresh process (unseeded counter) must not lose these writes.
            ts::seed_seq(conn);

            state.sessions.get_mut(&id).unwrap().name = "From new process".to_string();
            ts::upsert(conn, state.sessions.get(&id).unwrap());
            assert_eq!(
                ts::get_row_name(conn, &id).as_deref(),
                Some("From new process"),
                "after seeding, a new process's writes must land on high-seq rows"
            );

            ts::meta_set(conn, "active_session_id", "other-session");
            assert_eq!(
                ts::meta(conn, "active_session_id").as_deref(),
                Some("other-session"),
                "after seeding, meta writes must land on high-seq keys"
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
        let json = std::env::var("HOBBES_IMPORT_TEST_JSON")
            .ok()
            .map(std::path::PathBuf::from);
        let archive = std::env::var("HOBBES_IMPORT_TEST_ARCHIVE")
            .ok()
            .map(std::path::PathBuf::from);
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
        let old_format = make_old_session_json(
            r#"{
            "id": "0b0e9e2e-49e2-4a4d-bd58-9d1b4a55b573",
            "author": "User",
            "content": { "Text": "hello from the past" },
            "attachments": [],
            "comments": [],
            "created_at": "2026-01-01T00:00:00Z"
        }"#,
        );
        let state = SessionState::migrate_from_raw_json(&old_format).unwrap();
        let session = state.sessions.get("test-session-1").unwrap();
        assert_eq!(session.messages.len(), 1);
    }
}
