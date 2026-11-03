//! Session rotation for the system sessions (Heartbeat, Voice).
//!
//! Each session is one SQLite row whose whole `data` JSON is re-serialized
//! on every save, and the two system sessions never end, so their saves get
//! slower forever (the prompt is budgeted; storage and save cost are not).
//! When one passes a size threshold, at a quiet moment, Hobbes **rotates**
//! it: a fresh session becomes the system session and the old one stays in
//! History as a read-only archive named after its date range
//! ("Heartbeat · Oct 22 – Nov 3"). Nothing is deleted or trimmed.
//!
//! - **Continuity.** The successor carries the conversation summary, the
//!   scratchpad, the handoff note, the Composio profile, any connector pin,
//!   the dispatch-relay worker list, pending timers not due soon, the tab
//!   emoji and the name ([`CarryOver`]), and opens with a Hobbes message
//!   ([`continuation_text`]) restating the summary, current task and handoff
//!   — the summary otherwise reaches the prompt only after a history trim,
//!   and the handoff only through a briefing that needs a previous turn.
//! - **Atomic.** The archive row (renamed), the successor row, the archive
//!   list and the feature's pointer are written in one transaction
//!   ([`commit`]). Only then does memory change; a failed write leaves
//!   everything as it was, and the next check retries. A pointer that a
//!   late, older save sets back to the archive is followed forward at the
//!   next load (`system_sessions::resolve_successor`).
//! - **Lineage.** `system_sessions` records the archive, so the archive keeps
//!   its icon and exemptions, and anything still addressed to it (a dispatch
//!   result, a relay, a queued message) lands in the successor
//!   (`system_sessions::current_for`).
//! - **Tabs.** An open tab of the old session is replaced in place by the
//!   successor (`crate::request_tab_replace`), staying active if it was; the
//!   archive leaves memory (it stays on disk, in History) so it no longer
//!   rides every save.
//!
//! The decision ([`should_rotate`]) is pure; [`check_all`] runs it from the
//! Heartbeat runtime's tick, which runs every 30 s whether or not the
//! Heartbeat is on, at most every [`CHECK_EVERY_MINUTES`].

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Duration, Local, Utc};
use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::session::{ConversationSummary, Session, SessionState};
use crate::session_events::SessionEvent;
use crate::system_sessions::{self, SystemSession};
use crate::timers::{ScheduledTimer, TimerStatus};

/// Rotate past this many messages. The Heartbeat measured 287 messages /
/// 2.7 MB after 12 days (~9 KB a message, tool results included), so 250
/// lands near the byte cap for it and archives roughly every week and a
/// half, a range still worth reading back in History.
pub const MAX_MESSAGES: usize = 250;
/// Rotate past this many bytes of stored JSON. 1.5 MB keeps each save of a
/// system session to a few milliseconds of serialization and hashing (every
/// save re-serializes every hydrated session to fingerprint it); Voice,
/// at ~2 MB a day, rotates about daily on this cap.
pub const MAX_BYTES: u64 = 1_500_000;
/// Never rotate a session shorter than this, whatever its size: a few huge
/// tool results would otherwise make archives of a handful of messages.
pub const MIN_MESSAGES: usize = 20;
/// Past this multiple of a threshold the soft rules (a stale approval, a
/// recent read from another company) stop postponing rotation, so a session
/// can't grow without bound.
pub const FORCE_FACTOR: u64 = 2;
/// How often the sizes are checked (one indexed `length()` query each).
pub const CHECK_EVERY_MINUTES: i64 = 5;
/// Nothing may have happened in the session for this long: no message, no
/// update, no timer fired. It covers the short gaps inside a turn that no
/// flag marks (the MCP context fetch before a stream starts, the hand-off to
/// a continuation, the streaming mirror's lag, a popped timer prompt), which
/// last seconds; three minutes is far above them and costs nothing, as
/// rotation is never urgent.
pub const IDLE_MINUTES: i64 = 3;
/// A pending timer due within this long holds rotation (it is about to fire
/// in this session); later ones move to the successor.
pub const TIMER_SOON_MINUTES: i64 = 60;
/// An unresolved approval newer than this holds rotation (the person may
/// still answer it); older ones stay in the archive, unanswerable, unless
/// they belong to the latest turn.
pub const APPROVAL_STALE_HOURS: i64 = 24;

/// Session ids with turn machinery in flight beyond `streaming_sessions`
/// (stream map, continuation, summarization). Mirrored by `StreamBridge`
/// in `main.rs` from `StreamManagerContext::turn_activity`.
pub static TURN_ACTIVITY: GlobalSignal<HashSet<String>> = Signal::global(HashSet::new);

/// A system session's size: messages in memory, bytes of its stored row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSize {
    pub messages: usize,
    /// `None` when the row hasn't been written yet.
    pub bytes: Option<u64>,
}

/// Over a threshold (and long enough to be worth archiving).
pub fn over_threshold(size: SessionSize) -> bool {
    size.messages >= MIN_MESSAGES
        && (size.messages > MAX_MESSAGES || size.bytes.is_some_and(|b| b > MAX_BYTES))
}

