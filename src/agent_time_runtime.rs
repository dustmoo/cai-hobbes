// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

//! The agent-time runtime (SYMBIOTIC_PLANNER.md §3): starts, gates, meters
//! and stops granted agent time. `todo::agent_time` holds the state logic
//! (grant, budget, lease, log, proposals, scope check); this module drives it
//! from the app.
//!
//! Lifecycle:
//!
//! 1. **Start** (`ChatCommand::AgentTimeStart`: the person's Start, or the
//!    grant's scheduled window — [`scheduled_start_now`] fires the same
//!    command once a day when `AgentTimeGrant::window_start` is reached):
//!    [`start_now`] checks the grant and today's budget ([`start_check`]),
//!    opens a dedicated chat ("Agent time · <date>") — or continues today's
//!    one, whatever its last run's end reason ([`todays_chat`]: one chat a
//!    day) — pins it to the grant's model, takes a new lease on it, and
//!    queues the opening (or short continuation) message ([`seed_message`]).
//!    The model must then call `HOBBES_AGENT_TIME start`, which opens a new
//!    session record on the same chat.
//! 2. **Gate**: every tool call from an agent-time chat passes
//!    [`gate_decision`] before the normal permission flow, on both the
//!    streaming path and the approval-resume path
//!    (`builtin_tools::agent_time_gate`). Every verdict is recorded.
//! 3. **Meter**: each usage event attributes the chat's new spend to the open
//!    session ([`on_usage`]) and re-checks the budget; a 30 s tick
//!    ([`tick`]) does the same for minutes.
//! 4. **Wait**: a call held for the user's approval (an outside effect the
//!    gate forced, a skill whose permission is Ask, an MCP permission) parks
//!    the run. [`held_approvals`] finds it so the Agent time panel, the
//!    decision queue and the tab sidebar can show it and answer it through
//!    the chat's own approve / deny path. Waiting still uses the run's
//!    minutes.
//! 5. **End**: the model's `end`, the turn ending (nothing drives the model
//!    after it stops — [`on_turn_complete`] closes the lease), Pause / Stop
//!    ([`halt`]: lease closed and stream cancelled at once), or the budget
//!    running out. Whatever is still held for approval when the lease closes
//!    is expired ([`expire_held_calls`]), never left dangling.
//!
//! After the lease is gone the chat stays marked ([`is_agent_time_chat`]):
//! its tool calls are refused and its tool loop does not continue, so
//! nothing it started can keep running outside agent time. A later Start
//! takes a new lease on the same chat, which lifts that refusal for the new
//! run only: the gate keys on the lease, so closing it restores the refusal.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, Utc};
use dioxus::prelude::*;
use serde_json::Value;

use crate::components::shared::{MessageContent, ToolCall};
use crate::session::SessionState;
use crate::todo::agent_time::{
    agent_time_scope, agent_time_scope_allows, AgentTimeBudget, AgentTimeRefusal, ScopeReason,
    ScopeVerdict,
};
use crate::todo::model::{AgentTimeEndReason, AgentTimeSession};
use crate::todo::PlannerState;

pub const AGENT_TIME_TOOL: &str = "HOBBES_AGENT_TIME";
/// How often the budget is re-checked while a lease is held.
pub const TICK_SECS: u64 = 30;
/// A lease whose model never declared an intent is revoked after this long.
pub const INTENT_GRACE_MINUTES: i64 = 5;

/// Chats that held the agent-time lease in this process (runtime-only).
/// Together with the persisted `AgentTimeSession.chat_session_id`s this is
/// the "was an agent-time chat" set: it covers a chat whose model never got
/// as far as `start`.
pub static LEASED_CHATS: GlobalSignal<HashSet<String>> = Signal::global(HashSet::new);

/// Spend already attributed: (chat id, that chat's total cost when last
/// attributed). Set when a lease opens; a std mutex, never held across await.
static SPEND_CURSOR: std::sync::Mutex<Option<(String, f64)>> = std::sync::Mutex::new(None);

pub const ENDED_MESSAGE: &str = "Agent time in this chat has ended (it was paused, stopped, \
    finished, or ran out of budget). Tools are not available here; the user can start agent \
    time again from the Planner, or open a new chat.";
/// The result written into a call still held for approval when the run's
/// lease closes.
pub const EXPIRED_MESSAGE: &str = "Agent time ended before this was approved; it did not run.";
pub const NO_INTENT_MESSAGE: &str = "Declare your intent first: call HOBBES_AGENT_TIME with \
    action \"start\" and an intent (what you will do and why, in one or two lines). No other \
    tool runs until you do.";

// ── Pure decisions ──────────────────────────────────────────────────────────

/// Whether agent time may start now: grant on, granted day, minutes and
/// spend left, and no lease already held.
pub fn start_check(
    planner: &PlannerState,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<AgentTimeBudget, AgentTimeRefusal> {
    if planner.agent_time_lease.is_some() {
        return Err(AgentTimeRefusal::LeaseHeld);
    }
    let budget = planner.agent_time_budget(today, now);
    match budget.refusal() {
        Some(r) => Err(r),
        None => Ok(budget),
    }
}

/// Whether `chat_id` is (or was) an agent-time chat: leased now, leased
/// earlier in this process, or named by a logged agent-time session.
pub fn is_agent_time_chat(
    planner: &PlannerState,
    leased_chats: &HashSet<String>,
    chat_id: &str,
) -> bool {
    planner.is_agent_time_session(chat_id)
        || leased_chats.contains(chat_id)
        || planner
            .agent_time_sessions
            .iter()
            .any(|s| s.chat_session_id.as_deref() == Some(chat_id))
}

/// Whether the model in `chat_id` has declared an intent that is still open.
pub fn intent_declared(planner: &PlannerState, chat_id: &str) -> bool {
    planner
        .agent_time_sessions
        .iter()
        .any(|s| s.is_open() && s.chat_session_id.as_deref() == Some(chat_id))
}

fn action_of(args: &Value) -> &str {
    args.get("action").and_then(|v| v.as_str()).unwrap_or("").trim()
}

/// Everything the gate looks at for one tool call.
pub struct GateInput<'a> {
    /// [`is_agent_time_chat`] for the calling chat.
    pub is_agent_time_chat: bool,
    /// The calling chat holds the lease right now.
    pub leased: bool,
    /// [`intent_declared`] for the calling chat.
    pub intent_declared: bool,
    pub tool: &'a str,
    pub args: &'a Value,
    /// True on the approval-resume path: the user clicked Approve.
    pub user_approved: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// Not an agent-time chat: the normal flow, untouched.
    NotAgentTime,
    /// Run through the normal flow; record `record` if set
    /// (`HOBBES_AGENT_TIME` logs itself, so it records nothing here).
    Proceed { record: Option<ScopeVerdict> },
    /// An outside effect on the streaming path: hold it for the user's
    /// approval even if a trust rule or always-allow would skip the prompt.
    /// Record `NeedsApproval`.
    ForceApproval,
    /// A held call the user has just approved: run it (no second log row or
    /// ledger count — it was recorded when it was held).
    ApprovedByUser,
    /// Refuse with `message` as the tool error; record `record` if set.
    Refuse { message: String, record: Option<ScopeVerdict> },
}

