//! Agent time: a small, human-granted budget in which the harness chooses its
//! own work, in the open (SYMBIOTIC_PLANNER.md §3).
//!
//! The rules this module enforces:
//!
//! - **Granted, not taken.** Nothing runs unless `AgentTimeGrant.enabled`,
//!   today is a granted day, and minutes and spend remain
//!   ([`agent_time_budget`]).
//! - **Only from an agent-time session.** The runtime opens an
//!   [`AgentTimeLease`] naming the chat session it runs agent time in
//!   ([`PlannerState::open_agent_time_lease`]); `HOBBES_AGENT_TIME` is refused
//!   from every other session, and the tool definition is withheld from
//!   ordinary chats (`PlannerClient::agent_time_tools`).
//! - **Declared up front, logged in full.** `start(intent)` opens a logged
//!   [`AgentTimeSession`]; every call (and every tool call the runtime gate
//!   sees, via [`PlannerState::record_agent_time_tool_call`]) appends to it.
//! - **Interruptible.** [`PlannerState::close_agent_time_lease`] ends the
//!   open session and revokes the lease at once — the next call is refused.
//! - **Four jobs, one review.** Outputs are results tagged with a [`Job`]:
//!   `deliver` files finished Prepare / Finish / Tend work the person grades
//!   Keep / Needs work / Discard ([`PlannerState::grade_result`]); `propose`
//!   files a Notice proposal they accept or dismiss
//!   ([`PlannerState::resolve_proposal`]); `ask` saves a question instead of
//!   stopping the run ([`PlannerState::agent_time_ask`]). Grading, answering
//!   and commenting are UI actions, never tools; [`review_for`] builds the
//!   one review the person reads.
//! - **Scoped.** [`agent_time_scope_allows`] is the pure check the runtime's
//!   tool gate applies to every call made during agent time.
//!
//! Everything here is in-memory state logic; callers persist the rows the
//! methods return (`store::save_agent_time_session`, `save_proposal`,
//! `save_trust_ledger`).

// Public API whose consumers are wave 2 (the agent-time runtime (lease, gate hooks, budget tick) and the Agent time panel); the
// tool handlers use only part of it so far.
#![allow(dead_code)]

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

use super::model::{
    AgentQuestion, AgentState, AgentTimeEndReason, AgentTimeGrant, AgentTimeLogEntry,
    AgentTimeLogKind, AgentTimeSession, Job, Lane, Proposal, ProposalKind, ProposalRevision,
    ProposalStatus, ResultNote, Todo, TrustLedger,
};
use super::PlannerState;

/// Longest declared intent kept, in characters ("one or two lines").
pub const MAX_INTENT_CHARS: usize = 300;
/// Longest log summary kept per entry.
pub const MAX_LOG_SUMMARY_CHARS: usize = 200;

/// The runtime's claim that a chat session is running agent time. Runtime
/// only (never persisted): a restart revokes it, and open sessions are
/// closed as `recovered` on load.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTimeLease {
    /// The Hobbes chat session the runtime runs agent time in. The tool
    /// dispatcher compares the calling session id against this.
    pub chat_session_id: String,
    pub opened_at: DateTime<Utc>,
}

/// Today's agent-time budget against the grant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentTimeBudget {
    pub enabled: bool,
    pub granted_today: bool,
    pub minutes_granted: u32,
    pub minutes_used: u32,
    pub spend_cap_usd: f64,
    pub spend_used_usd: f64,
}

impl AgentTimeBudget {
    pub fn minutes_left(&self) -> u32 {
        if !self.enabled || !self.granted_today {
            return 0;
        }
        self.minutes_granted.saturating_sub(self.minutes_used)
    }

    pub fn spend_left_usd(&self) -> f64 {
        if !self.enabled || !self.granted_today {
            return 0.0;
        }
        (self.spend_cap_usd - self.spend_used_usd).max(0.0)
    }

    /// Why agent time can't run right now, if it can't.
    pub fn refusal(&self) -> Option<AgentTimeRefusal> {
        if !self.enabled {
            Some(AgentTimeRefusal::NotGranted)
        } else if !self.granted_today {
            Some(AgentTimeRefusal::NotToday)
        } else if self.minutes_left() == 0 {
            Some(AgentTimeRefusal::MinutesExhausted)
        } else if self.spend_used_usd >= self.spend_cap_usd {
            Some(AgentTimeRefusal::SpendCapReached)
        } else {
            None
        }
    }
}

/// Sessions count against the local day they started on.
fn started_on(s: &AgentTimeSession, today: NaiveDate) -> bool {
    s.started_at.with_timezone(&chrono::Local).date_naive() == today
}

/// Today's budget: minutes and spend of every session started today
/// (counting a live one up to `now`) against the grant.
pub fn agent_time_budget(
    grant: &AgentTimeGrant,
    sessions: &[AgentTimeSession],
    today: NaiveDate,
    now: DateTime<Utc>,
) -> AgentTimeBudget {
    let todays = sessions.iter().filter(|s| started_on(s, today));
    let (minutes_used, spend_used_usd) = todays.fold((0u32, 0.0f64), |(m, c), s| {
        (m.saturating_add(s.minutes_used(now)), c + s.cost_usd.max(0.0))
    });
    AgentTimeBudget {
        enabled: grant.enabled,
        granted_today: grant.grants_day(today),
        minutes_granted: grant.daily_minutes,
        minutes_used,
        spend_cap_usd: grant.spend_cap_usd.max(0.0),
        spend_used_usd,
    }
}

/// Why a `HOBBES_AGENT_TIME` call (or a lease) was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTimeRefusal {
    /// The grant is off.
    NotGranted,
    /// Today isn't one of the granted days.
    NotToday,
    MinutesExhausted,
    SpendCapReached,
    /// The call didn't come from the session the runtime leased.
    NotAgentTimeSession,
    /// The runtime already holds a lease in another session.
    LeaseHeld,
    /// `start` while a session is already open.
    AlreadyRunning,
    /// `log` / `propose` / `end` with no open session.
    NoOpenSession,
    /// Bad input (empty intent, unknown kind, out-of-scope kind…).
    Invalid(String),
}

