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
//! - **Continuity.** The new session carries the conversation summary, the
//!   scratchpad, the handoff note, the Composio profile, any connector pin,
//!   the dispatch-relay worker list, the tab emoji and the name
//!   ([`CarryOver`]). The turn-context ledger and the turn briefing are
//!   per-session and rebuilt from live state on the first turn, so they
//!   carry nothing.
//! - **Lineage.** `system_sessions` records the archive, so the archive keeps
//!   its icon and exemptions, and anything still addressed to it (a dispatch
//!   result, a relay, a queued message) lands in the new session
//!   (`system_sessions::current_for`).
//! - **Tabs.** An open tab of the old session is replaced in place by the new
//!   one (`crate::request_tab_replace`), staying active if it was; the
//!   archive is dropped from memory once saved (it stays on disk, in
//!   History) so it no longer rides every save.
//!
//! The decision ([`should_rotate`]) is pure; [`check_all`] runs it from the
//! Heartbeat runtime's tick, which runs every 30 s whether or not the
//! Heartbeat is on, at most every [`CHECK_EVERY_MINUTES`].

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Local, Utc};
use dioxus::prelude::*;

use crate::session::{ConversationSummary, Session, SessionState};
use crate::session_events::SessionEvent;
use crate::system_sessions::{self, SystemSession};

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
/// How often the sizes are checked (one indexed `length()` query each).
pub const CHECK_EVERY_MINUTES: i64 = 5;

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

/// What could be in flight in a system session. Rotation waits until none of
/// it is: a turn, queued message, approval or open wake would otherwise run
/// against (or be read back from) a session that is no longer current.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    /// A turn is streaming in the session.
    pub streaming: bool,
    /// Messages wait in its background chat queue.
    pub queued_messages: bool,
    /// A tool call waits on the person's approval.
    pub pending_approval: bool,
    /// A timer is pending. Timers stay with the session that set them
    /// (rotation waits rather than migrating them): each has a deadline, so
    /// the wait is bounded, and the archive never has a live timer.
    pub pending_timer: bool,
    /// Heartbeat: a fired wake hasn't been read back yet (its cost and
    /// outcome come from this session's messages).
    pub open_wake: bool,
    /// Voice: a conversation is live.
    pub voice_live: bool,
    /// Voice: brain hand-offs still waiting on a turn in this session.
    pub brain_waits: usize,
    /// Composio data was read within `profile_reach::READ_WINDOW`. The
    /// cross-company hold looks for such reads in the session's own
    /// messages, which stay in the archive, while the carried-over summary
    /// may still hold that data: rotating only once the window has passed
    /// keeps the boundary intact.
    pub recent_profile_reads: bool,
}

impl Activity {
    pub fn is_quiet(&self) -> bool {
        !self.streaming
            && !self.queued_messages
            && !self.pending_approval
            && !self.pending_timer
            && !self.open_wake
            && !self.voice_live
            && self.brain_waits == 0
            && !self.recent_profile_reads
    }
}