/// Past [`FORCE_FACTOR`] × a threshold: only the hard rules still hold it.
pub fn oversized(size: SessionSize) -> bool {
    size.messages as u64 > MAX_MESSAGES as u64 * FORCE_FACTOR
        || size.bytes.is_some_and(|b| b > MAX_BYTES * FORCE_FACTOR)
}

/// What could be in flight in a system session. The hard rules always hold
/// rotation: something would otherwise run against (or be read back from) a
/// session that is no longer current. The soft rules hold it only until the
/// session is [`oversized`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    // ── Hard ──
    /// A turn is under way: streaming, a stream or continuation in flight,
    /// a summarization running, or a popped queue message not yet streaming.
    pub turn_in_flight: bool,
    /// Messages wait in its background chat queue.
    pub queued_messages: bool,
    /// Something happened within [`IDLE_MINUTES`].
    pub recently_active: bool,
    /// An unresolved approval in the latest turn.
    pub latest_turn_approval: bool,
    /// A pending timer due within [`TIMER_SOON_MINUTES`].
    pub timer_due_soon: bool,
    /// Heartbeat: a fired wake hasn't been read back yet (its cost and
    /// outcome come from this session's messages).
    pub open_wake: bool,
    /// Voice: a conversation is live.
    pub voice_live: bool,
    /// Voice: brain hand-offs still waiting on a turn in this session.
    pub brain_waits: usize,
    // ── Soft ──
    /// An unresolved approval newer than [`APPROVAL_STALE_HOURS`].
    pub recent_approval: bool,
    /// Composio data from a profile other than home was read within
    /// `profile_reach::READ_WINDOW`. The cross-company hold looks for such
    /// reads in the session's own messages, which stay in the archive, while
    /// the carried summary may still hold that data.
    pub recent_foreign_reads: bool,
}

impl Activity {
    /// None of the hard rules hold.
    pub fn hard_quiet(&self) -> bool {
        !self.turn_in_flight
            && !self.queued_messages
            && !self.recently_active
            && !self.latest_turn_approval
            && !self.timer_due_soon
            && !self.open_wake
            && !self.voice_live
            && self.brain_waits == 0
    }

    /// None of the soft rules hold.
    pub fn soft_quiet(&self) -> bool {
        !self.recent_approval && !self.recent_foreign_reads
    }
}

/// Rotate now: over a threshold, at a quiet moment (the soft rules give way
/// once the session is [`oversized`]).
pub fn should_rotate(size: SessionSize, activity: &Activity) -> bool {
    over_threshold(size) && activity.hard_quiet() && (activity.soft_quiet() || oversized(size))
}

/// Unresolved approvals: (any in the latest turn, any newer than
/// [`APPROVAL_STALE_HOURS`]). The latest turn starts at the last user text
/// message.
pub fn approval_state(messages: &[Message], now: DateTime<Utc>) -> (bool, bool) {
    let turn_start = messages
        .iter()
        .rposition(|m| m.author == "User" && matches!(m.content, MessageContent::Text { .. }))
        .unwrap_or(0);
    let mut latest = false;
    let mut recent = false;
    for (i, m) in messages.iter().enumerate() {
        if matches!(m.content, MessageContent::PermissionRequest(_) | MessageContent::SkillPermissionRequest(_)) {
            latest |= i >= turn_start;
            recent |= now - m.created_at < Duration::hours(APPROVAL_STALE_HOURS);
        }
    }
    (latest, recent)
}

/// When the session last did anything: its newest message, its last update,
/// or a timer firing.
pub fn last_activity(session: &Session) -> DateTime<Utc> {
    let newest_message = session.messages.iter().map(|m| m.created_at).max();
    let fired = session.scheduled_timers.iter().filter_map(|t| t.fired_at).max();
    [Some(session.last_updated), newest_message, fired].into_iter().flatten().max().unwrap_or(session.last_updated)
}

/// A pending timer is due within [`TIMER_SOON_MINUTES`].
pub fn timer_due_soon(timers: &[ScheduledTimer], now: DateTime<Utc>) -> bool {
    timers
        .iter()
        .any(|t| t.status == TimerStatus::Pending && t.fire_at <= now + Duration::minutes(TIMER_SOON_MINUTES))
}

/// The archive's name: the title and the local date range of its messages,
/// "Heartbeat · Oct 22 – Nov 3" (one date when they share a day, years
/// when the range crosses one).
pub fn archive_name(title: &str, first: DateTime<Local>, last: DateTime<Local>) -> String {
    let (first, last) = if first <= last { (first, last) } else { (last, first) };
    let range = if first.date_naive() == last.date_naive() {
        first.format("%b %-d").to_string()
    } else if first.year() == last.year() {
        format!("{} – {}", first.format("%b %-d"), last.format("%b %-d"))
    } else {
        format!("{} – {}", first.format("%b %-d, %Y"), last.format("%b %-d, %Y"))
    };
    format!("{title} · {range}")
}