impl AgentTimeRefusal {
    pub fn message(&self) -> String {
        match self {
            AgentTimeRefusal::NotGranted => {
                "Agent time is off. Only the user can grant it (Planner → Agent time).".into()
            }
            AgentTimeRefusal::NotToday => "Today is not a granted agent-time day.".into(),
            AgentTimeRefusal::MinutesExhausted => {
                "Today's granted agent-time minutes are used up.".into()
            }
            AgentTimeRefusal::SpendCapReached => "Today's agent-time spend cap is reached.".into(),
            AgentTimeRefusal::NotAgentTimeSession => {
                "HOBBES_AGENT_TIME is only available inside a granted agent-time session, \
                 never in normal chat."
                    .into()
            }
            AgentTimeRefusal::LeaseHeld => "Agent time is already running in another session.".into(),
            AgentTimeRefusal::AlreadyRunning => {
                "An agent-time session is already open — call end(summary) first.".into()
            }
            AgentTimeRefusal::NoOpenSession => {
                "No agent-time session is open — call start(intent) first.".into()
            }
            AgentTimeRefusal::Invalid(why) => why.clone(),
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// Remove the trailing `Note` rows (the model's `log` notes no call has
/// followed yet) and return their text, joined, for the call about to be
/// recorded.
fn take_trailing_notes(s: &mut AgentTimeSession) -> Option<String> {
    let keep = s
        .tool_calls
        .iter()
        .rposition(|e| e.kind != AgentTimeLogKind::Note)
        .map_or(0, |i| i + 1);
    if keep == s.tool_calls.len() {
        return None;
    }
    let notes: Vec<String> = s.tool_calls.drain(keep..).map(|e| e.summary).collect();
    Some(clip(&notes.join(" \u{b7} "), MAX_LOG_SUMMARY_CHARS))
}

fn short_id(prefix: &str) -> String {
    format!("{}_{}", prefix, &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// Which ledger column a verdict counts in: accepted / kept (incl. edited),
/// needs work, dismissed / discarded. Re-verdicts within a bucket are no-ops.
fn verdict_bucket(s: ProposalStatus) -> u8 {
    match s {
        ProposalStatus::Open => 0,
        ProposalStatus::Accepted | ProposalStatus::Edited => 1,
        ProposalStatus::NeedsWork => 2,
        ProposalStatus::Dismissed => 3,
    }
}

/// Add (or, with `add: false`, take back) one verdict on the trust ledger:
/// Notice proposals move the proposal counters, graded results (Prepare /
/// Finish / Tend) the result counters.
fn ledger_count(l: &mut TrustLedger, job: Job, status: ProposalStatus, add: bool) {
    let bump = |n: &mut u32| *n = if add { n.saturating_add(1) } else { n.saturating_sub(1) };
    if job.is_graded() {
        match status {
            ProposalStatus::Accepted | ProposalStatus::Edited => bump(&mut l.results_kept),
            ProposalStatus::NeedsWork => bump(&mut l.results_needs_work),
            ProposalStatus::Dismissed => bump(&mut l.results_discarded),
            ProposalStatus::Open => {}
        }
    } else {
        match status {
            ProposalStatus::Accepted => bump(&mut l.proposals_accepted),
            ProposalStatus::Edited => bump(&mut l.proposals_edited),
            ProposalStatus::Dismissed => bump(&mut l.proposals_dismissed),
            // Refused for Notice upstream; nothing to count.
            ProposalStatus::NeedsWork | ProposalStatus::Open => {}
        }
    }
}

/// Most questions one run may leave open (`ask`).
pub const MAX_OPEN_QUESTIONS_PER_RUN: usize = 5;
/// Most suggested answers kept per question.
pub const MAX_QUESTION_OPTIONS: usize = 6;
/// Longest comment (or answer) kept, in characters.
pub const MAX_NOTE_CHARS: usize = 1000;

/// A graded result and the to-do grading moved (a Finish result's to-do).
#[derive(Debug, Clone, PartialEq)]
pub struct GradedResult {
    pub proposal: Proposal,
    pub todo: Option<Todo>,
}

/// What `deliver` files: finished work for one of the graded jobs.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery<'a> {
    /// `Prepare`, `Finish` or `Tend` (`Notice` is refused: use `propose`).
    pub job: Job,
    /// `None` = the job's default (prepare/tend → note, finish → other).
    pub kind: Option<ProposalKind>,
    pub title: &'a str,
    pub body: &'a str,
    /// Prepare: the prep-flagged human to-do (required). Finish: the
    /// agent-lane to-do it finishes (optional). Tend: ignored.
    pub todo_id: Option<&'a str>,
    /// Tend only: a correction to something Hobbes told the person.
    pub correction: bool,
}

/// What a `deliver` changed, for the caller to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveredResult {
    pub result: Proposal,
    pub session: AgentTimeSession,
    /// A Finish result's agent-lane to-do, now `NeedsYou` (awaiting review).
    pub todo: Option<Todo>,
}

impl PlannerState {
    /// Today's budget for the current grant.
    pub fn agent_time_budget(&self, today: NaiveDate, now: DateTime<Utc>) -> AgentTimeBudget {
        agent_time_budget(&self.agent_time_grant, &self.agent_time_sessions, today, now)
    }

    /// The open agent-time session, if any (at most one).
    pub fn open_agent_time_session(&self) -> Option<&AgentTimeSession> {
        self.agent_time_sessions.iter().find(|s| s.is_open())
    }

    fn open_agent_time_session_mut(&mut self) -> Option<&mut AgentTimeSession> {
        self.agent_time_sessions.iter_mut().find(|s| s.is_open())
    }

    /// Whether `chat_session_id` is the leased agent-time session — the
    /// agent-time session context `HOBBES_AGENT_TIME` requires.
    pub fn is_agent_time_session(&self, chat_session_id: &str) -> bool {
        self.agent_time_lease
            .as_ref()
            .is_some_and(|l| l.chat_session_id == chat_session_id)
    }

    /// **Runtime entry point.** Claim `chat_session_id` as the agent-time
    /// session. Checks the grant and today's budget first; refuses while a
    /// lease is held elsewhere. Re-leasing the same session is a no-op.
    pub fn open_agent_time_lease(
        &mut self,
        chat_session_id: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(), AgentTimeRefusal> {
        if let Some(l) = &self.agent_time_lease {
            return if l.chat_session_id == chat_session_id {
                Ok(())
            } else {
                Err(AgentTimeRefusal::LeaseHeld)
            };
        }
        if let Some(r) = self.agent_time_budget(today, now).refusal() {
            return Err(r);
        }
        self.agent_time_lease = Some(AgentTimeLease {
            chat_session_id: chat_session_id.to_string(),
            opened_at: now,
        });
        Ok(())
    }

    /// **Pause / stop.** Ends the open session (if any) with `reason` and
    /// revokes the lease immediately, so the very next `HOBBES_AGENT_TIME`
    /// call is refused. Returns the closed session to persist (the trust
    /// ledger may also have changed — persist it too). The runtime must also
    /// cancel the chat's in-flight stream.
    pub fn close_agent_time_lease(
        &mut self,
        now: DateTime<Utc>,
        reason: AgentTimeEndReason,
    ) -> Option<AgentTimeSession> {
        self.agent_time_lease = None;
        self.end_open_agent_time(now, reason, None)
    }

    fn end_open_agent_time(
        &mut self,
        now: DateTime<Utc>,
        reason: AgentTimeEndReason,
        summary: Option<String>,
    ) -> Option<AgentTimeSession> {
        let s = self.open_agent_time_session_mut()?;
        s.ended_at = Some(now.max(s.started_at));
        s.end_reason = Some(reason);
        if summary.is_some() {
            s.summary = summary;
        }
        let clean = s.out_of_scope_attempts == 0;
        let closed = s.clone();
        if clean {
            self.trust_ledger.clean_sessions = self.trust_ledger.clean_sessions.saturating_add(1);
        }
        Some(closed)
    }

    /// The common gate for every `HOBBES_AGENT_TIME` action: leased session,
    /// grant on, granted day, budget left. Pure check — the tool handler
    /// first runs [`Self::enforce_agent_time_budget`] so a session found over
    /// budget is closed on the record before the refusal.
    fn agent_time_gate(
        &self,
        caller: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeBudget, AgentTimeRefusal> {
        if !self.is_agent_time_session(caller) {
            return Err(AgentTimeRefusal::NotAgentTimeSession);
        }
        let budget = self.agent_time_budget(today, now);
        match budget.refusal() {
            Some(r) => Err(r),
            None => Ok(budget),
        }
    }

    /// **Runtime tick.** End the open session when today's minutes or spend
    /// ran out (or the grant was switched off). Returns it to persist.
    pub fn enforce_agent_time_budget(
        &mut self,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Option<AgentTimeSession> {
        self.open_agent_time_session()?;
        let reason = match self.agent_time_budget(today, now).refusal()? {
            AgentTimeRefusal::SpendCapReached => AgentTimeEndReason::SpendCapReached,
            AgentTimeRefusal::MinutesExhausted => AgentTimeEndReason::BudgetExhausted,
            _ => AgentTimeEndReason::Stopped,
        };
        self.end_open_agent_time(now, reason, None)
    }

    /// Append a tool call to the log. A `log` note the model wrote just
    /// before it (trailing `Note` rows) is folded into this call's `note`
    /// instead of standing as a row of its own, so one call is one row.
    fn push_log(s: &mut AgentTimeSession, tool: &str, summary: &str, now: DateTime<Utc>) {
        let note = take_trailing_notes(s);
        s.tool_calls.push(AgentTimeLogEntry {
            at: now,
            tool: clip(tool, 80),
            summary: clip(summary, MAX_LOG_SUMMARY_CHARS),
            kind: AgentTimeLogKind::Call,
            note,
        });
    }

    /// Append a non-call line (`Note` or `Event`).
    fn push_line(
        s: &mut AgentTimeSession,
        kind: AgentTimeLogKind,
        tool: &str,
        summary: &str,
        now: DateTime<Utc>,
    ) {
        s.tool_calls.push(AgentTimeLogEntry {
            at: now,
            tool: clip(tool, 80),
            summary: clip(summary, MAX_LOG_SUMMARY_CHARS),
            kind,
            note: None,
        });
    }

    /// `start(intent)`: declare the intent and open a logged session of
    /// Hobbes-space work (no to-do).
    pub fn agent_time_start(
        &mut self,
        caller: &str,
        intent: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeSession, AgentTimeRefusal> {
        self.agent_time_start_on(caller, intent, None, &|_: &str| false, today, now)
    }

    /// `start(intent, todo_id?)`: as [`Self::agent_time_start`], optionally
    /// on one agent-lane to-do, which must be [`agent_time_eligible`]
    /// (`worker_live` says whether a linked worker is running now). A
    /// human-lane to-do is refused: it is the person's, and agent time may
    /// only offer to take it (`propose` kind `offer`).
    pub fn agent_time_start_on(
        &mut self,
        caller: &str,
        intent: &str,
        todo_id: Option<&str>,
        worker_live: &dyn Fn(&str) -> bool,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeSession, AgentTimeRefusal> {
        let budget = self.agent_time_gate(caller, today, now)?;
        let todo_id = todo_id.map(str::trim).filter(|t| !t.is_empty());
        if let Some(id) = todo_id {
            self.check_workable(id, worker_live)?;
        }
        if self.open_agent_time_session().is_some() {
            return Err(AgentTimeRefusal::AlreadyRunning);
        }
        let intent = intent.trim();
        if intent.is_empty() {
            return Err(AgentTimeRefusal::Invalid(
                "Declare an 'intent' first: what you will do and why, in one or two lines.".into(),
            ));
        }
        let mut s = AgentTimeSession {
            id: short_id("at"),
            intent: clip(intent, MAX_INTENT_CHARS),
            started_at: now,
            ended_at: None,
            cost_usd: 0.0,
            tool_calls: Vec::new(),
            outputs: Vec::new(),
            end_reason: None,
            summary: None,
            chat_session_id: Some(caller.to_string()),
            out_of_scope_attempts: 0,
            todo_id: todo_id.map(str::to_string),
            reviewed_at: None,
        };
        let on = todo_id
            .and_then(|id| self.todo(id))
            .map(|t| format!(" · working on {}", t.title))
            .unwrap_or_default();
        Self::push_log(
            &mut s,
            "HOBBES_AGENT_TIME.start",
            &format!(
                "intent declared · grant ok · {} min, ${:.2} left{}",
                budget.minutes_left(),
                budget.spend_left_usd(),
                on
            ),
            now,
        );
        self.agent_time_sessions.push(s.clone());
        Ok(s)
    }

    /// `log(summary)`: the agent's own optional note. The runtime records
    /// every call itself, so this is never a call row: it waits as a `Note`
    /// and the next recorded call absorbs it as that call's note; a note no
    /// call follows stays a (muted) note row. `tool` is only a fallback text
    /// when `summary` is empty.
    pub fn agent_time_log(
        &mut self,
        caller: &str,
        tool: &str,
        summary: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeSession, AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        let s = self
            .open_agent_time_session_mut()
            .ok_or(AgentTimeRefusal::NoOpenSession)?;
        let text = if summary.trim().is_empty() { tool } else { summary };
        if text.trim().is_empty() {
            return Err(AgentTimeRefusal::Invalid("log needs a 'summary'.".into()));
        }
        Self::push_line(s, AgentTimeLogKind::Note, "note", text, now);
        Ok(s.clone())
    }

    /// Why agent time may not work on to-do `id`, if it may not.
    fn check_workable(
        &self,
        id: &str,
        worker_live: &dyn Fn(&str) -> bool,
    ) -> Result<(), AgentTimeRefusal> {
        let Some(t) = self.todo(id) else {
            return Err(AgentTimeRefusal::Invalid(format!(
                "No to-do '{id}'. Pick one from \"Agent lane: yours to work on\", or omit todo_id \
                 to work in Hobbes-owned space."
            )));
        };
        if t.lane == Lane::Human {
            return Err(AgentTimeRefusal::Invalid(format!(
                "'{}' is the user's own task (human lane), not yours to work on. You may offer to \
                 take it with propose(kind: \"offer\", todo_id: \"{}\", body: why you could help); \
                 only the user's acceptance hands it over.",
                t.title, t.id
            )));
        }
        if !agent_time_eligible(t, worker_live) {
            let why = if t.status.is_closed() {
                "it is closed".to_string()
            } else if t.linked_fleet_session.as_deref().is_some_and(worker_live) {
                "a worker is already running it".to_string()
            } else {
                format!(
                    "it is {}, and agent time takes only queued or stuck ones",
                    t.agent_state.map(|s| s.label()).unwrap_or("not started")
                )
            };
            return Err(AgentTimeRefusal::Invalid(format!(
                "'{}' isn't available to agent time: {why}.",
                t.title
            )));
        }
        Ok(())
    }

    /// `propose(kind, title, body)`: file an output as an open proposal.
    /// For kind `Offer` use [`Self::agent_time_offer`].
    pub fn agent_time_propose(
        &mut self,
        caller: &str,
        kind: ProposalKind,
        title: &str,
        body: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(Proposal, AgentTimeSession), AgentTimeRefusal> {
        self.agent_time_propose_in(caller, kind, title, body, None, today, now)
    }

    /// [`Self::agent_time_propose`] with the lane of a suggested to-do:
    /// `Some(Lane::Agent)` when the agent files it as its own work (accepting
    /// it can then start it); ignored for every kind but `Todo`.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_time_propose_in(
        &mut self,
        caller: &str,
        kind: ProposalKind,
        title: &str,
        body: &str,
        lane: Option<Lane>,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(Proposal, AgentTimeSession), AgentTimeRefusal> {
        if kind == ProposalKind::Offer {
            return Err(AgentTimeRefusal::Invalid(
                "An offer needs the 'todo_id' of the task you offer to take.".into(),
            ));
        }
        let lane = lane.filter(|l| kind == ProposalKind::Todo && *l == Lane::Agent);
        let filed = self.file_proposal(caller, kind, title, body, None, Job::Notice, false, today, now)?;
        if lane.is_none() {
            return Ok(filed);
        }
        let (mut p, s) = filed;
        p.lane = lane;
        if let Some(stored) = self.proposals.iter_mut().find(|x| x.id == p.id) {
            stored.lane = lane;
        }
        Ok((p, s))
    }

    /// `propose(kind: offer, todo_id, body)`: offer to take one of the
    /// person's own to-dos into the agent lane. The to-do must be open and in
    /// the human lane; the offer changes nothing until the person accepts it
    /// ([`Self::apply_accepted_offer`]). Allowed whatever the grant's
    /// proposal kinds: it is a question, not an output.
    pub fn agent_time_offer(
        &mut self,
        caller: &str,
        todo_id: &str,
        pitch: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(Proposal, AgentTimeSession), AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        let id = todo_id.trim();
        let t = self.todo(id).ok_or_else(|| {
            AgentTimeRefusal::Invalid(format!("No to-do '{id}' to offer to take."))
        })?;
        if t.status.is_closed() {
            return Err(AgentTimeRefusal::Invalid(format!(
                "'{}' is already closed; there is nothing to take.",
                t.title
            )));
        }
        if t.lane == Lane::Agent {
            return Err(AgentTimeRefusal::Invalid(format!(
                "'{}' is already in the agent lane. If it is queued or stuck, start on it with \
                 start(todo_id) instead of offering.",
                t.title
            )));
        }
        let (title, id) = (t.title.clone(), t.id.clone());
        self.file_proposal(caller, ProposalKind::Offer, &title, pitch, Some(id), Job::Notice, false, today, now)
    }

    #[allow(clippy::too_many_arguments)]
    fn file_proposal(
        &mut self,
        caller: &str,
        kind: ProposalKind,
        title: &str,
        body: &str,
        todo_id: Option<String>,
        job: Job,
        correction: bool,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(Proposal, AgentTimeSession), AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        let allowed = &self.agent_time_grant.scope.proposal_kinds;
        if kind != ProposalKind::Offer && !allowed.is_empty() && !allowed.contains(&kind) {
            return Err(AgentTimeRefusal::Invalid(format!(
                "The grant's scope does not include '{}' proposals.",
                kind.as_str()
            )));
        }
        let title = title.trim();
        if title.is_empty() {
            return Err(AgentTimeRefusal::Invalid("A proposal needs a 'title'.".into()));
        }
        let session_id = self
            .open_agent_time_session()
            .map(|s| s.id.clone())
            .ok_or(AgentTimeRefusal::NoOpenSession)?;
        let proposal = Proposal {
            id: short_id("pr"),
            kind,
            title: clip(title, 200),
            body: body.trim().to_string(),
            status: ProposalStatus::Open,
            created_at: now,
            resolved_at: None,
            session_id,
            todo_id,
            lane: None,
            revisions: Vec::new(),
            removed: false,
            job,
            correction: correction && job == Job::Tend,
            notes: Vec::new(),
        };
        let s = self
            .open_agent_time_session_mut()
            .expect("checked open above");
        s.outputs.push(proposal.id.clone());
        if job.is_graded() {
            let flag = if proposal.correction { " · correction" } else { "" };
            Self::push_log(
                s,
                "HOBBES_AGENT_TIME.deliver",
                &format!("{} · {} · {}{flag}", job.as_str(), kind.as_str(), proposal.title),
                now,
            );
        } else {
            Self::push_log(
                s,
                "HOBBES_AGENT_TIME.propose",
                &format!("{} · {}", kind.as_str(), proposal.title),
                now,
            );
        }
        let session = s.clone();
        self.proposals.push(proposal.clone());
        Ok((proposal, session))
    }

    /// `end(summary)`: close the session. Allowed even when the budget has
    /// just run out (closing is always honest); still requires the lease.
    pub fn agent_time_end(
        &mut self,
        caller: &str,
        summary: &str,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeSession, AgentTimeRefusal> {
        if !self.is_agent_time_session(caller) {
            return Err(AgentTimeRefusal::NotAgentTimeSession);
        }
        {
            let s = self
                .open_agent_time_session_mut()
                .ok_or(AgentTimeRefusal::NoOpenSession)?;
            Self::push_log(s, "HOBBES_AGENT_TIME.end", summary, now);
        }
        let summary = Some(summary.trim().to_string()).filter(|s| !s.is_empty());
        self.end_open_agent_time(now, AgentTimeEndReason::Completed, summary)
            .ok_or(AgentTimeRefusal::NoOpenSession)
    }

    /// **Runtime gate hook.** Record a tool call made during agent time,
    /// with the scope verdict the gate reached, so the log is complete
    /// without relying on the model. Counts in/out-of-scope evidence on the
    /// trust ledger (`NeedsApproval` and `Deny` are out-of-scope attempts).
    /// Returns the session to persist; persist the ledger too.
    pub fn record_agent_time_tool_call(
        &mut self,
        chat_session_id: &str,
        tool: &str,
        summary: &str,
        verdict: ScopeVerdict,
        now: DateTime<Utc>,
    ) -> Option<AgentTimeSession> {
        if !self.is_agent_time_session(chat_session_id) {
            return None;
        }
        let s = self.open_agent_time_session_mut()?;
        let tag = match verdict {
            ScopeVerdict::Allow => "",
            ScopeVerdict::NeedsApproval => " [held for approval]",
            ScopeVerdict::Deny => " [denied: out of scope]",
        };
        Self::push_log(s, tool, &format!("{}{}", summary, tag), now);
        if verdict != ScopeVerdict::Allow {
            s.out_of_scope_attempts = s.out_of_scope_attempts.saturating_add(1);
        }
        let session = s.clone();
        let l = &mut self.trust_ledger;
        match verdict {
            ScopeVerdict::Allow => l.in_scope_attempts = l.in_scope_attempts.saturating_add(1),
            _ => l.out_of_scope_attempts = l.out_of_scope_attempts.saturating_add(1),
        }
        Some(session)
    }

    /// **Runtime cost hook.** Add spend (USD) to the open session.
    pub fn add_agent_time_cost(&mut self, usd: f64) -> Option<AgentTimeSession> {
        let s = self.open_agent_time_session_mut()?;
        s.cost_usd += usd.max(0.0);
        Some(s.clone())
    }

    /// **Runtime note.** Append a line to the open session run in
    /// `chat_session_id`, without touching the trust ledger (for events the
    /// gate already counted, like a held call the user then approved, or the
    /// runtime closing a session). Returns the session to persist.
    pub fn note_agent_time(
        &mut self,
        chat_session_id: &str,
        tool: &str,
        summary: &str,
        now: DateTime<Utc>,
    ) -> Option<AgentTimeSession> {
        let s = self
            .agent_time_sessions
            .iter_mut()
            .find(|s| s.is_open() && s.chat_session_id.as_deref() == Some(chat_session_id))?;
        Self::push_line(s, AgentTimeLogKind::Event, tool, summary, now);
        Some(s.clone())
    }

    /// **Runtime cost hook, keyed by chat.** Add spend (USD) to the open
    /// session run in `chat_session_id`, or failing that to the latest
    /// session that chat ran (the closing reply after `end` still costs
    /// something and belongs on that session's record).
    pub fn add_agent_time_cost_for_chat(
        &mut self,
        chat_session_id: &str,
        usd: f64,
    ) -> Option<AgentTimeSession> {
        let in_chat = |s: &AgentTimeSession| s.chat_session_id.as_deref() == Some(chat_session_id);
        // While a lease on this chat is held, only a session started under
        // that lease takes the fallback: the chat is reused across the day's
        // runs, and a new run's spend before its `start` must stay pending,
        // not land on the previous run's record.
        let lease_opened = self
            .agent_time_lease
            .as_ref()
            .filter(|l| l.chat_session_id == chat_session_id)
            .map(|l| l.opened_at);
        let idx = self
            .agent_time_sessions
            .iter()
            .position(|s| s.is_open() && in_chat(s))
            .or_else(|| {
                self.agent_time_sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| in_chat(s))
                    .filter(|(_, s)| lease_opened.is_none_or(|at| s.started_at >= at))
                    .max_by_key(|(_, s)| s.started_at)
                    .map(|(i, _)| i)
            })?;
        let s = &mut self.agent_time_sessions[idx];
        s.cost_usd += usd.max(0.0);
        Some(s.clone())
    }

    /// **UI action** (never a tool): accept, accept-after-edit, or dismiss a
    /// proposal — or, for a graded result, Keep (`Accepted`), Needs work
    /// (`NeedsWork`) or Discard (`Dismissed`). Only open (ungraded) rows
    /// move; `NeedsWork` is refused for Notice proposals. The trust ledger
    /// counts the outcome (proposal counters for Notice, result counters for
    /// graded jobs). `edited` replaces the title/body (turns `Accepted` into
    /// `Edited`). Returns the proposal to persist; persist the ledger too.
    /// Turning an accepted `Todo` proposal into a to-do is the UI's call
    /// ([`proposal_to_todo`]); to-do effects of grading are
    /// [`Self::grade_result`]'s.
    pub fn resolve_proposal(
        &mut self,
        id: &str,
        status: ProposalStatus,
        edited: Option<(String, String)>,
        now: DateTime<Utc>,
    ) -> Option<Proposal> {
        let status = if edited.is_some() && status == ProposalStatus::Accepted {
            ProposalStatus::Edited
        } else {
            status
        };
        if status == ProposalStatus::Open {
            return None;
        }
        let p = self
            .proposals
            .iter_mut()
            .find(|p| p.id == id && p.status == ProposalStatus::Open)?;
        if status == ProposalStatus::NeedsWork && !p.job.is_graded() {
            return None;
        }
        if let Some((title, body)) = edited {
            if status != ProposalStatus::Dismissed {
                if !title.trim().is_empty() {
                    p.title = title.trim().to_string();
                }
                p.body = body;
            }
        }
        p.status = status;
        p.resolved_at = Some(now);
        let out = p.clone();
        ledger_count(&mut self.trust_ledger, out.job, status, true);
        Some(out)
    }

    /// **UI action**: grade a delivered result (Prepare / Finish / Tend):
    /// `Accepted` = Keep, `NeedsWork` = Needs work, `Dismissed` = Discard.
    /// Moves the ledger's result counters ([`Self::resolve_proposal`]). A
    /// Finish result linked to an open agent-lane to-do moves it too: Keep
    /// completes it (`AgentState::Done`); Needs work or Discard puts it back
    /// in the queue (`Queued`) for the next window, the person's note
    /// ([`Self::add_result_note`]) telling the agent why. Returns `None` for
    /// a Notice proposal, an already graded result, or `Open`. Persist the
    /// proposal, the to-do and the ledger.
    pub fn grade_result(
        &mut self,
        id: &str,
        grade: ProposalStatus,
        now: DateTime<Utc>,
    ) -> Option<GradedResult> {
        let graded = self
            .proposals
            .iter()
            .any(|p| p.id == id && p.status == ProposalStatus::Open && p.job.is_graded());
        if !graded || grade == ProposalStatus::Open {
            return None;
        }
        let grade = if grade == ProposalStatus::Edited { ProposalStatus::Accepted } else { grade };
        let p = self.resolve_proposal(id, grade, None, now)?;
        let todo = if p.job == Job::Finish {
            p.todo_id.as_deref().and_then(|tid| self.finish_graded(tid, grade, now))
        } else {
            None
        };
        Some(GradedResult { proposal: p, todo })
    }

    /// The to-do side of grading a Finish result. `None` when nothing moved.
    fn finish_graded(&mut self, todo_id: &str, grade: ProposalStatus, now: DateTime<Utc>) -> Option<Todo> {
        let t = self.todo(todo_id)?;
        if t.lane != Lane::Agent || t.status.is_closed() {
            return None;
        }
        match grade {
            ProposalStatus::Accepted | ProposalStatus::Edited => {
                self.complete_todo(todo_id, now);
            }
            _ => {
                let t = self.todo_mut(todo_id)?;
                t.agent_state = Some(AgentState::Queued);
                t.updated_at = now;
            }
        }
        self.todo(todo_id).cloned()
    }

    /// **UI action**, after [`Self::resolve_proposal`] accepted an `Offer`:
    /// hand the to-do over — it moves to the agent lane as `Queued`
    /// (`Todo::set_lane`). Dispatching it still happens the normal way.
    /// Only an accepted (or edited-then-accepted) offer of an open,
    /// human-lane to-do moves anything. Returns the to-do to persist.
    pub fn apply_accepted_offer(&mut self, p: &Proposal, now: DateTime<Utc>) -> Option<Todo> {
        if p.kind != ProposalKind::Offer
            || !matches!(p.status, ProposalStatus::Accepted | ProposalStatus::Edited)
        {
            return None;
        }
        let t = self.todo_mut(p.todo_id.as_deref()?)?;
        if t.status.is_closed() || t.lane == Lane::Agent {
            return None;
        }
        t.set_lane(Lane::Agent);
        t.agent_state = Some(AgentState::Queued);
        t.updated_at = now;
        Some(t.clone())
    }

    /// **UI action** (Trust ledger → Review): change a resolved proposal's
    /// verdict (or a graded result's grade: `Accepted` = Keep, `NeedsWork`,
    /// `Dismissed` = Discard) after the fact, or take it off the ledger
    /// (`to: None`). The
    /// ledger counts move with it and the change is kept on the proposal as
    /// a [`ProposalRevision`] (with the person's note) so the agent sees it.
    /// Changes the record only: a to-do an accepted proposal created or
    /// handed over stays where it is. Persist the proposal and the ledger
    /// afterwards.
    pub fn revise_proposal(
        &mut self,
        id: &str,
        to: Option<ProposalStatus>,
        note: &str,
        now: DateTime<Utc>,
    ) -> Result<Proposal, String> {
        if to == Some(ProposalStatus::Open) {
            return Err("a proposal can't be reopened".into());
        }
        // Re-accepting counts as plain accepted, never as edited again.
        let to = to.map(|t| if t == ProposalStatus::Edited { ProposalStatus::Accepted } else { t });
        let p = self
            .proposals
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("no such proposal")?;
        if p.status == ProposalStatus::Open {
            return Err(if p.job.is_graded() {
                "not graded yet; grade it first".into()
            } else {
                "still open; accept or dismiss it first".into()
            });
        }
        if p.removed {
            return Err("already removed from the ledger".into());
        }
        if to == Some(ProposalStatus::NeedsWork) && !p.job.is_graded() {
            return Err("'needs work' is only for delivered results, not proposals".into());
        }
        let from = p.status;
        if to.is_some_and(|t| verdict_bucket(t) == verdict_bucket(from)) {
            return Err("that is already the verdict".into());
        }
        let job = p.job;
        let l = &mut self.trust_ledger;
        ledger_count(l, job, from, false);
        match to {
            Some(t) => {
                ledger_count(l, job, t, true);
                p.status = t;
            }
            None => p.removed = true,
        }
        p.revisions.push(ProposalRevision { at: now, from, to, note: note.trim().to_string() });
        Ok(p.clone())
    }

    /// Record a promise the agent made ("ready by Friday") as kept or broken.
    /// Evidence only. No automatic source exists yet — the UI (or a later
    /// promise tracker) calls this. Persist the ledger afterwards.
    pub fn record_agent_promise(&mut self, kept: bool) {
        let l = &mut self.trust_ledger;
        if kept {
            l.promises_kept = l.promises_kept.saturating_add(1);
        } else {
            l.promises_broken = l.promises_broken.saturating_add(1);
        }
    }

    /// Close every open agent-time session as `recovered` (app restart).
    pub fn recover_open_agent_time(&mut self, now: DateTime<Utc>) -> Vec<AgentTimeSession> {
        self.agent_time_lease = None;
        let mut out = Vec::new();
        for s in self.agent_time_sessions.iter_mut().filter(|s| s.is_open()) {
            s.ended_at = Some(now.max(s.started_at));
            s.end_reason = Some(AgentTimeEndReason::Recovered);
            out.push(s.clone());
        }
        out
    }

    /// `deliver(job, kind?, title, body, todo_id?, correction?)`: file
    /// finished work as an ungraded result (status `Open`) for the person to
    /// grade. Rules: `Notice` is refused (that is `propose`); `correction`
    /// only on `Tend`; a body is required; `Prepare` needs the `todo_id` of
    /// an open human-lane to-do the person flagged "Prep this"; `Finish`
    /// may name an open agent-lane to-do, which moves to `NeedsYou`
    /// (awaiting review; Keep completes it — [`Self::grade_result`]); a
    /// human-lane to-do is never Finish work. A correction counts on the
    /// trust ledger (`corrections_delivered`). Persist the result, the
    /// session, the to-do and the ledger.
    pub fn agent_time_deliver(
        &mut self,
        caller: &str,
        d: Delivery<'_>,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<DeliveredResult, AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        if d.job == Job::Notice {
            return Err(AgentTimeRefusal::Invalid(
                "'notice' is not delivered: file a proposal for the user's stuff with \
                 propose(kind, title, body) instead."
                    .into(),
            ));
        }
        if d.correction && d.job != Job::Tend {
            return Err(AgentTimeRefusal::Invalid(
                "'correction' is only for job \"tend\" (correcting something you told the user)."
                    .into(),
            ));
        }
        if d.body.trim().is_empty() {
            return Err(AgentTimeRefusal::Invalid(
                "deliver needs a 'body': the finished work itself, ready for the user to read."
                    .into(),
            ));
        }
        if d.kind == Some(ProposalKind::Offer) {
            return Err(AgentTimeRefusal::Invalid(
                "An offer is a proposal: use propose(kind: \"offer\", todo_id, body).".into(),
            ));
        }
        let todo_id = d.todo_id.map(str::trim).filter(|t| !t.is_empty());
        let linked = match d.job {
            Job::Prepare => {
                let Some(id) = todo_id else {
                    return Err(AgentTimeRefusal::Invalid(
                        "A \"prepare\" result needs the 'todo_id' of the task the user flagged \
                         \"Prep this\"."
                            .into(),
                    ));
                };
                let t = self.todo(id).ok_or_else(|| {
                    AgentTimeRefusal::Invalid(format!("No to-do '{id}' to prepare for."))
                })?;
                if t.lane != Lane::Human || t.status.is_closed() || !t.prep_requested {
                    return Err(AgentTimeRefusal::Invalid(format!(
                        "'{}' isn't on the prep list: you may only prepare for the user's open \
                         tasks they flagged \"Prep this\". You may offer help with \
                         propose(kind: \"offer\", todo_id) instead.",
                        t.title
                    )));
                }
                Some(t.id.clone())
            }
            Job::Finish => match todo_id {
                None => None,
                Some(id) => {
                    let t = self.todo(id).ok_or_else(|| {
                        AgentTimeRefusal::Invalid(format!(
                            "No to-do '{id}'. Name an agent-lane to-do, or omit todo_id."
                        ))
                    })?;
                    if t.lane == Lane::Human {
                        return Err(AgentTimeRefusal::Invalid(format!(
                            "'{}' is the user's own task (human lane), not yours to finish. You \
                             may offer to take it with propose(kind: \"offer\", todo_id: \"{}\").",
                            t.title, t.id
                        )));
                    }
                    if t.status.is_closed() {
                        return Err(AgentTimeRefusal::Invalid(format!(
                            "'{}' is already closed.",
                            t.title
                        )));
                    }
                    Some(t.id.clone())
                }
            },
            // Tend works in Hobbes-owned space: no to-do.
            Job::Tend | Job::Notice => None,
        };
        let kind = d.kind.unwrap_or(match d.job {
            Job::Finish => ProposalKind::Other,
            _ => ProposalKind::Note,
        });
        let (result, session) = self.file_proposal(
            caller,
            kind,
            d.title,
            d.body,
            linked.clone(),
            d.job,
            d.correction,
            today,
            now,
        )?;
        if result.correction {
            let l = &mut self.trust_ledger;
            l.corrections_delivered = l.corrections_delivered.saturating_add(1);
        }
        let todo = match (d.job, linked) {
            (Job::Finish, Some(id)) => self.todo_mut(&id).map(|t| {
                t.agent_state = Some(AgentState::NeedsYou);
                t.updated_at = now;
                t.clone()
            }),
            _ => None,
        };
        Ok(DeliveredResult { result, session, todo })
    }

    /// `ask(question, options)`: save a question for the person instead of
    /// stopping the run. Returns at once so the run continues; the person
    /// answers in the review and the answer reaches the next window. At most
    /// [`MAX_OPEN_QUESTIONS_PER_RUN`] unanswered per run. Persist the
    /// question and the session.
    pub fn agent_time_ask(
        &mut self,
        caller: &str,
        question: &str,
        options: &[String],
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(AgentQuestion, AgentTimeSession), AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        let session_id = self
            .open_agent_time_session()
            .map(|s| s.id.clone())
            .ok_or(AgentTimeRefusal::NoOpenSession)?;
        let question = question.trim();
        if question.is_empty() {
            return Err(AgentTimeRefusal::Invalid("ask needs a 'question'.".into()));
        }
        let open_here = self
            .agent_questions
            .iter()
            .filter(|q| q.session_id == session_id && q.is_open())
            .count();
        if open_here >= MAX_OPEN_QUESTIONS_PER_RUN {
            return Err(AgentTimeRefusal::Invalid(format!(
                "You already saved {MAX_OPEN_QUESTIONS_PER_RUN} questions this run. Carry on with \
                 what you can do without the answers, or end the run."
            )));
        }
        let mut opts: Vec<String> = Vec::new();
        for o in options.iter().map(|o| clip(o, 120)).filter(|o| !o.is_empty()) {
            if opts.len() < MAX_QUESTION_OPTIONS && !opts.contains(&o) {
                opts.push(o);
            }
        }
        let q = AgentQuestion {
            id: short_id("aq"),
            session_id,
            question: clip(question, 500),
            options: opts,
            asked_at: now,
            answer: None,
            answered_at: None,
            consumed: false,
        };
        let s = self
            .open_agent_time_session_mut()
            .expect("checked open above");
        Self::push_log(s, "HOBBES_AGENT_TIME.ask", &q.question, now);
        let session = s.clone();
        self.agent_questions.push(q.clone());
        Ok((q, session))
    }

    /// **UI action**: answer a saved question (or change the answer before
    /// it reached agent time). Refused once the answer was delivered
    /// (`consumed`). Persist the returned question.
    pub fn answer_question(
        &mut self,
        id: &str,
        answer: &str,
        now: DateTime<Utc>,
    ) -> Result<AgentQuestion, String> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err("an answer can't be empty".into());
        }
        let q = self
            .agent_questions
            .iter_mut()
            .find(|q| q.id == id)
            .ok_or("no such question")?;
        if q.consumed {
            return Err("already delivered to agent time".into());
        }
        q.answer = Some(clip(answer, MAX_NOTE_CHARS));
        q.answered_at = Some(now);
        Ok(q.clone())
    }

    /// Questions still waiting for the person's answer, oldest first.
    pub fn open_questions(&self) -> Vec<&AgentQuestion> {
        self.agent_questions.iter().filter(|q| q.is_open()).collect()
    }

    /// Answered questions whose answer hasn't reached agent time yet,
    /// oldest first (the next window's opening context).
    pub fn pending_answers(&self) -> Vec<&AgentQuestion> {
        self.agent_questions
            .iter()
            .filter(|q| q.answer.is_some() && !q.consumed)
            .collect()
    }

    /// **Runtime**, once the opening message carrying them is queued: mark
    /// these answers delivered so they reach agent time exactly once.
    /// Returns the questions to persist.
    pub fn consume_answers(&mut self, ids: &[String]) -> Vec<AgentQuestion> {
        self.agent_questions
            .iter_mut()
            .filter(|q| q.answer.is_some() && !q.consumed && ids.contains(&q.id))
            .map(|q| {
                q.consumed = true;
                q.clone()
            })
            .collect()
    }

    /// **UI action**: comment on a result (or proposal). Kept on it and shown
    /// to the agent in the next window's opening context. Persist the
    /// returned proposal.
    pub fn add_result_note(
        &mut self,
        id: &str,
        text: &str,
        now: DateTime<Utc>,
    ) -> Result<Proposal, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a comment can't be empty".into());
        }
        let p = self
            .proposals
            .iter_mut()
            .find(|p| p.id == id && !p.removed)
            .ok_or("no such result")?;
        p.notes.push(ResultNote { at: now, text: clip(text, MAX_NOTE_CHARS) });
        Ok(p.clone())
    }

    /// **UI action only** ("Prep this"): flag or unflag one of the person's
    /// own to-dos for agent time to prepare. Flagging needs an open
    /// human-lane to-do; unflagging always works. The model's to-do tools
    /// refuse this field. Persist the returned to-do.
    pub fn set_prep_requested(
        &mut self,
        todo_id: &str,
        on: bool,
        now: DateTime<Utc>,
    ) -> Result<Todo, String> {
        let t = self.todo_mut(todo_id).ok_or("no such to-do")?;
        if on && (t.lane != Lane::Human || t.status.is_closed()) {
            return Err("only your own open to-dos can be flagged \"Prep this\"".into());
        }
        if t.prep_requested != on {
            t.prep_requested = on;
            t.updated_at = now;
        }
        Ok(t.clone())
    }

    /// **UI action**: mark these agent-time runs as read in the review
    /// ([`AgentTimeReview::session_ids`]). Runs still open are skipped.
    /// Returns the sessions to persist.
    pub fn mark_reviewed(&mut self, session_ids: &[String], now: DateTime<Utc>) -> Vec<AgentTimeSession> {
        self.agent_time_sessions
            .iter_mut()
            .filter(|s| !s.is_open() && s.reviewed_at.is_none() && session_ids.contains(&s.id))
            .map(|s| {
                s.reviewed_at = Some(now);
                s.clone()
            })
            .collect()
    }

    /// Whether the runtime should start today's scheduled window now
    /// ([`scheduled_start_due`] over this planner).
    pub fn scheduled_start_due(
        &self,
        today: NaiveDate,
        local_time: NaiveTime,
        now: DateTime<Utc>,
        attempted_today: bool,
    ) -> bool {
        scheduled_start_due(
            &self.agent_time_grant,
            &self.agent_time_sessions,
            self.agent_time_lease.is_some(),
            today,
            local_time,
            now,
            attempted_today,
        )
    }

    /// Open proposals, newest first (the Agent time panel's list).
    pub fn open_proposals(&self) -> Vec<&Proposal> {
        let mut v: Vec<&Proposal> = self
            .proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Open)
            .collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }
}

// ── What agent time may work on ─────────────────────────────────────────────

/// Whether agent time may work on `t`: an open agent-lane to-do that is
/// `Queued`, `Stuck` or `Review` with no live worker attached (`worker_live` says
/// whether its linked fleet session is running now). Human-lane to-dos are
/// the person's: never eligible (agent time may only offer to take them).
pub fn agent_time_eligible(t: &Todo, worker_live: &dyn Fn(&str) -> bool) -> bool {
    t.lane == Lane::Agent
        && !t.status.is_closed()
        // Review = a run finished with an open item: as retryable as Stuck
        // (before the Review state existed those runs were Stuck).
        && matches!(
            t.agent_state,
            Some(AgentState::Queued) | Some(AgentState::Stuck) | Some(AgentState::Review)
        )
        && !t.linked_fleet_session.as_deref().is_some_and(worker_live)
}

/// Most of the person's own to-dos listed in the agent-time context.
pub const MAX_HUMAN_TASKS_LISTED: usize = 10;
/// Most prep-flagged to-dos listed ("Prepare").
pub const MAX_PREP_LISTED: usize = 10;
/// Most comments on results listed in the agent-time context.
pub const MAX_NOTES_LISTED: usize = 6;
/// Most still-unanswered questions listed (so the agent doesn't re-ask).
pub const MAX_WAITING_LISTED: usize = 8;

/// What agent time is shown at the start of a window: what is its to work
/// on (prep list, agent lane), what is the person's, and what the person
/// said since (answers, revisions, comments).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentTimeTaskLists {
    /// Eligible agent-lane to-dos: (id, title, outcome).
    pub agent: Vec<(String, String, Option<String>)>,
    /// The person's open to-dos they flagged "Prep this", soonest first:
    /// (id, title, when — the timeline block / scheduled day / deadline).
    pub prepare: Vec<(String, String, Option<String>)>,
    /// The person's other open to-dos, overdue and today first: (id, title).
    pub human: Vec<(String, String)>,
    /// Open human to-dos beyond [`MAX_HUMAN_TASKS_LISTED`].
    pub human_more: usize,
    /// The person's after-the-fact changes to recent proposals and grades,
    /// newest first (title, line), so the agent learns from them.
    pub revisions: Vec<(String, String)>,
    /// Answers to saved questions not yet delivered, oldest first:
    /// (question id, question, answer). The runtime marks them consumed
    /// once the opening message carrying them is queued.
    pub answers: Vec<(String, String, String)>,
    /// Saved questions still waiting for an answer, oldest first.
    pub waiting: Vec<String>,
    /// The person's comments on results, newest first: (title, comment).
    pub notes: Vec<(String, String)>,
}

/// Overdue first, then today, then the rest (each by list order).
fn human_rank(t: &Todo, today: NaiveDate) -> u8 {
    let overdue = t.deadline.is_some_and(|d| d < today) || t.scheduled_for.is_some_and(|d| d < today);
    let due_today = t.deadline == Some(today) || t.scheduled_for == Some(today);
    if overdue {
        0
    } else if due_today {
        1
    } else {
        2
    }
}

/// When a prep-flagged to-do happens, as far as the planner knows: its next
/// timeline block (today or later), else its scheduled day and / or
/// deadline. Also the sort key (earliest first; unknown last).
fn prep_when(planner: &PlannerState, t: &Todo, today: NaiveDate) -> (Option<NaiveDate>, Option<String>) {
    let block = planner
        .blocks
        .iter()
        .filter(|b| b.todo_id.as_deref() == Some(t.id.as_str()))
        .map(|b| b.start.with_timezone(&chrono::Local))
        .filter(|s| s.date_naive() >= today)
        .min();
    if let Some(start) = block {
        return (Some(start.date_naive()), Some(start.format("%a %b %-d %H:%M").to_string()));
    }
    let mut parts = Vec::new();
    if let Some(d) = t.scheduled_for {
        parts.push(format!("scheduled {}", d.format("%a %b %-d")));
    }
    if let Some(d) = t.deadline {
        parts.push(format!("due {}", d.format("%a %b %-d")));
    }
    let key = match (t.scheduled_for, t.deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    (key, (!parts.is_empty()).then(|| parts.join(" \u{b7} ")))
}

/// **Pure.** Build the lists from the planner.
pub fn agent_time_task_lists(
    planner: &PlannerState,
    today: NaiveDate,
    worker_live: &dyn Fn(&str) -> bool,
) -> AgentTimeTaskLists {
    let agent = planner
        .todos
        .iter()
        .filter(|t| agent_time_eligible(t, worker_live))
        .map(|t| (t.id.clone(), t.title.clone(), t.outcome.clone().filter(|o| !o.trim().is_empty())))
        .collect();
    let open_human = |t: &&Todo| t.lane == Lane::Human && !t.status.is_closed();
    let mut prep: Vec<(Option<NaiveDate>, &Todo, Option<String>)> = planner
        .todos
        .iter()
        .filter(open_human)
        .filter(|t| t.prep_requested)
        .map(|t| {
            let (key, when) = prep_when(planner, t, today);
            (key, t, when)
        })
        .collect();
    // Soonest first; undated last.
    prep.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.sort_order.partial_cmp(&b.1.sort_order).unwrap_or(std::cmp::Ordering::Equal),
    });
    let prepare = prep
        .into_iter()
        .take(MAX_PREP_LISTED)
        .map(|(_, t, when)| (t.id.clone(), t.title.clone(), when))
        .collect();
    let mut human: Vec<&Todo> = planner
        .todos
        .iter()
        .filter(open_human)
        .filter(|t| !t.prep_requested)
        .collect();
    human.sort_by(|a, b| {
        human_rank(a, today)
            .cmp(&human_rank(b, today))
            .then(a.sort_order.partial_cmp(&b.sort_order).unwrap_or(std::cmp::Ordering::Equal))
    });
    let human_more = human.len().saturating_sub(MAX_HUMAN_TASKS_LISTED);
    let human = human
        .into_iter()
        .take(MAX_HUMAN_TASKS_LISTED)
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();
    let revisions = proposal_revision_lines(planner, MAX_REVISIONS_LISTED);
    let answers = planner
        .pending_answers()
        .into_iter()
        .map(|q| (q.id.clone(), q.question.clone(), q.answer.clone().unwrap_or_default()))
        .collect();
    let waiting = planner
        .open_questions()
        .into_iter()
        .take(MAX_WAITING_LISTED)
        .map(|q| q.question.clone())
        .collect();
    let notes = result_note_lines(planner, MAX_NOTES_LISTED);
    AgentTimeTaskLists { agent, prepare, human, human_more, revisions, answers, waiting, notes }
}

/// **Pure.** The person's comments on results (and proposals), newest
/// first: `("title", "comment")`.
pub fn result_note_lines(planner: &PlannerState, max: usize) -> Vec<(String, String)> {
    let mut notes: Vec<(&Proposal, &ResultNote)> = planner
        .proposals
        .iter()
        .filter(|p| !p.removed)
        .flat_map(|p| p.notes.iter().map(move |n| (p, n)))
        .collect();
    notes.sort_by(|a, b| b.1.at.cmp(&a.1.at));
    notes
        .into_iter()
        .take(max)
        .map(|(p, n)| (p.title.clone(), n.text.clone()))
        .collect()
}

/// Revised proposals listed in the agent-time context.
pub const MAX_REVISIONS_LISTED: usize = 6;

/// **Pure.** One line per revised proposal or result (its latest revision),
/// newest first: `("title", "changed from accepted to dismissed: <note>")`,
/// or for a graded result `"changed from kept to needs work"`.
pub fn proposal_revision_lines(planner: &PlannerState, max: usize) -> Vec<(String, String)> {
    let mut revised: Vec<(&Proposal, &ProposalRevision)> = planner
        .proposals
        .iter()
        .filter_map(|p| p.revisions.last().map(|r| (p, r)))
        .collect();
    revised.sort_by(|a, b| b.1.at.cmp(&a.1.at));
    revised
        .into_iter()
        .take(max)
        .map(|(p, r)| {
            let word = |s: ProposalStatus| s.verdict_word(p.job);
            let what = match r.to {
                Some(to) => format!("changed from {} to {}", word(r.from), word(to)),
                None => format!("was {}, then removed from your trust ledger", word(r.from)),
            };
            let line = if r.note.is_empty() { what } else { format!("{what}: \u{201c}{}\u{201d}", r.note) };
            (p.title.clone(), line)
        })
        .collect()
}

/// **Pure.** The context block for the opening message and the `start`
/// result, in the order agent time should work: answers to its saved
/// questions → Prepare (the flagged tasks only) → Finish (the agent lane) →
/// the owner's other tasks (Notice: offer only) → what the owner said about
/// earlier work. `owner` is how the person is named ("Dustin", or "the
/// user"). The wording draws the boundary: the prep list and the agent lane
/// are agent time's; the owner's other tasks are not, and may only be
/// offered for.
pub fn agent_time_context(lists: &AgentTimeTaskLists, owner: &str) -> String {
    let mut out = String::new();
    if !lists.answers.is_empty() {
        out.push_str("Answers to your saved questions (use them first):\n");
        for (_, q, a) in &lists.answers {
            out.push_str(&format!(
                "- You asked \u{201c}{q}\u{201d}; {owner} answered \u{201c}{a}\u{201d}.\n"
            ));
        }
        out.push('\n');
    }
    if !lists.prepare.is_empty() {
        out.push_str(&format!(
            "Prepare: {owner} asked you to prep these (briefs, drafts, background; deliver with \
             job \"prepare\" and the todo_id). Research their subject; don't act on them:\n"
        ));
        for (id, title, when) in &lists.prepare {
            match when {
                Some(w) => out.push_str(&format!("- [{id}] {title} \u{2014} {w}\n")),
                None => out.push_str(&format!("- [{id}] {title}\n")),
            }
        }
        out.push('\n');
    }
    out.push_str(
        "Agent lane: yours to work on (Finish: start with todo_id to take one, deliver the \
         finished work with job \"finish\")\n",
    );
    if lists.agent.is_empty() {
        out.push_str("- (none queued or stuck right now; work in Hobbes-owned space instead)\n");
    }
    for (id, title, outcome) in &lists.agent {
        match outcome {
            Some(o) => out.push_str(&format!("- [{id}] {title} \u{2014} done when: {o}\n")),
            None => out.push_str(&format!("- [{id}] {title}\n")),
        }
    }
    let other = if lists.prepare.is_empty() { "" } else { " other" };
    out.push_str(&format!(
        "\n{owner}'s{other} tasks: NOT yours. Don't work on them, research them or act on them. \
         You may offer help with propose(kind: \"offer\", todo_id, body: why you could take it); \
         only {owner}'s acceptance hands one over.\n"
    ));
    if lists.human.is_empty() {
        out.push_str("- (none open)\n");
    }
    for (id, title) in &lists.human {
        out.push_str(&format!("- [{id}] {title}\n"));
    }
    if lists.human_more > 0 {
        out.push_str(&format!("- \u{2026}and {} more\n", lists.human_more));
    }
    if !lists.waiting.is_empty() {
        out.push_str(&format!(
            "\nQuestions still waiting for {owner}'s answer (don't ask them again):\n"
        ));
        for q in &lists.waiting {
            out.push_str(&format!("- {q}\n"));
        }
    }
    if !lists.revisions.is_empty() {
        out.push_str(&format!(
            "\n{owner} revisited some of your earlier proposals and results. Take the reasons into \
             account:\n"
        ));
        for (title, line) in &lists.revisions {
            out.push_str(&format!("- {title}: {line}\n"));
        }
    }
    if !lists.notes.is_empty() {
        out.push_str(&format!("\n{owner}'s comments on your results, newest first:\n"));
        for (title, text) in &lists.notes {
            out.push_str(&format!("- On {title}, {owner} said: \u{201c}{text}\u{201d}\n"));
        }
    }
    out
}

// ── Scheduled window ────────────────────────────────────────────────────────

/// **Pure.** Whether today's scheduled agent-time window should start now:
/// the grant is on with a `window_start`, today is granted, local time has
/// reached it, minutes and spend remain, no run holds the lease, no run
/// started today (a run the person paused or stopped counts), and the
/// runtime hasn't already tried today (`attempted_today`: it fired once, or
/// a lease was taken today whose model never called `start`). Never twice a
/// day.
pub fn scheduled_start_due(
    grant: &AgentTimeGrant,
    sessions: &[AgentTimeSession],
    lease_held: bool,
    today: NaiveDate,
    local_time: NaiveTime,
    now: DateTime<Utc>,
    attempted_today: bool,
) -> bool {
    let Some(window) = grant.window_start else {
        return false;
    };
    grant.enabled
        && grant.grants_day(today)
        && local_time >= window
        && !lease_held
        && !attempted_today
        && !sessions.iter().any(|s| started_on(s, today))
        && agent_time_budget(grant, sessions, today, now).refusal().is_none()
}

// ── Review ──────────────────────────────────────────────────────────────────

/// How far back the review looks for unreviewed runs when none was ever
/// reviewed (so the first review isn't the whole history).
pub const REVIEW_LOOKBACK_DAYS: i64 = 7;

/// Counts for the review header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentTimeReviewCounts {
    /// Graded-job results listed (Prepare + Finish + Tend).
    pub results: usize,
    /// Of those, not graded yet.
    pub ungraded: usize,
    pub kept: usize,
    pub needs_work: usize,
    pub discarded: usize,
    /// Tend results flagged as corrections.
    pub corrections: usize,
    /// Notice proposals still open.
    pub proposals_open: usize,
    /// Saved questions still unanswered.
    pub questions_open: usize,
}