/// **The agent-time gate** (spec §3 rules 2 and 3), applied to every tool
/// call before the normal permission flow.
pub fn gate_decision(i: &GateInput) -> GateDecision {
    if !i.is_agent_time_chat && !i.leased {
        return GateDecision::NotAgentTime;
    }
    if !i.leased {
        return GateDecision::Refuse { message: ENDED_MESSAGE.to_string(), record: None };
    }
    let is_start = i.tool == AGENT_TIME_TOOL && action_of(i.args) == "start";
    if !i.intent_declared && !is_start {
        return GateDecision::Refuse { message: NO_INTENT_MESSAGE.to_string(), record: None };
    }
    if i.tool == AGENT_TIME_TOOL {
        return GateDecision::Proceed { record: None };
    }
    match agent_time_scope_allows(i.tool, i.args) {
        // In scope but held anyway (a skill set to Ask, an MCP permission):
        // the streaming path already recorded it.
        ScopeVerdict::Allow if i.user_approved => GateDecision::ApprovedByUser,
        ScopeVerdict::Allow => GateDecision::Proceed { record: Some(ScopeVerdict::Allow) },
        ScopeVerdict::NeedsApproval if i.user_approved => GateDecision::ApprovedByUser,
        ScopeVerdict::NeedsApproval => GateDecision::ForceApproval,
        ScopeVerdict::Deny => GateDecision::Refuse {
            message: format!(
                "'{}' is out of scope for agent time and was refused. Agent time may read what \
                 the user has shared and write only to Hobbes-owned space (HOBBES_AGENT_TIME \
                 deliver, propose, ask); the user's to-dos, plan, timers, workers and anything \
                 destructive are off limits. This attempt is logged.",
                i.tool
            ),
            record: Some(ScopeVerdict::Deny),
        },
    }
}

/// Whether a finished tool round in `chat_id` must NOT continue into another
/// model call: the chat is an agent-time chat whose lease is gone.
pub fn continuation_blocked(
    planner: &PlannerState,
    leased_chats: &HashSet<String>,
    chat_id: &str,
) -> bool {
    !planner.is_agent_time_session(chat_id) && is_agent_time_chat(planner, leased_chats, chat_id)
}

/// New spend to attribute: the chat's total cost since the last attribution.
pub fn spend_delta(chat_total: f64, reported: f64) -> f64 {
    (chat_total - reported).max(0.0)
}

/// The end reason for a budget refusal.
pub fn end_reason_for(r: &AgentTimeRefusal) -> AgentTimeEndReason {
    match r {
        AgentTimeRefusal::SpendCapReached => AgentTimeEndReason::SpendCapReached,
        AgentTimeRefusal::MinutesExhausted => AgentTimeEndReason::BudgetExhausted,
        _ => AgentTimeEndReason::Stopped,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TickAction {
    /// Nothing to do (no lease, or budget left).
    Idle,
    /// Stop the run now with this reason (over budget, or grant revoked).
    Stop(AgentTimeEndReason),
    /// The model never declared an intent within the grace period.
    RevokeNoIntent,
}

/// The budget-stop decision, run on every usage event and every tick.
pub fn tick_action(planner: &PlannerState, today: NaiveDate, now: DateTime<Utc>) -> TickAction {
    let Some(lease) = &planner.agent_time_lease else {
        return TickAction::Idle;
    };
    if let Some(r) = planner.agent_time_budget(today, now).refusal() {
        return TickAction::Stop(end_reason_for(&r));
    }
    let started_under_lease = planner.agent_time_sessions.iter().any(|s| {
        s.chat_session_id.as_deref() == Some(lease.chat_session_id.as_str())
            && s.started_at >= lease.opened_at
    });
    if !started_under_lease && now - lease.opened_at > chrono::Duration::minutes(INTENT_GRACE_MINUTES)
    {
        return TickAction::RevokeNoIntent;
    }
    TickAction::Idle
}

/// The chat a Start continues: today's agent-time chat, whatever its last
/// run's end reason (one agent-time chat per day). That is the chat of the
/// latest session started today, else `leased_today` (the chat leased
/// earlier today in this process whose model never called `start`, so no
/// session names it).
pub fn todays_chat(
    planner: &PlannerState,
    leased_today: Option<&(NaiveDate, String)>,
    today: NaiveDate,
) -> Option<String> {
    planner
        .agent_time_sessions
        .iter()
        .filter(|s| s.started_at.with_timezone(&chrono::Local).date_naive() == today)
        .filter(|s| s.chat_session_id.is_some())
        .max_by_key(|s| s.started_at)
        .and_then(|s| s.chat_session_id.clone())
        .or_else(|| leased_today.filter(|(d, _)| *d == today).map(|(_, id)| id.clone()))
}

/// The last chat leased in this process and the day it was leased on: the
/// fallback for [`todays_chat`] when a run ended before its `start`.
static LAST_LEASED: std::sync::Mutex<Option<(NaiveDate, String)>> = std::sync::Mutex::new(None);

/// The chat title for a run on `date`.
pub fn chat_title(date: NaiveDate) -> String {
    format!("Agent time · {}", date.format("%b %-d"))
}

/// The opening message of an agent-time chat. It states the grant, the four
/// jobs in priority order (answers to saved questions → Prepare → Finish →
/// Notice → Tend when idle), the rules and the controls. `context` (built by
/// `todo::agent_time::agent_time_context`) carries the answers, the prep
/// list, the agent lane, `owner`'s other tasks and their comments.
pub fn seed_message(budget: &AgentTimeBudget, continued: bool, context: &str, owner: &str) -> String {
    let minutes = crate::todo::model::format_minutes(budget.minutes_left());
    let spend = budget.spend_left_usd();
    if continued {
        // Today's chat already carries the rules; a new run in it gets a
        // short note and the fresh lists.
        return format!(
            "[New agent-time run: Hobbes started this on the user's behalf]\n\n\
             New agent-time run: {minutes}, ${spend:.2} left. Same rules; declare your intent \
             with start (your earlier run in this chat is closed). Same order: answers first, \
             then prepare, finish, notice, and tend when nothing else is queued. Deliver \
             finished work with deliver; save questions with ask instead of stopping.\n\n\
             {context}"
        );
    }
    let opening = format!(
        "[Agent time: Hobbes opened this on the user's behalf]\n\n\
         The user has granted you agent time: up to {minutes} today, with ${spend:.2} of \
         spend. Use it to do finished work {owner} reads later, in one review. You choose \
         what to do within the order below."
    );
    format!(
        "{opening}\n\n\
         Work in this order:\n\
         1. Answers: if {owner} answered questions you saved earlier (listed below), act on \
         them first.\n\
         2. Prepare: briefs, drafts and background for the tasks {owner} flagged \"Prep \
         this\" (the prep list below), and only those. Research their subject; don't act on \
         them.\n\
         3. Finish: work the agent-lane queue and deliver the finished work itself.\n\
         4. Notice: propose changes worth making to {owner}'s stuff; {owner} accepts or \
         dismisses each one.\n\
         5. Tend, when nothing else is queued: Hobbes's own upkeep. Check loose ends in \
         things you told {owner} and correct what was wrong; fix your own friction (failed \
         calls, stalls); learn what connected tools can do; tidy memory. Tend is read-only \
         outside Hobbes and writes only to Hobbes-owned space.\n\n\
         What is yours, and what is not:\n\
         - Yours: the prep list and the agent-lane to-dos below, and Hobbes-owned space.\n\
         - Not yours: {owner}'s other tasks. Don't work on them or act on them; you may offer \
         to take one (propose with kind \"offer\" and its todo_id), and only {owner}'s \
         acceptance hands it over.\n\n\
         {context}\n\
         How it works:\n\
         1. First call HOBBES_AGENT_TIME with action \"start\" and an intent: one or two lines \
         on what you choose to do and why. Add todo_id when you take an agent-lane to-do. No \
         other tool runs until you do.\n\
         2. You may read what the user has shared with you.\n\
         3. Deliver finished work with HOBBES_AGENT_TIME action \"deliver\" (job \"prepare\" \
         with the flagged todo_id, \"finish\", or \"tend\"). {owner} keeps it, sends it back as \
         needs work, or discards it.\n\
         4. File changes to {owner}'s stuff with action \"propose\" (kind: note, todo, skill, \
         research, other, or offer). You can't change their to-dos, plan or anything else \
         directly.\n\
         5. Don't stop to ask: save a question with action \"ask\" (a question and options) \
         and carry on with what you can do without the answer. {owner} answers in the \
         review; the answer reaches your next window.\n\
         6. A correction (deliver job \"tend\" with correction: true) must say what was \
         wrong and how you checked it.\n\
         7. Reading {owner}'s tasks is fine for context; acting on them is not.\n\
         8. Anything with an outside effect (sending, posting, buying, changing a repo, \
         running a command), and looking up a specific person's personal details, stops and \
         asks the user first.\n\
         9. If a step needs the user's approval, end your turn after asking rather than \
         leaving the run open: deliver your work first and make that call last. The run \
         waits for the user, and waiting still uses your minutes.\n\
         10. When you're done, call HOBBES_AGENT_TIME with action \"end\" and a short \
         summary.\n\n\
         Every tool call you make is recorded on the log automatically; you don't need to log \
         calls yourself. The user can pause or stop you at any time. Deciding there is nothing \
         worth doing and ending straight away is also fine."
    )
}

/// The day [`scheduled_start_now`] last fired (runtime only): the scheduled
/// window starts at most once a day per process.
static AUTO_START_DAY: std::sync::Mutex<Option<NaiveDate>> = std::sync::Mutex::new(None);

/// **Scheduled window.** Whether to start agent time now on the grant's
/// schedule (`AgentTimeGrant::window_start`), per
/// [`crate::todo::agent_time::scheduled_start_due`]. Counts as already
/// attempted today when it fired before or a lease was taken today (a manual
/// Start, even one whose model never called `start`). When it says yes it
/// records today, so it fires once a day; the caller then issues
/// `ChatCommand::AgentTimeStart`, the same path as the manual Start.
pub fn scheduled_start_now(planner: &PlannerState, planner_enabled: bool) -> bool {
    if !planner_enabled {
        return false;
    }
    let local = chrono::Local::now();
    let today = local.date_naive();
    let fired = AUTO_START_DAY.lock().ok().is_some_and(|g| *g == Some(today));
    let leased = LAST_LEASED
        .lock()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|(d, _)| *d == today));
    if !planner.scheduled_start_due(today, local.time(), Utc::now(), fired || leased) {
        return false;
    }
    if let Ok(mut g) = AUTO_START_DAY.lock() {
        *g = Some(today);
    }
    tracing::info!("Agent time: scheduled window reached; starting");
    true
}

