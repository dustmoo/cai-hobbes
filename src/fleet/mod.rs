//! Fleet observation & approval gates (Phase B, Pro).
//!
//! Every Claude Code session on the machine reports in through global
//! `type: "http"` hooks registered in `~/.claude/settings.json`
//! ([`hooks_config`]), POSTing event JSON to a localhost listener
//! ([`server`]). Events are parsed ([`events`]), appended to `fleet_events`,
//! and folded into the live session map by the pure [`reduce`] function;
//! session rows persist to `fleet_sessions` ([`store`]).
//!
//! # Architecture
//!
//! The listener runs on plain tokio tasks, outside the Dioxus runtime, so the
//! canonical live state lives in [`FleetShared`] (std `Mutex<FleetState>` — a
//! plain struct, never a Signal) and a `watch` channel pokes a drain loop in
//! `main.rs` that mirrors snapshots into the UI's `Signal<FleetState>`.
//! Signals are never touched from server tasks.
//!
//! Lock discipline (P-010 spirit): the state mutex is only ever held across
//! synchronous work — parse and awaits happen before the lock, store writes
//! inside it are synchronous rusqlite calls on the shared connection.
//!
//! # Time model
//!
//! A session's "active minutes" accumulate over *Working spans*, split across
//! **local** days (UTC in storage, Local at the split — the `blocks_on` trap).
//! A span opens on `SessionStart` and closes with full credit on any event
//! that proves the turn ran until then (`Stop`, `Notification`,
//! `PermissionRequest`). Closes without such evidence — `SessionEnd`, a
//! superseding `SessionStart`, or aggregation over a span that never closed —
//! are capped at [`STALENESS_MINUTES`] past the last event, so one abandoned
//! terminal can't bank hours. The staleness sweep only flips display status
//! to Idle; it deliberately does NOT close the span, because a long tool-less
//! turn emits no events between start and stop and would otherwise lose real
//! minutes that the eventual `Stop` proves happened.
//!
//! Fleet minutes are stored entirely in the `fleet_*` tables — a distinct
//! source from the planner's `todo_focus_sessions` agent rows — so in-app
//! agent focus and external fleet time stay separable in data even though the
//! Today rail displays their sum in one agent lane (Pro surface).

pub mod briefs;
pub mod events;
pub mod hooks_config;
pub mod server;
pub mod store;
pub mod transcript;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use events::FleetEvent;

/// No events for this long while Working → the sweep shows the session as
/// Idle, and unproven span tails are capped at this length.
pub const STALENESS_MINUTES: i64 = 10;

/// `fleet_port` / `fleet_token` meta keys (session_store `meta` table).
pub const META_FLEET_PORT: &str = "fleet_port";
pub const META_FLEET_TOKEN: &str = "fleet_token";

// ── State model ─────────────────────────────────────────────────────────────

/// Why a session needs the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionKind {
    /// A `Notification` hook event (permission_prompt, idle_prompt, …).
    Notification { kind: String, message: String },
    /// A `PermissionRequest` is (or was) waiting on an approval.
    Gate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetStatus {
    Working,
    /// The main turn ended (the user may type) but background subagents are
    /// still running — idle for input, still doing work.
    WorkingBackground,
    Idle,
    NeedsAttention(AttentionKind),
}

impl FleetStatus {
    pub fn needs_attention(&self) -> bool {
        matches!(self, FleetStatus::NeedsAttention(_))
    }
}

/// A held `PermissionRequest` awaiting an in-app Approve/Deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGate {
    /// Hobbes-generated id (the hook payload carries no `tool_use_id` for
    /// PermissionRequest) — keys the oneshot in [`FleetShared::gates`].
    pub request_id: String,
    pub tool_name: String,
    /// Compact JSON of `tool_input`, truncated for display.
    pub input_summary: String,
    pub requested_at: DateTime<Utc>,
}

/// An LLM-generated re-entry brief for one session: what happened since you
/// last looked, derived from the session's transcript tail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionBrief {
    /// One sentence, clipped for display.
    pub headline: String,
    /// Key decisions / topics, capped small.
    #[serde(default)]
    pub bullets: Vec<String>,
    /// What the session is blocked on / awaiting from the user, if anything.
    #[serde(default)]
    pub blocked_on: Option<String>,
    pub generated_at: DateTime<Utc>,
    /// Generated after `SessionEnd` — the session's closing brief.
    #[serde(default)]
    pub final_brief: bool,
}

/// One observed Claude Code session (live map entry / `fleet_sessions` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetSession {
    pub id: String,
    pub cwd: String,
    /// Display name — the tail component of `cwd`.
    pub name: String,
    pub status: FleetStatus,
    pub last_event_at: DateTime<Utc>,
    /// Open Working span start. Survives a stale→Idle sweep (see module docs).
    pub working_since: Option<DateTime<Utc>>,
    /// Banked active minutes per **local** day.
    #[serde(default)]
    pub day_minutes: BTreeMap<NaiveDate, u32>,
    /// When set, PermissionRequests for this session are answered with an
    /// immediate empty-body passthrough (terminal prompt appears; no UI hold).
    #[serde(default)]
    pub auto_passthrough: bool,
    /// Set on `SessionEnd`; ended rows leave the live map but stay in the DB.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pending_gate: Option<PendingGate>,
    /// The session's JSONL transcript, recorded off the raw hook payload
    /// (the parsed [`FleetEvent`] enum deliberately doesn't carry it).
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub brief: Option<SessionBrief>,
    /// Set on Stop/SessionEnd; cleared when a brief generated at-or-after it
    /// lands. Persisted, so pending briefs survive a restart.
    #[serde(default)]
    pub brief_dirty_at: Option<DateTime<Utc>>,
}

