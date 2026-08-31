//! Append-only session event journal (Phase 1: dual-write).
//!
//! Every model-visible or user-authored session mutation is mirrored into the
//! `session_events` table alongside the existing `SessionState` model. In this
//! phase the journal is **write-only** — nothing reads it at runtime — so a
//! logging failure must never affect app behavior (all append paths swallow
//! errors after logging them).
//!
//! Design rules:
//! - Serde-tagged enum (`kind` tag). Rows are deserialized **individually** on
//!   read; a row whose kind is unknown to this build (written by a newer
//!   version) is skipped with a warning, never fails the whole read.
//! - Only final messages are journaled — never streaming chunks. Assistant
//!   messages are logged once, at end-of-turn finalization.
//! - Timestamps live in the row (`ts` column, captured at append time) so a
//!   future replay never needs to call `Utc::now()`.
//! - Appends follow P-009: the event is serialized to a row buffer on the
//!   calling thread; only the prepared buffers move to the background writer
//!   (see `session_store::append_events`).

use serde::{Deserialize, Serialize};

use crate::components::chat::{Comment, Message};
use crate::session::{Session, SessionState};
use crate::session_store::LoadedSessionEvent;
use crate::timers::ScheduledTimer;

/// One journaled session mutation. The `kind` serde tag doubles as the
/// `session_events.kind` column value (see [`SessionEvent::kind`]).
///
/// Message-shaped variants embed the full `Message` as it exists in the model:
/// tool calls and their results are folded into a single message
/// (`MessageContent::ToolCall` mutated in place when the result arrives), so
/// `ToolCall` carries the message at dispatch time (status `Running`) and
/// `ToolResult` carries the same message id again with the final
/// status/response. Skill invocations ride the same pair: a completed
/// `MessageContent::SkillCall` update is journaled as `ToolResult`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    /// Birth event: the session was created. A journal that *starts* with this
    /// event is "journal-complete" — the session can be rebuilt from nothing
    /// via [`project`]. Pre-journal sessions never have one.
    SessionCreated { id: String, name: String },
    /// A user-authored message (typed sends, /skill bubbles, /unload
    /// confirmations, synthetic recovery prompts authored as "User").
    UserMessage { message: Message },
    /// A finalized assistant message (end-of-turn text, error bubbles,
    /// turn-limit warnings, memory-optimization notices).
    AssistantMessage { message: Message },
    /// A tool/skill call message at dispatch time (status Running/Pending).
    ToolCall { message: Message },
    /// The same tool/skill call message after its result was written in place.
    ToolResult { message: Message },
    /// HOBBES_UPDATE_SCRATCHPAD overwrite.
    ScratchpadSet { content: String },
    /// A skill payload was inserted into `session.loaded_skills`.
    SkillLoaded { name: String, payload: String },
    /// A skill was removed from `session.loaded_skills`.
    SkillUnloaded { name: String },
    /// A background summarizer wrote `active_context.conversation_summary`.
    /// Stored as JSON to mirror `ConversationSummary` without pinning its shape.
    SummaryComputed { summary: serde_json::Value },
    /// HOBBES_SET_TIMER scheduled a timer.
    TimerCreated { timer: ScheduledTimer },
    /// HOBBES_CANCEL_TIMER cancelled a pending timer.
    TimerCancelled { timer_id: String },
    /// The poll scheduler marked a due timer as fired.
    TimerFired { timer: ScheduledTimer },
    /// The session was pinned to a connector/model (per-tab picker).
    ConnectorPinned {
        connector_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    },
    /// The session was renamed.
    SessionRenamed { name: String },
    /// `delete_message_and_after` truncated history. `seq` is the journal seq
    /// of the first event referencing the deleted message (everything at or
    /// after it is undone); 0 when the message predates the journal.
    /// `message_id` is carried so Phase 2 can re-anchor even without a seq.
    RewoundTo {
        seq: i64,
        #[serde(default)]
        message_id: String,
    },
    /// The user cancelled a streaming turn. `partial_message` is the assistant
    /// message with whatever text survived (None when the empty placeholder was
    /// discarded — that placeholder was never journaled). `removed_message_ids`
    /// are trailing orphaned Running tool-call messages that WERE journaled as
    /// `ToolCall` and got trimmed by the cancel.
    StreamCancelled {
        partial_message: Option<Message>,
        #[serde(default)]
        removed_message_ids: Vec<uuid::Uuid>,
    },
    /// The Forget/Optimize-Memory flow rewrote the session's memory.
    /// Only the summary is journaled: the wholesale `active_context`
    /// replacement is deliberately NOT replayed — mcp_tools/tools/extra are
    /// reactively rebuilt (P-001 sync, ToolCallSummarizer), and the summary is
    /// the only user-visible durable outcome.
    MemoryOptimized { summary: String },
    /// An inline comment was added — or edited (upsert by `comment.id`).
    CommentAdded { message_id: String, comment: Comment },
    /// An inline comment was removed.
    CommentRemoved { message_id: String, comment_id: String },
    /// The session's Composio profile binding changed.
    ComposioProfileSet { profile: Option<String> },
    /// Fork marker: this session was created by copying `from_session_id`'s
    /// journal up to `at_seq` (source seqs, inclusive). No-op on replay; kept
    /// for provenance.
    SessionForked { from_session_id: String, at_seq: i64 },
    /// A user-authored text message was edited in place. Replaces only the
    /// text of `MessageContent::Text`; attachments, comments, usage, and
    /// `created_at` are untouched. A target that doesn't exist at fold time —
    /// unknown id, or a message truncated by an earlier `RewoundTo` — is
    /// skipped with a warning: the natural consequence of the fold. (The
    /// Save & Resend flow journals the edit *before* the rewind for
    /// provenance; either ordering projects to the same state.)
    MessageEdited { message_id: uuid::Uuid, content: String },
}