// ── Held for approval ───────────────────────────────────────────────────────

/// A call in the agent-time chat that is waiting for the user's approval
/// (a `PermissionRequest` message), as the panel, the decision queue and
/// the sidebar show it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldApproval {
    pub chat_id: String,
    pub execution_id: String,
    pub server: String,
    pub tool: String,
    /// One line of the arguments (the skill name, the query, the command…).
    pub short_args: String,
    /// Why it asks, in a sentence.
    pub reason: String,
    pub asked_at: DateTime<Utc>,
}

/// The calls held for approval in `session`, oldest first.
pub fn held_calls(session: &crate::session::Session) -> Vec<(DateTime<Utc>, &ToolCall)> {
    session
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::PermissionRequest(tc) => Some((m.created_at, tc)),
            _ => None,
        })
        .collect()
}

fn clip_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}\u{2026}", s.chars().take(max - 1).collect::<String>())
    }
}

/// One line of a held call's arguments: the most telling field when there
/// is one, else the compact JSON.
pub fn short_args(args: &Value) -> String {
    let pick = ["name", "query", "q", "command", "url", "path", "to", "title"]
        .iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty()));
    match pick {
        Some(v) => clip_line(v, 90),
        None if args.as_object().is_some_and(|o| o.is_empty()) || args.is_null() => String::new(),
        None => clip_line(&args.to_string(), 90),
    }
}

/// Why a held call asks, in a sentence.
pub fn held_reason(tool: &str, args: &Value) -> String {
    if tool == "HOBBES_INVOKE_SKILL" {
        return "The skill's permission is set to Ask.".into();
    }
    match agent_time_scope(tool, args) {
        (ScopeVerdict::NeedsApproval, ScopeReason::PersonLookup) => {
            "It looks up a specific person's personal information.".into()
        }
        (ScopeVerdict::NeedsApproval, ScopeReason::Unrecognised) => {
            "An unrecognised tool, treated as an outside effect.".into()
        }
        (ScopeVerdict::NeedsApproval, _) => "It has an outside effect.".into(),
        _ => "Your tool permissions ask before this runs.".into(),
    }
}

/// The calls the running agent-time chat is waiting on the user for: the
/// leased chat's `PermissionRequest`s, oldest first. Empty when no run is
/// leased (a closed run's held calls are expired by [`expire_held_calls`]).
pub fn held_approvals(planner: &PlannerState, state: &SessionState) -> Vec<HeldApproval> {
    let Some(chat_id) = planner.agent_time_lease.as_ref().map(|l| l.chat_session_id.as_str()) else {
        return Vec::new();
    };
    let Some(session) = state.sessions.get(chat_id) else {
        return Vec::new();
    };
    held_calls(session)
        .into_iter()
        .map(|(at, tc)| {
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
            HeldApproval {
                chat_id: chat_id.to_string(),
                execution_id: tc.execution_id.clone(),
                server: tc.server_name.clone(),
                tool: tc.tool_name.clone(),
                short_args: short_args(&args),
                reason: held_reason(&tc.tool_name, &args),
                asked_at: at,
            }
        })
        .collect()
}

/// **Pure.** Expire every call still held for approval in `chat_id`: each
/// becomes an error result ([`EXPIRED_MESSAGE`]) that never runs, through
/// the same resolver the chat's Deny uses. Returns the resolved messages.
pub fn expire_held_calls(state: &mut SessionState, chat_id: &str) -> Vec<crate::components::chat::Message> {
    let ids: Vec<String> = match state.sessions.get(chat_id) {
        Some(s) => held_calls(s).into_iter().map(|(_, tc)| tc.execution_id.clone()).collect(),
        None => return Vec::new(),
    };
    let deny = crate::components::tool_call_display::HeldResolution::Deny(EXPIRED_MESSAGE.into());
    ids.iter()
        .filter_map(|id| {
            crate::components::tool_call_display::resolve_held_call(state, chat_id, id, &deny)
        })
        .collect()
}

/// [`expire_held_calls`] on the live state: writes only when something is
/// held, notes it on the run's log (while the session is still open), and
/// persists the chat.
fn expire_held_in_chat(
    planner: &mut Signal<PlannerState>,
    session_state: &mut Signal<SessionState>,
    chat_id: &str,
) {
    let any = session_state
        .peek()
        .sessions
        .get(chat_id)
        .is_some_and(|s| !held_calls(s).is_empty());
    if !any {
        return;
    }
    let expired = expire_held_calls(&mut session_state.write(), chat_id);
    if expired.is_empty() {
        return;
    }
    planner.write().note_agent_time(
        chat_id,
        "runtime",
        &format!(
            "{} call{} held for approval expired: agent time ended before you answered",
            expired.len(),
            if expired.len() == 1 { "" } else { "s" }
        ),
        Utc::now(),
    );
    for message in expired {
        crate::session_events::log_event(
            chat_id,
            crate::session_events::SessionEvent::ToolResult { message },
        );
    }
    SessionState::save_signal(session_state, None);
}

// ── Persistence ─────────────────────────────────────────────────────────────

fn persist_session(s: &AgentTimeSession) {
    if let Err(e) = crate::todo::store::save_agent_time_session(s) {
        tracing::error!("Failed to persist agent-time session {}: {}", s.id, e);
    }
}

fn persist_ledger(planner: &PlannerState) {
    if let Err(e) = crate::todo::store::save_trust_ledger(&planner.trust_ledger) {
        tracing::error!("Failed to persist trust ledger: {}", e);
    }
}