impl FleetSession {
    fn new(id: &str, cwd: &str, now: DateTime<Utc>) -> Self {
        Self {
            id: id.to_string(),
            cwd: cwd.to_string(),
            name: name_from_cwd(cwd),
            status: FleetStatus::Idle,
            last_event_at: now,
            working_since: None,
            day_minutes: BTreeMap::new(),
            auto_passthrough: false,
            ended_at: None,
            pending_gate: None,
            transcript_path: None,
            brief: None,
            brief_dirty_at: None,
        }
    }

    /// Banked + live-span minutes attributable to local day `date` as of
    /// `now`. The live span is capped at [`STALENESS_MINUTES`] past the last
    /// event — an unclosed span never banks more than the staleness window
    /// beyond its last proof of life.
    pub fn minutes_on(&self, date: NaiveDate, now: DateTime<Utc>) -> u32 {
        let banked = self.day_minutes.get(&date).copied().unwrap_or(0);
        let live = match self.working_since {
            Some(since) => {
                let end = capped_span_end(now, self.last_event_at);
                span_minutes_on(since, end, date)
            }
            None => 0,
        };
        banked.saturating_add(live)
    }
}

/// Live session map — only sessions that haven't ended. Mirrored into a
/// Dioxus Signal for the UI; canonical copy lives in [`FleetShared`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetState {
    pub sessions: HashMap<String, FleetSession>,
}

impl FleetState {
    pub fn attention_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.status.needs_attention())
            .count()
    }
}

/// Last path component of a cwd, for display.
pub fn name_from_cwd(cwd: &str) -> String {
    let tail = cwd
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    if tail.is_empty() {
        "(unknown)".to_string()
    } else {
        tail.to_string()
    }
}

/// Truncate on a char boundary with an ellipsis.
pub fn truncate_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", cut)
}

// ── Span math (UTC storage, local-day split) ────────────────────────────────

/// The latest instant an unproven span may extend to.
fn capped_span_end(now: DateTime<Utc>, last_event_at: DateTime<Utc>) -> DateTime<Utc> {
    now.min(last_event_at + chrono::Duration::minutes(STALENESS_MINUTES))
}

/// UTC bounds of a **local** calendar day (same convention as
/// `todo::model`'s day math).
fn local_day_bounds_utc(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let start_local = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        .earliest()
        .unwrap_or_else(Local::now);
    let end_local = Local
        .from_local_datetime(
            &date
                .succ_opt()
                .unwrap_or(date)
                .and_hms_opt(0, 0, 0)
                .expect("midnight exists"),
        )
        .earliest()
        .unwrap_or_else(Local::now);
    (
        start_local.with_timezone(&Utc),
        end_local.with_timezone(&Utc),
    )
}

/// Whole minutes of `[start, end)` that fall on local day `date`.
fn span_minutes_on(start: DateTime<Utc>, end: DateTime<Utc>, date: NaiveDate) -> u32 {
    if end <= start {
        return 0;
    }
    let (day_start, day_end) = local_day_bounds_utc(date);
    let from = start.max(day_start);
    let to = end.min(day_end);
    if to > from {
        (to - from).num_minutes().max(0) as u32
    } else {
        0
    }
}