/// [`archive_name`] for a session: its first and last message times, else
/// its last update.
pub fn archive_name_for(title: &str, session: &Session) -> String {
    let first = session.messages.iter().map(|m| m.created_at).min().unwrap_or(session.last_updated);
    let last = session.messages.iter().map(|m| m.created_at).max().unwrap_or(session.last_updated);
    archive_name(title, first.with_timezone(&Local), last.with_timezone(&Local))
}

/// What a rotated system session hands to its successor.
#[derive(Debug, Clone, PartialEq)]
pub struct CarryOver {
    pub name: String,
    pub tab_emoji: Option<String>,
    pub composio_profile: Option<String>,
    pub llm_connector_id: Option<String>,
    pub llm_provider: Option<crate::settings::LlmProvider>,
    pub chat_model: Option<String>,
    pub scratchpad: String,
    pub conversation_summary: ConversationSummary,
    pub handoff: Option<crate::handoff::HandoffNote>,
    /// Dispatch-relay workers: their reports, and the parent's
    /// `HOBBES_MESSAGE_WORKER`, keep working from the new session.
    pub dispatched_workers: Vec<String>,
}

impl CarryOver {
    pub fn from_session(s: &Session) -> Self {
        Self {
            name: s.name.clone(),
            tab_emoji: s.tab_emoji.clone(),
            composio_profile: s.composio_profile.clone(),
            llm_connector_id: s.llm_connector_id.clone(),
            llm_provider: s.llm_provider,
            chat_model: s.chat_model.clone(),
            scratchpad: s.scratchpad.clone(),
            conversation_summary: s.active_context.conversation_summary.clone(),
            handoff: s.handoff.clone(),
            dispatched_workers: s.dispatched_workers.clone(),
        }
    }

    /// Write everything onto the new session except the name (set through
    /// `update_session_name_raw`, which journals it as a user-set name so
    /// it is never retitled).
    pub fn apply(&self, s: &mut Session) {
        s.tab_emoji = self.tab_emoji.clone();
        s.composio_profile = self.composio_profile.clone();
        s.llm_connector_id = self.llm_connector_id.clone();
        s.llm_provider = self.llm_provider;
        s.chat_model = self.chat_model.clone();
        s.scratchpad = self.scratchpad.clone();
        s.active_context.conversation_summary = self.conversation_summary.clone();
        s.handoff = self.handoff.clone();
        s.dispatched_workers = self.dispatched_workers.clone();
    }

    /// The journal events that reproduce [`Self::apply`] on replay (the
    /// profile is journaled at creation, the name by the rename).
    pub fn events(&self) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        if self.tab_emoji.is_some() {
            out.push(SessionEvent::TabTitled { emoji: self.tab_emoji.clone(), title: None });
        }
        if self.llm_connector_id.is_some() || self.llm_provider.is_some() || self.chat_model.is_some() {
            out.push(SessionEvent::ConnectorPinned {
                connector_id: self.llm_connector_id.clone(),
                // Journaled via `{:?}` like the per-tab picker does.
                provider: self.llm_provider.map(|p| format!("{p:?}")),
                model: self.chat_model.clone(),
            });
        }
        if !self.scratchpad.is_empty() {
            out.push(SessionEvent::ScratchpadSet { content: self.scratchpad.clone() });
        }
        if self.conversation_summary != ConversationSummary::default() {
            if let Ok(summary) = serde_json::to_value(&self.conversation_summary) {
                out.push(SessionEvent::SummaryComputed { summary });
            }
        }
        if let Some(note) = &self.handoff {
            out.push(SessionEvent::HandoffWritten { note: note.clone() });
        }
        for w in &self.dispatched_workers {
            out.push(SessionEvent::WorkerAdded { worker_session_id: w.clone() });
        }
        out
    }
}

/// The opening message of a successor: where it continues from, then the
/// summary, current task and handoff note it carries.
pub fn continuation_text(archive_title: &str, carry: &CarryOver) -> String {
    let mut out = format!("Continuing from \"{archive_title}\" (archived in History).");
    let cs = &carry.conversation_summary;
    if !cs.summary.trim().is_empty() {
        out.push_str(&format!("\n\nSummary so far: {}", cs.summary.trim()));
    }
    if !cs.current_task.trim().is_empty() {
        out.push_str(&format!("\n\nCurrent task: {}", cs.current_task.trim()));
    }
    if let Some(note) = &carry.handoff {
        out.push_str(&format!("\n\nHandoff note:\nDoing: {}\nNext: {}", note.doing.trim(), note.expect_next.trim()));
        if let Some(change) = note.would_change_mind.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            out.push_str(&format!("\nWould change plan if: {change}"));
        }
    }
    out
}

/// A rotation worked out in full before anything is written.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub old_id: String,
    pub new_id: String,
    pub archive_title: String,
    /// The old session as it is archived: renamed, its moved timers marked
    /// cancelled.
    pub archive: Session,
    pub successor: Session,
    /// Journal events for the successor (birth, carry-over, intro message).
    pub successor_events: Vec<SessionEvent>,
    /// Journal events for the archive (rename, moved timers).
    pub archive_events: Vec<SessionEvent>,
}