/// The one review the person reads after agent time ([`review_for`]).
/// Owned clones, ready to render.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentTimeReview {
    /// The runs this review covers, oldest first. Pass their ids to
    /// [`PlannerState::mark_reviewed`] once read.
    pub sessions: Vec<AgentTimeSession>,
    pub session_ids: Vec<String>,
    /// Results by job, oldest first; Tend lists corrections first.
    pub prepare: Vec<Proposal>,
    pub finish: Vec<Proposal>,
    pub tend: Vec<Proposal>,
    /// Notice proposals, oldest first.
    pub notice: Vec<Proposal>,
    /// Saved questions still waiting for an answer, oldest first.
    pub questions: Vec<AgentQuestion>,
    /// Wall-clock minutes and spend of the covered runs.
    pub minutes_used: u32,
    pub spend_usd: f64,
    pub counts: AgentTimeReviewCounts,
}

impl AgentTimeReview {
    /// Nothing to read: no runs, results, proposals or questions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.prepare.is_empty()
            && self.finish.is_empty()
            && self.tend.is_empty()
            && self.notice.is_empty()
            && self.questions.is_empty()
    }
}

/// **Pure.** The agent-time review.
///
/// Runs covered: with `date`, every closed run started on that local day;
/// without, every closed, unreviewed run started after the latest review
/// (and within [`REVIEW_LOOKBACK_DAYS`] of `now` when none was ever
/// reviewed). A run still open is never covered.
///
/// Items: every result and proposal (not removed) filed by a covered run,
/// plus anything still open (ungraded / undecided) from any run, so nothing
/// waiting on the person drops out of the review. Grouped by job; within
/// Tend, corrections come first. `questions` is every unanswered question.
pub fn review_for(planner: &PlannerState, date: Option<NaiveDate>, now: DateTime<Utc>) -> AgentTimeReview {
    let last_review = planner.agent_time_sessions.iter().filter_map(|s| s.reviewed_at).max();
    let floor = now - chrono::Duration::days(REVIEW_LOOKBACK_DAYS);
    let covered: Vec<&AgentTimeSession> = planner
        .agent_time_sessions
        .iter()
        .filter(|s| !s.is_open())
        .filter(|s| match date {
            Some(d) => started_on(s, d),
            None => {
                s.reviewed_at.is_none()
                    && match last_review {
                        Some(at) => s.started_at > at || s.ended_at.is_some_and(|e| e > at),
                        None => s.started_at >= floor,
                    }
            }
        })
        .collect();
    let ids: Vec<String> = covered.iter().map(|s| s.id.clone()).collect();
    let mut items: Vec<&Proposal> = planner
        .proposals
        .iter()
        .filter(|p| !p.removed)
        .filter(|p| ids.contains(&p.session_id) || p.status == ProposalStatus::Open)
        .collect();
    items.sort_by_key(|p| p.created_at);
    let by_job = |job: Job| -> Vec<Proposal> {
        items.iter().filter(|p| p.job == job).map(|p| (*p).clone()).collect()
    };
    let prepare = by_job(Job::Prepare);
    let finish = by_job(Job::Finish);
    let mut tend = by_job(Job::Tend);
    // Stable: corrections first, each group oldest first.
    tend.sort_by_key(|p| !p.correction);
    let notice = by_job(Job::Notice);
    let questions: Vec<AgentQuestion> = planner.open_questions().into_iter().cloned().collect();

    let mut counts = AgentTimeReviewCounts::default();
    for p in prepare.iter().chain(&finish).chain(&tend) {
        counts.results += 1;
        match p.status {
            ProposalStatus::Open => counts.ungraded += 1,
            ProposalStatus::Accepted | ProposalStatus::Edited => counts.kept += 1,
            ProposalStatus::NeedsWork => counts.needs_work += 1,
            ProposalStatus::Dismissed => counts.discarded += 1,
        }
        if p.correction {
            counts.corrections += 1;
        }
    }
    counts.proposals_open = notice.iter().filter(|p| p.status == ProposalStatus::Open).count();
    counts.questions_open = questions.len();

    let minutes_used = covered.iter().map(|s| s.minutes_used(now)).sum();
    let spend_usd = covered.iter().map(|s| s.cost_usd.max(0.0)).sum();
    AgentTimeReview {
        sessions: covered.into_iter().cloned().collect(),
        session_ids: ids,
        prepare,
        finish,
        tend,
        notice,
        questions,
        minutes_used,
        spend_usd,
        counts,
    }
}

