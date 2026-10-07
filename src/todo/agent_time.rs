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
//! - **Outputs are proposals.** `propose` is the only write; accepting one
//!   is a UI action ([`PlannerState::resolve_proposal`]), never a tool.
//! - **Scoped.** [`agent_time_scope_allows`] is the pure check the runtime's
//!   tool gate applies to every call made during agent time.
//!
//! Everything here is in-memory state logic; callers persist the rows the
//! methods return (`store::save_agent_time_session`, `save_proposal`,
//! `save_trust_ledger`).

// Public API whose consumers are wave 2 (the agent-time runtime (lease, gate hooks, budget tick) and the Agent time panel); the
// tool handlers use only part of it so far.
#![allow(dead_code)]

use chrono::{DateTime, NaiveDate, Utc};

use super::model::{
    AgentState, AgentTimeEndReason, AgentTimeGrant, AgentTimeLogEntry, AgentTimeLogKind,
    AgentTimeSession, Lane, Proposal, ProposalKind, ProposalRevision, ProposalStatus, Todo,
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
        let filed = self.file_proposal(caller, kind, title, body, None, today, now)?;
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
        self.file_proposal(caller, ProposalKind::Offer, &title, pitch, Some(id), today, now)
    }

    #[allow(clippy::too_many_arguments)]
    fn file_proposal(
        &mut self,
        caller: &str,
        kind: ProposalKind,
        title: &str,
        body: &str,
        todo_id: Option<String>,
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
        };
        let s = self
            .open_agent_time_session_mut()
            .expect("checked open above");
        s.outputs.push(proposal.id.clone());
        Self::push_log(
            s,
            "HOBBES_AGENT_TIME.propose",
            &format!("{} · {}", kind.as_str(), proposal.title),
            now,
        );
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
    /// proposal. Only open proposals move; the trust ledger counts the
    /// outcome. `edited` replaces the title/body (implies `Edited`). Returns
    /// the proposal to persist; persist the ledger too. Turning an accepted
    /// `Todo` proposal into a to-do is the UI's call ([`proposal_to_todo`]).
    pub fn resolve_proposal(
        &mut self,
        id: &str,
        status: ProposalStatus,
        edited: Option<(String, String)>,
        now: DateTime<Utc>,
    ) -> Option<Proposal> {
        let status = if edited.is_some() && status != ProposalStatus::Dismissed {
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
        if let Some((title, body)) = edited {
            if !title.trim().is_empty() {
                p.title = title.trim().to_string();
            }
            p.body = body;
        }
        p.status = status;
        p.resolved_at = Some(now);
        let out = p.clone();
        let l = &mut self.trust_ledger;
        match status {
            ProposalStatus::Accepted => l.proposals_accepted = l.proposals_accepted.saturating_add(1),
            ProposalStatus::Edited => l.proposals_edited = l.proposals_edited.saturating_add(1),
            ProposalStatus::Dismissed => {
                l.proposals_dismissed = l.proposals_dismissed.saturating_add(1)
            }
            ProposalStatus::Open => {}
        }
        Some(out)
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
    /// verdict after the fact, or take it off the ledger (`to: None`). The
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
        let p = self
            .proposals
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("no such proposal")?;
        if p.status == ProposalStatus::Open {
            return Err("still open; accept or dismiss it first".into());
        }
        if p.removed {
            return Err("already removed from the ledger".into());
        }
        let from = p.status;
        let counts_same = |a: ProposalStatus, b: ProposalStatus| {
            a == b || (a != ProposalStatus::Dismissed && b != ProposalStatus::Dismissed)
        };
        if to.is_some_and(|t| counts_same(from, t)) {
            return Err("that is already the verdict".into());
        }
        let l = &mut self.trust_ledger;
        match from {
            ProposalStatus::Accepted => l.proposals_accepted = l.proposals_accepted.saturating_sub(1),
            ProposalStatus::Edited => l.proposals_edited = l.proposals_edited.saturating_sub(1),
            ProposalStatus::Dismissed => l.proposals_dismissed = l.proposals_dismissed.saturating_sub(1),
            ProposalStatus::Open => {}
        }
        match to {
            Some(ProposalStatus::Dismissed) => l.proposals_dismissed = l.proposals_dismissed.saturating_add(1),
            Some(_) => l.proposals_accepted = l.proposals_accepted.saturating_add(1),
            None => p.removed = true,
        }
        if let Some(t) = to {
            p.status = t;
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
/// `Queued` or `Stuck` with no live worker attached (`worker_live` says
/// whether its linked fleet session is running now). Human-lane to-dos are
/// the person's: never eligible (agent time may only offer to take them).
pub fn agent_time_eligible(t: &Todo, worker_live: &dyn Fn(&str) -> bool) -> bool {
    t.lane == Lane::Agent
        && !t.status.is_closed()
        && matches!(t.agent_state, Some(AgentState::Queued) | Some(AgentState::Stuck))
        && !t.linked_fleet_session.as_deref().is_some_and(worker_live)
}

/// Most of the person's own to-dos listed in the agent-time context.
pub const MAX_HUMAN_TASKS_LISTED: usize = 10;

/// The two lists agent time is shown: what is its to work on, and what is
/// the person's.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTimeTaskLists {
    /// Eligible agent-lane to-dos: (id, title, outcome).
    pub agent: Vec<(String, String, Option<String>)>,
    /// The person's open to-dos, overdue and today first: (id, title).
    pub human: Vec<(String, String)>,
    /// Open human to-dos beyond [`MAX_HUMAN_TASKS_LISTED`].
    pub human_more: usize,
    /// The person's after-the-fact changes to recent proposals, newest
    /// first (title, line), so the agent learns from them.
    pub revisions: Vec<(String, String)>,
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

/// **Pure.** Build the two lists from the planner.
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
    let mut human: Vec<&Todo> = planner
        .todos
        .iter()
        .filter(|t| t.lane == Lane::Human && !t.status.is_closed())
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
    AgentTimeTaskLists { agent, human, human_more, revisions }
}

/// Revised proposals listed in the agent-time context.
pub const MAX_REVISIONS_LISTED: usize = 6;

fn verdict_word(s: ProposalStatus) -> &'static str {
    match s {
        ProposalStatus::Dismissed => "dismissed",
        ProposalStatus::Open => "open",
        _ => "accepted",
    }
}

/// **Pure.** One line per revised proposal (its latest revision), newest
/// first: `("title", "changed from accepted to dismissed: <note>")`.
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
            let what = match r.to {
                Some(to) => format!("changed from {} to {}", verdict_word(r.from), verdict_word(to)),
                None => format!("was {}, then removed from your trust ledger", verdict_word(r.from)),
            };
            let line = if r.note.is_empty() { what } else { format!("{what}: \u{201c}{}\u{201d}", r.note) };
            (p.title.clone(), line)
        })
        .collect()
}