/// Work out a rotation of `old` into a new session `new_id` at `now`. Pure:
/// one clone of the old session (once per rotation) so nothing in memory
/// changes until the write succeeds.
pub fn prepare(kind: SystemSession, old: &Session, new_id: &str, now: DateTime<Utc>) -> Prepared {
    let carry = CarryOver::from_session(old);
    let archive_title = archive_name_for(kind.title(), old);

    let mut archive = old.clone();
    archive.name = archive_title.clone();
    archive.name_user_set = true;
    let mut archive_events = vec![SessionEvent::SessionRenamed { name: archive_title.clone() }];
    // Pending timers (none due soon, or rotation would wait) move to the
    // successor under the same id; the archive's copies are cancelled, so
    // the archive never fires or loads at startup.
    let mut moved = Vec::new();
    for t in archive.scheduled_timers.iter_mut().filter(|t| t.status == TimerStatus::Pending) {
        let mut m = t.clone();
        m.session_id = new_id.to_string();
        moved.push(m);
        t.status = TimerStatus::Cancelled;
        archive_events.push(SessionEvent::TimerCancelled { timer_id: t.id.clone() });
    }

    let mut successor = crate::session_events::blank_session();
    successor.id = new_id.to_string();
    successor.name = carry.name.clone();
    successor.name_user_set = true;
    successor.last_updated = now;
    carry.apply(&mut successor);
    successor.scheduled_timers = moved.clone();
    let intro = Message {
        id: uuid::Uuid::new_v4(),
        author: "Hobbes".to_string(),
        content: MessageContent::Text {
            content: continuation_text(&archive_title, &carry),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        },
        attachments: Vec::new(),
        comments: Vec::new(),
        created_at: now,
        usage: None,
    };
    successor.messages.push(intro.clone());

    let mut successor_events = vec![SessionEvent::SessionCreated { id: new_id.to_string(), name: carry.name.clone() }];
    if carry.composio_profile.is_some() {
        successor_events.push(SessionEvent::ComposioProfileSet { profile: carry.composio_profile.clone() });
    }
    successor_events.push(SessionEvent::SessionRenamed { name: carry.name.clone() });
    successor_events.extend(carry.events());
    successor_events.extend(moved.into_iter().map(|timer| SessionEvent::TimerCreated { timer }));
    successor_events.push(SessionEvent::AssistantMessage { message: intro });

    Prepared {
        old_id: old.id.clone(),
        new_id: new_id.to_string(),
        archive_title,
        archive,
        successor,
        successor_events,
        archive_events,
    }
}

// ── The Dioxus side ─────────────────────────────────────────────────────────

/// The current id of a system session.
fn current_id(kind: SystemSession) -> Option<String> {
    match kind {
        SystemSession::Heartbeat => crate::heartbeat::HEARTBEAT.peek().session_id.clone(),
        SystemSession::Voice => crate::voice::session::VOICE_SESSION_ID.peek().clone(),
    }
}

/// What is in flight for a system session right now.
fn activity(
    kind: SystemSession,
    session: &Session,
    settings: &crate::settings::Settings,
    streaming: &HashSet<String>,
    now: DateTime<Utc>,
) -> Activity {
    use crate::mcp::manager::profile_reach;
    let id = session.id.as_str();
    let (latest_turn_approval, recent_approval) = approval_state(&session.messages, now);
    let home = profile_reach::home_profile(settings, session.composio_profile.as_deref());
    let recent_foreign_reads = profile_reach::session_read_profiles(settings, &session.messages, now)
        .iter()
        .any(|p| Some(p) != home.as_ref());
    let mut a = Activity {
        turn_in_flight: streaming.contains(id)
            || TURN_ACTIVITY.peek().contains(id)
            || crate::components::chat_queue::DISPATCHING.peek().contains(id),
        queued_messages: crate::components::chat_queue::CHAT_QUEUE.peek().get(id).is_some_and(|q| !q.is_empty()),
        recently_active: now - last_activity(session) < Duration::minutes(IDLE_MINUTES),
        latest_turn_approval,
        timer_due_soon: timer_due_soon(&session.scheduled_timers, now),
        recent_approval,
        recent_foreign_reads,
        ..Default::default()
    };
    match kind {
        SystemSession::Heartbeat => {
            a.open_wake = crate::heartbeat::HEARTBEAT
                .peek()
                .trail
                .iter()
                .any(|w| !w.deferred && w.cost_usd.is_none() && w.outcome.is_none());
        }
        SystemSession::Voice => {
            a.voice_live = crate::voice::controller::is_live();
            a.brain_waits = crate::voice::brain::waits_in_flight();
        }
    }
    a
}