/// Rotate now: over a threshold, at a quiet moment.
pub fn should_rotate(size: SessionSize, activity: &Activity) -> bool {
    over_threshold(size) && activity.is_quiet()
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

// ── The Dioxus side ─────────────────────────────────────────────────────────

/// The current id of a system session.
fn current_id(kind: SystemSession) -> Option<String> {
    match kind {
        SystemSession::Heartbeat => crate::heartbeat::HEARTBEAT.peek().session_id.clone(),
        SystemSession::Voice => crate::voice::session::VOICE_SESSION_ID.peek().clone(),
    }
}

/// What is in flight for `id` right now.
fn activity(
    kind: SystemSession,
    session: &Session,
    settings: &crate::settings::Settings,
    streaming: &HashSet<String>,
) -> Activity {
    let id = session.id.as_str();
    let mut a = Activity {
        recent_profile_reads: !crate::mcp::manager::profile_reach::session_read_profiles(
            settings,
            &session.messages,
            Utc::now(),
        )
        .is_empty(),
        streaming: streaming.contains(id),
        queued_messages: crate::components::chat_queue::CHAT_QUEUE.peek().get(id).is_some_and(|q| !q.is_empty()),
        pending_approval: crate::heartbeat::wake::has_pending_approval(&session.messages),
        pending_timer: session.next_pending_timer().is_some(),
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
    for kind in SystemSession::ALL {
        let Some(id) = current_id(kind) else { continue };
        let decision = {
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
            let a = activity(kind, session, settings, streaming);
            if !should_rotate(size, &a) {
                tracing::debug!(session_id = %id, "rotation: {kind:?} session is due but busy");
            }
            should_rotate(size, &a).then_some(size)
        };
        if let Some(size) = decision {
            rotate(session_state, kind, &id, size);
        }
    }
}

/// Rotate `old`: create the successor, carry state over, rename and archive
/// `old`, switch the feature and its tab to the new id. Returns the new id.
pub fn rotate(
    mut session_state: Signal<SessionState>,
    kind: SystemSession,
    old: &str,
    size: SessionSize,
) -> Option<String> {
    let (carry, archive_title, closing_brief) = {
        let state = session_state.peek();
        let s = state.sessions.get(old)?;
        let cs = &s.active_context.conversation_summary;
        let brief = (!cs.summary.trim().is_empty() || !cs.current_task.trim().is_empty())
            .then(|| serde_json::to_value(cs).ok())
            .flatten();
        (CarryOver::from_session(s), archive_name_for(kind.title(), s), brief)
    };
    let new_id = {
        let mut state = session_state.write();
        // create_session_raw makes the new session active; put it back,
        // unless the archive was the active one — the successor takes over.
        let previous_active = state.active_session_id.clone();
        let id = state.create_session_raw(carry.composio_profile.clone());
        state.active_session_id = if previous_active == old || previous_active.is_empty() {
            id.clone()
        } else {
            previous_active
        };
        if let Some(s) = state.sessions.get_mut(&id) {
            carry.apply(s);
        }
        state.update_session_name_raw(&id, carry.name.clone());
        state.update_session_name_raw(old, archive_title.clone());
        id
    };
    crate::session_events::log_events(&new_id, carry.events());

    // Lineage first, then the feature's own pointer, so nothing ever sees
    // the new id without knowing the old one is its archive.
    system_sessions::record_rotation(kind, old, &new_id);
    match kind {
        SystemSession::Heartbeat => {
            crate::heartbeat::HEARTBEAT.write().session_id = Some(new_id.clone());
            crate::heartbeat::save_signal();
        }
        SystemSession::Voice => {
            if let Err(e) = crate::session_store::meta_set(crate::voice::session::META_KEY, &new_id) {
                tracing::warn!("rotation: could not persist the new Voice session id: {e}");
            }
            *crate::voice::session::VOICE_SESSION_ID.write() = Some(new_id.clone());
        }
    }

    // The archive's Fleet row ends, with its summary as the closing brief.
    crate::fleet::bridge::report_with_brief(old, "", crate::fleet::bridge::HobbesSignal::Closed, closing_brief);
    crate::llm::claude::keepalive::cancel_session(old);

    // One P-009 save carries the renamed archive and the new session; then
    // the archive leaves memory (it is on disk and in History), so it no
    // longer rides every save.
    SessionState::save_signal(&session_state, None);
    session_state.write().sessions.remove(old);
    crate::request_tab_replace(old, &new_id);
    tracing::info!(
        old_session = old,
        new_session = %new_id,
        messages = size.messages,
        bytes = size.bytes.unwrap_or(0),
        "rotation: {kind:?} session archived and replaced"
    );
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
        let busy = [
            Activity { streaming: true, ..quiet },
            Activity { queued_messages: true, ..quiet },
            Activity { pending_approval: true, ..quiet },
            Activity { pending_timer: true, ..quiet },
            Activity { open_wake: true, ..quiet },
            Activity { voice_live: true, ..quiet },
            Activity { brain_waits: 1, ..quiet },
            Activity { recent_profile_reads: true, ..quiet },
        ];
        for a in busy {
            assert!(!a.is_quiet());
            assert!(!should_rotate(big, &a), "{a:?}");
        }
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