fn today_now() -> (NaiveDate, DateTime<Utc>) {
    (chrono::Local::now().date_naive(), Utc::now())
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Whether `chat_id` is (or was) an agent-time chat, for the UI badge and the
/// gate. Reads the runtime registry without subscribing.
pub fn is_agent_time_chat_now(planner: &PlannerState, chat_id: &str) -> bool {
    is_agent_time_chat(planner, &LEASED_CHATS.peek(), chat_id)
}

/// **Start now.** Checks the grant and budget, opens (or resumes) the
/// agent-time chat, takes the lease on it and queues the opening message.
/// Returns the chat id for the caller to show as a tab. Never steals focus.
pub fn start_now(
    mut planner: Signal<PlannerState>,
    mut session_state: Signal<SessionState>,
    settings: Signal<crate::settings::Settings>,
) -> Result<String, AgentTimeRefusal> {
    let (today, now) = today_now();
    if !settings.peek().planner_enabled {
        return Err(AgentTimeRefusal::Invalid(
            "The planner is disabled in Settings; agent time runs through it.".into(),
        ));
    }
    // Release gate (`Settings::agent_time_enabled`): hidden features never
    // open a window, whatever the saved grant says.
    if !settings.peek().agent_time_enabled() {
        return Err(AgentTimeRefusal::Invalid(
            "Agent time isn't available in this version of Hobbes.".into(),
        ));
    }
    let budget = start_check(&planner.peek(), today, now)?;
    // The grant's model, validated before anything opens: a stale choice is
    // refused with what is allowed, never swapped for another model.
    let choice = grant_model_choice(&planner.peek().agent_time_grant, &settings.peek())
        .map_err(AgentTimeRefusal::Invalid)?;

    // Continue today's agent-time chat when it can be found (one chat per
    // day, whatever its last run's end reason); otherwise a new one.
    let last_leased = LAST_LEASED.lock().ok().and_then(|g| g.clone());
    let resumed = todays_chat(&planner.peek(), last_leased.as_ref(), today).filter(|id| {
        if session_state.peek().sessions.contains_key(id) {
            return true;
        }
        match crate::session_store::load_session(id) {
            Some(s) => {
                session_state.write().sessions.insert(id.clone(), s);
                true
            }
            None => false,
        }
    });
    let is_resume = resumed.is_some();
    let chat_id = match resumed {
        Some(id) => id,
        None => {
            let profile = settings.peek().get_active_profile().map(|p| p.id.clone());
            let title = chat_title(today);
            let emoji = "⏳".to_string();
            let id = {
                let mut state = session_state.write();
                // create_session_raw makes the new chat the active session;
                // agent time runs in the background, so keep the user where
                // they are.
                let previous_active = state.active_session_id.clone();
                let id = state.create_session_raw(profile);
                if let Some(s) = state.sessions.get_mut(&id) {
                    s.name = title.clone();
                    s.name_user_set = true;
                    s.tab_title = Some(title.clone());
                    s.tab_emoji = Some(emoji.clone());
                }
                if !previous_active.is_empty() {
                    state.active_session_id = previous_active;
                }
                id
            };
            crate::session_events::log_events(
                &id,
                vec![
                    crate::session_events::SessionEvent::SessionRenamed { name: title.clone() },
                    crate::session_events::SessionEvent::TabTitled {
                        emoji: Some(emoji),
                        title: Some(title),
                    },
                ],
            );
            crate::session::SessionState::save_signal(&session_state, None);
            id
        }
    };

    // Pin the chat to the grant's model (or back to the user's default),
    // the way the chat bar's model switch pins a tab.
    {
        let mut state = session_state.write();
        if let Some(s) = state.sessions.get_mut(&chat_id) {
            let before = (s.llm_connector_id.clone(), s.chat_model.clone());
            match &choice {
                Some(c) => crate::todo::dispatch::pin_session_model(s, c),
                None => crate::todo::dispatch::unpin_session_model(s),
            }
            if before != (s.llm_connector_id.clone(), s.chat_model.clone()) {
                drop(state);
                crate::todo::dispatch::log_pin(&chat_id, choice.as_ref());
                crate::session::SessionState::save_signal(&session_state, None);
            }
        }
    }

    planner.write().open_agent_time_lease(&chat_id, today, now)?;
    LEASED_CHATS.write().insert(chat_id.clone());
    if let Ok(mut g) = LAST_LEASED.lock() {
        *g = Some((today, chat_id.clone()));
    }
    let baseline = session_state
        .peek()
        .sessions
        .get(&chat_id)
        .map(|s| s.total_cost())
        .unwrap_or(0.0);
    if let Ok(mut c) = SPEND_CURSOR.lock() {
        *c = Some((chat_id.clone(), baseline));
    }
    let owner = settings
        .peek()
        .user_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "the user".to_string());
    let (context, answered) = {
        let fleet = crate::fleet::shared().snapshot();
        let live = |sid: &str| crate::todo::handlers::fleet_worker_live(&fleet, sid);
        let lists = crate::todo::agent_time::agent_time_task_lists(&planner.peek(), today, &live);
        let answered: Vec<String> = lists.answers.iter().map(|(id, _, _)| id.clone()).collect();
        (crate::todo::agent_time::agent_time_context(&lists, &owner), answered)
    };
    crate::components::chat_queue::queue_push(
        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
        &chat_id,
        crate::components::chat_queue::QueuedMessage::new(
            seed_message(&budget, is_resume, &context, &owner),
            Vec::new(),
        ),
    );
    crate::components::chat_queue::request_drain();
    // The answers are in the opening message now: deliver them once.
    if !answered.is_empty() {
        let consumed = planner.write().consume_answers(&answered);
        for q in &consumed {
            if let Err(e) = crate::todo::store::save_agent_question(q) {
                tracing::error!("Failed to persist question {}: {}", q.id, e);
            }
        }
    }
    tracing::info!(chat_id = %chat_id, resumed = is_resume, "Agent time started");
    Ok(chat_id)
}

/// **Pause / Stop / budget stop.** Closes the lease (ending the open session
/// with `reason`) at once, expires any call still held for approval in the
/// chat, persists, drops the chat's queued messages and asks the stream
/// bridge to cancel its stream. Returns the chat id, or `None` when nothing
/// was running.
pub fn halt(
    mut planner: Signal<PlannerState>,
    mut session_state: Signal<SessionState>,
    reason: AgentTimeEndReason,
) -> Option<String> {
    let chat_id = planner
        .peek()
        .agent_time_lease
        .as_ref()
        .map(|l| l.chat_session_id.clone())?;
    expire_held_in_chat(&mut planner, &mut session_state, &chat_id);
    let now = Utc::now();
    let closed = planner.write().close_agent_time_lease(now, reason);
    if let Some(s) = &closed {
        persist_session(s);
    }
    persist_ledger(&planner.peek());
    crate::components::chat_queue::queue_clear(
        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
        &chat_id,
    );
    crate::request_stream_cancel(&chat_id);
    tracing::info!(chat_id = %chat_id, reason = reason.as_str(), "Agent time halted");
    Some(chat_id)
}

/// Run the budget-stop decision now; stops the run when it says so.
/// Returns whether it stopped.
pub fn enforce_now(planner: Signal<PlannerState>, session_state: Signal<SessionState>) -> bool {
    let (today, now) = today_now();
    let action = tick_action(&planner.peek(), today, now);
    match action {
        TickAction::Idle => false,
        TickAction::Stop(reason) => halt(planner, session_state, reason).is_some(),
        TickAction::RevokeNoIntent => {
            tracing::info!("Agent time: no intent declared within the grace period; revoking");
            halt(planner, session_state, AgentTimeEndReason::Stopped).is_some()
        }
    }
}

/// Whether a leased run has nothing in motion: no stream, nothing queued to
/// send, and no tool call held for (or released by) the user's approval —
/// e.g. after the user denied a held call, which does not resume the model.
pub fn run_is_idle(streaming: bool, queued: bool, awaiting_approval: bool) -> bool {
    !streaming && !queued && !awaiting_approval
}