/// An accepted `Todo` proposal as a new to-do. A suggestion for the person
/// lands in the Inbox as a human to-do with the body as notes; one the agent
/// filed as its own work (`Proposal::lane == Some(Agent)`) lands in the agent
/// lane, `Queued`, with the body as its outcome. Pure; the UI upserts and
/// persists it.
pub fn proposal_to_todo(p: &Proposal, sort_order: f64) -> super::model::Todo {
    let mut t = super::model::Todo::new(p.title.clone(), sort_order);
    if p.lane == Some(Lane::Agent) {
        t.set_lane(Lane::Agent);
        t.agent_state = Some(AgentState::Queued);
        t.outcome = Some(p.body.trim().to_string()).filter(|b| !b.is_empty());
    } else {
        t.notes = p.body.clone();
    }
    t
}

/// Whether accepting `p` can start work: an offer (the to-do moves to the
/// agent lane) or a suggested to-do the agent filed as its own work. Notes,
/// research, skills and plain suggestions are saved and start nothing.
pub fn proposal_starts_work(p: &Proposal) -> bool {
    if p.job.is_graded() {
        // A delivered result is finished work: grading it starts nothing.
        return false;
    }
    match p.kind {
        ProposalKind::Offer => true,
        ProposalKind::Todo => p.lane == Some(Lane::Agent),
        _ => false,
    }
}

/// Where an accepted to-do goes. Accepting never opens a new chat tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptStartRoute {
    /// An agent-time run is active with budget left: hand the to-do to the
    /// run in this chat.
    HandToRun { chat_id: String },
    /// Otherwise: it stays `Queued` in the agent lane for the next
    /// agent-time window.
    NextWindow,
}

/// **Pure.** Route an accepted to-do: the running agent-time chat when a
/// lease is held and today's budget has room, else the next window.
pub fn accept_start_route(planner: &PlannerState, today: NaiveDate, now: DateTime<Utc>) -> AcceptStartRoute {
    match &planner.agent_time_lease {
        Some(l) if planner.agent_time_budget(today, now).refusal().is_none() => {
            AcceptStartRoute::HandToRun { chat_id: l.chat_session_id.clone() }
        }
        _ => AcceptStartRoute::NextWindow,
    }
}

/// What accepting a proposal changed, for the UI to persist and show.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedProposal {
    pub proposal: Proposal,
    /// The to-do created (a `Todo` proposal) or handed over (an `Offer`).
    pub todo: Option<Todo>,
}

impl PlannerState {
    /// **UI action.** Accept (or accept-after-edit) a proposal and apply it:
    /// an offer hands the person's to-do to the agent lane (`Queued`); a
    /// suggested to-do is created ([`proposal_to_todo`]); every other kind is
    /// only marked accepted. Starting work is a separate step ("Accept &
    /// start"). Returns what changed; persist the proposal, the to-do and the
    /// trust ledger.
    pub fn accept_proposal(
        &mut self,
        id: &str,
        edited: Option<(String, String)>,
        now: DateTime<Utc>,
    ) -> Option<AcceptedProposal> {
        // A delivered result is graded, not accepted: "accept" = Keep.
        if self.proposals.iter().any(|p| p.id == id && p.job.is_graded()) {
            let g = self.grade_result(id, ProposalStatus::Accepted, now)?;
            return Some(AcceptedProposal { proposal: g.proposal, todo: g.todo });
        }
        let p = self.resolve_proposal(id, ProposalStatus::Accepted, edited, now)?;
        let todo = match p.kind {
            ProposalKind::Offer => self.apply_accepted_offer(&p, now),
            ProposalKind::Todo => {
                let t = proposal_to_todo(&p, self.next_sort_order());
                self.upsert_todo(t.clone());
                Some(t)
            }
            _ => None,
        };
        Some(AcceptedProposal { proposal: p, todo })
    }

    /// **UI action** ("Accept & start" while agent time runs): hand to-do
    /// `todo_id` to the agent-time run in `chat_id`. When the run has
    /// declared its intent and has no to-do yet, the to-do becomes its
    /// session's `todo_id`; either way the hand-off is an `Event` on the log.
    /// Returns the message to queue into the chat and the session to persist
    /// (`None` before `start`: the message tells the model to start on it).
    pub fn hand_todo_to_run(
        &mut self,
        chat_id: &str,
        todo_id: &str,
        owner: &str,
        offer: bool,
        now: DateTime<Utc>,
    ) -> Option<(String, Option<AgentTimeSession>)> {
        if !self.is_agent_time_session(chat_id) {
            return None;
        }
        let t = self.todo(todo_id)?;
        let (title, id) = (t.title.clone(), t.id.clone());
        let done_when = t
            .outcome
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(|o| format!(" Done when: {o}."))
            .unwrap_or_default();
        let what = if offer { "your offer" } else { "your proposed to-do" };
        let mut msg = format!("{owner} accepted {what}: work on {title} (todo {id}).{done_when}");
        let session = match self
            .agent_time_sessions
            .iter_mut()
            .find(|s| s.is_open() && s.chat_session_id.as_deref() == Some(chat_id))
        {
            Some(s) => {
                if s.todo_id.is_none() {
                    s.todo_id = Some(id.clone());
                    msg.push_str(" It is now this run's to-do; carry on.");
                } else {
                    msg.push_str(
                        " Take it up when you finish your current to-do, or end this session and \
                         start a new one with its todo_id.",
                    );
                }
                Self::push_line(
                    s,
                    AgentTimeLogKind::Event,
                    "runtime",
                    &format!("{owner} accepted and handed over: {title}"),
                    now,
                );
                Some(s.clone())
            }
            None => {
                msg.push_str(&format!(
                    " Declare your intent with HOBBES_AGENT_TIME start (todo_id: {id}) and work on it."
                ));
                None
            }
        };
        Some((msg, session))
    }
}

// ── Scope gate ──────────────────────────────────────────────────────────────

/// The runtime gate's verdict for one tool call during agent time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeVerdict {
    /// Reads of shared data, and writes to Hobbes-owned space (proposals,
    /// the session scratchpad).
    Allow,
    /// An outside effect (sending, publishing, purchasing, changing a repo,
    /// running a command): goes through the normal approval gate.
    NeedsApproval,
    /// Never during agent time: the person's own plan and lists, other
    /// workers, timers, and anything destructive.
    Deny,
}