/// Bank a closed span into the per-local-day minute map, splitting at local
/// midnight so each day gets its share.
fn bank_span(
    day_minutes: &mut BTreeMap<NaiveDate, u32>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) {
    if end <= start {
        return;
    }
    let mut day = start.with_timezone(&Local).date_naive();
    let last = end.with_timezone(&Local).date_naive();
    while day <= last {
        let m = span_minutes_on(start, end, day);
        if m > 0 {
            *day_minutes.entry(day).or_insert(0) += m;
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
}

// ── Reduction (pure) ────────────────────────────────────────────────────────

/// Fold one hook event into the live state. Returns the session rows that
/// changed and must be persisted — including the removed row on `SessionEnd`
/// (its `ended_at` is set; history stays in the DB while the live map drops
/// it).
pub fn reduce(state: &mut FleetState, ev: &FleetEvent, now: DateTime<Utc>) -> Vec<FleetSession> {
    let (id, cwd) = (ev.session_id(), ev.cwd());
    let session = state
        .sessions
        .entry(id.to_string())
        .or_insert_with(|| FleetSession::new(id, cwd, now));
    if !cwd.is_empty() && session.cwd != cwd {
        session.cwd = cwd.to_string();
        session.name = name_from_cwd(cwd);
    }

    // Span closes: full credit when the event proves the turn ran until now,
    // capped when it doesn't (see module docs).
    let close_span = |session: &mut FleetSession, proven_end: Option<DateTime<Utc>>| {
        if let Some(since) = session.working_since.take() {
            let end = proven_end.unwrap_or_else(|| capped_span_end(now, session.last_event_at));
            bank_span(&mut session.day_minutes, since, end);
        }
    };

    match ev {
        FleetEvent::SessionStart { .. } => {
            // A start while a span is open (resume/clear) closes the old span
            // without proof of continuous work.
            close_span(session, None);
            session.working_since = Some(now);
            session.status = FleetStatus::Working;
            session.pending_gate = None;
        }
        FleetEvent::Stop { background_tasks, .. } => {
            // Bank the proven span either way. With background agents still
            // running, immediately reopen: the session is idle for input but
            // still working, and their PostToolUse/SubagentStop heartbeats
            // keep the new span provable.
            close_span(session, Some(now));
            if *background_tasks > 0 {
                session.working_since = Some(now);
                session.status = FleetStatus::WorkingBackground;
            } else {
                session.status = FleetStatus::Idle;
            }
            session.pending_gate = None;
        }
        FleetEvent::Notification { kind, message, .. } => {
            close_span(session, Some(now));
            session.status = FleetStatus::NeedsAttention(AttentionKind::Notification {
                kind: kind.clone(),
                message: message.clone(),
            });
        }
        FleetEvent::PermissionRequest {
            request_id,
            tool_name,
            tool_input,
            ..
        } => {
            close_span(session, Some(now));
            session.status = FleetStatus::NeedsAttention(AttentionKind::Gate);
            session.pending_gate = Some(PendingGate {
                request_id: request_id.clone(),
                tool_name: tool_name.clone(),
                input_summary: truncate_summary(
                    &serde_json::to_string(tool_input).unwrap_or_default(),
                    200,
                ),
                requested_at: now,
            });
        }
        FleetEvent::SessionEnd { .. } => {
            close_span(session, None);
            session.last_event_at = now;
            session.ended_at = Some(now);
            session.pending_gate = None;
            let row = session.clone();
            state.sessions.remove(id);
            return vec![row];
        }
        FleetEvent::PromptSubmit { .. } => {
            // The user submitted a prompt: a turn is starting and the user is
            // demonstrably handling this session — attention clears. Any
            // stale open span (e.g. left by a background-work phase that went
            // quiet) is banked capped, like SessionStart's resume path.
            close_span(session, None);
            session.working_since = Some(now);
            session.status = FleetStatus::Working;
            session.pending_gate = None;
        }
        FleetEvent::ToolActivity { .. } | FleetEvent::SubagentStop { .. } => {
            // Proof that work ran until now: a finished tool call (main turn
            // or subagent) or a completed subagent. Also the "answered the
            // gate in the terminal" signal — a tool ran, so any pending
            // prompt was resolved outside Hobbes. A background phase stays
            // labeled as such; anything else shows Working.
            if session.working_since.is_none() {
                session.working_since = Some(now);
            }
            if session.status != FleetStatus::WorkingBackground {
                session.status = FleetStatus::Working;
            }
            session.pending_gate = None;
        }
    }

    session.last_event_at = now;
    vec![session.clone()]
}

/// Post-reduce marker: record the event's `transcript_path` on the session
/// and, for `Stop`/`SessionEnd` (turn boundaries), mark the brief dirty so
/// the brief supervisor picks it up. Mutates the live-map entry when present
/// and the persisted clones in `changed` in lockstep — on `SessionEnd` the
/// session is already gone from the live map, so only the returned row (the
/// one that gets persisted with `ended_at` set) is updated.
pub fn note_transcript_and_dirty(
    state: &mut FleetState,
    changed: &mut [FleetSession],
    ev: &FleetEvent,
    transcript_path: Option<&str>,
    now: DateTime<Utc>,
) {
    let mark_dirty = matches!(ev, FleetEvent::Stop { .. } | FleetEvent::SessionEnd { .. });
    let path = transcript_path.filter(|p| !p.is_empty());
    if !mark_dirty && path.is_none() {
        return;
    }
    let apply = |s: &mut FleetSession| {
        if let Some(p) = path {
            if s.transcript_path.as_deref() != Some(p) {
                s.transcript_path = Some(p.to_string());
            }
        }
        if mark_dirty {
            s.brief_dirty_at = Some(now);
        }
    };
    if let Some(s) = state.sessions.get_mut(ev.session_id()) {
        apply(s);
    }
    for row in changed.iter_mut().filter(|r| r.id == ev.session_id()) {
        apply(row);
    }
}

/// How a held gate was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Allow,
    Deny,
    /// Hobbes stepped aside (timeout margin hit or auto-passthrough): the
    /// hook gets an empty 2xx body and the terminal prompt appears.
    Passthrough,
}

impl GateOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            GateOutcome::Allow => "allow",
            GateOutcome::Deny => "deny",
            GateOutcome::Passthrough => "passthrough",
        }
    }
}

/// Pure state transition for a resolved gate. Allow/Deny mean the turn
/// resumes (a fresh Working span opens); passthrough keeps the session in
/// NeedsAttention — the user still has a terminal prompt to answer — but
/// clears the no-longer-actionable pending gate.
pub fn resolve_gate_in_state(
    state: &mut FleetState,
    request_id: &str,
    outcome: GateOutcome,
    now: DateTime<Utc>,
) -> Option<FleetSession> {
    let session = state.sessions.values_mut().find(|s| {
        s.pending_gate
            .as_ref()
            .is_some_and(|g| g.request_id == request_id)
    })?;
    session.pending_gate = None;
    match outcome {
        GateOutcome::Allow | GateOutcome::Deny => {
            session.status = FleetStatus::Working;
            session.working_since = Some(now);
            session.last_event_at = now;
        }
        GateOutcome::Passthrough => {
            // Status stays NeedsAttention(Gate): the decision moved to the
            // terminal, the user is still needed.
        }
    }
    Some(session.clone())
}

