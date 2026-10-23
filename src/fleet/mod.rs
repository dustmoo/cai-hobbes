//! Fleet observation (Phase B, Pro).
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
//! are capped at the session's provable window past the last event —
//! [`STALENESS_MINUTES`] for a foreground turn, [`BACKGROUND_STALENESS_MINUTES`]
//! for a background phase (silent by design; its quiet stretch is real agent
//! work and accrues live, feeding the exocortex numerator) — so one abandoned
//! terminal can't bank hours. The staleness sweep only flips display status
//! to Idle; it deliberately does NOT close the span, because a long tool-less
//! turn emits no events between start and stop and would otherwise lose real
//! minutes that the eventual `Stop` proves happened.
//!
//! Fleet minutes are stored entirely in the `fleet_*` tables — a distinct
//! source from the planner's `todo_focus_sessions` agent rows — so in-app
//! agent focus and external fleet time stay separable in data even though the
//! Today rail displays their sum in one agent lane (Pro surface).

pub mod bridge;
pub mod briefs;
pub mod events;
pub mod mention;
pub mod reconcile;
pub mod replay;
pub mod hooks_config;
pub mod server;
pub mod store;
pub mod terminal_focus;
pub mod transcript;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use events::FleetEvent;

/// No events for this long while Working → the sweep shows the session as
/// Idle, and unproven span tails are capped at this length.
pub const STALENESS_MINUTES: i64 = 10;

/// The staleness window for `WorkingBackground` sessions. Background jobs
/// (a long benchmark shell, a monitor with an hour-long timeout) emit no
/// hook events and never touch the transcript until they finish — silence
/// is their normal operating state, not evidence of death. The generous
/// window covers multi-hour runs; the cost is that a hard-killed terminal
/// (no `SessionEnd`) mid-background-phase shows "background agents" for up
/// to this long before idling. Time accounting is unaffected either way:
/// span credit is capped at [`STALENESS_MINUTES`] past the last proof.
pub const BACKGROUND_STALENESS_MINUTES: i64 = 240;

/// Default minutes of idle silence before a session retires off the live
/// board (ended_at = its last event; history and banked time stay in the DB,
/// and any later event resurrects it — see the revival path in
/// `server`/`bridge`). Needs-attention sessions never retire: things waiting
/// on the user must not fall off silently. Tunable via
/// `settings.fleet_retire_minutes`, mirrored into [`set_retire_minutes`] by
/// the supervisor.
pub const RETIRE_MINUTES: i64 = 360;

static RETIRE_MINUTES_SETTING: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(RETIRE_MINUTES);

/// Supervisor mirror of `settings.fleet_retire_minutes` (the sweep runs on
/// plain tokio tasks with no Settings access). Clamped to 15–1440.
pub fn set_retire_minutes(minutes: u32) {
    RETIRE_MINUTES_SETTING.store(
        i64::from(minutes.clamp(15, 1440)),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn retire_minutes() -> i64 {
    RETIRE_MINUTES_SETTING.load(std::sync::atomic::Ordering::Relaxed)
}

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
    /// A Hobbes chat between turns with a timer pending: nothing is running,
    /// but the session will pick itself back up at `FleetSession::wake_at`.
    /// Not working time (no span is open) and never stale or retired before
    /// the timer is due.
    Waiting,
    Idle,
    NeedsAttention(AttentionKind),
}

impl FleetStatus {
    pub fn needs_attention(&self) -> bool {
        matches!(self, FleetStatus::NeedsAttention(_))
    }

    /// Still on the job: running a turn, running background work, or
    /// waiting on its own timer.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            FleetStatus::Working | FleetStatus::WorkingBackground | FleetStatus::Waiting
        )
    }
}

/// A Claude Code permission prompt the session is waiting on. Display only:
/// the hook is answered at once with "no decision", so the user answers the
/// prompt in the terminal (it carries choices the Fleet can't offer —
/// always-allow, "tell Claude what to do instead", AskUserQuestion options).
/// Cleared when the session moves on (a tool runs, the turn stops, a prompt
/// is submitted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGate {
    /// Hobbes-generated id (the hook payload carries no `tool_use_id` for
    /// PermissionRequest); keys the Planner's gate decision.
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

/// Where a fleet session's events come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FleetOrigin {
    /// An external Claude Code session reporting via HTTP hooks.
    #[default]
    External,
    /// One of Hobbes' own chat tabs, reporting via [`bridge`].
    Hobbes,
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
    /// Banked active minutes per **local** day (legacy rows; new banking
    /// goes to `day_seconds` — always read both via [`Self::minutes_on`]).
    #[serde(default)]
    pub day_minutes: BTreeMap<NaiveDate, u32>,
    /// Banked active seconds per **local** day. Seconds, because chat-style
    /// turns often run under a minute and per-span minute flooring silently
    /// zeroed them out.
    #[serde(default)]
    pub day_seconds: BTreeMap<NaiveDate, u32>,
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
    #[serde(default)]
    pub origin: FleetOrigin,
    /// Claude Code's own session title (from the transcript's `ai-title`
    /// entries), refreshed with each brief. Display prefers it over the
    /// cwd-derived `name`; the folder name stays as the stable fallback.
    #[serde(default)]
    pub session_title: Option<String>,
    /// The prompt currently running (set by UserPromptSubmit).
    #[serde(default)]
    pub active_prompt: Option<String>,
    /// Recently stopped prompt ids (ring, cap 8): a heartbeat carrying one
    /// of these is background work — liveness only, never a status change.
    #[serde(default)]
    pub stopped_prompts: Vec<String>,
    /// When the current NeedsAttention began — reconciliation compares it
    /// against transcript activity to clear alerts answered while Hobbes
    /// wasn't listening.
    #[serde(default)]
    pub attention_at: Option<DateTime<Utc>>,
    /// Set when this session was launched by HOBBES_DISPATCH for a todo.
    /// A flag on External origin (NOT a new origin variant — reconciliation
    /// and grouping treat dispatched runs as the Claude Code sessions they
    /// are).
    #[serde(default)]
    pub dispatched_todo: Option<String>,
    /// The Hobbes chat session that issued the HOBBES_DISPATCH. When set, the
    /// parent chat is continued automatically once this run finishes (see
    /// [`sweep_dispatch_continuations`]).
    #[serde(default)]
    pub dispatched_from: Option<String>,
    /// Latch: the parent chat has already been handed this run's result.
    /// Persisted, so a restart mid-run never double-fires the continuation.
    #[serde(default)]
    pub continuation_sent: bool,
    /// When a `Waiting` session's next timer is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<DateTime<Utc>>,
    /// The terminal a Claude Code session runs in, from hook headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<terminal_focus::TerminalHint>,
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
            day_seconds: BTreeMap::new(),
            ended_at: None,
            pending_gate: None,
            transcript_path: None,
            brief: None,
            brief_dirty_at: None,
            origin: FleetOrigin::External,
            session_title: None,
            active_prompt: None,
            stopped_prompts: Vec::new(),
            attention_at: None,
            dispatched_todo: None,
            dispatched_from: None,
            continuation_sent: false,
            wake_at: None,
            terminal: None,
        }
    }

    /// What to call this session: the Claude Code title when known, else the
    /// cwd-derived (or tab) name.
    pub fn display_name(&self) -> &str {
        self.session_title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.name)
    }

    /// Banked seconds on local day `date` (legacy minute rows included).
    pub fn banked_seconds_on(&self, date: NaiveDate) -> u32 {
        self.day_minutes
            .get(&date)
            .copied()
            .unwrap_or(0)
            .saturating_mul(60)
            .saturating_add(self.day_seconds.get(&date).copied().unwrap_or(0))
    }

    /// How far past the last event this session's open span stays provable.
    /// Background phases are silent by design (their jobs emit nothing until
    /// they finish), so they get the long window — the same trade the
    /// staleness sweep makes for display status.
    fn provable_window_minutes(&self) -> i64 {
        if self.status == FleetStatus::WorkingBackground {
            BACKGROUND_STALENESS_MINUTES
        } else {
            STALENESS_MINUTES
        }
    }

    /// The latest instant this session's open span may extend to without a
    /// proving event.
    fn provable_end(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now.min(self.last_event_at + chrono::Duration::minutes(self.provable_window_minutes()))
    }

    /// Banked + live-span minutes attributable to local day `date` as of
    /// `now` (summed in seconds, floored once at the end — twenty 45-second
    /// turns are 15 minutes, not zero). The live span is capped at the
    /// session's provable window past the last event — an unclosed span
    /// never banks more than that window beyond its last proof of life.
    pub fn minutes_on(&self, date: NaiveDate, now: DateTime<Utc>) -> u32 {
        let live = match self.working_since {
            Some(since) => {
                let end = self.provable_end(now);
                span_seconds_on(since, end, date)
            }
            None => 0,
        };
        self.banked_seconds_on(date).saturating_add(live) / 60
    }
}