impl SessionEvent {
    /// The serde tag, mirrored into the `kind` column for cheap querying.
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEvent::SessionCreated { .. } => "SessionCreated",
            SessionEvent::UserMessage { .. } => "UserMessage",
            SessionEvent::AssistantMessage { .. } => "AssistantMessage",
            SessionEvent::ToolCall { .. } => "ToolCall",
            SessionEvent::ToolResult { .. } => "ToolResult",
            SessionEvent::ScratchpadSet { .. } => "ScratchpadSet",
            SessionEvent::SkillLoaded { .. } => "SkillLoaded",
            SessionEvent::SkillUnloaded { .. } => "SkillUnloaded",
            SessionEvent::SummaryComputed { .. } => "SummaryComputed",
            SessionEvent::TimerCreated { .. } => "TimerCreated",
            SessionEvent::TimerCancelled { .. } => "TimerCancelled",
            SessionEvent::TimerFired { .. } => "TimerFired",
            SessionEvent::ConnectorPinned { .. } => "ConnectorPinned",
            SessionEvent::SessionRenamed { .. } => "SessionRenamed",
            SessionEvent::RewoundTo { .. } => "RewoundTo",
            SessionEvent::StreamCancelled { .. } => "StreamCancelled",
            SessionEvent::MemoryOptimized { .. } => "MemoryOptimized",
            SessionEvent::CommentAdded { .. } => "CommentAdded",
            SessionEvent::CommentRemoved { .. } => "CommentRemoved",
            SessionEvent::ComposioProfileSet { .. } => "ComposioProfileSet",
            SessionEvent::SessionForked { .. } => "SessionForked",
            SessionEvent::MessageEdited { .. } => "MessageEdited",
        }
    }
}

/// Append one event to a session's journal. Fire-and-forget: serializes on
/// the calling thread, hands the prepared row to the background writer, and
/// no-ops (with a debug log) when the store isn't initialized (tests).
pub fn log_event(session_id: &str, event: SessionEvent) {
    crate::session_store::append_events(session_id, vec![event]);
}

/// Batch variant of [`log_event`] — one background hop for a whole turn.
pub fn log_events(session_id: &str, events: Vec<SessionEvent>) {
    crate::session_store::append_events(session_id, events);
}

/// Synchronous append: the row is written before this returns, so an
/// immediately-following `load_events` sees it. Used by rewind-as-replay,
/// which projects the journal right after journaling the rewind.
pub fn log_event_sync(session_id: &str, event: SessionEvent) {
    let rows = crate::session_store::prepare_event_rows(session_id, std::slice::from_ref(&event));
    if let Err(e) = crate::session_store::write_event_rows(&rows) {
        tracing::error!("session_events: sync append failed for {session_id}: {e}");
    }
}

// ── Projection (Phase 2) ────────────────────────────────────────────────────

/// A deterministic, empty session shell for `project(None, …)`. No clocks,
/// no settings: `last_updated` starts at the epoch and is advanced to each
/// applied event's row timestamp.
fn blank_session() -> Session {
    Session {
        id: String::new(),
        name: String::new(),
        messages: Vec::new(),
        active_context: Default::default(),
        last_updated: chrono::DateTime::<chrono::Utc>::default(),
        accumulated_cost: 0.0,
        accumulated_tokens: 0,
        accumulated_turns: 0,
        memory_optimization_summary: None,
        composio_profile: None,
        llm_connector_id: None,
        llm_provider: None,
        chat_model: None,
        loaded_skills: std::collections::HashMap::new(),
        scratchpad: String::new(),
        current_ai_turn_count: 0,
        watch_word_recovery_count: 0,
        scheduled_timers: Vec::new(),
    }
}

/// Insert-or-replace a message by id. Replacement keeps the slot; insertion
/// goes at the `created_at`-sorted position (after equal timestamps) rather
/// than the journal position: a streaming assistant placeholder sits *before*
/// its turn's tool calls in the live vec (it was pushed at send time) but is
/// only journaled at end-of-turn, *after* the `ToolCall` events. `created_at`
/// is capture-time truth, so sorting by it reproduces the live layout.
fn upsert_message(session: &mut Session, message: &Message) {
    if let Some(slot) = session.messages.iter_mut().find(|m| m.id == message.id) {
        *slot = message.clone();
        return;
    }
    let at = session
        .messages
        .iter()
        .rposition(|m| m.created_at <= message.created_at)
        .map(|i| i + 1)
        .unwrap_or(0);
    session.messages.insert(at, message.clone());
}

/// Rebuild a [`Session`] by folding `events` over `base`.
///
/// Pure and deterministic: never calls `Utc::now()`, never reads settings or
/// globals — everything comes from `base` plus the event rows (row `ts` was
/// captured at append time). `base` is `None` for journal-complete sessions
/// (journal starts with `SessionCreated`); pre-journal sessions pass their
/// stored row as the base and replay the suffix.
///
/// Deliberately NOT reconstructed (bookkeeping/derived state): turn counters,
/// `accumulated_turns`, `active_context.mcp_tools`/`tools`/`extra` snapshots
/// (reactively rebuilt), and the ephemeral page queue. `accumulated_cost` /
/// `accumulated_tokens` ARE reconstructed — they are exactly the usage
/// harvested from messages truncated by rewinds (see `RewoundTo` below).
pub fn project(base: Option<Session>, events: &[LoadedSessionEvent]) -> Session {
    let mut session = base.unwrap_or_else(blank_session);
    for loaded in events {
        apply_event(&mut session, &loaded.event);
        session.last_updated = loaded.ts;
    }
    session
}