/// Staleness sweep: Working sessions silent for over [`STALENESS_MINUTES`]
/// show as Idle. The open span is deliberately kept (module docs) — display
/// math caps it, and a later `Stop` restores full credit.
pub fn sweep_stale(state: &mut FleetState, now: DateTime<Utc>) -> Vec<FleetSession> {
    let cutoff = now - chrono::Duration::minutes(STALENESS_MINUTES);
    let mut changed = Vec::new();
    for session in state.sessions.values_mut() {
        if matches!(
            session.status,
            FleetStatus::Working | FleetStatus::WorkingBackground
        ) && session.last_event_at < cutoff
        {
            session.status = FleetStatus::Idle;
            changed.push(session.clone());
        }
    }
    changed
}

// ── Day aggregation ─────────────────────────────────────────────────────────

/// Fleet minutes on a local day from the live map only.
pub fn live_minutes_on(state: &FleetState, date: NaiveDate, now: DateTime<Utc>) -> u32 {
    state
        .sessions
        .values()
        .fold(0u32, |acc, s| acc.saturating_add(s.minutes_on(date, now)))
}

/// Total fleet agent minutes on a local day: live sessions (state) plus
/// ended rows (store). Live rows are persisted too, but the store query
/// filters to `ended_at IS NOT NULL`, so nothing double-counts.
pub fn fleet_agent_minutes_on(state: &FleetState, date: NaiveDate, now: DateTime<Utc>) -> u32 {
    live_minutes_on(state, date, now).saturating_add(store::ended_minutes_on(date))
}

/// The Today rail's agent-lane figure. In-app agent focus minutes and fleet
/// minutes come from distinct sources (`todo_focus_sessions` vs `fleet_*`);
/// the lane shows their sum — and only for Pro, the lane being a gated
/// surface while recording never is.
pub fn agent_lane_minutes(pro: bool, in_app_agent: u32, fleet: u32) -> u32 {
    if pro {
        in_app_agent.saturating_add(fleet)
    } else {
        0
    }
}

// ── Shared runtime ──────────────────────────────────────────────────────────

/// Process-wide fleet runtime shared between the HTTP listener tasks and the
/// UI drain loop. Both mutexes are std (synchronous) and never held across an
/// `.await`.
pub struct FleetShared {
    pub state: Mutex<FleetState>,
    /// Held PermissionRequest responders, keyed by request id.
    gates: Mutex<HashMap<String, tokio::sync::oneshot::Sender<GateOutcome>>>,
    /// Bumped after every state change; the UI drain loop subscribes.
    notify: tokio::sync::watch::Sender<u64>,
}

impl Default for FleetShared {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetShared {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FleetState::default()),
            gates: Mutex::new(HashMap::new()),
            notify: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.notify.subscribe()
    }

    pub fn poke(&self) {
        self.notify.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn snapshot(&self) -> FleetState {
        self.state.lock().expect("fleet state lock poisoned").clone()
    }

    pub(crate) fn register_gate(
        &self,
        request_id: &str,
    ) -> tokio::sync::oneshot::Receiver<GateOutcome> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.gates
            .lock()
            .expect("fleet gates lock poisoned")
            .insert(request_id.to_string(), tx);
        rx
    }

    pub(crate) fn take_gate(
        &self,
        request_id: &str,
    ) -> Option<tokio::sync::oneshot::Sender<GateOutcome>> {
        self.gates
            .lock()
            .expect("fleet gates lock poisoned")
            .remove(request_id)
    }

    /// UI resolution of a held gate. No-op when the gate already timed out
    /// (its responder is gone) — the server task owns all follow-up state
    /// changes and event logging.
    pub fn resolve_gate(&self, request_id: &str, allow: bool) {
        if let Some(tx) = self.take_gate(request_id) {
            let _ = tx.send(if allow {
                GateOutcome::Allow
            } else {
                GateOutcome::Deny
            });
        }
    }

    /// Per-session auto-passthrough toggle (UI). Persists and pokes.
    pub fn set_auto_passthrough(&self, session_id: &str, on: bool) {
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            match state.sessions.get_mut(session_id) {
                Some(s) => {
                    s.auto_passthrough = on;
                    Some(s.clone())
                }
                None => None,
            }
        };
        if let Some(row) = row {
            store::persist_sessions(std::slice::from_ref(&row));
            self.poke();
        }
    }

    /// Store a generated brief on a LIVE session. Persists and pokes.
    /// `brief_dirty_at` is cleared only when it is at or before `started_at`
    /// (the instant generation began) — a Stop that raced in mid-generation
    /// keeps the session dirty so the supervisor re-briefs it. Returns
    /// `false` when the session has left the live map (ended mid-generation);
    /// the caller falls back to [`store::merge_session`].
    pub fn set_brief(
        &self,
        session_id: &str,
        brief: SessionBrief,
        started_at: DateTime<Utc>,
    ) -> bool {
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            match state.sessions.get_mut(session_id) {
                Some(s) => {
                    s.brief = Some(brief);
                    if s.brief_dirty_at.is_some_and(|d| d <= started_at) {
                        s.brief_dirty_at = None;
                    }
                    Some(s.clone())
                }
                None => None,
            }
        };
        match row {
            Some(row) => {
                store::persist_sessions(std::slice::from_ref(&row));
                self.poke();
                true
            }
            None => false,
        }
    }

    /// Clear a LIVE session's dirty mark without writing a brief (unreadable
    /// transcript, empty digest) so the supervisor doesn't spin on it. Same
    /// `started_at` race rule as [`Self::set_brief`]. Returns false when the
    /// session isn't in the live map.
    pub fn clear_brief_dirty(&self, session_id: &str, started_at: DateTime<Utc>) -> bool {
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            match state.sessions.get_mut(session_id) {
                Some(s) if s.brief_dirty_at.is_some_and(|d| d <= started_at) => {
                    s.brief_dirty_at = None;
                    Some(s.clone())
                }
                Some(_) => return true, // newer dirty mark: leave it
                None => None,
            }
        };
        match row {
            Some(row) => {
                store::persist_sessions(std::slice::from_ref(&row));
                self.poke();
                true
            }
            None => false,
        }
    }

    /// Hydrate the live map from `fleet_sessions` rows that never ended
    /// (previous process died with sessions open). Their open spans stay
    /// capped by the staleness window; the sweep idles them.
    pub fn hydrate_from_store(&self) {
        let rows = store::load_live();
        if rows.is_empty() {
            return;
        }
        {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            for row in rows {
                state.sessions.entry(row.id.clone()).or_insert(row);
            }
        }
        self.poke();
    }
}

