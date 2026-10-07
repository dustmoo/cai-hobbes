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
    AgentTimeEndReason, AgentTimeGrant, AgentTimeLogEntry, AgentTimeSession, Proposal,
    ProposalKind, ProposalStatus,
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

    fn push_log(s: &mut AgentTimeSession, tool: &str, summary: &str, now: DateTime<Utc>) {
        s.tool_calls.push(AgentTimeLogEntry {
            at: now,
            tool: clip(tool, 80),
            summary: clip(summary, MAX_LOG_SUMMARY_CHARS),
        });
    }

    /// `start(intent)`: declare the intent and open a logged session.
    pub fn agent_time_start(
        &mut self,
        caller: &str,
        intent: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<AgentTimeSession, AgentTimeRefusal> {
        let budget = self.agent_time_gate(caller, today, now)?;
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
        };
        Self::push_log(
            &mut s,
            "HOBBES_AGENT_TIME.start",
            &format!(
                "intent declared · grant ok · {} min, ${:.2} left",
                budget.minutes_left(),
                budget.spend_left_usd()
            ),
            now,
        );
        self.agent_time_sessions.push(s.clone());
        Ok(s)
    }

    /// `log(tool, summary)`: the agent's own note in the log.
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
        let tool = if tool.trim().is_empty() { "note" } else { tool };
        Self::push_log(s, tool, summary, now);
        Ok(s.clone())
    }

    /// `propose(kind, title, body)`: file an output as an open proposal.
    pub fn agent_time_propose(
        &mut self,
        caller: &str,
        kind: ProposalKind,
        title: &str,
        body: &str,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(Proposal, AgentTimeSession), AgentTimeRefusal> {
        self.agent_time_gate(caller, today, now)?;
        let allowed = &self.agent_time_grant.scope.proposal_kinds;
        if !allowed.is_empty() && !allowed.contains(&kind) {
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

/// An accepted `Todo` proposal as a new human to-do in the Inbox. Pure; the
/// UI upserts and persists it.
pub fn proposal_to_todo(p: &Proposal, sort_order: f64) -> super::model::Todo {
    let mut t = super::model::Todo::new(p.title.clone(), sort_order);
    t.notes = p.body.clone();
    t
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
    "download", "export", "summary", "summarize",
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
];

/// Name verbs that are never allowed during agent time.
const DESTRUCTIVE_TOKENS: &[&str] = &[
    "delete", "remove", "destroy", "drop", "purge", "wipe", "erase", "truncate", "revoke",
    "kill", "terminate", "uninstall", "reset", "rm", "rmdir", "force",
];

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

/// **The agent-time scope check** — pure, applied by the runtime's tool gate
/// to every call made during agent time (`tool` is the tool name as the
/// model emitted it; `args` its arguments).
///
/// Rules, first match wins:
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
/// 2. Any other tool (MCP, Composio, terminal) by the verbs in its name:
///    a destructive verb → Deny; an outside-effect verb → NeedsApproval; a
///    read verb → Allow; no recognisable verb → NeedsApproval (unknown is
///    treated as an outside effect, never as a read).
pub fn agent_time_scope_allows(tool: &str, args: &serde_json::Value) -> ScopeVerdict {
    use ScopeVerdict::*;
    match tool {
        "HOBBES_AGENT_TIME" | "HOBBES_TODO_LIST" | "HOBBES_CALENDAR_LIST"
        | "HOBBES_FLEET_STATUS" | "HOBBES_LIST_TIMERS" | "HOBBES_PAGE_RESULT"
        | "HOBBES_UPDATE_SCRATCHPAD" | "HOBBES_INVOKE_SKILL" | "HOBBES_UNLOAD_SKILL"
        | "MCP_LOAD_SERVER_TOOLS" | "MCP_UNLOAD_SERVER_TOOLS" => return Allow,
        "HOBBES_DECISIONS" => {
            return if action_of(args) == "list" { Allow } else { Deny };
        }
        "HOBBES_TIME_GOALS" => {
            return if action_of(args) == "get" || action_of(args).is_empty() {
                Allow
            } else {
                Deny
            };
        }
        t if t.starts_with("HOBBES_") => return Deny,
        _ => {}
    }

    let tokens = name_tokens(tool);
    let has = |set: &[&str]| tokens.iter().any(|t| set.contains(&t.as_str()));
    if has(DESTRUCTIVE_TOKENS) {
        Deny
    } else if has(WRITE_TOKENS) {
        NeedsApproval
    } else if has(READ_TOKENS) {
        Allow
    } else {
        NeedsApproval
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
        assert_eq!(ended.tool_calls.len(), 4, "start, log, propose, end");
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
}