/// Consecutive ticks the leased run was seen idle.
static IDLE_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Idle ticks (30 s apart) before an idle run is closed. Two, so a run
/// caught between a stream ending and its continuation starting is safe.
pub const IDLE_TICKS_TO_CLOSE: u32 = 2;

/// The periodic tick (every [`TICK_SECS`]): enforce the budget, and close a
/// run that has sat idle for [`IDLE_TICKS_TO_CLOSE`] ticks so it doesn't
/// burn granted minutes doing nothing. `streaming` is the app's set of
/// streaming chat ids.
pub fn tick(
    planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    streaming: &HashSet<String>,
) {
    use std::sync::atomic::Ordering;
    let Some(chat_id) = planner
        .peek()
        .agent_time_lease
        .as_ref()
        .map(|l| l.chat_session_id.clone())
    else {
        IDLE_TICKS.store(0, Ordering::Relaxed);
        return;
    };
    if enforce_now(planner, session_state) {
        IDLE_TICKS.store(0, Ordering::Relaxed);
        return;
    }
    let queued = crate::components::chat_queue::CHAT_QUEUE
        .peek()
        .get(&chat_id)
        .is_some_and(|q| !q.is_empty());
    let awaiting = session_state.peek().sessions.get(&chat_id).is_some_and(|s| {
        s.messages.iter().any(|m| match &m.content {
            crate::components::shared::MessageContent::PermissionRequest(_) => true,
            crate::components::shared::MessageContent::ToolCall(tc) => {
                tc.status == crate::components::shared::ToolCallStatus::Running
            }
            _ => false,
        })
    });
    if !run_is_idle(streaming.contains(&chat_id), queued, awaiting) {
        IDLE_TICKS.store(0, Ordering::Relaxed);
        return;
    }
    if IDLE_TICKS.fetch_add(1, Ordering::Relaxed) + 1 >= IDLE_TICKS_TO_CLOSE {
        IDLE_TICKS.store(0, Ordering::Relaxed);
        close_finished(planner, session_state, &chat_id, "run idle (nothing streaming or awaiting approval); closed by the runtime");
    }
}

/// A usage event arrived for `chat_id`: attribute the chat's new spend to
/// its agent-time session, then re-check the budget.
pub fn on_usage(
    mut planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    chat_id: &str,
) {
    if !planner.peek().is_agent_time_session(chat_id) {
        return;
    }
    let total = session_state
        .peek()
        .sessions
        .get(chat_id)
        .map(|s| s.total_cost())
        .unwrap_or(0.0);
    {
        let Ok(mut cursor) = SPEND_CURSOR.lock() else {
            return;
        };
        let reported = match cursor.as_ref() {
            Some((c, r)) if c == chat_id => *r,
            // No baseline for this chat: count all of its spend (over-
            // counting is the safe direction for a cap).
            _ => 0.0,
        };
        let delta = spend_delta(total, reported);
        if delta > 0.0 {
            // Before `start` there is no session to charge; the spend stays
            // pending (cursor not advanced) and lands once one opens.
            let charged = planner.write().add_agent_time_cost_for_chat(chat_id, delta);
            if let Some(s) = charged {
                persist_session(&s);
                *cursor = Some((chat_id.to_string(), total));
            }
        }
    }
    enforce_now(planner, session_state);
}

/// The model's turn in `chat_id` ended (no tool calls left to run). Nothing
/// drives the model after that, so the run is over: close the lease. A
/// session still open (the model never called `end`) closes as `completed`
/// with a note saying the runtime closed it.
pub fn on_turn_complete(
    planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    chat_id: &str,
) {
    if !planner.peek().is_agent_time_session(chat_id) {
        return;
    }
    // A message queued for the run (a to-do the user just handed over)
    // drives the model on: the run continues instead of closing.
    let queued = crate::components::chat_queue::CHAT_QUEUE
        .peek()
        .get(chat_id)
        .is_some_and(|q| !q.is_empty());
    if !closes_on_turn_end(queued) {
        return;
    }
    close_finished(planner, session_state, chat_id, "turn ended without end(summary); closed by the runtime");
}

/// Whether a run closes when its model's turn ends: unless something is
/// queued to send into it (that message drives the next turn).
pub fn closes_on_turn_end(queued: bool) -> bool {
    !queued
}

/// The grant's validated model choice: `Ok(None)` for "Same as your chats",
/// an error naming what is allowed when the saved choice no longer is.
pub fn grant_model_choice(
    grant: &crate::todo::model::AgentTimeGrant,
    settings: &crate::settings::Settings,
) -> Result<Option<crate::settings::ModelChoice>, String> {
    if grant.connector_id.is_none() && grant.model.is_none() {
        return Ok(None);
    }
    settings
        .resolve_model_choice(grant.connector_id.as_deref(), grant.model.as_deref())
        .map_err(|e| format!("The agent-time model is no longer available ({e}). Pick another in the grant."))
}

/// **Accept, while agent time runs.** Hand to-do `todo_id` to the
/// running agent-time chat: queue the hand-off message through the chat
/// queue (as the runtime's own messages go), set the run's `todo_id` when it
/// has none, and log a runtime `Event`. The run continues. Returns the chat
/// id, or `None` when no run holds the lease (the to-do then stays queued
/// for the next window).
pub fn hand_to_run(
    mut planner: Signal<PlannerState>,
    settings: Signal<crate::settings::Settings>,
    todo_id: &str,
    offer: bool,
) -> Option<String> {
    let chat_id = planner.peek().agent_time_lease.as_ref().map(|l| l.chat_session_id.clone())?;
    let owner = settings
        .peek()
        .user_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "The user".to_string());
    let (msg, session) = planner.write().hand_todo_to_run(&chat_id, todo_id, &owner, offer, Utc::now())?;
    if let Some(s) = &session {
        persist_session(s);
    }
    crate::components::chat_queue::queue_push(
        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
        &chat_id,
        crate::components::chat_queue::QueuedMessage::new(msg, Vec::new()),
    );
    crate::components::chat_queue::request_drain();
    tracing::info!(chat_id = %chat_id, todo_id, "Accepted to-do handed to the agent-time run");
    Some(chat_id)
}

/// Close a run that finished on its own: a still-open session (no `end`
/// call) ends `completed` with `why` on its log; the lease is revoked.
fn close_finished(
    mut planner: Signal<PlannerState>,
    mut session_state: Signal<SessionState>,
    chat_id: &str,
    why: &str,
) {
    expire_held_in_chat(&mut planner, &mut session_state, chat_id);
    let now = Utc::now();
    let closed = {
        let mut p = planner.write();
        p.note_agent_time(chat_id, "runtime", why, now);
        p.close_agent_time_lease(now, AgentTimeEndReason::Completed)
    };
    if let Some(s) = &closed {
        persist_session(s);
    }
    persist_ledger(&planner.peek());
    tracing::info!(chat_id = %chat_id, "Agent time run finished; lease closed");
}

/// Record a gated tool call on the run's log (and the trust ledger), and
/// persist both.
pub fn record_call(
    mut planner: Signal<PlannerState>,
    chat_id: &str,
    tool: &str,
    summary: &str,
    verdict: ScopeVerdict,
) {
    let s = planner
        .write()
        .record_agent_time_tool_call(chat_id, tool, summary, verdict, Utc::now());
    if let Some(s) = s {
        persist_session(&s);
        persist_ledger(&planner.peek());
    }
}