fn apply_event(session: &mut Session, event: &SessionEvent) {
    use SessionEvent as E;
    match event {
        E::SessionCreated { id, name } => {
            session.id = id.clone();
            session.name = name.clone();
        }
        E::UserMessage { message }
        | E::AssistantMessage { message }
        | E::ToolCall { message }
        | E::ToolResult { message } => upsert_message(session, message),
        E::ScratchpadSet { content } => session.scratchpad = content.clone(),
        E::SkillLoaded { name, payload } => {
            session.loaded_skills.insert(name.clone(), payload.clone());
        }
        E::SkillUnloaded { name } => {
            session.loaded_skills.remove(name);
        }
        E::SummaryComputed { summary } => {
            match serde_json::from_value(summary.clone()) {
                Ok(parsed) => session.active_context.conversation_summary = parsed,
                Err(e) => tracing::warn!("project: unparseable SummaryComputed payload: {e}"),
            }
        }
        E::TimerCreated { timer } | E::TimerFired { timer } => {
            match session.scheduled_timers.iter_mut().find(|t| t.id == timer.id) {
                Some(slot) => *slot = timer.clone(),
                None => session.scheduled_timers.push(timer.clone()),
            }
        }
        E::TimerCancelled { timer_id } => {
            if let Some(t) = session.scheduled_timers.iter_mut().find(|t| &t.id == timer_id) {
                t.status = crate::timers::TimerStatus::Cancelled;
            }
        }
        E::ConnectorPinned { connector_id, provider, model } => {
            session.llm_connector_id = connector_id.clone();
            session.chat_model = model.clone();
            // The legacy provider-kind mirror was journaled via `{:?}`, which
            // matches the serde enum names — parse best-effort.
            session.llm_provider = provider.as_ref().and_then(|p| {
                serde_json::from_value(serde_json::Value::String(p.clone())).ok()
            });
        }
        E::SessionRenamed { name } => session.name = name.clone(),
        E::RewoundTo { message_id, .. } => {
            // Single home of truth for undo semantics: harvest usage from the
            // truncated messages into accumulated_cost/accumulated_tokens,
            // drop tool_snapshot_* extras for deleted tool calls, and reset
            // conversation_summary. The legacy in-place path delegates to the
            // same helper (Session::truncate_from_message).
            session.truncate_from_message(message_id);
        }
        E::StreamCancelled { partial_message, removed_message_ids } => {
            session
                .messages
                .retain(|m| !removed_message_ids.contains(&m.id));
            if let Some(partial) = partial_message {
                upsert_message(session, partial);
            }
        }
        E::MemoryOptimized { summary } => {
            // active_context replacement is deliberately not replayed — see
            // the variant doc. Only the durable summary is applied.
            session.memory_optimization_summary = Some(summary.clone());
        }
        E::CommentAdded { message_id, comment } => {
            if let Ok(uuid) = uuid::Uuid::parse_str(message_id) {
                if let Some(msg) = session.messages.iter_mut().find(|m| m.id == uuid) {
                    match msg.comments.iter_mut().find(|c| c.id == comment.id) {
                        Some(slot) => *slot = comment.clone(),
                        None => msg.comments.push(comment.clone()),
                    }
                }
            }
        }
        E::CommentRemoved { message_id, comment_id } => {
            if let Ok(uuid) = uuid::Uuid::parse_str(message_id) {
                if let Some(msg) = session.messages.iter_mut().find(|m| m.id == uuid) {
                    msg.comments.retain(|c| &c.id != comment_id);
                }
            }
        }
        E::ComposioProfileSet { profile } => session.composio_profile = profile.clone(),
        E::SessionForked { .. } => {} // provenance marker — no state effect
        E::MessageEdited { message_id, content } => {
            use crate::components::shared::MessageContent;
            match session.messages.iter_mut().find(|m| &m.id == message_id) {
                Some(msg) => match &mut msg.content {
                    MessageContent::Text { content: text, .. } => *text = content.clone(),
                    _ => tracing::warn!(
                        "project: MessageEdited targets non-text message {message_id}; skipping"
                    ),
                },
                None => tracing::warn!(
                    "project: MessageEdited targets unknown (or truncated) message {message_id}; skipping"
                ),
            }
        }
    }
}

// ── Rewind as replay (Phase 2, Part C) ──────────────────────────────────────

/// Undo from `message_id` (inclusive) to the end of the session.
///
/// Journal-complete sessions (journal starts with `SessionCreated`) take the
/// replay path: journal a `RewoundTo`, then rebuild the whole session with
/// `project(None, all_events)` and swap it in — no surgical mutation.
/// Pre-journal sessions fall back to the legacy in-place
/// `delete_message_and_after` (which journals its own `RewoundTo`).
///
/// Returns the number of messages removed (0 when nothing matched).
pub fn rewind_session_state(
    state: &mut SessionState,
    session_id: &str,
    message_id: &str,
) -> usize {
    use crate::session_store as store;

    let replayable = store::journal_starts_with_creation(session_id);
    let anchor = store::first_event_seq_for_message(session_id, message_id);

    if replayable {
        if let Some(seq) = anchor {
            log_event_sync(
                session_id,
                SessionEvent::RewoundTo { seq, message_id: message_id.to_string() },
            );
            let events = store::load_events(session_id, 0);
            let projected = project(None, &events);
            let old_len = state
                .sessions
                .get(session_id)
                .map(|s| s.messages.len())
                .unwrap_or(0);
            let removed = old_len.saturating_sub(projected.messages.len());
            state.sessions.insert(session_id.to_string(), projected);
            tracing::info!(
                "rewind_session_state: replayed session {session_id} ({removed} message(s) removed)"
            );
            return removed;
        }
        tracing::warn!(
            "rewind_session_state: no journal anchor for message {message_id} in journal-complete session {session_id}; falling back to legacy undo"
        );
    }

    state
        .sessions
        .get_mut(session_id)
        .map(|s| s.delete_message_and_after(message_id))
        .unwrap_or(0)
}