/// Check both system sessions and rotate any that are due. Only a session
/// already in memory is considered (one on disk isn't growing). Synchronous:
/// no signal guard crosses an await.
pub fn check_all(
    session_state: Signal<SessionState>,
    settings: &crate::settings::Settings,
    streaming: &HashSet<String>,
) {
    if session_state.peek().save_disabled {
        return;
    }
    let now = Utc::now();
    for kind in SystemSession::ALL {
        let Some(id) = current_id(kind) else { continue };
        let prepared = {
            let state = session_state.peek();
            let Some(session) = state.sessions.get(&id) else { continue };
            let messages = session.messages.len();
            if messages < MIN_MESSAGES {
                continue;
            }
            let size = SessionSize { messages, bytes: crate::session_store::row_bytes(&id) };
            if !over_threshold(size) {
                continue;
            }
            let a = activity(kind, session, settings, streaming, now);
            if !should_rotate(size, &a) {
                tracing::debug!(session_id = %id, "rotation: {kind:?} session is due but busy");
                continue;
            }
            tracing::info!(session_id = %id, messages, bytes = size.bytes.unwrap_or(0), "rotation: rotating the {kind:?} session");
            prepare(kind, session, &uuid::Uuid::new_v4().to_string(), now)
        };
        rotate(session_state, kind, prepared);
    }
}

/// The `meta` entry holding the feature's pointer, set to `new_id`.
fn pointer_meta(kind: SystemSession, new_id: &str) -> Result<(String, String), String> {
    match kind {
        SystemSession::Heartbeat => {
            let mut hb = crate::heartbeat::HEARTBEAT.peek().clone();
            hb.session_id = Some(new_id.to_string());
            let json = serde_json::to_string(&hb).map_err(|e| e.to_string())?;
            Ok((crate::heartbeat::META_KEY.to_string(), json))
        }
        SystemSession::Voice => Ok((crate::voice::session::META_KEY.to_string(), new_id.to_string())),
    }
}

/// Write the archive row, the successor row, the archive list and the
/// feature's pointer in one transaction, on the calling thread (P-009's
/// serialize-then-write, minus the hop: memory must not change before the
/// write is known to have landed). A rotation happens about once a week.
fn commit(kind: SystemSession, p: &Prepared) -> Result<(), String> {
    use crate::session_store as store;
    let rows = vec![
        store::build_row(&p.archive, store::next_seq()).map_err(|e| e.to_string())?,
        store::build_row(&p.successor, store::next_seq()).map_err(|e| e.to_string())?,
    ];
    let archives = system_sessions::archives_after_rotation(kind, &p.old_id, &p.new_id);
    let archives_json = serde_json::to_string(&archives).map_err(|e| e.to_string())?;
    let (pointer_key, pointer_value) = pointer_meta(kind, &p.new_id)?;
    let meta = vec![
        (system_sessions::ARCHIVES_META_KEY.to_string(), archives_json, store::next_seq()),
        (pointer_key, pointer_value, store::next_seq()),
    ];
    store::write_rows_and_meta(&rows, &meta)?;
    store::mark_rows_saved(&rows);
    Ok(())
}