/// **Pure.** The two-list context block for the opening message and the
/// `start` result. `owner` is how the person is named ("Dustin", or "the
/// user"). The wording draws the boundary: the agent lane is agent time's;
/// the owner's tasks are not, and may only be offered for.
pub fn agent_time_context(lists: &AgentTimeTaskLists, owner: &str) -> String {
    let mut out = String::from("Agent lane: yours to work on (start with todo_id to take one)\n");
    if lists.agent.is_empty() {
        out.push_str("- (none queued or stuck right now; work in Hobbes-owned space instead)\n");
    }
    for (id, title, outcome) in &lists.agent {
        match outcome {
            Some(o) => out.push_str(&format!("- [{id}] {title} \u{2014} done when: {o}\n")),
            None => out.push_str(&format!("- [{id}] {title}\n")),
        }
    }
    out.push_str(&format!(
        "\n{owner}'s tasks: NOT yours. Don't work on them, research them or act on them. You may \
         offer help with propose(kind: \"offer\", todo_id, body: why you could take it); only \
         {owner}'s acceptance hands one over.\n"
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
    if !lists.revisions.is_empty() {
        out.push_str(&format!(
            "\n{owner} revisited some of your earlier proposals. Take the reasons into account:\n"
        ));
        for (title, line) in &lists.revisions {
            out.push_str(&format!("- {title}: {line}\n"));
        }
    }
    out
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
    match p.kind {
        ProposalKind::Offer => true,
        ProposalKind::Todo => p.lane == Some(Lane::Agent),
        _ => false,
    }
}

/// What "Accept & start" does with the accepted to-do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptStartRoute {
    /// An agent-time run is active with budget left: hand the to-do to the
    /// run in this chat.
    HandToRun { chat_id: String },
    /// Otherwise: dispatch it to a new chat worker (user-initiated, so it
    /// uses no agent time).
    NewChat,
}

/// **Pure.** Route an "Accept & start": the running agent-time chat when a
/// lease is held and today's budget has room, else a new chat worker.
pub fn accept_start_route(planner: &PlannerState, today: NaiveDate, now: DateTime<Utc>) -> AcceptStartRoute {
    match &planner.agent_time_lease {
        Some(l) if planner.agent_time_budget(today, now).refusal().is_none() => {
            AcceptStartRoute::HandToRun { chat_id: l.chat_session_id.clone() }
        }
        _ => AcceptStartRoute::NewChat,
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
///      `HOBBES_TODO_LIST`, `HOBBES_CALENDAR_LIST`, `HOBBES_FLEET_STATUS`,
///      `HOBBES_LIST_TIMERS`, `HOBBES_PAGE_RESULT`,
///      `HOBBES_UPDATE_SCRATCHPAD` (the session's own memory),
///      `HOBBES_INVOKE_SKILL` / `HOBBES_UNLOAD_SKILL` (capability loading;
///      the skill's tools are gated one by one), `MCP_LOAD_SERVER_TOOLS` /
///      `MCP_UNLOAD_SERVER_TOOLS`, `HOBBES_DECISIONS action=list`,
///      `HOBBES_TIME_GOALS action=get`.
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
        "HOBBES_AGENT_TIME" | "HOBBES_TODO_LIST" | "HOBBES_CALENDAR_LIST"
        | "HOBBES_FLEET_STATUS" | "HOBBES_LIST_TIMERS" | "HOBBES_PAGE_RESULT"
        | "HOBBES_UPDATE_SCRATCHPAD" | "HOBBES_INVOKE_SKILL" | "HOBBES_UNLOAD_SKILL"
        | "MCP_LOAD_SERVER_TOOLS" | "MCP_UNLOAD_SERVER_TOOLS" => return (Allow, R::HobbesAllowed),
        "HOBBES_DECISIONS" => {
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
        assert_eq!(agent_time_scope_allows("HOBBES_UPDATE_SCRATCHPAD", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_INVOKE_SKILL", &none), Allow);
        assert_eq!(agent_time_scope_allows("MCP_LOAD_SERVER_TOOLS", &none), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "list"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "resolve"})), Deny);
        assert_eq!(agent_time_scope_allows("HOBBES_DECISIONS", &json!({"action": "ask"})), Deny);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_GOALS", &json!({"action": "get"})), Allow);
        assert_eq!(agent_time_scope_allows("HOBBES_TIME_GOALS", &json!({"action": "set"})), Deny);
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
    fn accept_and_start_goes_to_the_running_session_or_a_new_chat() {
        let now = Utc::now();
        // A run with budget left takes it.
        let s = leased();
        assert_eq!(
            accept_start_route(&s, today(), now),
            AcceptStartRoute::HandToRun { chat_id: "chat-at".into() }
        );
        // No run: a new chat worker.
        let idle = PlannerState { agent_time_grant: granted(), ..Default::default() };
        assert_eq!(accept_start_route(&idle, today(), now), AcceptStartRoute::NewChat);
        // A run whose budget is spent doesn't take more work.
        let mut spent = leased();
        spent.agent_time_start("chat-at", "x", today(), now).unwrap();
        spent.add_agent_time_cost(5.0);
        assert_eq!(accept_start_route(&spent, today(), now), AcceptStartRoute::NewChat);
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
}