// ── Drift guard (Phase 2, Part E — debug builds only) ───────────────────────

/// Compare a freshly-hydrated stored session against its journal projection
/// and `tracing::warn!` a concise diff on mismatch. Never panics, never blocks
/// hydration; only runs for journal-complete sessions.
#[cfg(debug_assertions)]
pub fn debug_check_drift(stored: &Session) {
    use crate::session_store as store;
    if !store::journal_starts_with_creation(&stored.id) {
        return;
    }
    let events = store::load_events(&stored.id, 0);
    if events.is_empty() {
        return;
    }
    let projected = project(None, &events);

    let mut diffs: Vec<String> = Vec::new();
    if projected.name != stored.name {
        diffs.push(format!("name: stored={:?} projected={:?}", stored.name, projected.name));
    }
    if projected.scratchpad != stored.scratchpad {
        diffs.push(format!(
            "scratchpad: stored {}B vs projected {}B",
            stored.scratchpad.len(),
            projected.scratchpad.len()
        ));
    }
    let mut stored_skills: Vec<&String> = stored.loaded_skills.keys().collect();
    let mut projected_skills: Vec<&String> = projected.loaded_skills.keys().collect();
    stored_skills.sort();
    projected_skills.sort();
    if stored_skills != projected_skills {
        diffs.push(format!(
            "loaded_skills keys: stored={stored_skills:?} projected={projected_skills:?}"
        ));
    }
    let fingerprint = |m: &Message| {
        (m.id, serde_json::to_string(&m.content).map(|s| s.len()).unwrap_or(0))
    };
    let stored_msgs: Vec<_> = stored.messages.iter().map(fingerprint).collect();
    let projected_msgs: Vec<_> = projected.messages.iter().map(fingerprint).collect();
    if stored_msgs != projected_msgs {
        let first_diff = stored_msgs
            .iter()
            .zip(projected_msgs.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(stored_msgs.len().min(projected_msgs.len()));
        diffs.push(format!(
            "messages: stored {} vs projected {} (first divergence at index {first_diff})",
            stored_msgs.len(),
            projected_msgs.len()
        ));
    }

    if !diffs.is_empty() {
        tracing::warn!(
            "session_events drift: session {} diverges from its journal projection — {}",
            stored.id,
            diffs.join("; ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus, UsageData};
    use chrono::{Duration, TimeZone, Utc};

    fn base_ts() -> chrono::DateTime<chrono::Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    /// Wrap events into LoadedSessionEvents with deterministic seqs/timestamps
    /// (one second apart, in order).
    fn loaded(events: Vec<SessionEvent>) -> Vec<LoadedSessionEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoadedSessionEvent {
                seq: (i as i64) + 1,
                ts: base_ts() + Duration::seconds(i as i64),
                event,
            })
            .collect()
    }

    fn text_message(author: &str, content: &str, at_secs: i64) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.to_string(),
            content: MessageContent::Text {
                content: content.to_string(),
                thought_signature: None,
                thought_summary: None,
            },
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: base_ts() + Duration::seconds(at_secs),
            usage: None,
        }
    }

    fn usage(total: i32, cost: f64) -> Option<UsageData> {
        Some(UsageData {
            prompt_tokens: total / 2,
            completion_tokens: total - total / 2,
            total_tokens: total,
            thoughts_tokens: None,
            cached_content_tokens: None,
            cost: Some(cost),
        })
    }

    fn tool_call_message(status: ToolCallStatus, at_secs: i64) -> Message {
        let mut tc = ToolCall::new(
            "server".to_string(),
            "TOOL_NAME".to_string(),
            serde_json::json!({"arg": 1}),
            None,
            None,
        );
        tc.status = status;
        Message {
            id: uuid::Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(tc),
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: base_ts() + Duration::seconds(at_secs),
            usage: None,
        }
    }

    fn exec_id(msg: &Message) -> String {
        match &msg.content {
            MessageContent::ToolCall(tc) => tc.execution_id.clone(),
            _ => panic!("not a tool call"),
        }
    }

    // ── Projector ───────────────────────────────────────────────────────────

    /// Birth-to-current replay of a scripted session reproduces every field
    /// the journal covers.
    #[test]
    fn project_replays_scripted_session_from_birth() {
        let user = text_message("User", "read that file", 1);
        let mut tool = tool_call_message(ToolCallStatus::Running, 2);
        // Assistant placeholder was pushed before the tool call (created_at 1.5s)
        // but is journaled after it — sorted insertion must restore live order.
        let mut assistant = text_message("Hobbes", "", 1);
        assistant.created_at = base_ts() + Duration::milliseconds(1500);
        assistant.content = MessageContent::Text {
            content: "done — the file says hi".to_string(),
            thought_signature: None,
            thought_summary: None,
        };
        assistant.usage = usage(100, 0.01);

        let comment = Comment {
            id: "c-1".to_string(),
            text_selection: "hi".to_string(),
            start_offset: 0,
            end_offset: 0,
            comment: "check this".to_string(),
        };
        let mut edited = comment.clone();
        edited.comment = "checked!".to_string();

        let timer = ScheduledTimer {
            id: "tmr_1".to_string(),
            session_id: "s1".to_string(),
            created_at: base_ts(),
            fire_at: base_ts() + Duration::seconds(600),
            mode: crate::timers::TimerMode::Notify,
            label: Some("tea".to_string()),
            prompt: None,
            status: crate::timers::TimerStatus::Pending,
        };

        let tool_done = {
            if let MessageContent::ToolCall(tc) = &mut tool.content {
                tc.status = ToolCallStatus::Completed;
                tc.response = "file contents".to_string();
            }
            tool.clone()
        };

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Birth".into() },
            SessionEvent::ComposioProfileSet { profile: Some("profile-1".into()) },
            SessionEvent::UserMessage { message: user.clone() },
            SessionEvent::ToolCall { message: tool.clone() },
            SessionEvent::ToolResult { message: tool_done.clone() },
            SessionEvent::AssistantMessage { message: assistant.clone() },
            SessionEvent::ScratchpadSet { content: "notes".into() },
            SessionEvent::SkillLoaded { name: "sk".into(), payload: "{}".into() },
            SessionEvent::SkillLoaded { name: "gone".into(), payload: "{}".into() },
            SessionEvent::SkillUnloaded { name: "gone".into() },
            SessionEvent::SummaryComputed {
                summary: serde_json::json!({"summary": "user read a file", "sentiment": "neutral"}),
            },
            SessionEvent::TimerCreated { timer: timer.clone() },
            SessionEvent::TimerCancelled { timer_id: "tmr_1".into() },
            SessionEvent::ConnectorPinned {
                connector_id: Some("conn-1".into()),
                provider: Some("Gemini".into()),
                model: Some("gemini-x".into()),
            },
            SessionEvent::CommentAdded {
                message_id: assistant.id.to_string(),
                comment: comment.clone(),
            },
            SessionEvent::CommentAdded {
                message_id: assistant.id.to_string(),
                comment: edited.clone(),
            },
            SessionEvent::MemoryOptimized { summary: "trimmed old context".into() },
            SessionEvent::SessionRenamed { name: "Renamed".into() },
        ]);

        let s = project(None, &events);
        assert_eq!(s.id, "s1");
        assert_eq!(s.name, "Renamed");
        assert_eq!(s.composio_profile.as_deref(), Some("profile-1"));
        assert_eq!(s.scratchpad, "notes");
        assert_eq!(s.loaded_skills.len(), 1);
        assert!(s.loaded_skills.contains_key("sk"));
        assert_eq!(s.active_context.conversation_summary.summary, "user read a file");
        assert_eq!(s.scheduled_timers.len(), 1);
        assert_eq!(s.scheduled_timers[0].status, crate::timers::TimerStatus::Cancelled);
        assert_eq!(s.llm_connector_id.as_deref(), Some("conn-1"));
        assert_eq!(s.llm_provider, Some(crate::settings::LlmProvider::Gemini));
        assert_eq!(s.chat_model.as_deref(), Some("gemini-x"));
        assert_eq!(s.memory_optimization_summary.as_deref(), Some("trimmed old context"));
        // Messages: user, assistant (created_at 1.5s — sorted before the tool
        // call despite being journaled last), tool call with folded result.
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[0].id, user.id);
        assert_eq!(s.messages[1].id, assistant.id);
        assert_eq!(s.messages[2].id, tool.id);
        match &s.messages[2].content {
            MessageContent::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(tc.response, "file contents");
            }
            other => panic!("expected folded ToolCall, got {other:?}"),
        }
        // Comment edit upserted, not duplicated.
        assert_eq!(s.messages[1].comments.len(), 1);
        assert_eq!(s.messages[1].comments[0].comment, "checked!");
        // last_updated advanced to the final event's row ts — no clock calls.
        assert_eq!(s.last_updated, events.last().unwrap().ts);
    }

    /// Differential test: RewoundTo through the projector produces the exact
    /// same outcome as the legacy `delete_message_and_after` on an identical
    /// session — cost/token harvest, tool_snapshot cleanup, summary reset.
    #[test]
    fn rewound_to_matches_legacy_delete_semantics() {
        let keep = text_message("User", "keep me", 0);
        let mut anchor = text_message("User", "undo from here", 1);
        anchor.usage = usage(50, 0.005);
        let tool = tool_call_message(ToolCallStatus::Completed, 2);
        let tool_exec = exec_id(&tool);
        let mut reply = text_message("Hobbes", "reply", 3);
        reply.usage = usage(200, 0.02);

        let prefix = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Diff".into() },
            SessionEvent::UserMessage { message: keep.clone() },
            SessionEvent::UserMessage { message: anchor.clone() },
            SessionEvent::ToolCall { message: tool.clone() },
            SessionEvent::AssistantMessage { message: reply.clone() },
            SessionEvent::SummaryComputed {
                summary: serde_json::json!({"summary": "stale summary"}),
            },
        ]);

        // Identical starting state for both paths, including summarizer
        // snapshots (never journaled — simulated on both copies).
        let build = || {
            let mut s = project(None, &prefix);
            s.active_context.extra.insert(
                format!("tool_snapshot_{tool_exec}"),
                serde_json::json!({"result_summary": "done"}),
            );
            s.active_context
                .extra
                .insert("unrelated_key".into(), serde_json::json!("survives"));
            s
        };
        let mut legacy = build();
        let projector_base = build();

        // Path A: legacy in-place undo.
        let removed_legacy = legacy.delete_message_and_after(&anchor.id.to_string());

        // Path B: the same rewind applied through the projector.
        let rewind = vec![LoadedSessionEvent {
            seq: 100,
            ts: base_ts() + Duration::seconds(100),
            event: SessionEvent::RewoundTo {
                seq: 3,
                message_id: anchor.id.to_string(),
            },
        }];
        let replayed = project(Some(projector_base), &rewind);

        assert_eq!(removed_legacy, 3);
        assert_eq!(legacy.messages.len(), replayed.messages.len());
        assert_eq!(legacy.messages[0].id, replayed.messages[0].id);
        assert!((legacy.accumulated_cost - replayed.accumulated_cost).abs() < 1e-9);
        assert_eq!(legacy.accumulated_tokens, replayed.accumulated_tokens);
        assert!((replayed.accumulated_cost - 0.025).abs() < 1e-9);
        assert_eq!(replayed.accumulated_tokens, 250);
        assert_eq!(legacy.active_context.extra, replayed.active_context.extra);
        assert!(!replayed
            .active_context
            .extra
            .contains_key(&format!("tool_snapshot_{tool_exec}")));
        assert!(replayed.active_context.extra.contains_key("unrelated_key"));
        assert_eq!(
            legacy.active_context.conversation_summary,
            replayed.active_context.conversation_summary
        );
        assert!(replayed.active_context.conversation_summary.summary.is_empty());
    }

    /// Nested rewinds accumulate harvested usage across both truncations.
    #[test]
    fn nested_rewinds_accumulate_usage() {
        let keep = text_message("User", "keep", 0);
        let mut first = text_message("User", "first undo target", 1);
        first.usage = usage(100, 0.01);
        let mut second = text_message("User", "second undo target", 2);
        second.usage = usage(40, 0.004);
        let mut third = text_message("Hobbes", "reply", 3);
        third.usage = usage(60, 0.006);

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Nested".into() },
            SessionEvent::UserMessage { message: keep.clone() },
            SessionEvent::UserMessage { message: first.clone() },
            // First rewind removes `first` (100 tokens, $0.01).
            SessionEvent::RewoundTo { seq: 3, message_id: first.id.to_string() },
            // New turn after the rewind…
            SessionEvent::UserMessage { message: second.clone() },
            SessionEvent::AssistantMessage { message: third.clone() },
            // …rewound again (40 + 60 tokens, $0.01 more).
            SessionEvent::RewoundTo { seq: 5, message_id: second.id.to_string() },
        ]);

        let s = project(None, &events);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].id, keep.id);
        assert_eq!(s.accumulated_tokens, 200);
        assert!((s.accumulated_cost - 0.02).abs() < 1e-9);
        // Mirrors Session::total_tokens semantics: harvested usage keeps
        // counting after the messages are gone.
        assert_eq!(s.total_tokens(), 200);
    }

    /// StreamCancelled removes journaled orphaned Running tool calls and
    /// preserves the partial assistant text.
    #[test]
    fn stream_cancelled_removes_orphans_and_keeps_partial() {
        let user = text_message("User", "go", 0);
        let orphan1 = tool_call_message(ToolCallStatus::Running, 2);
        let orphan2 = tool_call_message(ToolCallStatus::Running, 3);
        let partial = text_message("Hobbes", "partial thoughts…", 1);

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Cancel".into() },
            SessionEvent::UserMessage { message: user.clone() },
            SessionEvent::ToolCall { message: orphan1.clone() },
            SessionEvent::ToolCall { message: orphan2.clone() },
            SessionEvent::StreamCancelled {
                partial_message: Some(partial.clone()),
                removed_message_ids: vec![orphan2.id, orphan1.id],
            },
        ]);

        let s = project(None, &events);
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].id, user.id);
        assert_eq!(s.messages[1].id, partial.id);
    }

    /// Unknown kinds written by a future build are skipped by load_events and
    /// the projection still folds the rest.
    #[test]
    fn project_tolerates_unknown_kinds_in_journal() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            let user = text_message("User", "hello", 1);
            ts::append_events_conn(
                conn,
                "s1",
                &[SessionEvent::SessionCreated { id: "s1".into(), name: "Future".into() }],
            );
            ts::insert_raw_event(conn, "s1", "FromTheFuture", r#"{"kind":"FromTheFuture","x":1}"#);
            ts::append_events_conn(conn, "s1", &[SessionEvent::UserMessage { message: user.clone() }]);

            let events = ts::load_events_for_test(conn, "s1", 0);
            assert_eq!(events.len(), 2, "unknown kind must be skipped, not fatal");
            let s = project(None, &events);
            assert_eq!(s.id, "s1");
            assert_eq!(s.messages.len(), 1);
            assert_eq!(s.messages[0].id, user.id);
        });
    }

    // ── Fork ────────────────────────────────────────────────────────────────

    /// Fork copies exactly the at_seq prefix under new monotonic seqs,
    /// rewrites the birth identity, appends the marker, and leaves the
    /// source journal untouched.
    #[test]
    fn fork_copies_prefix_with_new_seqs() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            let m1 = text_message("User", "one", 1);
            let m2 = text_message("Hobbes", "two", 2);
            let m3 = text_message("User", "three", 3);
            ts::append_events_conn(
                conn,
                "src",
                &[
                    SessionEvent::SessionCreated { id: "src".into(), name: "Source".into() },
                    SessionEvent::UserMessage { message: m1.clone() },
                    SessionEvent::AssistantMessage { message: m2.clone() },
                    SessionEvent::UserMessage { message: m3.clone() },
                ],
            );
            let source_events = ts::load_events_for_test(conn, "src", 0);
            let source_max_seq = source_events.last().unwrap().seq;
            // Fork at m2's event — m3 must not come along.
            let at_seq = source_events[2].seq;

            let forked = ts::fork_events_for_test(conn, "src", Some(at_seq), "fork", "Fork of Source")
                .expect("fork should succeed");

            // Prefix (3 events) + SessionForked marker.
            assert_eq!(forked.len(), 4);
            match &forked[0].event {
                SessionEvent::SessionCreated { id, name } => {
                    assert_eq!(id, "fork");
                    assert_eq!(name, "Fork of Source");
                }
                other => panic!("first forked event must be SessionCreated, got {other:?}"),
            }
            match &forked[3].event {
                SessionEvent::SessionForked { from_session_id, at_seq: marker_seq } => {
                    assert_eq!(from_session_id, "src");
                    assert_eq!(*marker_seq, at_seq);
                }
                other => panic!("last forked event must be SessionForked, got {other:?}"),
            }
            // New seqs: strictly monotonic and past the source's high water.
            for pair in forked.windows(2) {
                assert!(pair[1].seq > pair[0].seq, "forked seqs must be monotonic");
            }
            assert!(forked[0].seq > source_max_seq);

            // Persisted rows match what was returned.
            let stored = ts::load_events_for_test(conn, "fork", 0);
            assert_eq!(stored.len(), 4);
            assert!(ts::journal_complete_for_test(conn, "fork"));

            // Source untouched.
            let source_after = ts::load_events_for_test(conn, "src", 0);
            assert_eq!(source_after.len(), 4);
            assert!(matches!(
                &source_after[0].event,
                SessionEvent::SessionCreated { id, .. } if id == "src"
            ));

            // Projection of the fork: m1 + m2 only, fork identity.
            let s = project(None, &forked);
            assert_eq!(s.id, "fork");
            assert_eq!(s.name, "Fork of Source");
            assert_eq!(s.messages.len(), 2);
            assert_eq!(s.messages[0].id, m1.id);
            assert_eq!(s.messages[1].id, m2.id);
        });
    }

    /// A source renamed within the copied prefix still forks under the fork
    /// name (the rename would otherwise clobber the rewritten birth name).
    #[test]
    fn fork_survives_copied_rename() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            ts::append_events_conn(
                conn,
                "src",
                &[
                    SessionEvent::SessionCreated { id: "src".into(), name: "Old".into() },
                    SessionEvent::SessionRenamed { name: "Shiny New Name".into() },
                ],
            );
            let forked = ts::fork_events_for_test(conn, "src", None, "fork", "Fork of Shiny New Name")
                .expect("fork should succeed");
            let s = project(None, &forked);
            assert_eq!(s.name, "Fork of Shiny New Name");
        });
    }

    /// Pre-journal sessions (no SessionCreated at the head) refuse to fork.
    #[test]
    fn fork_refuses_pre_journal_session() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            // A journal that exists but doesn't start with SessionCreated —
            // typical of sessions born before Phase 2.
            ts::append_events_conn(
                conn,
                "old",
                &[SessionEvent::ScratchpadSet { content: "legacy".into() }],
            );
            assert!(!ts::journal_complete_for_test(conn, "old"));
            let err = ts::fork_events_for_test(conn, "old", None, "fork", "Fork of old")
                .expect_err("pre-journal fork must fail");
            assert!(err.contains("predates the event journal"), "unexpected error: {err}");

            // No journal at all: same refusal.
            let err2 = ts::fork_events_for_test(conn, "missing", None, "fork2", "Fork")
                .expect_err("missing journal fork must fail");
            assert!(err2.contains("predates the event journal"));
            assert_eq!(ts::load_events_for_test(conn, "fork", 0).len(), 0);
        });
    }

    // ── MessageEdited (Phase 3) ─────────────────────────────────────────────

    /// The projector replaces only the text: attachments, comments, usage,
    /// created_at, and message ordering all survive the edit.
    #[test]
    fn message_edited_replaces_text_preserving_metadata() {
        let mut target = text_message("User", "original text", 1);
        target.attachments.push(hobbes_core::models::Attachment {
            file_name: "a.png".into(),
            mime_type: "image/png".into(),
            data: "b64data".into(),
        });
        target.usage = usage(10, 0.001);
        let reply = text_message("Hobbes", "reply", 2);
        let comment = Comment {
            id: "c1".into(),
            text_selection: "original".into(),
            start_offset: 0,
            end_offset: 0,
            comment: "note".into(),
        };

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Edit".into() },
            SessionEvent::UserMessage { message: target.clone() },
            SessionEvent::AssistantMessage { message: reply.clone() },
            SessionEvent::CommentAdded {
                message_id: target.id.to_string(),
                comment: comment.clone(),
            },
            SessionEvent::MessageEdited {
                message_id: target.id,
                content: "edited text".into(),
            },
        ]);

        let s = project(None, &events);
        // Ordering stable: the edit rewrites in place, it never moves the slot.
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].id, target.id);
        assert_eq!(s.messages[1].id, reply.id);
        let m = &s.messages[0];
        assert_eq!(m.content.get_text_content().as_deref(), Some("edited text"));
        assert_eq!(m.attachments, target.attachments);
        assert_eq!(m.comments, vec![comment]);
        assert_eq!(m.usage, target.usage);
        assert_eq!(m.created_at, target.created_at);
        // The neighbor is untouched.
        assert_eq!(s.messages[1].content.get_text_content().as_deref(), Some("reply"));
    }

    /// Unknown target ids skip with a warn; so does a target truncated by an
    /// earlier RewoundTo — the natural consequence of the fold. No panics,
    /// no resurrection.
    #[test]
    fn message_edited_unknown_or_truncated_target_skips() {
        let keep = text_message("User", "keep", 0);
        let victim = text_message("User", "will be rewound", 1);

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Skip".into() },
            SessionEvent::UserMessage { message: keep.clone() },
            SessionEvent::UserMessage { message: victim.clone() },
            // Unknown id: this message never existed.
            SessionEvent::MessageEdited {
                message_id: uuid::Uuid::new_v4(),
                content: "ghost".into(),
            },
            SessionEvent::RewoundTo { seq: 3, message_id: victim.id.to_string() },
            // Target was truncated by the rewind above — must skip, not re-add.
            SessionEvent::MessageEdited {
                message_id: victim.id,
                content: "too late".into(),
            },
        ]);

        let s = project(None, &events);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].id, keep.id);
        assert_eq!(s.messages[0].content.get_text_content().as_deref(), Some("keep"));
    }

    /// Save & Resend provenance: an edit journaled before a rewind of a LATER
    /// message replays to the edited prefix.
    #[test]
    fn edit_then_rewind_replays_edited_prefix() {
        let first = text_message("User", "hello", 1);
        let reply = text_message("Hobbes", "hi there", 2);
        let second = text_message("User", "follow-up", 3);

        let events = loaded(vec![
            SessionEvent::SessionCreated { id: "s1".into(), name: "Prefix".into() },
            SessionEvent::UserMessage { message: first.clone() },
            SessionEvent::AssistantMessage { message: reply.clone() },
            SessionEvent::UserMessage { message: second.clone() },
            SessionEvent::MessageEdited {
                message_id: first.id,
                content: "hello, edited".into(),
            },
            SessionEvent::RewoundTo { seq: 4, message_id: second.id.to_string() },
        ]);

        let s = project(None, &events);
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].id, first.id);
        assert_eq!(
            s.messages[0].content.get_text_content().as_deref(),
            Some("hello, edited")
        );
        assert_eq!(s.messages[1].id, reply.id);
    }

    /// The new variant round-trips through the store byte-for-byte and folds
    /// on load.
    #[test]
    fn message_edited_round_trips_through_store() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            let msg = text_message("User", "original", 1);
            let edit = SessionEvent::MessageEdited {
                message_id: msg.id,
                content: "edited".into(),
            };
            ts::append_events_conn(
                conn,
                "s1",
                &[
                    SessionEvent::SessionCreated { id: "s1".into(), name: "RT".into() },
                    SessionEvent::UserMessage { message: msg.clone() },
                    edit.clone(),
                ],
            );
            let events = ts::load_events_for_test(conn, "s1", 0);
            assert_eq!(events.len(), 3);
            assert_eq!(events[2].event, edit);
            assert_eq!(events[2].event.kind(), "MessageEdited");
            let s = project(None, &events);
            assert_eq!(s.messages.len(), 1);
            assert_eq!(s.messages[0].content.get_text_content().as_deref(), Some("edited"));
        });
    }

    /// Scripted Save flow on a journal-complete session: the live in-place
    /// mutation and the journal projection agree.
    #[test]
    fn save_flow_event_and_projection_match_live_mutation() {
        use crate::session_store::test_support as ts;
        ts::with_test_db(|conn| {
            let msg = text_message("User", "a tpyo here", 1);
            let reply = text_message("Hobbes", "noted", 2);
            ts::append_events_conn(
                conn,
                "s1",
                &[
                    SessionEvent::SessionCreated { id: "s1".into(), name: "Save".into() },
                    SessionEvent::UserMessage { message: msg.clone() },
                    SessionEvent::AssistantMessage { message: reply.clone() },
                ],
            );
            assert!(ts::journal_complete_for_test(conn, "s1"));

            // Live Save: mutate the hydrated session in place (what the UI
            // does)…
            let mut live = project(None, &ts::load_events_for_test(conn, "s1", 0));
            if let MessageContent::Text { content, .. } = &mut live.messages[0].content {
                *content = "a typo fixed".to_string();
            } else {
                panic!("expected text message");
            }
            // …and journal the same edit (what the Save handler appends).
            ts::append_events_conn(
                conn,
                "s1",
                &[SessionEvent::MessageEdited {
                    message_id: msg.id,
                    content: "a typo fixed".into(),
                }],
            );

            let projected = project(None, &ts::load_events_for_test(conn, "s1", 0));
            assert_eq!(projected.messages.len(), live.messages.len());
            for (p, l) in projected.messages.iter().zip(live.messages.iter()) {
                assert_eq!(p.id, l.id);
                assert_eq!(p.content, l.content);
                assert_eq!(p.created_at, l.created_at);
            }
        });
    }

    #[test]
    fn kind_matches_serde_tag() {
        let ev = SessionEvent::ScratchpadSet {
            content: "notes".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("kind").and_then(|k| k.as_str()), Some(ev.kind()));
    }

    #[test]
    fn rewound_to_tolerates_missing_message_id() {
        // Older rows (or trimmed writers) without message_id still parse.
        let ev: SessionEvent =
            serde_json::from_str(r#"{"kind":"RewoundTo","seq":42}"#).unwrap();
        assert_eq!(
            ev,
            SessionEvent::RewoundTo {
                seq: 42,
                message_id: String::new()
            }
        );
    }

    #[test]
    fn unknown_kind_fails_single_row_only() {
        // The enum itself must reject unknown kinds so load_events can skip
        // them row-by-row instead of poisoning a whole read.
        let res = serde_json::from_str::<SessionEvent>(
            r#"{"kind":"FromTheFuture","payload":123}"#,
        );
        assert!(res.is_err());
    }
}