/// Persist `p`, then switch memory, the feature and the tab over to the
/// successor. On a failed write nothing changes (the next check retries).
/// Returns the new id.
pub fn rotate(mut session_state: Signal<SessionState>, kind: SystemSession, p: Prepared) -> Option<String> {
    if let Err(e) = commit(kind, &p) {
        tracing::warn!(session_id = %p.old_id, "rotation: write failed, {kind:?} session left as it was: {e}");
        return None;
    }
    let Prepared { old_id, new_id, archive_title, archive, successor, successor_events, archive_events } = p;
    let closing_brief = {
        let cs = &archive.active_context.conversation_summary;
        (!cs.summary.trim().is_empty() || !cs.current_task.trim().is_empty())
            .then(|| serde_json::to_value(cs).ok())
            .flatten()
    };
    drop(archive);
    {
        let mut state = session_state.write();
        state.sessions.insert(new_id.clone(), successor);
        // The archive leaves memory: it is on disk and in History.
        state.sessions.remove(&old_id);
        if state.active_session_id == old_id || state.active_session_id.is_empty() {
            state.active_session_id = new_id.clone();
        }
    }
    system_sessions::apply_rotation(kind, &old_id, &new_id);
    match kind {
        SystemSession::Heartbeat => {
            crate::heartbeat::HEARTBEAT.write().session_id = Some(new_id.clone());
            // A newer generation than any save still in flight with the old
            // id, so a late one can't win (and a crash before it lands is
            // resolved at load).
            crate::heartbeat::save_signal();
        }
        SystemSession::Voice => {
            *crate::voice::session::VOICE_SESSION_ID.write() = Some(new_id.clone());
        }
    }
    crate::session_events::log_events(&new_id, successor_events);
    crate::session_events::log_events(&old_id, archive_events);

    // The archive's Fleet row ends, with its summary as the closing brief.
    crate::fleet::bridge::report_with_brief(&old_id, "", crate::fleet::bridge::HobbesSignal::Closed, closing_brief);
    crate::llm::claude::keepalive::cancel_session(&old_id);
    crate::request_tab_replace(&old_id, &new_id);
    tracing::info!(old_session = %old_id, new_session = %new_id, archive = %archive_title, "rotation: {kind:?} session archived and replaced");
    Some(new_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn thresholds() {
        let s = |messages, bytes| SessionSize { messages, bytes };
        assert!(!over_threshold(s(MAX_MESSAGES, Some(MAX_BYTES))), "at the caps is fine");
        assert!(over_threshold(s(MAX_MESSAGES + 1, None)));
        assert!(over_threshold(s(MIN_MESSAGES, Some(MAX_BYTES + 1))));
        // A short session is never archived, however large.
        assert!(!over_threshold(s(MIN_MESSAGES - 1, Some(MAX_BYTES * 10))));
        assert!(!over_threshold(s(100, None)));
    }

    #[test]
    fn rotates_only_at_a_quiet_moment() {
        let big = SessionSize { messages: MAX_MESSAGES + 1, bytes: Some(10) };
        let small = SessionSize { messages: 10, bytes: Some(10) };
        let quiet = Activity::default();
        assert!(should_rotate(big, &quiet));
        assert!(!should_rotate(small, &quiet));
        let hard = [
            Activity { turn_in_flight: true, ..quiet },
            Activity { queued_messages: true, ..quiet },
            Activity { recently_active: true, ..quiet },
            Activity { latest_turn_approval: true, ..quiet },
            Activity { timer_due_soon: true, ..quiet },
            Activity { open_wake: true, ..quiet },
            Activity { voice_live: true, ..quiet },
            Activity { brain_waits: 1, ..quiet },
        ];
        let huge = SessionSize { messages: MAX_MESSAGES * 3, bytes: Some(MAX_BYTES * 3) };
        for a in hard {
            assert!(!should_rotate(big, &a), "{a:?}");
            // Hard rules hold however large the session.
            assert!(!should_rotate(huge, &a), "{a:?}");
        }
        let soft = [Activity { recent_approval: true, ..quiet }, Activity { recent_foreign_reads: true, ..quiet }];
        for a in soft {
            assert!(!should_rotate(big, &a), "{a:?}");
            // Past twice a threshold the soft rules give way.
            assert!(should_rotate(huge, &a), "{a:?}");
            assert!(should_rotate(SessionSize { messages: 30, bytes: Some(MAX_BYTES * FORCE_FACTOR + 1) }, &a));
        }
        assert!(!oversized(SessionSize { messages: MAX_MESSAGES * 2, bytes: Some(MAX_BYTES * 2) }));
    }

    fn msg(author: &str, content: MessageContent, at: DateTime<Utc>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content,
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: at,
            usage: None,
        }
    }

    fn text(s: &str) -> MessageContent {
        MessageContent::Text { content: s.into(), thought_signature: None, thought_summary: None, signature_provider: None }
    }

    fn request() -> MessageContent {
        MessageContent::PermissionRequest(
            serde_json::from_value(serde_json::json!({
                "execution_id": "e", "server_name": "s", "tool_name": "SEND", "arguments": "{}",
                "status": "Running", "response": "", "thought_signature": null
            }))
            .unwrap(),
        )
    }

    #[test]
    fn only_a_live_approval_holds_rotation() {
        let now = Utc::now();
        let old = now - Duration::hours(APPROVAL_STALE_HOURS + 1);
        // A stale request in an earlier turn: neither latest nor recent.
        let msgs = vec![msg("User", text("hi"), old), msg("System", request(), old), msg("User", text("again"), now)];
        assert_eq!(approval_state(&msgs, now), (false, false));
        // A recent one in an earlier turn: soft only.
        let msgs = vec![msg("User", text("hi"), now), msg("System", request(), now), msg("User", text("again"), now)];
        assert_eq!(approval_state(&msgs, now), (false, true));
        // One in the latest turn, however old: hard.
        let msgs = vec![msg("User", text("hi"), old), msg("System", request(), old)];
        assert_eq!(approval_state(&msgs, now), (true, false));
        assert_eq!(approval_state(&[], now), (false, false));
    }

    fn timer(id: &str, fire_in_minutes: i64, status: TimerStatus) -> ScheduledTimer {
        let now = Utc::now();
        serde_json::from_value(serde_json::json!({
            "id": id, "session_id": "old", "created_at": now, "fire_at": now + Duration::minutes(fire_in_minutes),
            "mode": "prompt", "status": status,
        }))
        .unwrap()
    }

    #[test]
    fn idle_and_timer_rules() {
        let now = Utc::now();
        assert!(timer_due_soon(&[timer("a", 30, TimerStatus::Pending)], now));
        assert!(!timer_due_soon(&[timer("a", 30, TimerStatus::Fired)], now));
        assert!(!timer_due_soon(&[timer("a", TIMER_SOON_MINUTES + 30, TimerStatus::Pending)], now));

        let mut state = SessionState::new();
        let id = state.create_session_raw(None);
        let s = state.sessions.get_mut(&id).unwrap();
        s.last_updated = now - Duration::hours(1);
        s.messages = vec![msg("User", text("hi"), now - Duration::hours(1))];
        assert_eq!(last_activity(s), now - Duration::hours(1));
        // A timer that just fired counts as activity (its prompt may be popped).
        let mut fired = timer("f", -1, TimerStatus::Fired);
        fired.fired_at = Some(now - Duration::seconds(10));
        s.scheduled_timers.push(fired);
        assert_eq!(last_activity(s), now - Duration::seconds(10));
    }

    fn rotated_session() -> (SessionState, String) {
        let now = Utc::now();
        let mut state = SessionState::new();
        let old = state.create_session_raw(Some("home".into()));
        let s = state.sessions.get_mut(&old).unwrap();
        s.name = "Heartbeat".into();
        s.name_user_set = true;
        s.tab_emoji = Some("🫀".into());
        s.active_context.conversation_summary.summary = "Tracking the v0.13 release and CI on main".into();
        s.active_context.conversation_summary.current_task = "Watch the release workflow".into();
        s.handoff = Some(crate::handoff::HandoffNote {
            doing: "Waiting on the macOS release build".into(),
            expect_next: "CI finishes by noon; then post the notes".into(),
            would_change_mind: Some("a failed notarization".into()),
            written_at: now - Duration::hours(2),
        });
        s.messages = vec![msg("User", text("[Heartbeat wake] old"), now - Duration::days(3)), msg("Hobbes", text("done"), now - Duration::days(1))];
        s.scheduled_timers = vec![timer("later", 600, TimerStatus::Pending), timer("gone", -60, TimerStatus::Fired)];
        (state, old)
    }

    #[test]
    fn prepare_archives_the_old_and_seeds_the_successor() {
        let (state, old) = rotated_session();
        let now = Utc::now();
        let p = prepare(SystemSession::Heartbeat, &state.sessions[&old], "new-id", now);
        assert!(p.archive_title.starts_with("Heartbeat · "));
        assert_eq!(p.archive.name, p.archive_title);
        assert!(p.archive.name_user_set);
        assert_eq!(p.archive.messages.len(), 2, "history stays in the archive");
        // The pending timer moved; the archive keeps a cancelled copy.
        assert_eq!(p.archive.next_pending_timer(), None);
        assert_eq!(p.successor.scheduled_timers.len(), 1);
        assert_eq!(p.successor.scheduled_timers[0].id, "later");
        assert_eq!(p.successor.scheduled_timers[0].session_id, "new-id");
        assert!(p.archive_events.iter().any(|e| matches!(e, SessionEvent::TimerCancelled { timer_id } if timer_id == "later")));
        // The successor: carried state, one visible opening message.
        assert_eq!(p.successor.id, "new-id");
        assert_eq!(p.successor.name, "Heartbeat");
        assert_eq!(p.successor.composio_profile.as_deref(), Some("home"));
        assert_eq!(p.successor.messages.len(), 1);
        assert_eq!(p.successor.messages[0].author, "Hobbes");
        let intro = p.successor.messages[0].content.get_text_content().unwrap();
        assert!(intro.contains(&p.archive_title));
        assert!(intro.contains("Tracking the v0.13 release"));
        assert!(intro.contains("Watch the release workflow"));
        assert!(intro.contains("Waiting on the macOS release build"));
        assert!(intro.contains("a failed notarization"));

        // The journal rebuilds the successor.
        let projected = crate::session_events::project(None, &events_with_seq(p.successor_events.clone()));
        assert_eq!(projected.messages.len(), 1);
        assert_eq!(projected.scratchpad, p.successor.scratchpad);
        assert_eq!(projected.handoff, p.successor.handoff);
        assert_eq!(projected.scheduled_timers, p.successor.scheduled_timers);
        assert_eq!(projected.composio_profile, p.successor.composio_profile);
        assert!(projected.name_user_set);
    }

    #[test]
    fn the_successor_s_first_prompt_carries_the_summary_and_handoff() {
        let (state, old) = rotated_session();
        let p = prepare(SystemSession::Heartbeat, &state.sessions[&old], "new-id", Utc::now());
        let mut successor = p.successor;
        successor.messages.push(msg("User", text("[Heartbeat wake] new"), Utc::now()));
        let settings = crate::settings::Settings::default();
        let built = crate::context::prompt_builder::PromptBuilder::new(&successor, &settings).build_prompt(String::new());
        let msgs = &built.prompt.messages;
        let all: String = msgs
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                crate::llm::types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("Tracking the v0.13 release"), "summary reaches the prompt");
        assert!(all.contains("Waiting on the macOS release build"), "handoff reaches the prompt");
        // It opens with a user turn, then the carried assistant message.
        assert_eq!(msgs[0].role, crate::llm::types::ChatRole::User);
        let first_assistant = msgs.iter().position(|m| m.role == crate::llm::types::ChatRole::Assistant).unwrap();
        assert!(first_assistant >= 1);
        // No two non-system turns of the same role back to back before it.
        assert!(msgs[..first_assistant].iter().all(|m| m.role != crate::llm::types::ChatRole::Assistant));
    }

    #[test]
    fn continuation_text_skips_what_is_empty() {
        let carry = CarryOver::from_session(&crate::session_events::blank_session());
        assert_eq!(continuation_text("Voice · Nov 3", &carry), "Continuing from \"Voice · Nov 3\" (archived in History).");
    }

    #[test]
    fn archive_names_show_the_date_range() {
        assert_eq!(archive_name("Heartbeat", local(2026, 10, 22, 9), local(2026, 11, 3, 17)), "Heartbeat · Oct 22 – Nov 3");
        assert_eq!(archive_name("Voice", local(2026, 11, 3, 8), local(2026, 11, 3, 22)), "Voice · Nov 3");
        assert_eq!(
            archive_name("Heartbeat", local(2026, 12, 28, 9), local(2027, 1, 4, 9)),
            "Heartbeat · Dec 28, 2026 – Jan 4, 2027"
        );
        // Order-insensitive.
        assert_eq!(archive_name("Heartbeat", local(2026, 11, 3, 17), local(2026, 10, 22, 9)), "Heartbeat · Oct 22 – Nov 3");
    }

    #[test]
    fn archive_name_reads_the_messages() {
        let mut state = SessionState::new();
        let id = state.create_session_raw(None);
        let s = state.sessions.get_mut(&id).unwrap();
        let mut a = crate::voice::session::transcript_message(true, "hi");
        a.created_at = local(2026, 10, 22, 12).with_timezone(&Utc);
        let mut b = crate::voice::session::transcript_message(false, "hello");
        b.created_at = local(2026, 11, 3, 12).with_timezone(&Utc);
        s.messages = vec![a, b];
        assert_eq!(archive_name_for("Voice", s), "Voice · Oct 22 – Nov 3");
        s.messages.clear();
        s.last_updated = local(2026, 11, 1, 12).with_timezone(&Utc);
        assert_eq!(archive_name_for("Voice", s), "Voice · Nov 1");
    }

    #[test]
    fn carry_over_moves_continuity_not_history() {
        let mut state = SessionState::new();
        let old = state.create_session_raw(Some("home-profile".into()));
        {
            let s = state.sessions.get_mut(&old).unwrap();
            s.name = "Heartbeat".into();
            s.name_user_set = true;
            s.tab_emoji = Some("🫀".into());
            s.scratchpad = "watching CI on main".into();
            s.active_context.conversation_summary.summary = "Tracking the release".into();
            s.active_context.conversation_summary.current_task = "ship v0.13".into();
            s.llm_connector_id = Some("conn-1".into());
            s.chat_model = Some("model-x".into());
            s.dispatched_workers = vec!["worker-1".into()];
            s.messages = vec![crate::voice::session::transcript_message(true, "old history")];
            s.project_id = Some("should-not-carry".into());
        }
        let carry = CarryOver::from_session(&state.sessions[&old]);
        let new = state.create_session_raw(carry.composio_profile.clone());
        carry.apply(state.sessions.get_mut(&new).unwrap());
        let n = &state.sessions[&new];
        assert_eq!(carry.name, "Heartbeat");
        assert_eq!(n.tab_emoji.as_deref(), Some("🫀"));
        assert_eq!(n.composio_profile.as_deref(), Some("home-profile"));
        assert_eq!(n.scratchpad, "watching CI on main");
        assert_eq!(n.active_context.conversation_summary.current_task, "ship v0.13");
        assert_eq!(n.llm_connector_id.as_deref(), Some("conn-1"));
        assert_eq!(n.dispatched_workers, vec!["worker-1"]);
        assert!(n.messages.is_empty(), "history stays in the archive");
        assert!(n.project_id.is_none());

        // The journal reproduces the carry-over on replay.
        let mut events = vec![SessionEvent::SessionCreated { id: "x".into(), name: "x".into() }];
        events.push(SessionEvent::ComposioProfileSet { profile: carry.composio_profile.clone() });
        events.push(SessionEvent::SessionRenamed { name: carry.name.clone() });
        events.extend(carry.events());
        let projected = crate::session_events::project(None, &events_with_seq(events));
        assert_eq!(projected.scratchpad, n.scratchpad);
        assert_eq!(projected.active_context.conversation_summary, n.active_context.conversation_summary);
        assert_eq!(projected.tab_emoji, n.tab_emoji);
        assert_eq!(projected.dispatched_workers, n.dispatched_workers);
        assert_eq!(projected.llm_connector_id, n.llm_connector_id);
        assert_eq!(projected.chat_model, n.chat_model);
        assert!(projected.name_user_set);
    }

    fn events_with_seq(events: Vec<SessionEvent>) -> Vec<crate::session_store::LoadedSessionEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| crate::session_store::LoadedSessionEvent { seq: i as i64 + 1, ts: Utc::now(), event })
            .collect()
    }
}