/// Name verbs that mark a read. Matched as whole tokens of the tool name.
const READ_TOKENS: &[&str] = &[
    "get", "list", "search", "read", "fetch", "find", "query", "view", "show", "describe",
    "lookup", "retrieve", "status", "info", "count", "browse", "inspect", "preview", "cat",
    "head", "tail", "grep", "ls", "stat", "history", "logs", "log", "diff", "glob", "peek",
    "summary", "summarize",
];

/// Name verbs that mark an outside effect (normal approval gate).
const WRITE_TOKENS: &[&str] = &[
    "send", "post", "create", "update", "edit", "write", "set", "add", "insert", "upsert",
    "put", "patch", "move", "rename", "publish", "deploy", "push", "commit", "merge", "run",
    "exec", "execute", "call", "reply", "forward", "share", "invite", "schedule", "book",
    "buy", "purchase", "pay", "order", "transfer", "upload", "submit", "approve", "comment",
    "star", "fork", "archive", "label", "mark", "assign", "close", "start", "stop",
    "install", "apply", "sync", "save", "replace", "append", "mkdir", "touch", "chmod",
    "invoke", "trigger", "dispatch", "notify", "email", "message", "tweet", "react",
    // Write files outside Hobbes-owned space, so they ask too.
    "download", "export",
];

/// Name verbs that are never allowed during agent time.
const DESTRUCTIVE_TOKENS: &[&str] = &[
    "delete", "remove", "destroy", "drop", "purge", "wipe", "erase", "truncate", "revoke",
    "kill", "terminate", "uninstall", "reset", "rm", "rmdir", "force",
];

/// Name tokens that mark a lookup of a specific person's personal
/// information (profiles, contact details, enrichment). During agent time
/// these ask first even when the verb is a read: a person who never shared
/// anything with Hobbes is not "what the user has shared". `email` is not
/// listed because it is already an outside-effect token (so `find_email`
/// asks either way).
const PERSON_TOKENS: &[&str] = &[
    "person", "persons", "people", "profile", "profiles", "contact", "contacts", "phone",
    "phones", "linkedin", "enrich", "enrichment", "prospect", "prospects", "whois",
];

/// Words in a free-text argument that ask for someone's contact details.
const CONTACT_WORDS: &[&str] = &[
    "phone", "phones", "telephone", "cell", "mobile", "email", "emails", "mail", "address",
    "addresses", "contact", "contacts", "whatsapp",
];

/// A double-quoted span that reads like a person's name: two to four words,
/// each starting with an uppercase letter (`"Sophie Asher"`).
fn has_quoted_name(text: &str) -> bool {
    let mut parts = text.split('"');
    parts.next(); // before the first quote
    let mut inside = parts.step_by(2);
    inside.any(|q| {
        let words: Vec<&str> = q.split_whitespace().collect();
        (2..=4).contains(&words.len())
            && words.iter().all(|w| {
                w.chars().next().is_some_and(|c| c.is_uppercase())
                    && w.chars().all(|c| c.is_alphabetic() || c == '-' || c == '\'' || c == '.')
            })
    })
}

/// **Heuristic, deliberately simple.** Whether a free-text argument (a web
/// search query, say) targets a named individual's contact details: it holds
/// a quoted person-like name (see [`has_quoted_name`]) AND a contact word
/// (phone, email, address, contact…). Generic searches never match; a name
/// without a contact word, or contact words without a quoted name, don't
/// either.
pub fn targets_personal_contact(text: &str) -> bool {
    if !has_quoted_name(text) {
        return false;
    }
    let lower = text.to_lowercase();
    lower.contains("e-mail")
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| CONTACT_WORDS.contains(&w))
}

/// Every string in `args` (nested objects and arrays included).
fn arg_strings(args: &serde_json::Value, out: &mut Vec<String>) {
    match args {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| arg_strings(v, out)),
        serde_json::Value::Object(o) => o.values().for_each(|v| arg_strings(v, out)),
        _ => {}
    }
}

/// Split a tool name into lowercase word tokens: separators (`_ - . : /`
/// and the `__` server prefix) and camelCase boundaries both split.
fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn action_of(args: &serde_json::Value) -> String {
    args.get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Why a call got its verdict — for the approval card ("why it asks") and
/// the refusal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeReason {
    /// A Hobbes built-in agent time may use.
    HobbesAllowed,
    /// The person's own plan / lists / workers / timers.
    HobbesOffLimits,
    /// A destructive verb in the name.
    Destructive,
    /// Looks up a specific person's personal information (by tool name, or a
    /// query aimed at a named person's contact details).
    PersonLookup,
    /// An outside-effect verb in the name.
    OutsideEffect,
    /// A read of shared data.
    Read,
    /// No recognisable verb: treated as an outside effect.
    Unrecognised,
}

impl ScopeReason {
    /// One line for the "why it asks" text on an approval card.
    pub fn label(&self) -> &'static str {
        match self {
            ScopeReason::HobbesAllowed => "a Hobbes tool agent time may use",
            ScopeReason::HobbesOffLimits => "your plan, lists, workers and timers are off limits",
            ScopeReason::Destructive => "destructive, never allowed during agent time",
            ScopeReason::PersonLookup => "looks up a specific person's personal information",
            ScopeReason::OutsideEffect => "has an outside effect",
            ScopeReason::Read => "reads what you've shared",
            ScopeReason::Unrecognised => "unrecognised tool, treated as an outside effect",
        }
    }
}

/// **The agent-time scope check** — pure, applied by the runtime's tool gate
/// to every call made during agent time (`tool` is the tool name as the
/// model emitted it; `args` its arguments). See [`agent_time_scope`] for
/// the rules.
pub fn agent_time_scope_allows(tool: &str, args: &serde_json::Value) -> ScopeVerdict {
    agent_time_scope(tool, args).0
}