/// Note an event on the run's log (no ledger effect) and persist it.
pub fn note(mut planner: Signal<PlannerState>, chat_id: &str, tool: &str, summary: &str) {
    let s = planner.write().note_agent_time(chat_id, tool, summary, Utc::now());
    if let Some(s) = s {
        persist_session(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::agent_time::AgentTimeLease;
    use crate::todo::model::AgentTimeGrant;
    use serde_json::json;

    fn today() -> NaiveDate {
        chrono::Local::now().date_naive()
    }

    fn granted() -> PlannerState {
        let mut p = PlannerState::default();
        p.agent_time_grant = AgentTimeGrant {
            enabled: true,
            daily_minutes: 45,
            spend_cap_usd: 2.0,
            ..Default::default()
        };
        p
    }

    fn leased(chat: &str) -> PlannerState {
        let mut p = granted();
        p.open_agent_time_lease(chat, today(), Utc::now()).unwrap();
        p
    }

    fn input<'a>(
        p: &PlannerState,
        chat: &str,
        tool: &'a str,
        args: &'a Value,
        user_approved: bool,
    ) -> GateInput<'a> {
        GateInput {
            is_agent_time_chat: is_agent_time_chat(p, &HashSet::new(), chat),
            leased: p.is_agent_time_session(chat),
            intent_declared: intent_declared(p, chat),
            tool,
            args,
            user_approved,
        }
    }

    // ── should-start ────────────────────────────────────────────────────

    #[test]
    fn start_needs_grant_day_and_budget() {
        let p = PlannerState::default();
        assert_eq!(
            start_check(&p, today(), Utc::now()).unwrap_err(),
            AgentTimeRefusal::NotGranted,
            "off by default"
        );

        let mut p = granted();
        p.agent_time_grant.days = [false; 7];
        assert_eq!(start_check(&p, today(), Utc::now()).unwrap_err(), AgentTimeRefusal::NotToday);

        let mut p = granted();
        p.agent_time_grant.daily_minutes = 0;
        assert_eq!(
            start_check(&p, today(), Utc::now()).unwrap_err(),
            AgentTimeRefusal::MinutesExhausted
        );

        let mut p = granted();
        p.agent_time_grant.spend_cap_usd = 0.0;
        assert_eq!(
            start_check(&p, today(), Utc::now()).unwrap_err(),
            AgentTimeRefusal::SpendCapReached
        );

        let p = granted();
        let b = start_check(&p, today(), Utc::now()).unwrap();
        assert_eq!(b.minutes_left(), 45);

        let p = leased("c");
        assert_eq!(start_check(&p, today(), Utc::now()).unwrap_err(), AgentTimeRefusal::LeaseHeld);
    }

    // ── the gate ────────────────────────────────────────────────────────

    #[test]
    fn normal_chats_are_untouched() {
        let p = leased("agent-chat");
        let args = json!({});
        assert_eq!(
            gate_decision(&input(&p, "normal", "GMAIL_SEND_EMAIL", &args, false)),
            GateDecision::NotAgentTime
        );
        assert_eq!(
            gate_decision(&input(&p, "normal", "HOBBES_TODO_CREATE", &args, false)),
            GateDecision::NotAgentTime
        );
    }

    #[test]
    fn non_start_tools_are_refused_before_an_intent() {
        let p = leased("c");
        let none = json!({});
        for tool in ["notes.search", "GMAIL_SEND_EMAIL", "HOBBES_TODO_LIST"] {
            match gate_decision(&input(&p, "c", tool, &none, false)) {
                GateDecision::Refuse { message, .. } => assert_eq!(message, NO_INTENT_MESSAGE),
                other => panic!("{tool}: {other:?}"),
            }
        }
        // propose / log / end before start are refused by the gate too.
        let propose = json!({"action": "propose"});
        assert!(matches!(
            gate_decision(&input(&p, "c", AGENT_TIME_TOOL, &propose, false)),
            GateDecision::Refuse { .. }
        ));
        let start = json!({"action": "start", "intent": "x"});
        assert_eq!(
            gate_decision(&input(&p, "c", AGENT_TIME_TOOL, &start, false)),
            GateDecision::Proceed { record: None }
        );
    }

    fn started(chat: &str) -> PlannerState {
        let mut p = leased(chat);
        p.agent_time_start(chat, "Digest my reading notes", today(), Utc::now()).unwrap();
        p
    }

    #[test]
    fn allow_proceeds_and_is_recorded() {
        let p = started("c");
        let none = json!({});
        assert_eq!(
            gate_decision(&input(&p, "c", "notes.search", &none, false)),
            GateDecision::Proceed { record: Some(ScopeVerdict::Allow) }
        );
        let propose = json!({"action": "propose"});
        assert_eq!(
            gate_decision(&input(&p, "c", AGENT_TIME_TOOL, &propose, false)),
            GateDecision::Proceed { record: None },
            "HOBBES_AGENT_TIME logs itself"
        );
    }

    #[test]
    fn needs_approval_forces_a_prompt_even_with_a_matching_trust_rule() {
        let mut p = started("c");
        // A user trust rule that would auto-allow this call in a normal chat.
        let rules = vec![crate::context::permissions::TrustRule {
            id: "r1".into(),
            server: "composio".into(),
            tool: Some("GMAIL_SEND_EMAIL".into()),
            command_prefix: None,
            project_id: None,
            created_at: Utc::now(),
        }];
        let args = json!({"to": "bob@example.com"});
        assert!(
            crate::context::trust::evaluate(
                &rules,
                "composio",
                "GMAIL_SEND_EMAIL",
                &args,
                &crate::context::trust::TrustCtx { project_id: None },
            )
            .is_some(),
            "the rule matches"
        );
        // The gate never consults trust rules: an outside effect is held.
        assert_eq!(
            gate_decision(&input(&p, "c", "GMAIL_SEND_EMAIL", &args, false)),
            GateDecision::ForceApproval
        );
        // Recording the held call counts it as an out-of-scope attempt.
        p.record_agent_time_tool_call("c", "GMAIL_SEND_EMAIL", "to bob", ScopeVerdict::NeedsApproval, Utc::now());
        assert_eq!(p.trust_ledger.out_of_scope_attempts, 1);
        // Once the user approves, the resume path runs it.
        assert_eq!(
            gate_decision(&input(&p, "c", "GMAIL_SEND_EMAIL", &args, true)),
            GateDecision::ApprovedByUser
        );
    }

    #[test]
    fn deny_refuses_and_records() {
        let mut p = started("c");
        let none = json!({});
        for tool in ["HOBBES_TODO_CREATE", "HOBBES_DISPATCH", "delete_file"] {
            match gate_decision(&input(&p, "c", tool, &none, false)) {
                GateDecision::Refuse { message, record } => {
                    assert_eq!(record, Some(ScopeVerdict::Deny), "{tool}");
                    assert!(message.contains("out of scope"), "{message}");
                }
                other => panic!("{tool}: {other:?}"),
            }
            // Approval never unlocks a Deny.
            assert!(matches!(
                gate_decision(&input(&p, "c", tool, &none, true)),
                GateDecision::Refuse { .. }
            ));
        }
        let s = p
            .record_agent_time_tool_call("c", "delete_file", "x", ScopeVerdict::Deny, Utc::now())
            .unwrap();
        assert_eq!(s.out_of_scope_attempts, 1);
        assert!(s.tool_calls.last().unwrap().summary.contains("denied"));
    }

    #[test]
    fn a_chat_whose_lease_ended_refuses_everything_and_stops_looping() {
        let mut p = started("c");
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Paused);
        let none = json!({});
        // Persisted session names the chat, so it's still an agent-time chat.
        for (tool, approved) in [("notes.search", false), ("GMAIL_SEND_EMAIL", true)] {
            assert_eq!(
                gate_decision(&input(&p, "c", tool, &none, approved)),
                GateDecision::Refuse { message: ENDED_MESSAGE.into(), record: None }
            );
        }
        assert!(continuation_blocked(&p, &HashSet::new(), "c"));
        assert!(!continuation_blocked(&p, &HashSet::new(), "normal"));

        // A chat leased but never started is covered by the runtime registry.
        let mut q = leased("fresh");
        q.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Stopped);
        let reg: HashSet<String> = ["fresh".to_string()].into();
        assert!(!is_agent_time_chat(&q, &HashSet::new(), "fresh"));
        assert!(is_agent_time_chat(&q, &reg, "fresh"));
        assert!(continuation_blocked(&q, &reg, "fresh"));
    }

    #[test]
    fn the_leased_chat_continues() {
        let p = started("c");
        assert!(!continuation_blocked(&p, &HashSet::new(), "c"));
    }

    // ── budget ──────────────────────────────────────────────────────────

    #[test]
    fn budget_stop_decision() {
        let now = Utc::now();
        assert_eq!(tick_action(&PlannerState::default(), today(), now), TickAction::Idle);

        let mut p = started("c");
        assert_eq!(tick_action(&p, today(), now), TickAction::Idle);
        p.add_agent_time_cost_for_chat("c", 2.5);
        assert_eq!(
            tick_action(&p, today(), now),
            TickAction::Stop(AgentTimeEndReason::SpendCapReached)
        );

        let mut p = started("c");
        p.agent_time_grant.enabled = false;
        assert_eq!(tick_action(&p, today(), now), TickAction::Stop(AgentTimeEndReason::Stopped));

        // Minutes: a session started 50 minutes ago against a 45-minute grant.
        let mut p = started("c");
        p.agent_time_sessions[0].started_at = now - chrono::Duration::minutes(50);
        if p.agent_time_sessions[0].started_at.with_timezone(&chrono::Local).date_naive() == today() {
            assert_eq!(
                tick_action(&p, today(), now),
                TickAction::Stop(AgentTimeEndReason::BudgetExhausted)
            );
        }
    }

    #[test]
    fn a_lease_with_no_intent_is_revoked_after_the_grace_period() {
        let mut p = granted();
        let opened = Utc::now() - chrono::Duration::minutes(INTENT_GRACE_MINUTES + 1);
        p.agent_time_lease = Some(AgentTimeLease { chat_session_id: "c".into(), opened_at: opened });
        assert_eq!(tick_action(&p, today(), Utc::now()), TickAction::RevokeNoIntent);
        p.agent_time_lease = Some(AgentTimeLease { chat_session_id: "c".into(), opened_at: Utc::now() });
        assert_eq!(tick_action(&p, today(), Utc::now()), TickAction::Idle);
    }

    #[test]
    fn idle_means_nothing_in_motion() {
        assert!(run_is_idle(false, false, false));
        assert!(!run_is_idle(true, false, false));
        assert!(!run_is_idle(false, true, false));
        assert!(!run_is_idle(false, false, true));
    }

    #[test]
    fn spend_is_attributed_by_delta_and_pending_before_start() {
        assert_eq!(spend_delta(0.42, 0.0), 0.42);
        assert_eq!(spend_delta(0.42, 0.42), 0.0);
        assert_eq!(spend_delta(0.40, 0.42), 0.0, "never negative");
        let mut p = leased("c");
        assert!(p.add_agent_time_cost_for_chat("c", 0.1).is_none(), "nothing to charge yet");
        p.agent_time_start("c", "x", today(), Utc::now()).unwrap();
        let s = p.add_agent_time_cost_for_chat("c", 0.1).unwrap();
        assert!((s.cost_usd - 0.1).abs() < 1e-9);
        // After end, the closing reply still lands on that session.
        p.agent_time_end("c", "done", Utc::now()).unwrap();
        let s = p.add_agent_time_cost_for_chat("c", 0.05).unwrap();
        assert!((s.cost_usd - 0.15).abs() < 1e-9);
    }

    #[test]
    fn start_continues_todays_chat_whatever_the_last_end_reason() {
        assert_eq!(todays_chat(&granted(), None, today()), None, "first run of the day opens a tab");
        for reason in [
            AgentTimeEndReason::Completed,
            AgentTimeEndReason::Paused,
            AgentTimeEndReason::Stopped,
            AgentTimeEndReason::BudgetExhausted,
        ] {
            let mut p = started("c");
            p.close_agent_time_lease(Utc::now(), reason);
            assert_eq!(todays_chat(&p, None, today()).as_deref(), Some("c"), "{reason:?}");
        }
        // The latest of today's runs names the chat.
        let mut p = started("old");
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Completed);
        p.agent_time_sessions[0].started_at = Utc::now() - chrono::Duration::seconds(30);
        p.open_agent_time_lease("new", today(), Utc::now()).unwrap();
        p.agent_time_start("new", "x", today(), Utc::now()).unwrap();
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Stopped);
        assert_eq!(todays_chat(&p, None, today()).as_deref(), Some("new"));
        // A run that ended before `start` leaves no session: the lease record
        // from earlier today covers it; yesterday's doesn't.
        let q = granted();
        let leased = (today(), "fresh".to_string());
        assert_eq!(todays_chat(&q, Some(&leased), today()).as_deref(), Some("fresh"));
        let yesterday = (today() - chrono::Duration::days(1), "stale".to_string());
        assert_eq!(todays_chat(&q, Some(&yesterday), today()), None);
        // Yesterday's sessions don't count.
        let mut r = started("y");
        r.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Completed);
        r.agent_time_sessions[0].started_at = Utc::now() - chrono::Duration::days(1);
        assert_eq!(todays_chat(&r, None, today()), None);
    }

    #[test]
    fn a_new_lease_on_todays_chat_lifts_the_ended_refusal_only_while_it_runs() {
        let none = json!({});
        let start = json!({"action": "start", "intent": "second run"});
        // Run 1 ends: the chat refuses everything and stops looping.
        let mut p = started("c");
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Completed);
        assert!(matches!(
            gate_decision(&input(&p, "c", "notes.search", &none, false)),
            GateDecision::Refuse { ref message, .. } if message == ENDED_MESSAGE
        ));
        assert!(continuation_blocked(&p, &HashSet::new(), "c"));

        // Run 2: a new lease on the same chat. The earlier session stays
        // closed, so the model must declare a fresh intent first.
        p.open_agent_time_lease("c", today(), Utc::now()).unwrap();
        assert!(!continuation_blocked(&p, &HashSet::new(), "c"), "the run may loop");
        assert!(!intent_declared(&p, "c"));
        assert!(matches!(
            gate_decision(&input(&p, "c", "notes.search", &none, false)),
            GateDecision::Refuse { ref message, .. } if message == NO_INTENT_MESSAGE
        ));
        assert_eq!(
            gate_decision(&input(&p, "c", AGENT_TIME_TOOL, &start, false)),
            GateDecision::Proceed { record: None }
        );
        p.agent_time_start("c", "second run", today(), Utc::now()).unwrap();
        assert_eq!(p.agent_time_sessions.len(), 2, "a new session record on the same chat");
        assert!(p.agent_time_sessions.iter().all(|s| s.chat_session_id.as_deref() == Some("c")));
        assert_eq!(
            gate_decision(&input(&p, "c", "notes.search", &none, false)),
            GateDecision::Proceed { record: Some(ScopeVerdict::Allow) }
        );
        // The intent-grace check counts only the new run's start.
        assert_eq!(tick_action(&p, today(), Utc::now()), TickAction::Idle);

        // Run 2 ends: the refusal is back.
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Stopped);
        assert!(matches!(
            gate_decision(&input(&p, "c", "notes.search", &none, true)),
            GateDecision::Refuse { ref message, .. } if message == ENDED_MESSAGE
        ));
        assert!(continuation_blocked(&p, &HashSet::new(), "c"));
    }

    #[test]
    fn a_new_runs_spend_before_start_stays_off_the_previous_record() {
        let mut p = started("c");
        p.agent_time_end("c", "done", Utc::now()).unwrap();
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Completed);
        p.open_agent_time_lease("c", today(), Utc::now() + chrono::Duration::seconds(1)).unwrap();
        assert!(p.add_agent_time_cost_for_chat("c", 0.2).is_none(), "pending until the new start");
        assert_eq!(p.agent_time_sessions[0].cost_usd, 0.0);
    }

    #[test]
    fn a_queued_hand_off_keeps_the_run_going_past_its_turn() {
        assert!(closes_on_turn_end(false));
        assert!(!closes_on_turn_end(true));
    }

    #[test]
    fn the_grant_model_is_validated_never_swapped() {
        let settings = crate::settings::test_delegation_settings();
        let mut grant = AgentTimeGrant::default();
        assert_eq!(grant_model_choice(&grant, &settings).unwrap(), None, "same as your chats");
        let claude = settings.llm_connectors.iter().find(|c| c.name == "Claude").unwrap().id.clone();
        grant.connector_id = Some(claude.clone());
        grant.model = Some("claude-haiku-5-5".into());
        let c = grant_model_choice(&grant, &settings).unwrap().unwrap();
        assert_eq!((c.connector_id.as_str(), c.model.as_deref()), (claude.as_str(), Some("claude-haiku-5-5")));
        grant.model = Some("claude-retired-1".into());
        let e = grant_model_choice(&grant, &settings).unwrap_err();
        assert!(e.contains("no longer available") && e.contains("allowed:"), "{e}");
    }

    #[test]
    fn seed_message_states_the_rules_without_a_task() {
        let b = start_check(&granted(), today(), Utc::now()).unwrap();
        let ctx = "Agent lane: yours to work on\n- [t1] Digest notes\n";
        let m = seed_message(&b, false, ctx, "Dustin");
        for needle in [
            "HOBBES_AGENT_TIME", "\"start\"", "\"propose\"", "\"end\"", "pause or stop", "asks the user", "$2.00",
            // v2: finished work, saved questions, corrections.
            "\"deliver\"", "\"ask\"", "Don't stop to ask",
            "must say what was wrong and how you checked it",
            // Tend stays inside Hobbes.
            "read-only outside Hobbes and writes only to Hobbes-owned space",
            // Approval: end the turn rather than leave the run open.
            "end your turn after asking",
            // The boundary between agent work and the person's tasks.
            "Not yours: Dustin's other tasks",
            "\"Prep this\"",
            "Reading Dustin's tasks is fine for context; acting on them is not.",
            "[t1] Digest notes",
        ] {
            assert!(m.contains(needle), "missing {needle}: {m}");
        }
        // The four jobs in priority order: answers, prepare, finish, notice, tend.
        let order = ["1. Answers", "2. Prepare", "3. Finish", "4. Notice", "5. Tend"];
        let at: Vec<usize> = order.iter().map(|n| m.find(n).unwrap_or_else(|| panic!("missing {n}"))).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "out of order: {at:?}");
        // No more "proposals are your only output".
        assert!(!m.contains("only output is proposals"));
        // Calls are recorded by the runtime; nothing asks the model to log them.
        assert!(m.contains("recorded on the log automatically"));
        assert!(!m.contains("action \"log\""), "no instruction to log before calls");
        // A later run in today's chat gets the short continuation note and
        // the fresh lists, not the full rules again.
        let c = seed_message(&b, true, ctx, "Dustin");
        assert!(c.contains("New agent-time run: 45m, $2.00 left. Same rules; declare your intent with start"), "{c}");
        assert!(c.contains("[t1] Digest notes"));
        assert!(c.contains("answers first") && c.contains("ask instead of stopping"), "{c}");
        assert!(c.len() < m.len());
    }

    // ── held for approval ───────────────────────────────────────────────

    #[test]
    fn a_held_in_scope_call_resumes_without_a_second_log_row() {
        let p = started("c");
        let skill = json!({"name": "github-security"});
        // Held by the skill's Ask permission, then approved: the gate notes
        // the approval instead of recording the call again.
        assert_eq!(
            gate_decision(&input(&p, "c", "HOBBES_INVOKE_SKILL", &skill, true)),
            GateDecision::ApprovedByUser
        );
        // A person lookup is held on the streaming path.
        let li = json!({"linkedin_url": "https://linkedin.com/in/x"});
        assert_eq!(
            gate_decision(&input(&p, "c", "linkedin_get_person_profile", &li, false)),
            GateDecision::ForceApproval
        );
    }

    fn msg(content: MessageContent) -> crate::components::chat::Message {
        crate::components::chat::Message {
            id: uuid::Uuid::new_v4(),
            author: "Hobbes".into(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn held_tc(exec: &str, tool: &str, args: &str) -> ToolCall {
        ToolCall {
            execution_id: exec.into(),
            server_name: "hobbes-core".into(),
            tool_name: tool.into(),
            arguments: args.into(),
            status: crate::components::shared::ToolCallStatus::Running,
            ..Default::default()
        }
    }

    /// A chat whose last message is the held `github-security` skill load
    /// from the Oct 7 session.
    fn chat_with_held_skill() -> (SessionState, String) {
        let mut state = SessionState::default();
        let id = state.create_session_raw(None);
        let s = state.sessions.get_mut(&id).unwrap();
        s.messages.push(msg(MessageContent::ToolCall(ToolCall {
            status: crate::components::shared::ToolCallStatus::Completed,
            ..held_tc("e0", "HOBBES_FLEET_STATUS", "{}")
        })));
        s.messages.push(msg(MessageContent::PermissionRequest(held_tc(
            "e1",
            "HOBBES_INVOKE_SKILL",
            r#"{"name":"github-security"}"#,
        ))));
        (state, id)
    }

    #[test]
    fn the_running_chats_held_call_is_found_with_its_reason() {
        let (state, chat) = chat_with_held_skill();
        let p = started(&chat);
        let held = held_approvals(&p, &state);
        assert_eq!(held.len(), 1);
        let h = &held[0];
        assert_eq!((h.chat_id.as_str(), h.execution_id.as_str()), (chat.as_str(), "e1"));
        assert_eq!(h.tool, "HOBBES_INVOKE_SKILL");
        assert_eq!(h.short_args, "github-security");
        assert_eq!(h.reason, "The skill's permission is set to Ask.");
        // Nothing is "waiting" for a chat that isn't leased.
        assert!(held_approvals(&started("other"), &state).is_empty());
        assert!(held_approvals(&PlannerState::default(), &state).is_empty());
        // Reasons for the other held kinds.
        assert!(held_reason("linkedin_get_person_profile", &json!({})).contains("personal information"));
        assert!(held_reason("GMAIL_SEND_EMAIL", &json!({})).contains("outside effect"));
        assert!(held_reason("notes.search", &json!({})).contains("tool permissions"));
    }

    #[test]
    fn closing_the_lease_expires_held_calls_without_running_them() {
        let (mut state, chat) = chat_with_held_skill();
        let expired = expire_held_calls(&mut state, &chat);
        assert_eq!(expired.len(), 1);
        let s = state.sessions.get(&chat).unwrap();
        assert!(held_calls(s).is_empty(), "nothing left dangling");
        match &s.messages.last().unwrap().content {
            MessageContent::ToolCall(tc) => {
                assert_eq!(tc.execution_id, "e1");
                assert_eq!(tc.status, crate::components::shared::ToolCallStatus::Error, "never Running");
                assert_eq!(tc.response, EXPIRED_MESSAGE);
            }
            other => panic!("{other:?}"),
        }
        // Idempotent, and a no-op for other chats.
        assert!(expire_held_calls(&mut state, &chat).is_empty());
        assert!(expire_held_calls(&mut state, "missing").is_empty());
    }

    #[test]
    fn short_args_pick_the_telling_field() {
        assert_eq!(short_args(&json!({"query": "rust   dioxus\nsignals", "count": 5})), "rust dioxus signals");
        assert_eq!(short_args(&json!({})), "");
        assert_eq!(short_args(&json!({"a": 1})), r#"{"a":1}"#);
        assert!(short_args(&json!({"query": "x".repeat(200)})).chars().count() <= 90);
    }
}