/// Live session map — only sessions that haven't ended. Mirrored into a
/// Dioxus Signal for the UI; canonical copy lives in [`FleetShared`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetState {
    pub sessions: HashMap<String, FleetSession>,
    /// Banked "engaged" wall-clock seconds per local day: the union of time
    /// when at least one session was provably working. This is the exocortex
    /// denominator — overlapping agents bank once here while each banks its
    /// own `day_seconds`, which is exactly what makes the ratio read as
    /// "how many of you the fleet is while you're driving it".
    pub coverage_seconds: BTreeMap<NaiveDate, u32>,
    /// Open engaged period, if any session is currently accruing.
    pub coverage_open: Option<DateTime<Utc>>,
}

impl FleetState {
    pub fn attention_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.status.needs_attention())
            .count()
    }
}

/// Resolve a user/AI-supplied session reference — an exact fleet session id,
/// or a unique case-insensitive live-session name — to the session id.
/// Ambiguous or unknown references error, naming what IS live.
pub fn resolve_session_ref(live: &FleetState, input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty session reference".to_string());
    }
    if live.sessions.contains_key(input) {
        return Ok(input.to_string());
    }
    let matches: Vec<&FleetSession> = live
        .sessions
        .values()
        .filter(|s| {
            s.name.eq_ignore_ascii_case(input)
                || s.session_title
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case(input))
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => {
            let mut names: Vec<&str> = live.sessions.values().map(|s| s.name.as_str()).collect();
            names.sort_unstable();
            Err(format!(
                "no live session matches '{input}'. Live sessions: {}",
                if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
            ))
        }
        _ => Err(format!(
            "'{input}' matches {} live sessions — use the session id from HOBBES_FLEET_STATUS",
            matches.len()
        )),
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

/// UTC bounds of a **local** calendar day (same convention as
/// `todo::model`'s day math).
pub(crate) fn local_day_bounds_utc(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
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

/// Whole seconds of `[start, end)` that fall on local day `date`.
fn span_seconds_on(start: DateTime<Utc>, end: DateTime<Utc>, date: NaiveDate) -> u32 {
    if end <= start {
        return 0;
    }
    let (day_start, day_end) = local_day_bounds_utc(date);
    let from = start.max(day_start);
    let to = end.min(day_end);
    if to > from {
        (to - from).num_seconds().max(0) as u32
    } else {
        0
    }
}

/// Bank a closed span into the per-local-day seconds map, splitting at local
/// midnight so each day gets its share.
fn bank_span(
    day_seconds: &mut BTreeMap<NaiveDate, u32>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) {
    if end <= start {
        return;
    }
    let mut day = start.with_timezone(&Local).date_naive();
    let last = end.with_timezone(&Local).date_naive();
    while day <= last {
        let s = span_seconds_on(start, end, day);
        if s > 0 {
            *day_seconds.entry(day).or_insert(0) += s;
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
    reduce_with_prompt(state, ev, None, now)
}

/// [`reduce`] with the event's `prompt_id` (from the raw hook body). The
/// prompt id is the turn discriminator: a heartbeat whose prompt was already
/// stopped is background work — it proves liveness but must never change
/// status, open spans, or clear alerts the user hasn't addressed.
pub fn reduce_with_prompt(
    state: &mut FleetState,
    ev: &FleetEvent,
    prompt_id: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<FleetSession> {
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
    // Unproven closes cap at the session's provable window — status-aware,
    // so a background phase's quiet stretch (real work, silent by design)
    // banks when the user's PromptSubmit or a resume closes it, while an
    // abandoned foreground turn still caps at the short window.
    let close_span = |session: &mut FleetSession, proven_end: Option<DateTime<Utc>>| {
        if let Some(since) = session.working_since.take() {
            let end = proven_end.unwrap_or_else(|| session.provable_end(now));
            bank_span(&mut session.day_seconds, since, end);
        }
    };

    // Is this event's prompt already stopped? Then it is background work.
    let prompt_is_background = prompt_id.is_some_and(|pid| {
        session.stopped_prompts.iter().any(|p| p == pid)
            || session
                .active_prompt
                .as_deref()
                .is_some_and(|active| active != pid)
    });
    let mark_prompt_stopped = |session: &mut FleetSession, pid: Option<&str>| {
        if let Some(pid) = pid {
            if session.active_prompt.as_deref() == Some(pid) {
                session.active_prompt = None;
            }
            if !session.stopped_prompts.iter().any(|p| p == pid) {
                session.stopped_prompts.push(pid.to_string());
                let overflow = session.stopped_prompts.len().saturating_sub(8);
                if overflow > 0 {
                    session.stopped_prompts.drain(..overflow);
                }
            }
        }
    };

    match ev {
        FleetEvent::SessionStart { .. } => {
            // A start while a span is open (resume/clear) closes the old span
            // without proof of continuous work. Fresh process → fresh prompts.
            close_span(session, None);
            session.working_since = Some(now);
            session.status = FleetStatus::Working;
            session.pending_gate = None;
            session.attention_at = None;
            session.active_prompt = None;
            session.stopped_prompts.clear();
        }
        FleetEvent::Stop { background_tasks, .. } => {
            // Bank the proven span either way. With background agents still
            // running, immediately reopen: the session is idle for input but
            // still working, and their PostToolUse/SubagentStop heartbeats
            // keep the new span provable.
            close_span(session, Some(now));
            mark_prompt_stopped(session, prompt_id);
            if *background_tasks > 0 {
                session.working_since = Some(now);
                session.status = FleetStatus::WorkingBackground;
            } else {
                session.status = FleetStatus::Idle;
            }
            session.pending_gate = None;
            session.attention_at = None;
        }
        FleetEvent::Notification { kind, message, .. } => {
            // An idle "waiting for your input" nudge during a background
            // phase does NOT mean the session needs the user — it is waiting
            // on its own background work. Permission-flavored notifications
            // always alert, background phase or not.
            let permissionish = kind.contains("permission");
            if session.status == FleetStatus::WorkingBackground && !permissionish {
                // Hold the phase; the span stays open and the liveness tail
                // below records the event.
            } else {
                close_span(session, Some(now));
                session.status = FleetStatus::NeedsAttention(AttentionKind::Notification {
                    kind: kind.clone(),
                    message: message.clone(),
                });
                session.attention_at = Some(now);
            }
        }
        FleetEvent::PermissionRequest {
            request_id,
            tool_name,
            tool_input,
            ..
        } => {
            close_span(session, Some(now));
            session.status = FleetStatus::NeedsAttention(AttentionKind::Gate);
            session.attention_at = Some(now);
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
            session.attention_at = None;
            session.active_prompt = prompt_id.map(str::to_string);
        }
        FleetEvent::SubagentStop { background_tasks, .. }
            if *background_tasks == 0 && session.status == FleetStatus::WorkingBackground =>
        {
            // The LAST background job finished: the background phase ends
            // with full credit, regardless of which prompt owned the job.
            close_span(session, Some(now));
            mark_prompt_stopped(session, prompt_id);
            session.status = FleetStatus::Idle;
        }
        FleetEvent::SubagentStop { background_tasks, .. }
            if *background_tasks > 0
                && matches!(
                    &session.status,
                    FleetStatus::NeedsAttention(AttentionKind::Notification { kind, .. })
                        if !kind.contains("permission")
                ) =>
        {
            // Hard evidence that background work continues (a subagent
            // finished and MORE remain) while the card shows an idle-nudge
            // alert: the ordering race — Stop under-reported, or the idle
            // notification landed mid-phase. Demote the informational alert
            // back to the honest state. Gates and permission notifications
            // are never demoted.
            if session.working_since.is_none() {
                session.working_since = Some(now);
            }
            session.status = FleetStatus::WorkingBackground;
            session.attention_at = None;
        }
        FleetEvent::SubagentStop { background_tasks, .. }
            if *background_tasks > 0 && session.status == FleetStatus::Idle =>
        {
            // A background job finished and MORE remain while the card reads
            // Idle: the phase outlived a staleness demotion, a restart, or a
            // pre-fix persisted row. Heal to the honest state — same
            // evidence standard as the alert demotion above.
            if session.working_since.is_none() {
                session.working_since = Some(now);
            }
            session.status = FleetStatus::WorkingBackground;
        }
        FleetEvent::ToolActivity { .. } | FleetEvent::SubagentStop { .. }
            if prompt_is_background && session.status != FleetStatus::WorkingBackground =>
        {
            // Background work after the turn ended (a monitor firing, a
            // straggling shell): liveness only. Status, spans, gates, and
            // alerts all belong to the USER's turn state — a background
            // completion must not repaint the card or clear what still
            // needs the user. (WorkingBackground sessions fall through:
            // these events are exactly what keeps their span provable.)
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
                session.attention_at = None;
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
    // PromptSubmit dirties too: the user answering is exactly when blockers
    // die, so the next brief should regenerate against the fresh turn.
    let mark_dirty = matches!(
        ev,
        FleetEvent::Stop { .. } | FleetEvent::SessionEnd { .. } | FleetEvent::PromptSubmit { .. }
    );
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

/// Fold a terminal hint captured from hook headers into the session — the
/// live-map entry and its persisted clone in `changed` in lockstep. An empty
/// hint (variables unset or stripped by policy) changes nothing. When the
/// row isn't already in `changed` but the hint moved, the row is added so it
/// persists.
pub fn apply_terminal_hint(
    state: &mut FleetState,
    changed: &mut Vec<FleetSession>,
    session_id: &str,
    hint: &terminal_focus::TerminalHint,
) {
    if hint.is_empty() && hint.started_at.is_none() {
        return;
    }
    let Some(s) = state.sessions.get_mut(session_id) else { return };
    let moved = s.terminal.get_or_insert_with(Default::default).merge_from(hint);
    if !moved {
        return;
    }
    let row = s.clone();
    match changed.iter_mut().find(|r| r.id == session_id) {
        Some(r) => r.terminal = row.terminal,
        None => changed.push(row),
    }
}

/// Staleness sweep: Working sessions silent for over [`STALENESS_MINUTES`]
/// show as Idle; `WorkingBackground` sessions get
/// [`BACKGROUND_STALENESS_MINUTES`] — background jobs are silent by design.
/// `Waiting` sessions idle once their timer is [`STALENESS_MINUTES`] overdue
/// (it should have fired and started a turn; the app was likely closed).
/// The open span is deliberately kept (module docs) — display math caps it,
/// and a later `Stop` restores full credit.
pub fn sweep_stale(state: &mut FleetState, now: DateTime<Utc>) -> Vec<FleetSession> {
    let mut changed = Vec::new();
    for session in state.sessions.values_mut() {
        if session.status == FleetStatus::Waiting {
            let overdue = session
                .wake_at
                .is_none_or(|w| w < now - chrono::Duration::minutes(STALENESS_MINUTES));
            if overdue {
                session.status = FleetStatus::Idle;
                session.wake_at = None;
                changed.push(session.clone());
            }
            continue;
        }
        let window = match session.status {
            FleetStatus::Working => STALENESS_MINUTES,
            FleetStatus::WorkingBackground => BACKGROUND_STALENESS_MINUTES,
            _ => continue,
        };
        if session.last_event_at < now - chrono::Duration::minutes(window) {
            session.status = FleetStatus::Idle;
            changed.push(session.clone());
        }
    }
    changed
}

/// Retirement sweep: Idle sessions silent past [`RETIRE_MINUTES`] leave the
/// live map with `ended_at` set to their last event (no span is open — the
/// stale→Idle sweep or a Stop closed it long before). Working and
/// needs-attention sessions are exempt: work proves itself with events, and
/// attention must never fall off the board unanswered.
pub fn sweep_retired(state: &mut FleetState, now: DateTime<Utc>) -> Vec<FleetSession> {
    sweep_retired_after(state, now, retire_minutes())
}

/// Core with an explicit window (tests stay isolated from the global knob).
pub fn sweep_retired_after(
    state: &mut FleetState,
    now: DateTime<Utc>,
    minutes: i64,
) -> Vec<FleetSession> {
    let cutoff = now - chrono::Duration::minutes(minutes);
    let retire: Vec<String> = state
        .sessions
        .values()
        .filter(|s| s.status == FleetStatus::Idle && s.last_event_at < cutoff)
        .map(|s| s.id.clone())
        .collect();
    let mut rows = Vec::new();
    for id in retire {
        if let Some(mut s) = state.sessions.remove(&id) {
            s.ended_at = Some(s.last_event_at);
            rows.push(s);
        }
    }
    rows
}

/// User-initiated dismissal: the session leaves the live map with
/// `ended_at` at its last event, exactly like retirement — but on demand,
/// and regardless of status. This is the escape hatch for the one thing the
/// sweeps deliberately never touch: a needs-attention card whose terminal
/// was closed without a `SessionEnd` (a crashed shell, a killed tmux pane),
/// which would otherwise sit on the board forever. A later hook event from
/// the same session id simply re-creates the card.
pub fn dismiss_in_state(state: &mut FleetState, session_id: &str) -> Option<FleetSession> {
    let mut s = state.sessions.remove(session_id)?;
    // Any open span closes unproven — capped at the provable window, the
    // same treatment retirement gives an abandoned turn.
    if let Some(since) = s.working_since.take() {
        let end = s.provable_end(s.last_event_at);
        bank_span(&mut s.day_seconds, since, end);
    }
    s.ended_at = Some(s.last_event_at);
    s.pending_gate = None;
    Some(s)
}

// ── Dispatch continuations ──────────────────────────────────────────────────
//
// A HOBBES_DISPATCH'd run carries the id of the chat that issued it
// (`dispatched_from`). When the run finishes, the parent chat is handed the
// result and continues on its own — the "check in in 15 minutes?" round trip
// disappears. Detection is a pure sweep over the live map plus today's ended
// rows; delivery is the chat queue, reached through [`FleetShared`] because
// the sweep runs on a plain tokio task with no Signal access.

/// How long a finished dispatched run waits for its closing brief before the
/// continuation fires anyway. The brief supervisor debounces on
/// [`briefs::BRIEF_QUIET_SECS`]; the extra margin covers the LLM call itself.
pub const DISPATCH_CONTINUE_GRACE_SECS: i64 = briefs::BRIEF_QUIET_SECS + 30;

/// Supervisor mirror of `settings.dispatch_auto_continue` (the sweep has no
/// Settings access — same pattern as [`set_retire_minutes`]).
static DISPATCH_AUTO_CONTINUE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub fn set_dispatch_auto_continue(on: bool) {
    DISPATCH_AUTO_CONTINUE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn dispatch_auto_continue() -> bool {
    DISPATCH_AUTO_CONTINUE.load(std::sync::atomic::Ordering::Relaxed)
}

/// One finished dispatched run, ready to be handed back to its parent chat.
/// The todo *title* is resolved on the Dioxus side (the planner lives in a
/// Signal); the sweep carries only the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingContinuation {
    /// The chat session that issued the dispatch.
    pub parent_session_id: String,
    /// The fleet session that just finished.
    pub run_session_id: String,
    pub todo_id: Option<String>,
    /// The run's display name (session title, else cwd-derived).
    pub session_name: String,
    /// The closing brief's progress line, when a brief landed.
    pub progress: Option<String>,
}

/// When did this dispatched run finish, if it did and still owes its parent a
/// continuation? `None` for rows that were never dispatched from a chat, have
/// already fired, are still working, or are waiting on the user — a
/// NeedsAttention run (a permission gate) is deliberately *not* done: the
/// Fleet badge already surfaces it and the parent must not be told it
/// finished.
fn dispatch_done_at(s: &FleetSession) -> Option<DateTime<Utc>> {
    if s.dispatched_from.is_none() || s.continuation_sent {
        return None;
    }
    if let Some(ended) = s.ended_at {
        return Some(ended);
    }
    if s.status != FleetStatus::Idle || s.working_since.is_some() {
        return None;
    }
    // A pre-created row that never ran is Idle too. `brief_dirty_at` (set by
    // every Stop/SessionEnd) or an existing brief is the proof that a turn
    // actually completed.
    s.brief_dirty_at
        .or_else(|| s.brief.as_ref().map(|b| b.generated_at))
}

/// The message the parent chat receives. First person to Hobbes, so the
/// resumed turn reads as the user handing the result back.
pub fn continuation_text(todo_title: &str, session_name: &str, progress: Option<&str>) -> String {
    let progress = progress
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("No closing brief was generated — check the run's Fleet card for its output.");
    format!(
        "The background agent you dispatched for \"{todo_title}\" ({session_name}) has \
         finished.\n\n{progress}\n\nReview the result and continue from where we left off."
    )
}

/// The Hobbes-authored intro dropped into a row-dispatch's fresh review
/// chat: it explains why the tab opened with no user message in it and
/// points at the Fleet board for the live run.
pub fn dispatch_intro_text(todo_title: &str) -> String {
    format!(
        "Dispatched a background agent for \"{todo_title}\". I'll pick up here when it \
         finishes — you can watch it on the Fleet board meanwhile."
    )
}

/// What one continuation sweep produced: the messages to deliver, plus the
/// rows whose `continuation_sent` latch was set (live rows get persisted
/// wholesale; ended rows are merged in the store, since the sweep's copy of
/// them may be behind a concurrently-generated brief).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ContinuationSweep {
    pub continuations: Vec<PendingContinuation>,
    pub live_rows: Vec<FleetSession>,
    pub ended_ids: Vec<String>,
}

impl ContinuationSweep {
    pub fn is_empty(&self) -> bool {
        self.continuations.is_empty()
    }
}

/// Collect the dispatched runs whose parents are owed a continuation, and
/// latch them so a second sweep yields nothing.
///
/// `ended_rows` are store rows (SessionEnd removes a session from the live
/// map); rows already present live win. Nothing fires while `enabled` is
/// false — the latch stays open, so switching the setting back on hands over
/// any run that finished in the meantime.
pub fn sweep_dispatch_continuations(
    state: &mut FleetState,
    ended_rows: &[FleetSession],
    now: DateTime<Utc>,
    enabled: bool,
) -> ContinuationSweep {
    let mut out = ContinuationSweep::default();
    if !enabled {
        return out;
    }

    // `(row, done_at)` pairs, live first so an ended row that is still in the
    // live map is never handled twice.
    let mut ready: Vec<(String, PendingContinuation, bool)> = Vec::new();
    let consider = |s: &FleetSession| -> Option<PendingContinuation> {
        let done_at = dispatch_done_at(s)?;
        // Wait for the closing brief; fall back once the grace window passes.
        let brief = s.brief.as_ref().filter(|b| b.generated_at >= done_at);
        if brief.is_none() && (now - done_at).num_seconds() < DISPATCH_CONTINUE_GRACE_SECS {
            return None;
        }
        Some(PendingContinuation {
            parent_session_id: s.dispatched_from.clone()?,
            run_session_id: s.id.clone(),
            todo_id: s.dispatched_todo.clone(),
            session_name: s.display_name().to_string(),
            progress: brief.map(briefs::progress_line),
        })
    };

    for s in state.sessions.values() {
        if let Some(c) = consider(s) {
            ready.push((s.id.clone(), c, true));
        }
    }
    for r in ended_rows {
        if state.sessions.contains_key(&r.id) {
            continue;
        }
        if let Some(c) = consider(r) {
            ready.push((r.id.clone(), c, false));
        }
    }

    for (id, continuation, is_live) in ready {
        if is_live {
            if let Some(s) = state.sessions.get_mut(&id) {
                s.continuation_sent = true;
                out.live_rows.push(s.clone());
            }
        } else {
            out.ended_ids.push(id);
        }
        out.continuations.push(continuation);
    }
    out
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

/// Is this session accruing engaged time right now? An open span counts
/// only while its provable end keeps up with the clock — a span the
/// staleness cap has left behind is open in name only. Background phases
/// keep up through their long window: a silent benchmark is agent work.
fn session_accruing(s: &FleetSession, now: DateTime<Utc>) -> bool {
    s.working_since.is_some() && s.provable_end(now) >= now
}

/// Maintain the engaged-time union. Call after any state change (reduce,
/// sweeps) — NOT time-critical: a missed call only delays a transition
/// until the next event or sweep tick. Returns the `(day, total_seconds)`
/// rows that were banked and should be persisted.
///
/// Banking is PROGRESSIVE: while the period is open, every call banks the
/// slice since the last call and advances the open marker to `now`. The
/// open tail is therefore never more than one tick long, so a restart mid-
/// phase (an overnight background benchmark, a `dx serve` rebuild) loses at
/// most that tick — not the whole period. Without this, each session's
/// numerator survived a restart (`working_since` is persisted per row)
/// while the shared denominator did not, and the multiplier read as
/// 100× after a rebuild. Banking up to `now` is honest while accruing:
/// `session_accruing` means some span's provable end keeps up with the
/// clock. Only the CLOSE caps at the latest provable end — a walk-away
/// must not bank the walk.
pub fn sync_coverage(state: &mut FleetState, now: DateTime<Utc>) -> Vec<(NaiveDate, u32)> {
    let accruing = state.sessions.values().any(|s| session_accruing(s, now));
    let (since, end, still_open) = match (state.coverage_open, accruing) {
        (None, true) => {
            state.coverage_open = Some(now);
            return Vec::new();
        }
        (Some(since), true) => (since, now, true),
        (Some(since), false) => {
            // The engaged period ends at the latest provable activity, not
            // at the sweep tick that noticed it.
            let end = state
                .sessions
                .values()
                .map(|s| s.provable_end(now))
                .max()
                .unwrap_or(now)
                .clamp(since, now);
            (since, end, false)
        }
        _ => return Vec::new(),
    };
    state.coverage_open = if still_open { Some(end) } else { None };
    if end <= since {
        return Vec::new();
    }
    bank_span(&mut state.coverage_seconds, since, end);
    let mut day = since.with_timezone(&Local).date_naive();
    let last = end.with_timezone(&Local).date_naive();
    let mut banked = Vec::new();
    while day <= last {
        if let Some(total) = state.coverage_seconds.get(&day) {
            banked.push((day, *total));
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    banked
}

/// Engaged wall-clock seconds on a local day: banked union plus the live
/// tail of an open engaged period.
pub fn engaged_seconds_on(state: &FleetState, date: NaiveDate, now: DateTime<Utc>) -> u32 {
    let banked = state.coverage_seconds.get(&date).copied().unwrap_or(0);
    let open = state
        .coverage_open
        .map(|since| span_seconds_on(since, now, date))
        .unwrap_or(0);
    banked.saturating_add(open)
}

/// The exocortex multiplier: agent minutes banked today over ENGAGED
/// wall-clock minutes — time when at least one agent was actually working.
/// This is average concurrency while driving the fleet ("3 agents for an
/// hour = 3×"), so it holds steady through planning, breaks, and walks
/// instead of decaying like a momentum score. Returns `(multiplier,
/// engaged_minutes)`; `None` when there's nothing meaningful to divide
/// (no work yet, or under 5 engaged minutes — a fresh day would show a
/// silly spike otherwise).
pub fn exocortex_multiplier(agent_minutes: u32, engaged_minutes: u32) -> Option<(f64, u32)> {
    if agent_minutes == 0 || engaged_minutes < 5 {
        return None;
    }
    Some((
        agent_minutes as f64 / engaged_minutes as f64,
        engaged_minutes,
    ))
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
    /// Bumped after every state change; the UI drain loop subscribes.
    notify: tokio::sync::watch::Sender<u64>,
    /// Finished dispatched runs whose parent chats are owed a continuation.
    /// Filled by the server's sweep (a plain tokio task), taken by the UI
    /// drain loop in `main.rs` — the one place that may touch the chat queue.
    continuations: Mutex<Vec<PendingContinuation>>,
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
            notify: tokio::sync::watch::channel(0).0,
            continuations: Mutex::new(Vec::new()),
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

    /// Hand the UI side a batch of finished dispatched runs. Deduped by run
    /// id so a poke race can't enqueue the same continuation twice.
    pub fn push_continuations(&self, batch: Vec<PendingContinuation>) {
        if batch.is_empty() {
            return;
        }
        let mut q = self
            .continuations
            .lock()
            .expect("fleet continuations lock poisoned");
        for c in batch {
            if !q.iter().any(|e| e.run_session_id == c.run_session_id) {
                q.push(c);
            }
        }
    }

    /// Drain the queue (UI side only).
    pub fn take_continuations(&self) -> Vec<PendingContinuation> {
        std::mem::take(
            &mut *self
                .continuations
                .lock()
                .expect("fleet continuations lock poisoned"),
        )
    }

    /// Hide a session from the board (UI). Persists the ended row, logs a
    /// `Dismissed` ledger event, and pokes.
    pub fn dismiss_session(&self, session_id: &str) {
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            dismiss_in_state(&mut state, session_id)
        };
        let Some(row) = row else { return };
        store::persist_sessions(std::slice::from_ref(&row));
        store::append_event(session_id, "Dismissed", "{}");
        self.poke();
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
        session_title: Option<String>,
    ) -> bool {
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            match state.sessions.get_mut(session_id) {
                Some(s) => {
                    s.brief = Some(brief);
                    if let Some(t) = session_title {
                        s.session_title = Some(t);
                    }
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

    /// Refresh a LIVE session's Claude Code title (event-ingest path — no
    /// LLM, no debounce). No-op when unchanged or the session left the map.
    pub fn set_session_title(&self, session_id: &str, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let row = {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            match state.sessions.get_mut(session_id) {
                Some(s) if s.session_title.as_deref() != Some(title) => {
                    s.session_title = Some(title.to_string());
                    Some(s.clone())
                }
                _ => None,
            }
        };
        if let Some(row) = row {
            store::persist_sessions(std::slice::from_ref(&row));
            self.poke();
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
        let coverage = store::load_coverage();
        if rows.is_empty() && coverage.is_empty() {
            return;
        }
        {
            let mut state = self.state.lock().expect("fleet state lock poisoned");
            for row in rows {
                state.sessions.entry(row.id.clone()).or_insert(row);
            }
            // Banked engaged time survives restarts. Banking is progressive
            // (`sync_coverage`), so at most one tick of the open tail is
            // lost; reopen right away if a restored span is still accruing
            // rather than waiting for the next event.
            for (day, secs) in coverage {
                let slot = state.coverage_seconds.entry(day).or_insert(0);
                *slot = (*slot).max(secs);
            }
            store::save_coverage(&sync_coverage(&mut state, Utc::now()));
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
            source: "startup".into(),
        }
    }

    fn stop(id: &str) -> FleetEvent {
        FleetEvent::Stop {
            session_id: id.into(),
            cwd: "/Users/x/dev/hobbes".into(),
            background_tasks: 0,
        }
    }

    // ── Dispatch continuations ─────────────────────────────────────────────

    fn brief_at(t: DateTime<Utc>, headline: &str) -> SessionBrief {
        SessionBrief {
            headline: headline.to_string(),
            bullets: vec![],
            blocked_on: None,
            generated_at: t,
            final_brief: true,
        }
    }

    /// A dispatched run, started and stopped, with `parent` as its origin.
    fn dispatched_state(parent: Option<&str>) -> (FleetState, DateTime<Utc>) {
        let t0 = utc("2026-09-16T10:00:00Z");
        let mut state = FleetState::default();
        reduce(&mut state, &start("run1"), t0);
        {
            let s = state.sessions.get_mut("run1").unwrap();
            s.dispatched_todo = Some("todo-1".into());
            s.dispatched_from = parent.map(str::to_string);
            s.session_title = Some("CSV export".into());
        }
        let t1 = t0 + chrono::Duration::seconds(120);
        reduce(&mut state, &stop("run1"), t1);
        note_transcript_and_dirty(&mut state, &mut Vec::new(), &stop("run1"), Some("/t.jsonl"), t1);
        (state, t1)
    }

    #[test]
    fn finished_dispatched_run_continues_its_parent_once() {
        let (mut state, stopped) = dispatched_state(Some("parent"));
        // Stopped, but the closing brief hasn't landed: nothing yet — the
        // parent is worth waiting a moment to give a real answer to.
        let sweep = sweep_dispatch_continuations(&mut state, &[], stopped, true);
        assert!(sweep.is_empty());

        let landed = stopped + chrono::Duration::seconds(50);
        state.sessions.get_mut("run1").unwrap().brief =
            Some(brief_at(landed, "Shipped the CSV export behind a flag."));
        let sweep = sweep_dispatch_continuations(&mut state, &[], landed, true);
        assert_eq!(sweep.continuations.len(), 1);
        let c = &sweep.continuations[0];
        assert_eq!(c.parent_session_id, "parent");
        assert_eq!(c.run_session_id, "run1");
        assert_eq!(c.todo_id.as_deref(), Some("todo-1"));
        assert_eq!(
            c.progress.as_deref(),
            Some("Shipped the CSV export behind a flag.")
        );
        assert_eq!(sweep.live_rows.len(), 1);
        assert!(sweep.live_rows[0].continuation_sent);
        assert!(sweep.ended_ids.is_empty());

        let text = continuation_text("Ship CSV export", &c.session_name, c.progress.as_deref());
        assert!(text.contains("dispatched for \"Ship CSV export\" (CSV export) has finished"));
        assert!(text.contains("Shipped the CSV export behind a flag."));
        assert!(text.contains("continue from where we left off"));

        // The latch is what keeps the parent from being told twice.
        let again = sweep_dispatch_continuations(
            &mut state,
            &[],
            landed + chrono::Duration::hours(1),
            true,
        );
        assert!(again.is_empty());
    }

    #[test]
    fn dispatch_intro_text_names_the_todo_and_points_at_fleet() {
        let text = dispatch_intro_text("Ship CSV export");
        assert!(text.contains("\"Ship CSV export\""));
        assert!(text.contains("Fleet board"));
        assert!(text.starts_with("Dispatched a background agent"));
    }

    #[test]
    fn ended_run_without_a_brief_continues_after_the_grace_window() {
        let (mut state, _) = dispatched_state(Some("parent"));
        let ended_at = utc("2026-09-16T10:05:00Z");
        let mut changed = reduce(
            &mut state,
            &FleetEvent::SessionEnd {
                session_id: "run1".into(),
                cwd: String::new(),
                reason: "exit".into(),
            },
            ended_at,
        );
        assert!(!state.sessions.contains_key("run1"), "ended rows leave the map");
        let row = changed.pop().unwrap();

        // Inside the window we still hope for a final brief.
        let sweep = sweep_dispatch_continuations(
            &mut state,
            std::slice::from_ref(&row),
            ended_at + chrono::Duration::seconds(DISPATCH_CONTINUE_GRACE_SECS - 1),
            true,
        );
        assert!(sweep.is_empty());

        let late = ended_at + chrono::Duration::seconds(DISPATCH_CONTINUE_GRACE_SECS);
        let sweep = sweep_dispatch_continuations(&mut state, std::slice::from_ref(&row), late, true);
        assert_eq!(sweep.continuations.len(), 1);
        let c = &sweep.continuations[0];
        assert_eq!(c.parent_session_id, "parent");
        assert!(c.progress.is_none(), "no brief landed");
        // Ended rows are merged in the store, never re-persisted wholesale.
        assert_eq!(sweep.ended_ids, vec!["run1".to_string()]);
        assert!(sweep.live_rows.is_empty());

        let text = continuation_text("Ship CSV export", &c.session_name, None);
        assert!(text.contains("has finished"));
        assert!(text.contains("No closing brief was generated"));
    }

    #[test]
    fn undispatched_and_gated_runs_never_continue_anything() {
        // No parent → nothing to continue.
        let (mut state, t) = dispatched_state(None);
        state.sessions.get_mut("run1").unwrap().brief = Some(brief_at(t, "done"));
        assert!(sweep_dispatch_continuations(
            &mut state,
            &[],
            t + chrono::Duration::hours(1),
            true
        )
        .is_empty());

        // A run holding a permission gate is not finished, however long it waits.
        let (mut state, t) = dispatched_state(Some("parent"));
        reduce(
            &mut state,
            &FleetEvent::PermissionRequest {
                session_id: "run1".into(),
                cwd: String::new(),
                request_id: "r1".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
            t + chrono::Duration::seconds(1),
        );
        assert!(state.sessions["run1"].status.needs_attention());
        assert!(sweep_dispatch_continuations(
            &mut state,
            &[],
            t + chrono::Duration::hours(1),
            true
        )
        .is_empty());
    }

    #[test]
    fn a_precreated_run_that_never_started_continues_nothing() {
        let now = utc("2026-09-16T10:00:00Z");
        let mut state = FleetState::default();
        let mut row = FleetSession::new("run1", "/Users/x/dev/hobbes", now);
        row.dispatched_todo = Some("todo-1".into());
        row.dispatched_from = Some("parent".into());
        state.sessions.insert(row.id.clone(), row);
        // Idle from birth, but no Stop ever marked it dirty and no brief exists.
        assert!(sweep_dispatch_continuations(
            &mut state,
            &[],
            now + chrono::Duration::hours(2),
            true
        )
        .is_empty());
    }

    #[test]
    fn the_setting_off_continues_nothing_and_keeps_the_latch_open() {
        let (mut state, stopped) = dispatched_state(Some("parent"));
        let landed = stopped + chrono::Duration::seconds(50);
        state.sessions.get_mut("run1").unwrap().brief = Some(brief_at(landed, "Done."));

        assert!(sweep_dispatch_continuations(&mut state, &[], landed, false).is_empty());
        assert!(!state.sessions["run1"].continuation_sent);
        // Switching it back on hands over the run that finished meanwhile.
        assert_eq!(
            sweep_dispatch_continuations(&mut state, &[], landed, true)
                .continuations
                .len(),
            1
        );
    }

    #[test]
    fn legacy_rows_load_without_the_continuation_fields() {
        let legacy = r#"{"id":"x","cwd":"/a","name":"a","status":"Idle",
            "last_event_at":"2026-08-25T10:00:00Z","working_since":null,
            "dispatched_todo":"todo-1"}"#;
        let s: FleetSession = serde_json::from_str(legacy).unwrap();
        assert_eq!(s.dispatched_todo.as_deref(), Some("todo-1"));
        assert!(s.dispatched_from.is_none());
        assert!(!s.continuation_sent);
        // And they round-trip through the store's `data` JSON column.
        let round: FleetSession =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round, s);
    }

    #[test]
    fn the_shared_continuation_queue_dedupes_and_drains_once() {
        let shared = FleetShared::new();
        let c = |run: &str| PendingContinuation {
            parent_session_id: "parent".into(),
            run_session_id: run.into(),
            todo_id: None,
            session_name: "run".into(),
            progress: None,
        };
        shared.push_continuations(vec![c("run1"), c("run2")]);
        shared.push_continuations(vec![c("run1")]);
        let taken = shared.take_continuations();
        assert_eq!(taken.len(), 2);
        assert!(shared.take_continuations().is_empty());
    }

    #[test]
    fn session_refs_resolve_by_id_or_unique_name() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        reduce(&mut state, &start("s1"), t0);
        reduce(
            &mut state,
            &FleetEvent::SessionStart {
                session_id: "s2".into(),
                cwd: "/Users/x/dev/hobbes".into(), // same name as s1
                source: "startup".into(),
            },
            t0,
        );
        reduce(
            &mut state,
            &FleetEvent::SessionStart {
                session_id: "s3".into(),
                cwd: "/Users/x/dev/puget".into(),
                source: "startup".into(),
            },
            t0,
        );
        // Exact id always wins.
        assert_eq!(resolve_session_ref(&state, "s2").unwrap(), "s2");
        // Unique name (case-insensitive) resolves.
        assert_eq!(resolve_session_ref(&state, "PUGET").unwrap(), "s3");
        // Ambiguous name errors toward the id.
        assert!(resolve_session_ref(&state, "hobbes")
            .unwrap_err()
            .contains("HOBBES_FLEET_STATUS"));
        // Unknown names the live sessions.
        assert!(resolve_session_ref(&state, "nope").unwrap_err().contains("puget"));
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
        let total: u32 = rows[0].day_seconds.values().sum();
        assert_eq!(total, 120);
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
        let total: u32 = rows[0].day_seconds.values().sum();
        assert_eq!(total, STALENESS_MINUTES as u32 * 60);
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
            s.banked_seconds_on(day),
            STALENESS_MINUTES as u32 * 60
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
    fn sweep_gives_background_phases_the_long_staleness_window() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T09:00:00Z");
        reduce(&mut state, &start("s1"), t0);
        // Turn ends with a long-running background shell: WorkingBackground.
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 1 },
            t0 + chrono::Duration::minutes(5),
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::WorkingBackground);

        // 30 quiet minutes — well past the foreground window — is normal
        // for a background job: the sweep must not demote it.
        let quiet = t0 + chrono::Duration::minutes(35);
        assert!(sweep_stale(&mut state, quiet).is_empty());
        assert_eq!(state.sessions["s1"].status, FleetStatus::WorkingBackground);

        // Past the background window it finally idles.
        let long_gone = t0 + chrono::Duration::minutes(5 + BACKGROUND_STALENESS_MINUTES + 1);
        let changed = sweep_stale(&mut state, long_gone);
        assert_eq!(changed.len(), 1);
        assert_eq!(state.sessions["s1"].status, FleetStatus::Idle);
    }

    #[test]
    fn subagent_stop_with_remaining_tasks_heals_an_idle_demotion() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T09:00:00Z");
        reduce(&mut state, &start("s1"), t0);
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 2 },
            t0 + chrono::Duration::minutes(5),
        );
        // Something already demoted the phase to Idle (an old build's sweep,
        // a persisted pre-fix row).
        state.sessions.get_mut("s1").unwrap().status = FleetStatus::Idle;
        state.sessions.get_mut("s1").unwrap().working_since = None;

        // One background job finishes, one remains: proof the phase is live.
        let t1 = t0 + chrono::Duration::minutes(40);
        reduce(
            &mut state,
            &FleetEvent::SubagentStop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 1,
            },
            t1,
        );
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::WorkingBackground, "idle card healed");
        assert_eq!(s.working_since, Some(t1), "a fresh provable span opens");
    }

    #[test]
    fn dismiss_removes_any_status_and_ends_at_last_event() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T09:00:00Z");
        reduce(
            &mut state,
            &FleetEvent::Notification {
                session_id: "stuck".into(),
                cwd: "/x".into(),
                kind: "idle_prompt".into(),
                message: "waiting for your input".into(),
            },
            t0,
        );
        assert!(state.sessions["stuck"].status.needs_attention());

        let row = dismiss_in_state(&mut state, "stuck").expect("session dismissed");
        assert!(!state.sessions.contains_key("stuck"), "left the live map");
        assert_eq!(row.ended_at, Some(t0), "ends at its last event, not now");
        assert!(row.pending_gate.is_none());
        assert!(dismiss_in_state(&mut state, "stuck").is_none(), "idempotent");
    }

    #[test]
    fn retirement_clears_idle_sessions_but_never_attention() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T09:00:00Z");
        // Idle long ago; working recently; waiting on the user for hours.
        reduce(&mut state, &start("old-idle"), t0);
        reduce(&mut state, &stop("old-idle"), t0 + chrono::Duration::minutes(1));
        reduce(
            &mut state,
            &start("fresh"),
            t0 + chrono::Duration::minutes(90),
        );
        reduce(
            &mut state,
            &FleetEvent::Notification {
                session_id: "stuck-gate".into(),
                cwd: "/x".into(),
                kind: "permission_prompt".into(),
                message: "needs you".into(),
            },
            t0,
        );

        let sweep_at = t0 + chrono::Duration::minutes(95);
        let retired = sweep_retired_after(&mut state, sweep_at, 60);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id, "old-idle");
        assert_eq!(
            retired[0].ended_at,
            Some(t0 + chrono::Duration::minutes(1)),
            "retires at its last event, not at sweep time"
        );
        assert!(state.sessions.contains_key("fresh"));
        assert!(
            state.sessions.contains_key("stuck-gate"),
            "attention never falls off the board silently"
        );
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
    fn sub_minute_turns_accumulate_instead_of_flooring_to_zero() {
        // The Hobbes-tab signature: many chat turns under a minute each.
        // Per-span minute flooring banked all of them as 0.
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 9, 0);
        for i in 0..20u32 {
            let turn_start = t0 + chrono::Duration::seconds(i as i64 * 120);
            reduce(
                &mut state,
                &FleetEvent::PromptSubmit { session_id: "tab".into(), cwd: String::new() },
                turn_start,
            );
            reduce(
                &mut state,
                &stop("tab"),
                turn_start + chrono::Duration::seconds(45),
            );
        }
        // 20 × 45s = 15 minutes, not 0.
        let at = local_at(day, 10, 0);
        assert_eq!(state.sessions["tab"].minutes_on(day, at), 15);
    }

    #[test]
    fn cross_midnight_span_splits_between_local_days() {
        let day1 = date("2026-08-24");
        let day2 = date("2026-08-25");
        let mut minutes = BTreeMap::new();
        // 23:40 → 00:20 local: 20 minutes on each side of midnight.
        bank_span(&mut minutes, local_at(day1, 23, 40), local_at(day2, 0, 20));
        assert_eq!(minutes.get(&day1).copied(), Some(20 * 60));
        assert_eq!(minutes.get(&day2).copied(), Some(20 * 60));
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
    fn terminal_hint_folds_into_the_live_row_and_its_persisted_clone() {
        use terminal_focus::TerminalHint;
        let mut state = FleetState::default();
        let now = utc("2026-08-25T10:00:00Z");
        let mut changed = reduce(
            &mut state,
            &FleetEvent::SessionStart { session_id: "s1".into(), cwd: "/x".into(), source: "startup".into() },
            now,
        );
        let hint = TerminalHint {
            bundle_id: Some(terminal_focus::ITERM_BUNDLE.into()),
            started_at: Some(now),
            ..Default::default()
        };
        apply_terminal_hint(&mut state, &mut changed, "s1", &hint);
        assert_eq!(state.sessions["s1"].terminal.as_ref(), Some(&hint));
        assert_eq!(changed[0].terminal.as_ref(), Some(&hint));

        // An empty hint changes nothing; an unchanged one adds no row.
        let mut none = Vec::new();
        apply_terminal_hint(&mut state, &mut none, "s1", &TerminalHint::default());
        apply_terminal_hint(&mut state, &mut none, "s1", &hint);
        assert!(none.is_empty());
        // A moved hint for a row not in `changed` is added so it persists.
        let newer = TerminalHint { iterm_session: Some("w1t1p0:x".into()), ..Default::default() };
        apply_terminal_hint(&mut state, &mut none, "s1", &newer);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].terminal.as_ref().unwrap().iterm_session.as_deref(), Some("w1t1p0:x"));
        // Unknown session: no-op.
        apply_terminal_hint(&mut state, &mut none, "nope", &newer);
        assert_eq!(none.len(), 1);
    }

    #[test]
    fn exocortex_multiplier_math_and_guards() {
        // 3 agents driven through one engaged hour → 3×; a solo hour → 1×.
        let (m, engaged) = exocortex_multiplier(180, 60).unwrap();
        assert_eq!(engaged, 60);
        assert!((m - 3.0).abs() < 0.001);
        let (m, _) = exocortex_multiplier(60, 60).unwrap();
        assert!((m - 1.0).abs() < 0.001);
        // Guards: no work, or a just-started day → None.
        assert!(exocortex_multiplier(0, 60).is_none());
        assert!(exocortex_multiplier(10, 3).is_none());
    }

    #[test]
    fn coverage_unions_parallel_sessions_and_skips_breaks() {
        let mut state = FleetState::default();
        let day = date("2026-09-04");
        let submit = |id: &str| FleetEvent::PromptSubmit {
            session_id: id.into(),
            cwd: "/tmp/a".into(),
        };
        // s1 starts at 9:00, s2 joins at 9:10; s1 stops at 9:30, s2 at 9:40.
        reduce(&mut state, &submit("s1"), local_at(day, 9, 0));
        assert!(sync_coverage(&mut state, local_at(day, 9, 0)).is_empty());
        reduce(&mut state, &submit("s2"), local_at(day, 9, 10));
        // Progressive: each tick banks the slice since the last one.
        assert_eq!(sync_coverage(&mut state, local_at(day, 9, 10)), vec![(day, 600)]);
        // s2 heartbeats (tool activity) — without events a span stops
        // provably accruing after the staleness cap.
        let beat = |state: &mut FleetState, at| {
            reduce(
                state,
                &FleetEvent::ToolActivity {
                    session_id: "s2".into(),
                    cwd: "/tmp/a".into(),
                    tool_name: "Bash".into(),
                },
                at,
            );
        };
        beat(&mut state, local_at(day, 9, 20));
        reduce(&mut state, &stop("s1"), local_at(day, 9, 30));
        beat(&mut state, local_at(day, 9, 30));
        // s2 still accruing → the engaged period stays open, marker advanced.
        assert_eq!(sync_coverage(&mut state, local_at(day, 9, 30)), vec![(day, 1800)]);
        assert_eq!(state.coverage_open, Some(local_at(day, 9, 30)));
        reduce(&mut state, &stop("s2"), local_at(day, 9, 40));
        let banked = sync_coverage(&mut state, local_at(day, 9, 40));
        // Union = 40 engaged minutes (cumulative day total); agent time =
        // 30 + 30 = 60 → 1.5×.
        assert_eq!(banked, vec![(day, 2400)]);
        let engaged = engaged_seconds_on(&state, day, local_at(day, 9, 40)) / 60;
        let agent = live_minutes_on(&state, day, local_at(day, 9, 40));
        assert_eq!((engaged, agent), (40, 60));
        let (m, _) = exocortex_multiplier(agent, engaged).unwrap();
        assert!((m - 1.5).abs() < 0.001);
        // A long break does NOT accrue; re-engaging reopens and the live
        // tail counts from there.
        assert!(sync_coverage(&mut state, local_at(day, 11, 0)).is_empty());
        reduce(&mut state, &submit("s1"), local_at(day, 12, 0));
        sync_coverage(&mut state, local_at(day, 12, 0));
        assert_eq!(
            engaged_seconds_on(&state, day, local_at(day, 12, 5)),
            2400 + 300
        );
    }

    #[test]
    fn coverage_walk_away_banks_only_the_provable_tail() {
        let mut state = FleetState::default();
        let day = date("2026-09-04");
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit {
                session_id: "s1".into(),
                cwd: "/tmp/a".into(),
            },
            local_at(day, 9, 0),
        );
        sync_coverage(&mut state, local_at(day, 9, 0));
        // The session goes silent; the 9:30 sweep tick closes coverage at
        // the staleness cap past the last event — not at the tick itself.
        let banked = sync_coverage(&mut state, local_at(day, 9, 30));
        assert_eq!(banked, vec![(day, STALENESS_MINUTES as u32 * 60)]);
        assert!(state.coverage_open.is_none());
    }

    /// A background phase grinding through the night must bank its engaged
    /// time as it goes: a restart mid-phase loses at most one tick, and
    /// never the whole period — the numerator (per-row `working_since`)
    /// survives a restart, so the denominator has to as well or the
    /// multiplier explodes.
    #[test]
    fn coverage_survives_restart_mid_background_phase() {
        let mut state = FleetState::default();
        let day = date("2026-09-10");
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "bg".into(), cwd: "/tmp/a".into() },
            local_at(day, 0, 0),
        );
        // Stop with background jobs → WorkingBackground, span reopens.
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "bg".into(), cwd: "/tmp/a".into(), background_tasks: 2 },
            local_at(day, 0, 1),
        );
        sync_coverage(&mut state, local_at(day, 0, 1));
        // Sweep ticks through the night; subagent heartbeats keep it provable.
        let mut persisted: Vec<(NaiveDate, u32)> = Vec::new();
        for h in 1..=9u32 {
            reduce(
                &mut state,
                &FleetEvent::SubagentStop { session_id: "bg".into(), cwd: "/tmp/a".into(), background_tasks: 2 },
                local_at(day, h, 0),
            );
            persisted = sync_coverage(&mut state, local_at(day, h, 0));
        }
        assert_eq!(persisted, vec![(day, 9 * 3600 - 60)], "banked progressively to the last tick");
        assert_eq!(state.coverage_open, Some(local_at(day, 9, 0)));

        // Restart: rows + coverage come back from the store, the open marker
        // does not. Engaged time is intact up to the last tick.
        let mut fresh = FleetState::default();
        fresh.sessions = state.sessions.clone();
        for (d, secs) in &persisted {
            fresh.coverage_seconds.insert(*d, *secs);
        }
        let after = local_at(day, 9, 5);
        assert!(sync_coverage(&mut fresh, after).is_empty(), "reopens, nothing to bank yet");
        assert_eq!(fresh.coverage_open, Some(after));
        let engaged = engaged_seconds_on(&fresh, day, after) / 60;
        let agent = live_minutes_on(&fresh, day, after);
        // 9h of agent time over 9h of engaged time (minus one lost tick): ~1×,
        // not the 100× a lost denominator would show.
        let (m, _) = exocortex_multiplier(agent, engaged).unwrap();
        assert!(m < 1.05, "multiplier {m} should be ~1× after a restart");
    }

    /// Progressive banking must not soften the walk-away rule: once nothing
    /// is provably accruing, the close still ends at the provable tail, and
    /// slices banked earlier while accruing are not counted twice.
    #[test]
    fn coverage_progressive_then_walk_away_caps_at_provable_end() {
        let mut state = FleetState::default();
        let day = date("2026-09-04");
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: "/tmp/a".into() },
            local_at(day, 9, 0),
        );
        sync_coverage(&mut state, local_at(day, 9, 0));
        assert_eq!(sync_coverage(&mut state, local_at(day, 9, 5)), vec![(day, 300)]);
        // Silent since 9:00; the 9:30 tick closes at 9:00 + staleness cap.
        let banked = sync_coverage(&mut state, local_at(day, 9, 30));
        assert_eq!(banked, vec![(day, STALENESS_MINUTES as u32 * 60)]);
        assert!(state.coverage_open.is_none());
        // Further ticks with nothing open bank nothing.
        assert!(sync_coverage(&mut state, local_at(day, 9, 45)).is_empty());
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
        s.terminal = Some(terminal_focus::TerminalHint {
            term_program: Some("iTerm.app".into()),
            ..Default::default()
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: FleetSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn post_turn_background_events_are_liveness_only() {
        // The RCA smoking gun: SubagentStop/ToolActivity arriving AFTER the
        // turn's Stop must not re-light Working, open spans, or clear alerts.
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        let t0 = local_at(day, 9, 0);
        reduce_with_prompt(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: "/x".into() },
            Some("p1"),
            t0,
        );
        reduce_with_prompt(&mut state, &stop("s1"), Some("p1"), local_at(day, 9, 10));
        // CC raises "waiting for your input".
        reduce_with_prompt(
            &mut state,
            &FleetEvent::Notification {
                session_id: "s1".into(),
                cwd: String::new(),
                kind: "idle_prompt".into(),
                message: "Claude is waiting for your input".into(),
            },
            Some("p1"),
            local_at(day, 9, 11),
        );
        assert!(state.sessions["s1"].status.needs_attention());
        assert!(state.sessions["s1"].attention_at.is_some());

        // A background subagent completes 3 minutes later with MORE still
        // running: hard evidence the session is working, not waiting — the
        // idle-nudge alert demotes to WorkingBackground.
        let t_bg = local_at(day, 9, 14);
        reduce_with_prompt(
            &mut state,
            &FleetEvent::SubagentStop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 1,
            },
            Some("p1"),
            t_bg,
        );
        {
            let s = &state.sessions["s1"];
            assert_eq!(s.status, FleetStatus::WorkingBackground, "idle nudge demoted");
            assert!(s.attention_at.is_none());
            assert_eq!(s.last_event_at, t_bg);
        }
        // A further idle nudge during the background phase does NOT re-raise.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::Notification {
                session_id: "s1".into(),
                cwd: String::new(),
                kind: "idle_prompt".into(),
                message: "Claude is waiting for your input".into(),
            },
            Some("p1"),
            local_at(day, 9, 15),
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::WorkingBackground);
        // But a permission notification ALWAYS alerts, background or not.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::Notification {
                session_id: "s1".into(),
                cwd: String::new(),
                kind: "permission_prompt".into(),
                message: "Claude needs your permission to use Bash".into(),
            },
            Some("p1"),
            local_at(day, 9, 16),
        );
        assert!(state.sessions["s1"].status.needs_attention());
        // …and background completions never clear a permission alert.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::SubagentStop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 1,
            },
            Some("p1"),
            local_at(day, 9, 17),
        );
        assert!(
            state.sessions["s1"].status.needs_attention(),
            "permission alerts survive background activity"
        );
        // Reset to plain idle attention for the rest of the test.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: String::new() },
            Some("p1b"),
            local_at(day, 9, 18),
        );
        reduce_with_prompt(&mut state, &stop("s1"), Some("p1b"), local_at(day, 9, 19));

        // A NEW prompt's heartbeat behaves normally again.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: String::new() },
            Some("p2"),
            local_at(day, 9, 20),
        );
        reduce_with_prompt(
            &mut state,
            &FleetEvent::ToolActivity {
                session_id: "s1".into(),
                cwd: String::new(),
                tool_name: "Bash".into(),
            },
            Some("p2"),
            local_at(day, 9, 21),
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::Working);

        // Legacy events without prompt ids keep today's behavior.
        reduce_with_prompt(&mut state, &stop("s1"), None, local_at(day, 9, 25));
        reduce_with_prompt(
            &mut state,
            &FleetEvent::ToolActivity {
                session_id: "s1".into(),
                cwd: String::new(),
                tool_name: "Bash".into(),
            },
            None,
            local_at(day, 9, 26),
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::Working);
    }

    #[test]
    fn last_subagent_stop_ends_the_background_phase() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce_with_prompt(&mut state, &start("s1"), Some("p1"), local_at(day, 9, 0));
        reduce_with_prompt(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 1 },
            Some("p1"),
            local_at(day, 9, 10),
        );
        assert_eq!(state.sessions["s1"].status, FleetStatus::WorkingBackground);
        // Background completion with none remaining → phase over, proven credit.
        reduce_with_prompt(
            &mut state,
            &FleetEvent::SubagentStop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 0,
            },
            Some("p1"),
            local_at(day, 9, 25),
        );
        let s = &state.sessions["s1"];
        assert_eq!(s.status, FleetStatus::Idle);
        assert!(s.working_since.is_none());
        assert_eq!(s.minutes_on(day, local_at(day, 9, 25)), 25);
    }

    #[test]
    fn stopped_prompt_ring_caps_and_resets_on_session_start() {
        let mut state = FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        for i in 0..12 {
            reduce_with_prompt(
                &mut state,
                &stop("s1"),
                Some(&format!("p{i}")),
                t0 + chrono::Duration::minutes(i),
            );
        }
        assert_eq!(state.sessions["s1"].stopped_prompts.len(), 8);
        assert!(!state.sessions["s1"].stopped_prompts.contains(&"p0".to_string()));
        reduce_with_prompt(&mut state, &start("s1"), Some("p99"), t0 + chrono::Duration::hours(1));
        assert!(state.sessions["s1"].stopped_prompts.is_empty());
        assert!(state.sessions["s1"].active_prompt.is_none());
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
            &FleetEvent::SubagentStop {
                session_id: "s1".into(),
                cwd: String::new(),
                background_tasks: 1,
            },
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
    fn background_quiet_time_credits_live_and_caps_at_the_long_window() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 9, 0));
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 1 },
            local_at(day, 9, 10),
        );
        // A silent two-hour background stretch is real agent work: it shows
        // live (the exocortex numerator keeps up) and banks in full when the
        // user's return closes the span.
        let s = &state.sessions["s1"];
        assert_eq!(s.minutes_on(day, local_at(day, 11, 0)), 120, "quiet background accrues live");
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
            s.banked_seconds_on(day),
            (10 + 120) * 60,
            "10 proven + the full background stretch"
        );
    }

    #[test]
    fn background_span_still_caps_past_the_long_window() {
        let mut state = FleetState::default();
        let day = date("2026-08-25");
        reduce(&mut state, &start("s1"), local_at(day, 8, 0));
        reduce(
            &mut state,
            &FleetEvent::Stop { session_id: "s1".into(), cwd: String::new(), background_tasks: 1 },
            local_at(day, 8, 10),
        );
        // Six silent hours: only the long window past the last event is
        // provable — a killed terminal can't bank the whole afternoon.
        let t_return = local_at(day, 14, 10);
        reduce(
            &mut state,
            &FleetEvent::PromptSubmit { session_id: "s1".into(), cwd: String::new() },
            t_return,
        );
        assert_eq!(
            state.sessions["s1"].banked_seconds_on(day),
            (10 + BACKGROUND_STALENESS_MINUTES as u32) * 60,
            "10 proven + the capped background tail"
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
        assert!(shared.set_brief("s1", brief(started), started, None));
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
        assert!(shared.set_brief("s1", brief(started), started, None));
        {
            let state = shared.state.lock().unwrap();
            assert!(state.sessions["s1"].brief_dirty_at.is_some(), "racing Stop keeps it dirty");
        }
        // Unknown session → false (caller merges into the store instead).
        assert!(!shared.set_brief("nope", brief(started), started, None));
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
    fn a_timer_wait_holds_until_overdue_and_never_retires() {
        let now = Utc::now();
        let mut state = FleetState::default();
        let mut s = FleetSession::new("tab1", "", now - chrono::Duration::hours(6));
        s.status = FleetStatus::Waiting;
        s.wake_at = Some(now + chrono::Duration::hours(2));
        state.sessions.insert("tab1".into(), s);
        // Hours of silence before the timer is due: still waiting, still on
        // the board.
        assert!(sweep_stale(&mut state, now).is_empty());
        assert!(sweep_retired_after(&mut state, now, 60).is_empty());
        assert_eq!(state.sessions["tab1"].status, FleetStatus::Waiting);
        // Past due by more than the staleness window (it never fired, e.g.
        // the app was closed): idle.
        let late = now + chrono::Duration::hours(2) + chrono::Duration::minutes(STALENESS_MINUTES + 1);
        let changed = sweep_stale(&mut state, late);
        assert_eq!(changed.len(), 1);
        assert_eq!(state.sessions["tab1"].status, FleetStatus::Idle);
        assert_eq!(state.sessions["tab1"].wake_at, None);
    }
}