static SHARED: OnceLock<Arc<FleetShared>> = OnceLock::new();

/// Port the listener is actually bound to right now (0 = not running).
/// Written by the supervisor in `main.rs`, read by the settings status line.
static RUNNING_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

pub fn set_running_port(port: Option<u16>) {
    RUNNING_PORT.store(port.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

pub fn running_port() -> Option<u16> {
    match RUNNING_PORT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        p => Some(p),
    }
}

/// The process-wide shared runtime (listener tasks, drain loop, UI actions).
/// Tests build their own `Arc<FleetShared>` instead for isolation.
pub fn shared() -> &'static Arc<FleetShared> {
    SHARED.get_or_init(|| Arc::new(FleetShared::new()))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use events::FleetEvent;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A UTC instant that is `h:m` local time on local day `date` — keeps the
    /// local-day split tests correct in any timezone.
    fn local_at(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn start(id: &str) -> FleetEvent {
        FleetEvent::SessionStart {
            session_id: id.into(),
            cwd: "/Users/x/dev/hobbes".into(),
            reason: "startup".into(),
        }
    }

    fn stop(id: &str) -> FleetEvent {
        FleetEvent::Stop {
            session_id: id.into(),
            cwd: "/Users/x/dev/hobbes".into(),
            background_tasks: 0,
        }
    }

    #[test]
    fn name_derives_from_cwd_tail() {
        assert_eq!(name_from_cwd("/Users/x/dev/hobbes"), "hobbes");
        assert_eq!(name_from_cwd("/Users/x/dev/hobbes/"), "hobbes");
        assert_eq!(name_from_cwd("C:\\dev\\proj"), "proj");
        assert_eq!(name_from_cwd(""), "(unknown)");
        assert_eq!(name_from_cwd("/"), "(unknown)");
    }

    #[test]
    fn session_start_marks_working_and_opens_a_span() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        let rows = reduce(&mut state, &start("s1"), now);
        assert_eq!(rows.len(), 1);
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert_eq!(s.working_since, Some(now));
        assert_eq!(s.name, "hobbes");
        assert_eq!(s.last_event_at, now);
    }

    #[test]
    fn stop_banks_the_full_span_and_goes_idle() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 10, 0);
        reduce(&mut state, &start("s1"), t0);
        // A 30-minute turn with no intermediate events: Stop proves the whole
        // span ran, so no staleness cap applies.
        let t1 = local_at(day, 10, 30);
        reduce(&mut state, &stop("s1"), t1);
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Idle);
        assert_eq!(s.working_since, None);
        assert_eq!(s.minutes_on(day, t1), 30);
    }

    #[test]
    fn notification_needs_attention_and_banks_with_full_credit() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        let ev = FleetEvent::Notification {
            session_id: "s1".into(),
            cwd: String::new(),
            kind: "permission_prompt".into(),
            message: "Claude needs your permission to use Bash".into(),
        };
        let t1 = local_at(day, 9, 20);
        reduce(&mut state, &ev, t1);
        let s = &state.sessions["s1"];
        assert_eq!(
            s.status,
            FleetStatus::NeedsAttention(AttentionKind::Notification {
                kind: "permission_prompt".into(),
                message: "Claude needs your permission to use Bash".into(),
            })
        );
        assert_eq!(s.minutes_on(day, t1), 20);
        assert_eq!(s.working_since, None);
    }

    #[test]
    fn permission_request_records_a_pending_gate() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), now);
        let ev = FleetEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: String::new(),
            request_id: "g1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf node_modules"}),
        };
        reduce(&mut state, &ev, now + chrono::Duration::minutes(1));
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::NeedsAttention(AttentionKind::Gate));
        let gate = s.pending_gate.as_ref().unwrap();
        assert_eq!(gate.request_id, "g1");
        assert_eq!(gate.tool_name, "Bash");
        assert!(gate.input_summary.contains("rm -rf node_modules"));
    }

    #[test]
    fn long_tool_input_summary_is_truncated() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        let ev = FleetEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/x".into(),
            request_id: "g1".into(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"content": "x".repeat(5000)}),
        };
        reduce(&mut state, &ev, now);
        let gate = state.sessions["s1"].pending_gate.as_ref().unwrap();
        assert!(gate.input_summary.chars().count() <= 200);
        assert!(gate.input_summary.ends_with('…'));
    }

    #[test]
    fn session_end_removes_from_live_map_but_returns_the_ended_row() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), now);
        let rows = reduce(
            &mut state,
            &FleetEvent::SessionEnd {
                session_id: "s1".into(),
                cwd: String::new(),
                reason: "logout".into(),
            },
            now + chrono::Duration::minutes(2),
        );
        assert!(state.sessions.is_empty(), "live map must drop ended sessions");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ended_at.is_some());
        // 2 minutes of working span banked (under the cap).
        let total: u32 = rows[0].day_minutes.values().sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn session_end_caps_an_unproven_span_at_the_staleness_window() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        // Terminal sat abandoned for 3 hours, then exited: only the staleness
        // window past the last event is credited.
        let rows = reduce(
            &mut state,
            &FleetEvent::SessionEnd {
                session_id: "s1".into(),
                cwd: String::new(),
                reason: "other".into(),
            },
            local_at(day, 12, 0),
        );
        let total: u32 = rows[0].day_minutes.values().sum();
        assert_eq!(total, STALENESS_MINUTES as u32);
    }

    #[test]
    fn restart_while_working_closes_the_old_span_capped() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        // /clear an hour later: the old span had no proof of running that long.
        reduce(&mut state, &start("s1"), local_at(day, 10, 0));
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert_eq!(s.working_since, Some(local_at(day, 10, 0)));
        assert_eq!(
            s.day_minutes.get(&day).copied().unwrap_or(0),
            STALENESS_MINUTES as u32
        );
    }

    #[test]
    fn unknown_session_is_created_on_any_event() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(
            &mut state,
            &FleetEvent::Stop {
                session_id: "sX".into(),
                cwd: "/tmp/somewhere".into(),
                background_tasks: 0,
            },
            now,
        );
        let s = &state.sessions["sX"];
        assert_eq!(s.status, FleetStatus::Idle);
        assert_eq!(s.name, "somewhere");
    }

    #[test]
    fn sweep_idles_stale_working_sessions_but_keeps_the_span() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), now);
        reduce(&mut state, &start("s2"), now + chrono::Duration::minutes(9));

        let sweep_at = now + chrono::Duration::minutes(11);
        let changed = sweep_stale(&mut state, sweep_at);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].id, "s1");
        assert_eq!(state.sessions["s1"].status, FleetStatus::Idle);
        // Span survives so a late Stop can restore full credit.
        assert!(state.sessions["s1"].working_since.is_some());
        assert_eq!(state.sessions["s2"].status, FleetStatus::Working);

        // Late Stop after the sweep: the whole 30 minutes are proven.
        let stop_at = now + chrono::Duration::minutes(30);
        reduce(&mut state, &stop("s1"), stop_at);
        let day = now.with_timezone(&Local).date_naive();
        assert_eq!(state.sessions["s1"].minutes_on(day, stop_at), 30);
    }

    #[test]
    fn live_span_display_is_capped_at_the_staleness_window() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 9, 0);
        reduce(&mut state, &start("s1"), t0);
        let s = &state.sessions["s1"];
        // 5 minutes in: honest live count.
        assert_eq!(s.minutes_on(day, t0 + chrono::Duration::minutes(5)), 5);
        // 2 hours in with no events: capped, not 120.
        assert_eq!(
            s.minutes_on(day, t0 + chrono::Duration::hours(2)),
            STALENESS_MINUTES as u32
        );
    }

    #[test]
    fn cross_midnight_span_splits_between_local_days() {
        let day1 = date("2026-08-24");
        let day2 = date("2026-08-25");
        let mut minutes = BTreeMap::new();
        // 23:40 → 00:20 local: 20 minutes on each side of midnight.
        bank_span(&mut minutes, local_at(day1, 23, 40), local_at(day2, 0, 20));
        assert_eq!(minutes.get(&day1).copied(), Some(20));
        assert_eq!(minutes.get(&day2).copied(), Some(20));
    }

    #[test]
    fn multiple_spans_accumulate_on_one_day() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        reduce(&mut state, &stop("s1"), local_at(day, 9, 10));
        reduce(&mut state, &start("s1"), local_at(day, 11, 0));
        reduce(&mut state, &stop("s1"), local_at(day, 11, 25));
        let at = local_at(day, 12, 0);
        assert_eq!(state.sessions["s1"].minutes_on(day, at), 35);
        assert_eq!(live_minutes_on(&state, day, at), 35);
        // Other days see nothing.
        assert_eq!(state.sessions["s1"].minutes_on(date("2026-08-26"), at), 0);
    }

    #[test]
    fn resolve_gate_allow_resumes_working() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(
            &mut state,
            &FleetEvent::PermissionRequest {
                session_id: "s1".into(),
                cwd: "/x".into(),
                request_id: "g1".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
            now,
        );
        let later = now + chrono::Duration::minutes(1);
        let row = resolve_gate_in_state(&mut state, "g1", GateOutcome::Allow, later).unwrap();
        assert_eq!(row.status, FleetStatus::Working);
        assert_eq!(row.working_since, Some(later));
        assert!(row.pending_gate.is_none());
        // Unknown request id: no-op.
        assert!(resolve_gate_in_state(&mut state, "nope", GateOutcome::Deny, later).is_none());
    }

    #[test]
    fn resolve_gate_passthrough_keeps_needs_attention() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(
            &mut state,
            &FleetEvent::PermissionRequest {
                session_id: "s1".into(),
                cwd: "/x".into(),
                request_id: "g1".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
            now,
        );
        let row =
            resolve_gate_in_state(&mut state, "g1", GateOutcome::Passthrough, now).unwrap();
        assert!(row.status.needs_attention(), "terminal prompt still waits on the user");
        assert!(row.pending_gate.is_none(), "gate is no longer actionable in-app");
    }

    #[test]
    fn agent_lane_minutes_is_pro_gated() {
        assert_eq!(agent_lane_minutes(true, 30, 45), 75);
        assert_eq!(agent_lane_minutes(true, 0, 45), 45);
        assert_eq!(agent_lane_minutes(false, 30, 45), 0, "free build shows no agent lane");
    }

    #[test]
    fn fleet_session_serde_round_trips() {
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        reduce(
            &mut state,
            &FleetEvent::PermissionRequest {
                session_id: "s1".into(),
                cwd: "/x/y".into(),
                request_id: "g1".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            },
            now,
        );
        let mut s = state.sessions["s1"].clone();
        s.day_minutes.insert(date("2026-08-25"), 12);
        s.auto_passthrough = true;
        let json = serde_json::to_string(&s).unwrap();
        let back: FleetSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn stop_with_background_tasks_keeps_working_and_banking() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 9, 0);
        reduce(&mut state, &start("s1"), t0);
        // Main turn ends at +10 with two background agents still running.
        let t1 = local_at(day, 9, 10);
        reduce(
            &mut state,
            &FleetEvent::Stop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 2,
            },
            t1,
        );
        {
            let s = &state.sessions["s1"];
            assert_eq!(s.status, FleetStatus::WorkingBackground);
            assert_eq!(s.working_since, Some(t1), "a fresh span opens for the background phase");
        }
        // A subagent heartbeat at +18 keeps the label and proves the span.
        let t2 = local_at(day, 9, 18);
        reduce(
            &mut state,
            &FleetEvent::SubagentStop { session_id: "s1".into(), cwd: String::new() },
            t2,
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::WorkingBackground);
        assert_eq!(state.sessions["s1"].minutes_on(day, t2), 18);

        // Final Stop with nothing left in the background: idle, all banked.
        let t3 = local_at(day, 9, 25);
        reduce(&mut state, &stop("s1"), t3);
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Idle);
        assert_eq!(s.working_since, None);
        assert_eq!(s.minutes_on(day, t3), 25);
    }

    #[test]
    fn prompt_submit_caps_a_stale_background_span() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 1 },
            local_at(day, 9, 10),
        );
        // Background work went quiet; the user comes back two hours later.
        // The stale open span banks capped — the idle gap is never credited.
        let t_return = local_at(day, 11, 10);
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: String::new() },
            t_return,
        );
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert_eq!(s.working_since, Some(t_return));
        assert_eq!(
            s.day_minutes.get(&day).copied().unwrap_or(0),
            10 + STALENESS_MINUTES as u32,
            "10 proven + capped background tail, no idle-gap credit"
        );
    }

    #[test]
    fn prompt_submit_starts_working_and_clears_attention() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        // A notification left the session waiting on the user…
        let ev = FleetEvent::Notification {
            session_id: "s1".into(),
            cwd: "/x/proj".into(),
            kind: "idle_prompt".into(),
            message: "Claude is waiting for your input".into(),
        };
        reduce(&mut state, &ev, local_at(day, 9, 0));
        assert!(state.sessions["s1"].status.needs_attention());
        // …then the user typed a prompt: handled, working, span open.
        let t1 = local_at(day, 9, 30);
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: "/x/proj".into() },
            t1,
        );
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert_eq!(s.working_since, Some(t1));
        assert!(s.pending_gate.is_none());
    }

    #[test]
    fn tool_activity_heartbeat_extends_liveness_and_clears_gates() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 9, 0);
        reduce(&mut state, &start("s1"), t0);
        // Tool heartbeats every 8 minutes keep the span provable: minutes
        // display never hits the staleness cap mid-turn.
        for offset in [8i64, 16, 24] {
            reduce(
                &mut state,
                &FleetEvent::ToolActivity {
                    session_id: "s1".into(),
                    cwd: "/x/proj".into(),
                    tool_name: "Bash".into(),
                },
                t0 + chrono::Duration::minutes(offset),
            );
        }
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert_eq!(s.working_since, Some(t0), "heartbeats keep the original span");
        // 25 minutes in, last heartbeat a minute ago: full honest credit.
        assert_eq!(s.minutes_on(day, t0 + chrono::Duration::minutes(25)), 25);

        // A pending gate answered in the terminal: the next tool run clears it.
        reduce(
            &mut state,
            &FleetEvent::PermissionRequest {
                session_id: "s1".into(),
                cwd: String::new(),
                request_id: "g1".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
            t0 + chrono::Duration::minutes(26),
        );
        assert!(state.sessions["s1"].status.needs_attention());
        reduce(
            &mut state,
            &FleetEvent::ToolActivity {
                session_id: "s1".into(),
                cwd: String::new(),
                tool_name: "Bash".into(),
            },
            t0 + chrono::Duration::minutes(27),
        );
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Working);
        assert!(s.pending_gate.is_none());
    }

    fn brief(t: DateTime<Utc>) -> SessionBrief {
        SessionBrief {
            headline: "did things".into(),
            bullets: vec!["a decision".into()],
            blocked_on: None,
            generated_at: t,
            final_brief: false,
        }
    }

    #[test]
    fn note_transcript_marks_dirty_on_stop_and_records_path() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), t0);
        // SessionStart carries a path but is not a turn boundary.
        let mut changed = vec![state.sessions["s1"].clone()];
        note_transcript_and_dirty(&mut state, &mut changed, &start("s1"), Some("/t/s1.jsonl"), t0);
        assert_eq!(state.sessions["s1"].transcript_path.as_deref(), Some("/t/s1.jsonl"));
        assert!(state.sessions["s1"].brief_dirty_at.is_none());
        assert_eq!(changed[0].transcript_path.as_deref(), Some("/t/s1.jsonl"));

        // Stop marks dirty on both the live entry and the persisted clone.
        let t1 = t0 + chrono::Duration::minutes(5);
        let mut changed = reduce(&mut state, &stop("s1"), t1);
        note_transcript_and_dirty(&mut state, &mut changed, &stop("s1"), None, t1);
        assert_eq!(state.sessions["s1"].brief_dirty_at, Some(t1));
        assert_eq!(changed[0].brief_dirty_at, Some(t1));
        // Path recorded earlier survives.
        assert_eq!(changed[0].transcript_path.as_deref(), Some("/t/s1.jsonl"));
    }

    #[test]
    fn note_transcript_marks_the_removed_row_on_session_end() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), t0);
        let end = FleetEvent::SessionEnd {
            session_id: "s1".into(),
            cwd: String::new(),
            reason: "logout".into(),
        };
        let t1 = t0 + chrono::Duration::minutes(1);
        let mut changed = reduce(&mut state, &end, t1);
        note_transcript_and_dirty(&mut state, &mut changed, &end, Some("/t/s1.jsonl"), t1);
        assert!(state.sessions.is_empty());
        assert_eq!(changed[0].brief_dirty_at, Some(t1));
        assert_eq!(changed[0].transcript_path.as_deref(), Some("/t/s1.jsonl"));
    }

    #[test]
    fn set_brief_clears_dirty_unless_a_stop_raced_in() {
        let shared = FleetShared::new();
        let t0 = utc("2026-08-25T10:00:00Z");
        {
            let mut state = shared.state.lock().unwrap();
            reduce(&mut state, &stop("s1"), t0);
            state.sessions.get_mut("s1").unwrap().brief_dirty_at = Some(t0);
        }
        // Generation started after the dirty mark → clears it.
        let started = t0 + chrono::Duration::seconds(50);
        assert!(shared.set_brief("s1", brief(started), started));
        {
            let state = shared.state.lock().unwrap();
            let s = &state.sessions["s1"];
            assert!(s.brief_dirty_at.is_none());
            assert_eq!(s.brief.as_ref().unwrap().headline, "did things");
        }
        // A newer Stop raced in mid-generation → dirty survives the write.
        {
            let mut state = shared.state.lock().unwrap();
            state.sessions.get_mut("s1").unwrap().brief_dirty_at =
                Some(started + chrono::Duration::seconds(10));
        }
        assert!(shared.set_brief("s1", brief(started), started));
        {
            let state = shared.state.lock().unwrap();
            assert!(state.sessions["s1"].brief_dirty_at.is_some(), "racing Stop keeps it dirty");
        }
        // Unknown session → false (caller merges into the store instead).
        assert!(!shared.set_brief("nope", brief(started), started));
    }

    #[test]
    fn clear_brief_dirty_respects_the_race_rule() {
        let shared = FleetShared::new();
        let t0 = utc("2026-08-25T10:00:00Z");
        {
            let mut state = shared.state.lock().unwrap();
            reduce(&mut state, &stop("s1"), t0);
            state.sessions.get_mut("s1").unwrap().brief_dirty_at = Some(t0);
        }
        let started = t0 + chrono::Duration::seconds(50);
        assert!(shared.clear_brief_dirty("s1", started));
        assert!(shared.state.lock().unwrap().sessions["s1"].brief_dirty_at.is_none());

        // Newer dirty mark stays.
        {
            let mut state = shared.state.lock().unwrap();
            state.sessions.get_mut("s1").unwrap().brief_dirty_at =
                Some(started + chrono::Duration::seconds(5));
        }
        assert!(shared.clear_brief_dirty("s1", started));
        assert!(shared.state.lock().unwrap().sessions["s1"].brief_dirty_at.is_some());
        assert!(!shared.clear_brief_dirty("gone", started));
    }

    #[test]
    fn brief_fields_survive_serde_round_trip() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &stop("s1"), t0);
        let mut s = state.sessions["s1"].clone();
        s.transcript_path = Some("/t/s1.jsonl".into());
        s.brief_dirty_at = Some(t0);
        s.brief = Some(SessionBrief {
            headline: "h".into(),
            bullets: vec!["b1".into(), "b2".into()],
            blocked_on: Some("review".into()),
            generated_at: t0,
            final_brief: true,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: FleetSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Pre-brief rows (no new fields in the JSON) still deserialize.
        let legacy = r#"{"id":"x","cwd":"/a","name":"a","status":"Idle",
            "last_event_at":"2026-08-25T10:00:00Z","working_since":null}"#;
        let old: FleetSession = serde_json::from_str(legacy).unwrap();
        assert!(old.brief.is_none() && old.transcript_path.is_none() && old.brief_dirty_at.is_none());
    }

    #[test]
    fn shared_gate_registry_round_trips() {
        let shared = FleetShared::new();
        let mut rx = shared.register_gate("g1");
        shared.resolve_gate("g1", true);
        assert_eq!(rx.try_recv().unwrap(), GateOutcome::Allow);
        // Resolving an unknown gate is a no-op.
        shared.resolve_gate("g2", false);
        assert!(shared.take_gate("g1").is_none());
    }
}