/// The scope verdict with its reason. Rules, first match wins:
///
/// 1. Hobbes built-ins are classified explicitly:
///    - Allow: `HOBBES_AGENT_TIME` (its own grant checks still apply),
///      `HOBBES_TODO_LIST`, `HOBBES_CALENDAR_LIST`, `HOBBES_PROJECT_LIST`,
///      `HOBBES_FLEET_STATUS`,
///      `HOBBES_LIST_TIMERS`, `HOBBES_PAGE_RESULT`,
///      `HOBBES_UPDATE_SCRATCHPAD` (the session's own memory),
///      `HOBBES_MESSAGE_PARENT` / `HOBBES_MESSAGE_WORKER` (the dispatch
///      relay, along a parent ↔ worker link only),
///      `HOBBES_INVOKE_SKILL` / `HOBBES_UNLOAD_SKILL` (capability loading;
///      the skill's tools are gated one by one), `MCP_LOAD_SERVER_TOOLS` /
///      `MCP_UNLOAD_SERVER_TOOLS`, `HOBBES_DECISIONS action=list`,
///      `HOBBES_TIME_GOALS action=get`, `HOBBES_TIME_BLOCK action=list`.
///    - NeedsApproval: `HOBBES_PROJECT_MERGE` (it asks every time anyway).
///    - Deny: everything else of Hobbes' own — the person's to-dos, plan,
///      timeline, projects, goals (`HOBBES_TODO_CREATE/UPDATE`,
///      `HOBBES_PLAN_DAY`, `HOBBES_TIME_BLOCK`, `HOBBES_PROJECT_UPSERT`,
///      `HOBBES_TIME_GOALS set`), answering or raising decisions, launching
///      workers (`HOBBES_DISPATCH`) and timers (`HOBBES_SET_TIMER`,
///      `HOBBES_CANCEL_TIMER`). Agent time files a proposal instead.
/// 2. Any other tool (MCP, Composio, terminal) by the tokens in its name:
///    a destructive verb → Deny; a person-lookup token (person, people,
///    profile, contact, phone, linkedin, enrich…, see `PERSON_TOKENS`) →
///    NeedsApproval; an outside-effect verb → NeedsApproval; a read verb →
///    Allow, unless one of its string arguments targets a named person's
///    contact details ([`targets_personal_contact`]: a quoted person-like
///    name plus phone / email / address / contact) → NeedsApproval; no
///    recognisable verb → NeedsApproval (unknown is treated as an outside
///    effect, never as a read).
pub fn agent_time_scope(tool: &str, args: &serde_json::Value) -> (ScopeVerdict, ScopeReason) {
    use ScopeReason as R;
    use ScopeVerdict::*;
    match tool {
        "HOBBES_AGENT_TIME" | "HOBBES_TODO_LIST" | "HOBBES_CALENDAR_LIST" | "HOBBES_PROJECT_LIST"
        | "HOBBES_FLEET_STATUS" | "HOBBES_LIST_TIMERS" | "HOBBES_PAGE_RESULT"
        | "HOBBES_UPDATE_SCRATCHPAD" | "HOBBES_WRITE_HANDOFF" | "HOBBES_INVOKE_SKILL"
        | "HOBBES_UNLOAD_SKILL" | "MCP_LOAD_SERVER_TOOLS" | "MCP_UNLOAD_SERVER_TOOLS"
        // Dispatch relay: stays inside Hobbes, along a parent ↔ worker link
        // only (todo::relay refuses anything else); whatever the receiver
        // then does passes its own gates.
        | "HOBBES_MESSAGE_PARENT" | "HOBBES_MESSAGE_WORKER" => return (Allow, R::HobbesAllowed),
        // A write to the person's projects that always asks anyway: held
        // for their approval rather than refused outright.
        "HOBBES_PROJECT_MERGE" => return (NeedsApproval, R::OutsideEffect),
        "HOBBES_DECISIONS" => {
            return if action_of(args) == "list" {
                (Allow, R::HobbesAllowed)
            } else {
                (Deny, R::HobbesOffLimits)
            };
        }
        "HOBBES_TIME_BLOCK" => {
            return if action_of(args) == "list" {
                (Allow, R::HobbesAllowed)
            } else {
                (Deny, R::HobbesOffLimits)
            };
        }
        "HOBBES_TIME_GOALS" => {
            return if action_of(args) == "get" || action_of(args).is_empty() {
                (Allow, R::HobbesAllowed)
            } else {
                (Deny, R::HobbesOffLimits)
            };
        }
        t if t.starts_with("HOBBES_") => return (Deny, R::HobbesOffLimits),
        _ => {}
    }

    let tokens = name_tokens(tool);
    let has = |set: &[&str]| tokens.iter().any(|t| set.contains(&t.as_str()));
    if has(DESTRUCTIVE_TOKENS) {
        (Deny, R::Destructive)
    } else if has(PERSON_TOKENS) {
        (NeedsApproval, R::PersonLookup)
    } else if has(WRITE_TOKENS) {
        (NeedsApproval, R::OutsideEffect)
    } else if has(READ_TOKENS) {
        let mut texts = Vec::new();
        arg_strings(args, &mut texts);
        if texts.iter().any(|t| targets_personal_contact(t)) {
            (NeedsApproval, R::PersonLookup)
        } else {
            (Allow, R::Read)
        }
    } else {
        (NeedsApproval, R::Unrecognised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_run_with_an_open_item_stays_eligible() {
        let mut t = Todo::new("Check the SLA", 1.0);
        t.lane = Lane::Agent;
        let idle = |_: &str| false;
        for (state, eligible) in [
            (AgentState::Queued, true),
            (AgentState::Stuck, true),
            (AgentState::Review, true),
            (AgentState::Working, false),
            (AgentState::Done, false),
        ] {
            t.agent_state = Some(state);
            assert_eq!(agent_time_eligible(&t, &idle), eligible, "{state:?}");
        }
    }
    use serde_json::json;

    fn granted() -> AgentTimeGrant {
        AgentTimeGrant {
            enabled: true,
            daily_minutes: 45,
            spend_cap_usd: 2.0,
            ..Default::default()
        }
    }

    fn today() -> NaiveDate {
        chrono::Local::now().date_naive()
    }

    fn leased() -> PlannerState {
        let mut s = PlannerState {
            agent_time_grant: granted(),
            ..Default::default()
        };
        s.open_agent_time_lease("chat-at", today(), Utc::now()).unwrap();
        s
    }

    #[test]
    fn grant_is_off_by_default_and_refuses_everything() {
        let mut s = PlannerState::default();
        assert!(!s.agent_time_grant.enabled);
        assert_eq!(
            s.open_agent_time_lease("c", today(), Utc::now()),
            Err(AgentTimeRefusal::NotGranted)
        );
        // Even with a lease forced in, a disabled grant refuses start.
        s.agent_time_lease = Some(AgentTimeLease {
            chat_session_id: "c".into(),
            opened_at: Utc::now(),
        });
        assert_eq!(
            s.agent_time_start("c", "x", today(), Utc::now()).unwrap_err(),
            AgentTimeRefusal::NotGranted
        );
    }

    #[test]
    fn ungranted_day_refuses() {
        let mut g = granted();
        g.days = [false; 7];
        let mut s = PlannerState { agent_time_grant: g, ..Default::default() };
        assert_eq!(
            s.open_agent_time_lease("c", today(), Utc::now()),
            Err(AgentTimeRefusal::NotToday)
        );
    }

    #[test]
    fn only_the_leased_session_may_call() {
        let mut s = leased();
        assert_eq!(
            s.agent_time_start("some-normal-chat", "x", today(), Utc::now()).unwrap_err(),
            AgentTimeRefusal::NotAgentTimeSession
        );
        assert_eq!(
            s.open_agent_time_lease("other", today(), Utc::now()),
            Err(AgentTimeRefusal::LeaseHeld)
        );
        assert!(s.agent_time_start("chat-at", "Compare KV-cache offload approaches", today(), Utc::now()).is_ok());
    }

    #[test]
    fn full_session_lifecycle_is_logged() {
        let mut s = leased();
        let now = Utc::now();
        assert_eq!(
            s.agent_time_start("chat-at", "   ", today(), now).unwrap_err(),
            AgentTimeRefusal::Invalid(
                "Declare an 'intent' first: what you will do and why, in one or two lines.".into()
            )
        );
        let started = s.agent_time_start("chat-at", "Digest KV-cache notes", today(), now).unwrap();
        assert_eq!(started.tool_calls[0].tool, "HOBBES_AGENT_TIME.start");
        assert_eq!(
            s.agent_time_start("chat-at", "again", today(), now).unwrap_err(),
            AgentTimeRefusal::AlreadyRunning
        );
        s.agent_time_log("chat-at", "notes.search", "6 notes", today(), now).unwrap();
        let (p, sess) = s
            .agent_time_propose("chat-at", ProposalKind::Research, "KV-cache digest", "body", today(), now)
            .unwrap();
        assert_eq!(p.status, ProposalStatus::Open);
        assert_eq!(sess.outputs, vec![p.id.clone()]);
        let ended = s.agent_time_end("chat-at", "Filed one digest", now).unwrap();
        assert_eq!(ended.end_reason, Some(AgentTimeEndReason::Completed));
        assert_eq!(ended.summary.as_deref(), Some("Filed one digest"));
        assert_eq!(ended.tool_calls.len(), 3, "start, propose (carrying the log note), end");
        assert_eq!(ended.tool_calls[1].note.as_deref(), Some("6 notes"));
        assert_eq!(ended.call_count(), 3);
        assert_eq!(s.trust_ledger.clean_sessions, 1);
        assert_eq!(
            s.agent_time_log("chat-at", "x", "y", today(), now).unwrap_err(),
            AgentTimeRefusal::NoOpenSession
        );
    }

    #[test]
    fn pause_takes_effect_immediately() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let closed = s.close_agent_time_lease(now, AgentTimeEndReason::Paused).unwrap();
        assert_eq!(closed.end_reason, Some(AgentTimeEndReason::Paused));
        assert_eq!(
            s.agent_time_propose("chat-at", ProposalKind::Note, "t", "", today(), now).unwrap_err(),
            AgentTimeRefusal::NotAgentTimeSession
        );
    }

    #[test]
    fn budget_counts_todays_minutes_and_spend() {
        let now = Utc::now();
        let mut s = leased();
        // A 40-minute session earlier today that cost $1.50.
        s.agent_time_sessions.push(AgentTimeSession {
            id: "old".into(),
            intent: "x".into(),
            started_at: now - chrono::Duration::minutes(40),
            ended_at: Some(now),
            cost_usd: 1.5,
            tool_calls: vec![],
            outputs: vec![],
            end_reason: Some(AgentTimeEndReason::Completed),
            summary: None,
            chat_session_id: None,
            out_of_scope_attempts: 0,
            todo_id: None,
            reviewed_at: None,
        });
        let b = s.agent_time_budget(today(), now);
        // The fixture may straddle local midnight; only assert when it didn't.
        if (now - chrono::Duration::minutes(40)).with_timezone(&chrono::Local).date_naive() == today() {
            assert_eq!(b.minutes_used, 40);
            assert_eq!(b.minutes_left(), 5);
            assert!((b.spend_left_usd() - 0.5).abs() < 1e-9);
            s.agent_time_sessions[0].cost_usd = 2.0;
            assert_eq!(
                s.agent_time_start("chat-at", "x", today(), now).unwrap_err(),
                AgentTimeRefusal::SpendCapReached
            );
        }
    }

    #[test]
    fn enforce_ends_an_over_budget_session() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        s.add_agent_time_cost(5.0);
        let ended = s.enforce_agent_time_budget(today(), now).unwrap();
        assert_eq!(ended.end_reason, Some(AgentTimeEndReason::SpendCapReached));
        assert!(s.open_agent_time_session().is_none());
    }

    #[test]
    fn out_of_scope_calls_are_evidence() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        s.record_agent_time_tool_call("chat-at", "notes.search", "q", ScopeVerdict::Allow, now);
        s.record_agent_time_tool_call("chat-at", "GMAIL_SEND_EMAIL", "to bob", ScopeVerdict::NeedsApproval, now);
        assert!(s
            .record_agent_time_tool_call("normal-chat", "x", "y", ScopeVerdict::Allow, now)
            .is_none());
        assert_eq!(s.trust_ledger.in_scope_attempts, 1);
        assert_eq!(s.trust_ledger.out_of_scope_attempts, 1);
        let ended = s.agent_time_end("chat-at", "", now).unwrap();
        assert_eq!(ended.out_of_scope_attempts, 1);
        assert_eq!(s.trust_ledger.clean_sessions, 0, "not a clean session");
    }

    #[test]
    fn proposal_scope_kinds_are_enforced() {
        let mut s = leased();
        s.agent_time_grant.scope.proposal_kinds = vec![ProposalKind::Research];
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        assert!(s.agent_time_propose("chat-at", ProposalKind::Skill, "t", "", today(), now).is_err());
        assert!(s.agent_time_propose("chat-at", ProposalKind::Research, "t", "", today(), now).is_ok());
    }

    #[test]
    fn resolving_proposals_feeds_the_ledger_once() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let (a, _) = s.agent_time_propose("chat-at", ProposalKind::Todo, "Book node", "", today(), now).unwrap();
        let (b, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "n", "", today(), now).unwrap();
        let (c, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "m", "", today(), now).unwrap();
        assert_eq!(s.open_proposals().len(), 3);
        s.resolve_proposal(&a.id, ProposalStatus::Accepted, None, now).unwrap();
        assert!(s.resolve_proposal(&a.id, ProposalStatus::Dismissed, None, now).is_none(), "only open moves");
        let edited = s
            .resolve_proposal(&b.id, ProposalStatus::Accepted, Some(("New".into(), "nb".into())), now)
            .unwrap();
        assert_eq!(edited.status, ProposalStatus::Edited);
        assert_eq!(edited.title, "New");
        s.resolve_proposal(&c.id, ProposalStatus::Dismissed, None, now).unwrap();
        let l = s.trust_ledger;
        assert_eq!((l.proposals_accepted, l.proposals_edited, l.proposals_dismissed), (1, 1, 1));
        assert!((l.acceptance_rate().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        let t = proposal_to_todo(&a, 1.0);
        assert_eq!(t.title, "Book node");
    }

    #[test]
    fn revising_a_verdict_moves_the_ledger_and_leaves_a_record_for_the_agent() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let (a, _) = s.agent_time_propose("chat-at", ProposalKind::Research, "GPU digest", "", today(), now).unwrap();
        let (b, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "Edited note", "", today(), now).unwrap();
        let (c, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "Thin note", "", today(), now).unwrap();
        let (d, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "Still open", "", today(), now).unwrap();
        s.resolve_proposal(&a.id, ProposalStatus::Accepted, None, now).unwrap();
        s.resolve_proposal(&b.id, ProposalStatus::Accepted, Some(("Edited note".into(), "b".into())), now).unwrap();
        s.resolve_proposal(&c.id, ProposalStatus::Dismissed, None, now).unwrap();
        let counts = |s: &PlannerState| {
            let l = s.trust_ledger;
            (l.proposals_accepted, l.proposals_edited, l.proposals_dismissed)
        };
        assert_eq!(counts(&s), (1, 1, 1));

        // Accepted → dismissed, with a reason.
        let later = now + chrono::Duration::minutes(5);
        let p = s.revise_proposal(&a.id, Some(ProposalStatus::Dismissed), " too shallow ", later).unwrap();
        assert_eq!(p.status, ProposalStatus::Dismissed);
        assert_eq!(p.revisions.len(), 1);
        assert_eq!(p.revisions[0].from, ProposalStatus::Accepted);
        assert_eq!(p.revisions[0].note, "too shallow");
        assert_eq!(counts(&s), (0, 1, 2));
        // An edited acceptance counts as accepted: flipping it to dismissed
        // takes it out of `edited`; flipping it to accepted is a no-op error.
        assert!(s.revise_proposal(&b.id, Some(ProposalStatus::Accepted), "", later).is_err());
        // Dismissed → accepted.
        s.revise_proposal(&c.id, Some(ProposalStatus::Accepted), "", later).unwrap();
        assert_eq!(counts(&s), (1, 1, 1));
        // Remove: stops counting, keeps the record.
        let r = s.revise_proposal(&b.id, None, "duplicate", later + chrono::Duration::minutes(1)).unwrap();
        assert!(r.removed);
        assert_eq!(r.status, ProposalStatus::Edited, "status kept for the record");
        assert_eq!(counts(&s), (1, 0, 1));
        assert!(s.revise_proposal(&b.id, Some(ProposalStatus::Dismissed), "", later).is_err(), "removed is final");
        assert!(s.revise_proposal(&d.id, Some(ProposalStatus::Dismissed), "", later).is_err(), "open can't be revised");
        assert!(s.revise_proposal(&c.id, Some(ProposalStatus::Open), "", later).is_err());

        // The agent's next opening context lists the changes, newest first.
        let lines = proposal_revision_lines(&s, MAX_REVISIONS_LISTED);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], ("Edited note".to_string(), "was accepted, then removed from your trust ledger: \u{201c}duplicate\u{201d}".to_string()));
        assert!(lines.iter().any(|(t, l)| t == "GPU digest" && l == "changed from accepted to dismissed: \u{201c}too shallow\u{201d}"));
        assert!(lines.iter().any(|(t, l)| t == "Thin note" && l == "changed from dismissed to accepted"));
        let lists = agent_time_task_lists(&s, today(), &|_| false);
        let ctx = agent_time_context(&lists, "Dustin");
        assert!(ctx.contains("Dustin revisited some of your earlier proposals"), "{ctx}");
        assert!(ctx.contains("- GPU digest: changed from accepted to dismissed"), "{ctx}");
    }

    #[test]
    fn restart_recovers_open_sessions() {
        let mut s = leased();
        s.agent_time_start("chat-at", "x", today(), Utc::now()).unwrap();
        let rec = s.recover_open_agent_time(Utc::now());
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].end_reason, Some(AgentTimeEndReason::Recovered));
        assert!(s.agent_time_lease.is_none());
    }

    #[test]
    fn name_tokens_split_every_convention() {
        assert_eq!(name_tokens("GMAIL_SEND_EMAIL"), vec!["gmail", "send", "email"]);
        assert_eq!(name_tokens("notes.search"), vec!["notes", "search"]);
        assert_eq!(name_tokens("github__createIssue"), vec!["github", "create", "issue"]);
        assert_eq!(name_tokens("read_file"), vec!["read", "file"]);
    }

    #[test]
    fn scope_check_hobbes_builtins() {
        use ScopeVerdict::*;
        let none = json!({});
        assert_eq!(agent_time_scope_allows("HOBBES_AGENT_TIME", &json!({"action": "propose"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_TODO_LIST", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_CALENDAR_LIST", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_PROJECT_LIST", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_PROJECT_MERGE", &none), NeedsApproval);
        assert_eq!(agent_time_scope_allows("HOBBES_UPDATE_SCRATCHPAD", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_INVOKE_SKILL", &none), Allow);
        assert_eq!(agent_time_scope_allows("MCP_LOAD_SERVER_TOOLS", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "list"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "resolve"})), Deny);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "ask"})), Deny);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_GOALS", &json!({"action": "get"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_GOALS", &json!({"action": "set"})), Deny);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_BLOCK", &json!({"action": "list"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_BLOCK", &json!({"action": "move"})), Deny);
        for t in [
            "HOBBES_TODO_CREATE", "HOBBES_TODO_UPDATE", "HOBBES_PLAN_DAY", "HOBBES_TIME_BLOCK",
            "HOBBES_PROJECT_UPSERT", "HOBBES_DISPATCH", "HOBBES_SET_TIMER", "HOBBES_CANCEL_TIMER",
            "HOBBES_SOMETHING_NEW",
        ] {
            assert_eq!(agent_time_scope_allows(t, &none), Deny, "{t}");
        }
    }

    #[test]
    fn scope_check_outside_tools_by_verb() {
        use ScopeVerdict::*;
        let none = json!({});
        // Reads of shared data.
        for t in ["notes.search", "repo.read", "read_file", "list_directory", "GMAIL_FETCH_EMAILS",
                  "GOOGLECALENDAR_LIST_EVENTS", "github__getIssue", "web_search"] {
            assert_eq!(agent_time_scope_allows(t, &none), Allow, "{t}");
        }
        // Outside effects go through the normal gate.
        for t in ["GMAIL_SEND_EMAIL", "github__create_pull_request", "run_terminal_command",
                  "terminal_execute", "write_file", "SLACK_POST_MESSAGE", "stripe_create_payment",
                  "bash", "mystery_tool"] {
            assert_eq!(agent_time_scope_allows(t, &none), NeedsApproval, "{t}");
        }
        // Destructive verbs are refused outright — even when a read verb is
        // also present.
        for t in ["delete_file", "GMAIL_DELETE_MESSAGE", "github__remove_label", "drop_table",
                  "git_push_force", "list_and_delete"] {
            assert_eq!(agent_time_scope_allows(t, &none), Deny, "{t}");
        }
    }

    // ── log notes fold into the next call ───────────────────────────────

    #[test]
    fn a_log_note_attaches_to_the_next_recorded_call() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        // The model logs, then calls the tool; the gate records the call.
        s.agent_time_log("chat-at", "web-search_brave_web_search", "Look for fleet blockers", today(), now)
            .unwrap();
        let sess = s
            .record_agent_time_tool_call("chat-at", "brave_web_search", "query=fleet", ScopeVerdict::Allow, now)
            .unwrap();
        assert_eq!(sess.tool_calls.len(), 2, "start + one row for the search, not two");
        let row = sess.tool_calls.last().unwrap();
        assert_eq!(row.tool, "brave_web_search", "the gate's plain tool name, not the model's");
        assert_eq!(row.kind, AgentTimeLogKind::Call);
        assert_eq!(row.note.as_deref(), Some("Look for fleet blockers"));
        assert_eq!(sess.call_count(), 2);
    }

    #[test]
    fn a_trailing_log_note_stays_a_note_row_and_is_not_a_call() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        s.record_agent_time_tool_call("chat-at", "notes.search", "q", ScopeVerdict::Allow, now);
        s.agent_time_log("chat-at", "", "Wrapping up", today(), now).unwrap();
        let sess = s.agent_time_log("chat-at", "", "Nothing else worth doing", today(), now).unwrap();
        let notes: Vec<_> = sess.tool_calls.iter().filter(|e| e.kind == AgentTimeLogKind::Note).collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(sess.call_count(), 2, "start + notes.search");
        // Runtime events are neither calls nor merged into the next call.
        s.note_agent_time("chat-at", "runtime", "approved by the user", now);
        let sess = s
            .record_agent_time_tool_call("chat-at", "repo.read", "f", ScopeVerdict::Allow, now)
            .unwrap();
        assert_eq!(sess.call_count(), 3);
        assert!(sess.tool_calls.last().unwrap().note.is_none(), "an event sits between the notes and the call");
        assert!(s.agent_time_log("chat-at", "", "  ", today(), now).is_err());
    }

    #[test]
    fn consecutive_notes_join_onto_one_call() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        s.agent_time_log("chat-at", "", "first", today(), now).unwrap();
        s.agent_time_log("chat-at", "", "second", today(), now).unwrap();
        let (_, sess) = s
            .agent_time_propose("chat-at", ProposalKind::Note, "t", "", today(), now)
            .unwrap();
        assert_eq!(sess.tool_calls.len(), 2);
        assert_eq!(sess.tool_calls[1].note.as_deref(), Some("first \u{b7} second"));
    }

    #[test]
    fn old_log_rows_deserialize_as_calls() {
        let e: AgentTimeLogEntry = serde_json::from_value(json!({
            "at": "2026-10-07T14:42:00Z", "tool": "x", "summary": "y"
        }))
        .unwrap();
        assert_eq!(e.kind, AgentTimeLogKind::Call);
        assert!(e.note.is_none());
        // And a call row serializes without the new fields.
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("kind").is_none() && v.get("note").is_none());
    }

    // ── people lookups ask first ────────────────────────────────────────

    #[test]
    fn person_lookups_need_approval_even_as_reads() {
        use ScopeVerdict::*;
        let none = json!({});
        // The exact call from the Oct 7 session.
        assert_eq!(
            agent_time_scope("linkedin_get_person_profile", &json!({"linkedin_url": "https://www.linkedin.com/in/x"})),
            (NeedsApproval, ScopeReason::PersonLookup)
        );
        for t in [
            "linkedin_get_person_profile", "apollo_people_search", "get_user_profile",
            "GOOGLE_CONTACTS_LIST", "lookup_phone_number", "clearbit_enrichment",
            "people_data_labs_enrich_person", "hunter_email_finder", "whois_lookup",
        ] {
            assert_eq!(agent_time_scope_allows(t, &none), NeedsApproval, "{t}");
        }
        // Destructive still wins.
        assert_eq!(agent_time_scope_allows("delete_contact", &none), Deny);
    }

    #[test]
    fn searches_for_a_named_persons_contact_details_need_approval() {
        use ScopeVerdict::*;
        // The exact search from the Oct 7 session.
        let q = json!({"query": "\"Sophie Asher\" attorney Seattle phone OR email contact", "count": 10});
        assert_eq!(
            agent_time_scope("brave_web_search", &q),
            (NeedsApproval, ScopeReason::PersonLookup)
        );
        // Generic searches stay allowed.
        assert_eq!(agent_time_scope_allows("brave_web_search", &json!({"query": "rust dioxus signals"})), Allow);
        // A quoted name without a contact word, or contact words without a
        // quoted name, are ordinary research.
        assert_eq!(
            agent_time_scope_allows("brave_web_search", &json!({"query": "\"Ada Lovelace\" analytical engine"})),
            Allow
        );
        assert_eq!(
            agent_time_scope_allows("brave_web_search", &json!({"query": "how to change email address in git"})),
            Allow
        );
        assert_eq!(
            agent_time_scope_allows("brave_web_search", &json!({"query": "\"rust signals\" email"})),
            Allow,
            "a quoted lowercase phrase is not a name"
        );
        // Nested arguments count.
        assert_eq!(
            agent_time_scope_allows("web_search", &json!({"params": {"q": "\"Jane Q. Doe\" home address"}})),
            NeedsApproval
        );
    }

    #[test]
    fn contact_heuristic_unit_cases() {
        assert!(targets_personal_contact("\"Sophie Asher\" phone"));
        assert!(targets_personal_contact("find the e-mail for \"Bob O'Neil\""));
        assert!(!targets_personal_contact("\"Sophie\" phone"), "one word is not a full name");
        assert!(!targets_personal_contact("Sophie Asher phone"), "unquoted");
        assert!(!targets_personal_contact("\"Sophie Asher\" biography"));
    }

    // ── agent tasks vs the person's tasks ───────────────────────────────

    fn todo_in(s: &mut PlannerState, title: &str, lane: Lane, state: Option<AgentState>) -> String {
        let mut t = Todo::new(title, s.todos.len() as f64);
        t.lane = lane;
        t.agent_state = state;
        let id = t.id.clone();
        s.todos.push(t);
        id
    }

    #[test]
    fn start_takes_only_an_eligible_agent_lane_todo() {
        let none = |_: &str| false;
        let mut s = leased();
        let now = Utc::now();
        let queued = todo_in(&mut s, "Digest the KV-cache notes", Lane::Agent, Some(AgentState::Queued));
        let stuck = todo_in(&mut s, "Fix the flaky test", Lane::Agent, Some(AgentState::Stuck));
        let working = todo_in(&mut s, "Ship release", Lane::Agent, Some(AgentState::Working));
        let human = todo_in(&mut s, "Sophie Asher Probate Consult", Lane::Human, None);
        let mut done = Todo::new("Old agent task", 9.0);
        done.lane = Lane::Agent;
        done.agent_state = Some(AgentState::Queued);
        done.status = crate::todo::model::TodoStatus::Completed;
        let done_id = done.id.clone();
        s.todos.push(done);
        // A queued to-do whose linked worker is live right now.
        let mut live = Todo::new("Being worked", 10.0);
        live.lane = Lane::Agent;
        live.agent_state = Some(AgentState::Queued);
        live.linked_fleet_session = Some("w1".into());
        let live_id = live.id.clone();
        s.todos.push(live);
        let w1_live = |sid: &str| sid == "w1";

        // The person's task: refused, pointing at the offer flow.
        let err = s.agent_time_start_on("chat-at", "help", Some(&human), &none, today(), now).unwrap_err();
        match err {
            AgentTimeRefusal::Invalid(m) => {
                assert!(m.contains("not yours") && m.contains("\"offer\""), "{m}");
            }
            other => panic!("{other:?}"),
        }
        for id in [&working, &done_id, &"nope".to_string()] {
            assert!(s.agent_time_start_on("chat-at", "x", Some(id), &none, today(), now).is_err(), "{id}");
        }
        assert!(s.agent_time_start_on("chat-at", "x", Some(&live_id), &w1_live, today(), now).is_err());
        assert!(s.open_agent_time_session().is_none(), "no session opened by a refusal");

        // Eligible: queued, or stuck.
        let started = s.agent_time_start_on("chat-at", "Digest", Some(&queued), &none, today(), now).unwrap();
        assert_eq!(started.todo_id.as_deref(), Some(queued.as_str()));
        assert!(started.tool_calls[0].summary.contains("working on Digest the KV-cache notes"));
        s.agent_time_end("chat-at", "", now).unwrap();
        assert!(s.agent_time_start_on("chat-at", "Fix", Some(&stuck), &none, today(), now).is_ok());
        s.agent_time_end("chat-at", "", now).unwrap();
        // No to-do: Hobbes-space work.
        assert_eq!(s.agent_time_start("chat-at", "Tidy notes", today(), now).unwrap().todo_id, None);
    }

    #[test]
    fn an_accepted_offer_moves_the_todo_to_the_agent_lane_as_queued() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let human = todo_in(&mut s, "Draft the release notes", Lane::Human, None);
        let (offer, _) = s.agent_time_offer("chat-at", &human, "I can draft these from the changelog.", today(), now).unwrap();
        assert_eq!(offer.kind, ProposalKind::Offer);
        assert_eq!(offer.todo_id.as_deref(), Some(human.as_str()));
        assert_eq!(offer.title, "Draft the release notes");
        assert_eq!(offer.body, "I can draft these from the changelog.");
        // Filing it changes nothing.
        assert_eq!(s.todo(&human).unwrap().lane, Lane::Human);
        // Not applied while open, or when dismissed.
        assert!(s.apply_accepted_offer(&offer, now).is_none());

        let accepted = s.resolve_proposal(&offer.id, ProposalStatus::Accepted, None, now).unwrap();
        let t = s.apply_accepted_offer(&accepted, now).expect("handed over");
        assert_eq!(t.lane, Lane::Agent);
        assert_eq!(t.agent_state, Some(AgentState::Queued));
        assert_eq!(s.todo(&human).unwrap().lane, Lane::Agent);
        assert_eq!(s.trust_ledger.proposals_accepted, 1, "counted as accepted");
        assert!(s.apply_accepted_offer(&accepted, now).is_none(), "only once");

        // Dismissed: the to-do is untouched.
        let other = todo_in(&mut s, "Call the bank", Lane::Human, None);
        let (o2, _) = s.agent_time_offer("chat-at", &other, "pitch", today(), now).unwrap();
        let dismissed = s.resolve_proposal(&o2.id, ProposalStatus::Dismissed, None, now).unwrap();
        assert!(s.apply_accepted_offer(&dismissed, now).is_none());
        assert_eq!(s.todo(&other).unwrap().lane, Lane::Human);
    }

    #[test]
    fn an_offer_for_an_agent_lane_or_closed_todo_is_refused() {
        let mut s = leased();
        // Offers are allowed whatever proposal kinds the grant lists.
        s.agent_time_grant.scope.proposal_kinds = vec![ProposalKind::Research];
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let agent = todo_in(&mut s, "Agent thing", Lane::Agent, Some(AgentState::Queued));
        let mut closed = Todo::new("Done thing", 5.0);
        closed.status = crate::todo::model::TodoStatus::Completed;
        let closed_id = closed.id.clone();
        s.todos.push(closed);
        assert!(matches!(
            s.agent_time_offer("chat-at", &agent, "p", today(), now),
            Err(AgentTimeRefusal::Invalid(m)) if m.contains("already in the agent lane")
        ));
        assert!(matches!(
            s.agent_time_offer("chat-at", &closed_id, "p", today(), now),
            Err(AgentTimeRefusal::Invalid(m)) if m.contains("closed")
        ));
        assert!(s.agent_time_offer("chat-at", "missing", "p", today(), now).is_err());
        // propose() won't file an offer without a to-do.
        assert!(s.agent_time_propose("chat-at", ProposalKind::Offer, "t", "", today(), now).is_err());
        let human = todo_in(&mut s, "Mine", Lane::Human, None);
        assert!(s.agent_time_offer("chat-at", &human, "p", today(), now).is_ok());
    }

    #[test]
    fn the_context_lists_agent_work_as_yours_and_the_persons_as_not() {
        let mut s = PlannerState::default();
        let none = |_: &str| false;
        let d = today();
        let a = todo_in(&mut s, "Digest the KV-cache notes", Lane::Agent, Some(AgentState::Queued));
        s.todo_mut(&a).unwrap().outcome = Some("a one-page digest".into());
        todo_in(&mut s, "Ship release", Lane::Agent, Some(AgentState::Working));
        // Human: one later, one today, one overdue — overdue and today first.
        todo_in(&mut s, "Someday reading", Lane::Human, None);
        let t = todo_in(&mut s, "Sophie Asher Probate Consult", Lane::Human, None);
        s.todo_mut(&t).unwrap().scheduled_for = Some(d);
        let o = todo_in(&mut s, "File the taxes", Lane::Human, None);
        s.todo_mut(&o).unwrap().deadline = Some(d - chrono::Duration::days(2));

        let lists = agent_time_task_lists(&s, d, &none);
        assert_eq!(lists.agent.len(), 1, "only eligible agent work");
        assert_eq!(lists.agent[0].1, "Digest the KV-cache notes");
        let titles: Vec<&str> = lists.human.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(titles, vec!["File the taxes", "Sophie Asher Probate Consult", "Someday reading"]);

        let ctx = agent_time_context(&lists, "Dustin");
        let (yours, not_yours) = ctx.split_once("Dustin's tasks").expect("two sections");
        assert!(yours.starts_with("Agent lane: yours to work on"));
        assert!(yours.contains("Digest the KV-cache notes") && yours.contains("done when: a one-page digest"));
        assert!(!yours.contains("Probate"), "the person's task is never listed as agent work");
        assert!(not_yours.contains("NOT yours") && not_yours.contains("\"offer\""));
        assert!(not_yours.contains("Sophie Asher Probate Consult"));
        assert!(!not_yours.contains("Digest the KV-cache"));
        assert!(!ctx.contains("Ship release"), "a working to-do is not offered");

        // Capped at ten, with a count of the rest.
        for i in 0..12 {
            todo_in(&mut s, &format!("Chore {i}"), Lane::Human, None);
        }
        let lists = agent_time_task_lists(&s, d, &none);
        assert_eq!(lists.human.len(), MAX_HUMAN_TASKS_LISTED);
        assert_eq!(lists.human_more, 5);
        assert!(agent_time_context(&lists, "Dustin").contains("and 5 more"));
        // Nothing eligible: say so.
        let empty = agent_time_context(&agent_time_task_lists(&PlannerState::default(), d, &none), "the user");
        assert!(empty.contains("none queued or stuck") && empty.contains("the user's tasks"));
    }

    #[test]
    fn old_proposals_and_sessions_load_without_the_new_fields() {
        let p: Proposal = serde_json::from_value(json!({
            "id": "pr_1", "kind": "research", "title": "t", "created_at": "2026-10-01T10:00:00Z",
            "session_id": "at_1"
        }))
        .unwrap();
        assert_eq!(p.todo_id, None);
        let offer: Proposal = serde_json::from_value(json!({
            "id": "pr_2", "kind": "offer", "title": "t", "created_at": "2026-10-01T10:00:00Z",
            "session_id": "at_1", "todo_id": "td_1"
        }))
        .unwrap();
        assert_eq!((offer.kind, offer.todo_id.as_deref()), (ProposalKind::Offer, Some("td_1")));
        assert_eq!(ProposalKind::parse("offer"), Some(ProposalKind::Offer));
        let sess: AgentTimeSession = serde_json::from_value(json!({
            "id": "at_1", "intent": "x", "started_at": "2026-10-01T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(sess.todo_id, None);
    }

    // ── accepting starts work ───────────────────────────────────────────

    #[test]
    fn a_todo_the_agent_files_as_its_own_work_lands_in_the_agent_lane() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        let (mine, _) = s
            .agent_time_propose_in("chat-at", ProposalKind::Todo, "Digest the KV notes", "A one-page digest in Notes", Some(Lane::Agent), today(), now)
            .unwrap();
        assert_eq!(mine.lane, Some(Lane::Agent));
        assert_eq!(s.proposals.iter().find(|p| p.id == mine.id).unwrap().lane, Some(Lane::Agent), "stored too");
        assert!(proposal_starts_work(&mine));
        let t = proposal_to_todo(&mine, 1.0);
        assert_eq!(t.lane, Lane::Agent);
        assert_eq!(t.agent_state, Some(AgentState::Queued));
        assert_eq!(t.outcome.as_deref(), Some("A one-page digest in Notes"));

        // A suggestion for the person stays a human Inbox to-do.
        let (yours, _) = s
            .agent_time_propose_in("chat-at", ProposalKind::Todo, "Renew passport", "Expires in May", None, today(), now)
            .unwrap();
        assert!(!proposal_starts_work(&yours));
        let t = proposal_to_todo(&yours, 1.0);
        assert_eq!((t.lane, t.notes.as_str(), t.outcome.clone()), (Lane::Human, "Expires in May", None));
        // A lane on anything but a to-do is ignored; notes never start work.
        let (note, _) = s
            .agent_time_propose_in("chat-at", ProposalKind::Note, "n", "b", Some(Lane::Agent), today(), now)
            .unwrap();
        assert_eq!(note.lane, None);
        assert!(!proposal_starts_work(&note));
    }

    #[test]
    fn accept_only_leaves_the_work_queued() {
        let mut s = leased();
        let now = Utc::now();
        s.agent_time_start("chat-at", "x", today(), now).unwrap();
        // Offer: the person's to-do moves to the agent lane, Queued.
        let human = todo_in(&mut s, "Draft the release notes", Lane::Human, None);
        let (offer, _) = s.agent_time_offer("chat-at", &human, "From the changelog.", today(), now).unwrap();
        let done = s.accept_proposal(&offer.id, None, now).unwrap();
        let t = done.todo.expect("handed over");
        assert_eq!((t.id.as_str(), t.lane, t.agent_state), (human.as_str(), Lane::Agent, Some(AgentState::Queued)));
        assert_eq!(t.linked_fleet_session, None, "no worker started");
        // Agent-lane to-do proposal: created Queued.
        let (mine, _) = s
            .agent_time_propose_in("chat-at", ProposalKind::Todo, "Digest", "Digest in Notes", Some(Lane::Agent), today(), now)
            .unwrap();
        let done = s.accept_proposal(&mine.id, None, now).unwrap();
        let t = done.todo.unwrap();
        assert_eq!((t.lane, t.agent_state), (Lane::Agent, Some(AgentState::Queued)));
        assert!(s.todo(&t.id).is_some(), "created in the planner");
        // Research: saved, nothing made.
        let (r, _) = s.agent_time_propose("chat-at", ProposalKind::Research, "r", "b", today(), now).unwrap();
        let done = s.accept_proposal(&r.id, None, now).unwrap();
        assert_eq!((done.proposal.status, done.todo), (ProposalStatus::Accepted, None));
        assert_eq!(s.trust_ledger.proposals_accepted, 3);
        assert!(s.accept_proposal(&r.id, None, now).is_none(), "only once");
    }

    #[test]
    fn accept_and_start_goes_to_the_running_session_or_the_next_window() {
        let now = Utc::now();
        // A run with budget left takes it.
        let s = leased();
        assert_eq!(
            accept_start_route(&s, today(), now),
            AcceptStartRoute::HandToRun { chat_id: "chat-at".into() }
        );
        // No run: queued for the next window (never a new chat).
        let idle = PlannerState { agent_time_grant: granted(), ..Default::default() };
        assert_eq!(accept_start_route(&idle, today(), now), AcceptStartRoute::NextWindow);
        // A run whose budget is spent doesn't take more work.
        let mut spent = leased();
        spent.agent_time_start("chat-at", "x", today(), now).unwrap();
        spent.add_agent_time_cost(5.0);
        assert_eq!(accept_start_route(&spent, today(), now), AcceptStartRoute::NextWindow);
    }

    #[test]
    fn handing_a_todo_to_the_run_sets_its_todo_and_logs_an_event() {
        let now = Utc::now();
        let mut s = leased();
        let id = todo_in(&mut s, "Draft the release notes", Lane::Agent, Some(AgentState::Queued));
        s.todo_mut(&id).unwrap().outcome = Some("Notes drafted in the repo".into());
        // Before `start`: the message asks for start with the todo_id.
        let (msg, sess) = s.hand_todo_to_run("chat-at", &id, "Dustin", true, now).unwrap();
        assert!(sess.is_none());
        assert!(msg.starts_with(&format!(
            "Dustin accepted your offer: work on Draft the release notes (todo {id}). Done when: Notes drafted in the repo."
        )), "{msg}");
        assert!(msg.contains(&format!("start (todo_id: {id})")), "{msg}");

        // After `start` with no to-do: it becomes the run's to-do.
        s.agent_time_start("chat-at", "Tidy notes", today(), now).unwrap();
        let (msg, sess) = s.hand_todo_to_run("chat-at", &id, "Dustin", false, now).unwrap();
        let sess = sess.unwrap();
        assert_eq!(sess.todo_id.as_deref(), Some(id.as_str()));
        assert!(msg.contains("accepted your proposed to-do") && msg.contains("carry on"), "{msg}");
        let last = sess.tool_calls.last().unwrap();
        assert_eq!(last.kind, AgentTimeLogKind::Event);
        assert!(last.summary.contains("handed over: Draft the release notes"));
        assert_eq!(sess.call_count(), 1, "the event is not a call");

        // Only the leased chat.
        assert!(s.hand_todo_to_run("other-chat", &id, "Dustin", true, now).is_none());
    }

    // ── agent time v2: jobs, results, questions, review ─────────────────

    fn running() -> PlannerState {
        let mut s = leased();
        s.agent_time_start("chat-at", "Work the queue", today(), Utc::now()).unwrap();
        s
    }

    fn delivery<'a>(job: Job, title: &'a str, todo_id: Option<&'a str>) -> Delivery<'a> {
        Delivery { job, kind: None, title, body: "The finished work.", todo_id, correction: false }
    }

    fn invalid(r: Result<DeliveredResult, AgentTimeRefusal>) -> String {
        match r {
            Err(AgentTimeRefusal::Invalid(m)) => m,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn old_rows_load_as_notice_with_the_v2_defaults() {
        let p: Proposal = serde_json::from_value(json!({
            "id": "pr_1", "kind": "research", "title": "t", "created_at": "2026-10-01T10:00:00Z",
            "session_id": "at_1", "status": "accepted"
        }))
        .unwrap();
        assert_eq!((p.job, p.correction, p.notes.len()), (Job::Notice, false, 0));
        assert!(!p.is_result());
        // Defaults stay out of the saved JSON; set values round-trip.
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("\"job\"") && !json.contains("correction") && !json.contains("notes"), "{json}");
        let r = Proposal {
            job: Job::Tend,
            correction: true,
            status: ProposalStatus::NeedsWork,
            notes: vec![ResultNote { at: Utc::now(), text: "x".into() }],
            ..p
        };
        let back: Proposal = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(serde_json::to_string(&r).unwrap().contains("\"needs_work\""));

        let sess: AgentTimeSession = serde_json::from_value(json!({
            "id": "at_1", "intent": "x", "started_at": "2026-10-01T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(sess.reviewed_at, None);
        let l: TrustLedger = serde_json::from_value(json!({"proposals_accepted": 3})).unwrap();
        assert_eq!((l.results_kept, l.results_needs_work, l.results_discarded, l.corrections_delivered), (0, 0, 0, 0));
        assert_eq!(l.keep_rate(), None);
        let g: AgentTimeGrant = serde_json::from_value(json!({"enabled": true})).unwrap();
        assert_eq!(g.window_start, None);
        let t: Todo = serde_json::from_value(json!({
            "id": "td", "title": "t", "created_at": "2026-10-01T10:00:00Z", "updated_at": "2026-10-01T10:00:00Z"
        }))
        .unwrap();
        assert!(!t.prep_requested);
        let q: AgentQuestion = serde_json::from_value(json!({
            "id": "aq", "session_id": "at", "question": "?", "asked_at": "2026-10-01T10:00:00Z"
        }))
        .unwrap();
        assert!(q.is_open() && !q.consumed && q.options.is_empty());
    }

    #[test]
    fn deliver_validates_job_correction_body_and_the_todo() {
        let now = Utc::now();
        let mut s = running();
        let mine = todo_in(&mut s, "Board meeting", Lane::Human, None);
        let flagged = todo_in(&mut s, "Investor call", Lane::Human, None);
        s.set_prep_requested(&flagged, true, now).unwrap();
        let agent = todo_in(&mut s, "Digest notes", Lane::Agent, Some(AgentState::Queued));

        assert!(invalid(s.agent_time_deliver("chat-at", delivery(Job::Notice, "x", None), today(), now)).contains("propose"));
        let mut d = delivery(Job::Finish, "x", None);
        d.correction = true;
        assert!(invalid(s.agent_time_deliver("chat-at", d, today(), now)).contains("only for job \"tend\""));
        let mut d = delivery(Job::Tend, "x", None);
        d.body = "  ";
        assert!(invalid(s.agent_time_deliver("chat-at", d, today(), now)).contains("'body'"));
        let mut d = delivery(Job::Finish, "x", None);
        d.kind = Some(ProposalKind::Offer);
        assert!(invalid(s.agent_time_deliver("chat-at", d, today(), now)).contains("offer"));
        // Prepare: only a flagged human to-do.
        assert!(invalid(s.agent_time_deliver("chat-at", delivery(Job::Prepare, "x", None), today(), now)).contains("todo_id"));
        assert!(invalid(s.agent_time_deliver("chat-at", delivery(Job::Prepare, "x", Some(&mine)), today(), now)).contains("prep list"));
        assert!(invalid(s.agent_time_deliver("chat-at", delivery(Job::Prepare, "x", Some(&agent)), today(), now)).contains("prep list"));
        let ok = s.agent_time_deliver("chat-at", delivery(Job::Prepare, "Investor call brief", Some(&flagged)), today(), now).unwrap();
        assert_eq!((ok.result.job, ok.result.kind, ok.result.status), (Job::Prepare, ProposalKind::Note, ProposalStatus::Open));
        assert_eq!(ok.result.todo_id.as_deref(), Some(flagged.as_str()));
        assert!(ok.todo.is_none(), "preparing never changes the person's to-do");
        assert_eq!(ok.session.tool_calls.last().unwrap().tool, "HOBBES_AGENT_TIME.deliver");
        // Finish: never the person's task; an agent to-do awaits review.
        assert!(invalid(s.agent_time_deliver("chat-at", delivery(Job::Finish, "x", Some(&mine)), today(), now)).contains("not yours"));
        let fin = s.agent_time_deliver("chat-at", delivery(Job::Finish, "Digest", Some(&agent)), today(), now).unwrap();
        assert_eq!(fin.todo.unwrap().agent_state, Some(AgentState::NeedsYou));
        assert_eq!(fin.result.kind, ProposalKind::Other);
        // Tend: a correction is flagged and counted; a todo_id is ignored.
        let mut d = delivery(Job::Tend, "Correction: the deadline is the 14th", Some(&mine));
        d.correction = true;
        let tend = s.agent_time_deliver("chat-at", d, today(), now).unwrap();
        assert!(tend.result.correction && tend.result.todo_id.is_none());
        assert_eq!(s.trust_ledger.corrections_delivered, 1);
        assert_eq!(s.trust_ledger.proposals_accepted + s.trust_ledger.results_kept, 0, "delivering grades nothing");
        // Outside the leased session, or with no open session: refused.
        assert_eq!(
            s.agent_time_deliver("other", delivery(Job::Tend, "x", None), today(), now).unwrap_err(),
            AgentTimeRefusal::NotAgentTimeSession
        );
        s.agent_time_end("chat-at", "done", now).unwrap();
        assert_eq!(
            s.agent_time_deliver("chat-at", delivery(Job::Tend, "x", None), today(), now).unwrap_err(),
            AgentTimeRefusal::NoOpenSession
        );
    }

    #[test]
    fn grading_moves_the_result_counters_and_the_finish_todo() {
        use crate::todo::model::TodoStatus;
        let now = Utc::now();
        let mut s = running();
        let a = todo_in(&mut s, "Digest A", Lane::Agent, Some(AgentState::Queued));
        let b = todo_in(&mut s, "Digest B", Lane::Agent, Some(AgentState::Queued));
        let ra = s.agent_time_deliver("chat-at", delivery(Job::Finish, "A", Some(&a)), today(), now).unwrap().result.id;
        let rb = s.agent_time_deliver("chat-at", delivery(Job::Finish, "B", Some(&b)), today(), now).unwrap().result.id;
        let rt = s.agent_time_deliver("chat-at", delivery(Job::Tend, "Memory tidy", None), today(), now).unwrap().result.id;

        let kept = s.grade_result(&ra, ProposalStatus::Accepted, now).unwrap();
        assert_eq!(kept.proposal.status, ProposalStatus::Accepted);
        let t = kept.todo.unwrap();
        assert_eq!((t.status, t.agent_state), (TodoStatus::Completed, Some(AgentState::Done)));
        assert!(s.grade_result(&ra, ProposalStatus::Dismissed, now).is_none(), "graded once");

        let back = s.grade_result(&rb, ProposalStatus::NeedsWork, now).unwrap();
        assert_eq!(back.todo.unwrap().agent_state, Some(AgentState::Queued), "back in the queue for the next window");
        assert!(s.grade_result(&rt, ProposalStatus::Open, now).is_none());
        assert!(s.grade_result(&rt, ProposalStatus::Dismissed, now).unwrap().todo.is_none());

        let l = s.trust_ledger;
        assert_eq!((l.results_kept, l.results_needs_work, l.results_discarded), (1, 1, 1));
        assert_eq!((l.proposals_accepted, l.proposals_edited, l.proposals_dismissed), (0, 0, 0));
        assert!((l.keep_rate().unwrap() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn needs_work_is_only_for_results_and_accept_on_a_result_keeps_it() {
        let now = Utc::now();
        let mut s = running();
        let (notice, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "n", "b", today(), now).unwrap();
        assert!(s.resolve_proposal(&notice.id, ProposalStatus::NeedsWork, None, now).is_none());
        assert!(s.grade_result(&notice.id, ProposalStatus::Accepted, now).is_none(), "a proposal isn't graded");
        s.resolve_proposal(&notice.id, ProposalStatus::Accepted, None, now).unwrap();
        assert!(s.revise_proposal(&notice.id, Some(ProposalStatus::NeedsWork), "", now).unwrap_err().contains("only for delivered results"));

        let r = s.agent_time_deliver("chat-at", delivery(Job::Tend, "t", None), today(), now).unwrap().result.id;
        let done = s.accept_proposal(&r, None, now).unwrap();
        assert_eq!(done.proposal.status, ProposalStatus::Accepted);
        assert_eq!(s.trust_ledger.results_kept, 1);
        assert_eq!(s.trust_ledger.proposals_accepted, 1, "only the notice proposal");
        assert!(!proposal_starts_work(&done.proposal));
    }

    #[test]
    fn regrading_and_removing_a_result_keeps_the_record() {
        let now = Utc::now();
        let mut s = running();
        let flagged = todo_in(&mut s, "Investor call", Lane::Human, None);
        s.set_prep_requested(&flagged, true, now).unwrap();
        let id = s.agent_time_deliver("chat-at", delivery(Job::Prepare, "Brief", Some(&flagged)), today(), now).unwrap().result.id;
        assert!(s.revise_proposal(&id, Some(ProposalStatus::Dismissed), "", now).unwrap_err().contains("not graded yet"));
        s.grade_result(&id, ProposalStatus::Accepted, now).unwrap();
        assert_eq!(s.revise_proposal(&id, Some(ProposalStatus::Edited), "", now).unwrap_err(), "that is already the verdict");
        let p = s.revise_proposal(&id, Some(ProposalStatus::NeedsWork), "Missing the agenda", now).unwrap();
        assert_eq!(p.status, ProposalStatus::NeedsWork);
        assert_eq!((s.trust_ledger.results_kept, s.trust_ledger.results_needs_work), (0, 1));
        let lines = proposal_revision_lines(&s, 6);
        assert_eq!(lines[0].1, "changed from kept to needs work: \u{201c}Missing the agenda\u{201d}");
        let p = s.revise_proposal(&id, None, "", now + chrono::Duration::seconds(1)).unwrap();
        assert!(p.removed);
        assert_eq!(p.revisions.len(), 2);
        assert_eq!(s.trust_ledger.results_needs_work, 0);
        assert_eq!(proposal_revision_lines(&s, 6)[0].1, "was needs work, then removed from your trust ledger");
        assert_eq!((s.trust_ledger.proposals_accepted, s.trust_ledger.proposals_dismissed), (0, 0));
    }

    #[test]
    fn ask_saves_a_question_and_caps_open_ones_per_run() {
        let now = Utc::now();
        let mut s = running();
        let opts = vec![" Formal ".to_string(), "Casual".into(), "Formal".into(), "".into()];
        let (q, sess) = s.agent_time_ask("chat-at", "Formal or casual tone?", &opts, today(), now).unwrap();
        assert_eq!(q.options, vec!["Formal".to_string(), "Casual".into()], "trimmed, deduped, empties dropped");
        assert!(q.is_open() && !q.consumed);
        assert_eq!(sess.tool_calls.last().unwrap().tool, "HOBBES_AGENT_TIME.ask");
        assert!(s.agent_time_ask("chat-at", "  ", &[], today(), now).is_err());
        for i in 1..MAX_OPEN_QUESTIONS_PER_RUN {
            s.agent_time_ask("chat-at", &format!("Q{i}"), &[], today(), now).unwrap();
        }
        match s.agent_time_ask("chat-at", "one too many", &[], today(), now) {
            Err(AgentTimeRefusal::Invalid(m)) => assert!(m.contains("Carry on"), "{m}"),
            other => panic!("expected the cap, got {other:?}"),
        }
        // An answered question frees a slot.
        s.answer_question(&q.id, "Casual", now).unwrap();
        assert!(s.agent_time_ask("chat-at", "now fits", &[], today(), now).is_ok());
        // A new run has its own cap.
        s.agent_time_end("chat-at", "done", now).unwrap();
        s.agent_time_start("chat-at", "again", today(), now).unwrap();
        assert!(s.agent_time_ask("chat-at", "fresh run", &[], today(), now).is_ok());
        assert_eq!(s.open_questions().len(), MAX_OPEN_QUESTIONS_PER_RUN + 1);
    }

    #[test]
    fn answers_reach_the_next_window_exactly_once() {
        let now = Utc::now();
        let none = |_: &str| false;
        let mut s = running();
        let (q, _) = s.agent_time_ask("chat-at", "Formal or casual?", &["Formal".into()], today(), now).unwrap();
        let (q2, _) = s.agent_time_ask("chat-at", "Which repo?", &[], today(), now).unwrap();
        assert!(s.answer_question(&q.id, "  ", now).is_err());
        assert!(s.answer_question("nope", "x", now).is_err());
        let lists = agent_time_task_lists(&s, today(), &none);
        assert!(lists.answers.is_empty());
        assert_eq!(lists.waiting, vec!["Formal or casual?".to_string(), "Which repo?".into()]);

        s.answer_question(&q.id, "Casual", now).unwrap();
        let lists = agent_time_task_lists(&s, today(), &none);
        assert_eq!(lists.answers, vec![(q.id.clone(), "Formal or casual?".to_string(), "Casual".to_string())]);
        assert_eq!(lists.waiting, vec!["Which repo?".to_string()]);
        let ctx = agent_time_context(&lists, "Dustin");
        assert!(ctx.starts_with("Answers to your saved questions"), "answers come first: {ctx}");
        assert!(ctx.contains("You asked \u{201c}Formal or casual?\u{201d}; Dustin answered \u{201c}Casual\u{201d}."), "{ctx}");
        assert!(ctx.contains("don't ask them again") && ctx.contains("- Which repo?"), "{ctx}");

        let consumed = s.consume_answers(&[q.id.clone(), q2.id.clone()]);
        assert_eq!(consumed.len(), 1, "an unanswered question isn't consumed");
        assert!(consumed[0].consumed);
        assert!(s.consume_answers(std::slice::from_ref(&q.id)).is_empty(), "once");
        assert!(agent_time_task_lists(&s, today(), &none).answers.is_empty());
        assert_eq!(s.answer_question(&q.id, "Formal", now).unwrap_err(), "already delivered to agent time");
    }

    #[test]
    fn prep_flagged_tasks_are_the_prepare_list_and_not_the_persons_list() {
        let now = Utc::now();
        let none = |_: &str| false;
        let d = today();
        let mut s = PlannerState::default();
        let call = todo_in(&mut s, "Investor call", Lane::Human, None);
        s.todo_mut(&call).unwrap().deadline = Some(d + chrono::Duration::days(3));
        let board = todo_in(&mut s, "Board deck", Lane::Human, None);
        s.todo_mut(&board).unwrap().scheduled_for = Some(d + chrono::Duration::days(1));
        todo_in(&mut s, "Dentist", Lane::Human, None);
        let agent = todo_in(&mut s, "Digest", Lane::Agent, Some(AgentState::Queued));
        assert!(s.set_prep_requested(&agent, true, now).is_err(), "only the person's own to-dos");
        assert!(s.set_prep_requested("nope", true, now).is_err());
        assert!(s.set_prep_requested(&call, true, now).unwrap().prep_requested);
        s.set_prep_requested(&board, true, now).unwrap();

        let lists = agent_time_task_lists(&s, d, &none);
        let prep: Vec<&str> = lists.prepare.iter().map(|(_, t, _)| t.as_str()).collect();
        assert_eq!(prep, vec!["Board deck", "Investor call"], "soonest first");
        assert!(lists.prepare[0].2.as_deref().unwrap().starts_with("scheduled "));
        assert!(lists.prepare[1].2.as_deref().unwrap().starts_with("due "));
        let human: Vec<&str> = lists.human.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(human, vec!["Dentist"], "flagged tasks leave the not-yours list");
        let ctx = agent_time_context(&lists, "Dustin");
        let (prep_part, rest) = ctx.split_once("Agent lane: yours").unwrap();
        assert!(prep_part.contains("Prepare: Dustin asked you to prep these") && prep_part.contains("Investor call"));
        assert!(rest.contains("Dustin's other tasks: NOT yours") && rest.contains("Dentist"));

        // Unflagging always works; a to-do moved to the agent lane loses it.
        assert!(!s.set_prep_requested(&board, false, now).unwrap().prep_requested);
        s.todo_mut(&call).unwrap().set_lane(Lane::Agent);
        assert!(!s.todo(&call).unwrap().prep_requested);
        assert!(agent_time_task_lists(&s, d, &none).prepare.is_empty());
    }

    #[test]
    fn comments_on_results_reach_the_next_window_newest_first() {
        let now = Utc::now();
        let none = |_: &str| false;
        let mut s = running();
        let a = s.agent_time_deliver("chat-at", delivery(Job::Tend, "Memory tidy", None), today(), now).unwrap().result.id;
        let (b, _) = s.agent_time_propose("chat-at", ProposalKind::Note, "Inbox idea", "b", today(), now).unwrap();
        assert!(s.add_result_note(&a, "   ", now).is_err());
        assert!(s.add_result_note("nope", "x", now).is_err());
        s.add_result_note(&a, "Too long", now).unwrap();
        let p = s.add_result_note(&b.id, "Good idea", now + chrono::Duration::seconds(5)).unwrap();
        assert_eq!(p.notes.len(), 1);
        let lists = agent_time_task_lists(&s, today(), &none);
        assert_eq!(lists.notes, vec![
            ("Inbox idea".to_string(), "Good idea".to_string()),
            ("Memory tidy".to_string(), "Too long".to_string()),
        ]);
        let ctx = agent_time_context(&lists, "Dustin");
        assert!(ctx.contains("- On Memory tidy, Dustin said: \u{201c}Too long\u{201d}"), "{ctx}");
        for i in 0..10 {
            s.add_result_note(&a, &format!("n{i}"), now + chrono::Duration::seconds(10 + i)).unwrap();
        }
        assert_eq!(agent_time_task_lists(&s, today(), &none).notes.len(), MAX_NOTES_LISTED);
    }

    #[test]
    fn the_scheduled_window_starts_once_on_a_granted_day() {
        let now = Utc::now();
        let d = today();
        let nine = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let g = AgentTimeGrant { window_start: Some(nine), ..granted() };
        let at = |h: u32, m: u32| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let due = |g: &AgentTimeGrant, sessions: &[AgentTimeSession], lease: bool, t: NaiveTime, tried: bool| {
            scheduled_start_due(g, sessions, lease, d, t, now, tried)
        };
        assert!(!due(&g, &[], false, at(8, 59), false), "before the window");
        assert!(due(&g, &[], false, at(9, 0), false), "at the window");
        assert!(due(&g, &[], false, at(15, 30), false), "later the same day");
        assert!(!due(&g, &[], true, at(10, 0), false), "a run already holds the lease");
        assert!(!due(&g, &[], false, at(10, 0), true), "already tried today");
        assert!(!due(&AgentTimeGrant { window_start: None, ..g.clone() }, &[], false, at(10, 0), false), "manual only");
        assert!(!due(&AgentTimeGrant { enabled: false, ..g.clone() }, &[], false, at(10, 0), false), "grant off");
        assert!(!due(&AgentTimeGrant { days: [false; 7], ..g.clone() }, &[], false, at(10, 0), false), "not a granted day");
        assert!(!due(&AgentTimeGrant { daily_minutes: 0, ..g.clone() }, &[], false, at(10, 0), false), "no budget");
        // A run today (here: one the person stopped) means no second start.
        let mut s = PlannerState { agent_time_grant: g.clone(), ..Default::default() };
        s.open_agent_time_lease("chat-at", d, now).unwrap();
        s.agent_time_start("chat-at", "x", d, now).unwrap();
        s.close_agent_time_lease(now, AgentTimeEndReason::Stopped);
        assert!(!due(&g, &s.agent_time_sessions, false, at(23, 0), false), "stopped today stays stopped");
        assert!(!s.scheduled_start_due(d, at(23, 0), now, false));
    }

    #[test]
    fn the_review_groups_results_by_job_with_corrections_first() {
        let now = Utc::now();
        let mut s = running();
        let flagged = todo_in(&mut s, "Investor call", Lane::Human, None);
        s.set_prep_requested(&flagged, true, now).unwrap();
        let t1 = now - chrono::Duration::seconds(30);
        s.agent_time_deliver("chat-at", delivery(Job::Tend, "Learned the Linear tools", None), today(), t1).unwrap();
        let mut d = delivery(Job::Tend, "Correction: the 14th, not the 12th", None);
        d.correction = true;
        s.agent_time_deliver("chat-at", d, today(), now).unwrap();
        let prep = s.agent_time_deliver("chat-at", delivery(Job::Prepare, "Brief", Some(&flagged)), today(), now).unwrap().result.id;
        s.agent_time_deliver("chat-at", delivery(Job::Finish, "Digest", None), today(), now).unwrap();
        s.agent_time_propose("chat-at", ProposalKind::Note, "Idea", "b", today(), now).unwrap();
        s.agent_time_ask("chat-at", "Formal?", &[], today(), now).unwrap();
        // A running run isn't reviewed yet.
        assert!(review_for(&s, None, now).sessions.is_empty());
        s.agent_time_end("chat-at", "done", now).unwrap();
        s.grade_result(&prep, ProposalStatus::Accepted, now).unwrap();
        s.add_agent_time_cost_for_chat("chat-at", 0.25);

        let r = review_for(&s, None, now);
        assert_eq!(r.sessions.len(), 1);
        assert_eq!(r.session_ids, vec![r.sessions[0].id.clone()]);
        assert_eq!(r.prepare.len(), 1);
        assert_eq!(r.finish.len(), 1);
        assert_eq!(r.notice.len(), 1);
        let tend: Vec<&str> = r.tend.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(tend, vec!["Correction: the 14th, not the 12th", "Learned the Linear tools"], "corrections first");
        assert_eq!(r.questions.len(), 1);
        assert!((r.spend_usd - 0.25).abs() < 1e-9);
        assert_eq!(
            r.counts,
            AgentTimeReviewCounts {
                results: 4,
                ungraded: 3,
                kept: 1,
                needs_work: 0,
                discarded: 0,
                corrections: 1,
                proposals_open: 1,
                questions_open: 1,
            }
        );
        assert!(!r.is_empty());

        // Read: the run leaves the review; what still waits on the person stays.
        let marked = s.mark_reviewed(&r.session_ids, now);
        assert_eq!(marked.len(), 1);
        assert!(marked[0].reviewed_at.is_some());
        assert!(s.mark_reviewed(&r.session_ids, now).is_empty(), "once");
        let after = review_for(&s, None, now + chrono::Duration::seconds(1));
        assert!(after.sessions.is_empty());
        assert_eq!(after.prepare.len(), 0, "the kept brief was read with its run");
        assert_eq!(after.counts.ungraded, 3, "ungraded results stay until graded");
        // By date: every run that day, reviewed or not.
        assert_eq!(review_for(&s, Some(today()), now).sessions.len(), 1);
        assert!(review_for(&PlannerState::default(), None, now).is_empty());
    }

    #[test]
    fn preparing_may_research_the_flagged_subject_but_person_lookups_still_ask() {
        assert_eq!(agent_time_scope_allows("HOBBES_TODO_LIST", &json!({})), ScopeVerdict::Allow);
        assert_eq!(
            agent_time_scope_allows("web_search", &json!({"query": "Acme Corp Series B investors"})),
            ScopeVerdict::Allow
        );
        assert_eq!(
            agent_time_scope_allows("web_search", &json!({"query": "\"Sophie Asher\" phone number"})),
            ScopeVerdict::NeedsApproval
        );
        assert_eq!(
            agent_time_scope_allows("linkedin_get_person_profile", &json!({})),
            ScopeVerdict::NeedsApproval
        );
    }
}
