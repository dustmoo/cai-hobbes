// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

//! The agent-time runtime (SYMBIOTIC_PLANNER.md §3): starts, gates, meters
//! and stops granted agent time. `todo::agent_time` holds the state logic
//! (grant, budget, lease, log, proposals, scope check); this module drives it
//! from the app.
//!
//! Lifecycle:
//!
//! 1. **Start** (`ChatCommand::AgentTimeStart`, user-initiated only):
//!    [`start_now`] checks the grant and today's budget ([`start_check`]),
//!    opens a dedicated chat ("Agent time · <date>") — or reuses today's
//!    paused one — takes the lease on it, and queues the opening message
//!    ([`seed_message`]). The model must then call `HOBBES_AGENT_TIME start`.
//! 2. **Gate**: every tool call from an agent-time chat passes
//!    [`gate_decision`] before the normal permission flow, on both the
//!    streaming path and the approval-resume path
//!    (`builtin_tools::agent_time_gate`). Every verdict is recorded.
//! 3. **Meter**: each usage event attributes the chat's new spend to the open
//!    session ([`on_usage`]) and re-checks the budget; a 30 s tick
//!    ([`tick`]) does the same for minutes.
//! 4. **End**: the model's `end`, the turn ending (nothing drives the model
//!    after it stops — [`on_turn_complete`] closes the lease), Pause / Stop
//!    ([`halt`]: lease closed and stream cancelled at once), or the budget
//!    running out.
//!
//! After the lease is gone the chat stays marked ([`is_agent_time_chat`]):
//! its tool calls are refused and its tool loop does not continue, so
//! nothing it started can keep running outside agent time.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, Utc};
use dioxus::prelude::*;
use serde_json::Value;

use crate::session::SessionState;
use crate::todo::agent_time::{
    agent_time_scope_allows, AgentTimeBudget, AgentTimeRefusal, ScopeVerdict,
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
    /// An outside effect the user has just approved: run it (no second
    /// ledger count — it was counted when it was held).
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
        ScopeVerdict::Allow => GateDecision::Proceed { record: Some(ScopeVerdict::Allow) },
        ScopeVerdict::NeedsApproval if i.user_approved => GateDecision::ApprovedByUser,
        ScopeVerdict::NeedsApproval => GateDecision::ForceApproval,
        ScopeVerdict::Deny => GateDecision::Refuse {
            message: format!(
                "'{}' is out of scope for agent time and was refused. Agent time may read what \
                 the user has shared and write only proposals (HOBBES_AGENT_TIME propose); the \
                 user's to-dos, plan, timers, workers and anything destructive are off limits. \
                 This attempt is logged.",
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

/// The chat a Start resumes: the latest agent-time session started today,
/// if it ended `Paused`.
pub fn resumable_chat(planner: &PlannerState, today: NaiveDate) -> Option<String> {
    planner
        .agent_time_sessions
        .iter()
        .filter(|s| s.started_at.with_timezone(&chrono::Local).date_naive() == today)
        .max_by_key(|s| s.started_at)
        .filter(|s| s.end_reason == Some(AgentTimeEndReason::Paused))
        .and_then(|s| s.chat_session_id.clone())
}

/// The chat title for a run on `date`.
pub fn chat_title(date: NaiveDate) -> String {
    format!("Agent time · {}", date.format("%b %-d"))
}

/// The opening message of an agent-time chat. It states the grant, the rules
/// and the controls, and deliberately suggests no task: the model picks.
pub fn seed_message(budget: &AgentTimeBudget, resumed: bool) -> String {
    let minutes = crate::todo::model::format_minutes(budget.minutes_left());
    let spend = budget.spend_left_usd();
    let opening = if resumed {
        format!(
            "[Agent time resumed: Hobbes opened this on the user's behalf]\n\n\
             The user paused your agent time earlier and has resumed it. You have up to \
             {minutes} left today, with ${spend:.2} of spend. Your earlier session was closed \
             when it was paused, so declare your intent again (the same one is fine if you \
             still choose it)."
        )
    } else {
        format!(
            "[Agent time: Hobbes opened this on the user's behalf]\n\n\
             The user has granted you self-directed time: up to {minutes} today, with \
             ${spend:.2} of spend. Nothing here is assigned. What you do with it is your choice."
        )
    };
    format!(
        "{opening}\n\n\
         How it works:\n\
         1. First call HOBBES_AGENT_TIME with action \"start\" and an intent: one or two lines \
         on what you choose to do and why. No other tool runs until you do.\n\
         2. You may read what the user has shared with you.\n\
         3. Your only output is proposals, filed with HOBBES_AGENT_TIME action \"propose\" \
         (kind: note, todo, skill, research or other). The user will accept, edit or dismiss \
         each one. You can't change their to-dos, plan or anything else directly.\n\
         4. Anything with an outside effect (sending, posting, buying, changing a repo, \
         running a command) stops and asks the user first.\n\
         5. When you're done, call HOBBES_AGENT_TIME with action \"end\" and a short summary.\n\n\
         The user can pause or stop you at any time, and every tool call is logged where they \
         can read it. Deciding there is nothing worth doing and ending straight away is also \
         fine."
    )
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
    let budget = start_check(&planner.peek(), today, now)?;

    // Resume today's paused chat when it can be found; otherwise a new one.
    let resumed = resumable_chat(&planner.peek(), today).filter(|id| {
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

    planner.write().open_agent_time_lease(&chat_id, today, now)?;
    LEASED_CHATS.write().insert(chat_id.clone());
    let baseline = session_state
        .peek()
        .sessions
        .get(&chat_id)
        .map(|s| s.total_cost())
        .unwrap_or(0.0);
    if let Ok(mut c) = SPEND_CURSOR.lock() {
        *c = Some((chat_id.clone(), baseline));
    }
    crate::components::chat_queue::queue_push(
        &mut crate::components::chat_queue::CHAT_QUEUE.write(),
        &chat_id,
        crate::components::chat_queue::QueuedMessage::new(
            seed_message(&budget, is_resume),
            Vec::new(),
        ),
    );
    crate::components::chat_queue::request_drain();
    tracing::info!(chat_id = %chat_id, resumed = is_resume, "Agent time started");
    Ok(chat_id)
}

/// **Pause / Stop / budget stop.** Closes the lease (ending the open session
/// with `reason`) at once, persists, drops the chat's queued messages and
/// asks the stream bridge to cancel its stream. Returns the chat id, or
/// `None` when nothing was running.
pub fn halt(mut planner: Signal<PlannerState>, reason: AgentTimeEndReason) -> Option<String> {
    let chat_id = planner
        .peek()
        .agent_time_lease
        .as_ref()
        .map(|l| l.chat_session_id.clone())?;
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
pub fn enforce_now(planner: Signal<PlannerState>) -> bool {
    let (today, now) = today_now();
    let action = tick_action(&planner.peek(), today, now);
    match action {
        TickAction::Idle => false,
        TickAction::Stop(reason) => halt(planner, reason).is_some(),
        TickAction::RevokeNoIntent => {
            tracing::info!("Agent time: no intent declared within the grace period; revoking");
            halt(planner, AgentTimeEndReason::Stopped).is_some()
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
    if enforce_now(planner) {
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
        close_finished(planner, &chat_id, "run idle (nothing streaming or awaiting approval); closed by the runtime");
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
    enforce_now(planner);
}

/// The model's turn in `chat_id` ended (no tool calls left to run). Nothing
/// drives the model after that, so the run is over: close the lease. A
/// session still open (the model never called `end`) closes as `completed`
/// with a note saying the runtime closed it.
pub fn on_turn_complete(planner: Signal<PlannerState>, chat_id: &str) {
    if !planner.peek().is_agent_time_session(chat_id) {
        return;
    }
    close_finished(planner, chat_id, "turn ended without end(summary); closed by the runtime");
}

/// Close a run that finished on its own: a still-open session (no `end`
/// call) ends `completed` with `why` on its log; the lease is revoked.
fn close_finished(mut planner: Signal<PlannerState>, chat_id: &str, why: &str) {
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
    fn resume_picks_todays_paused_chat_only() {
        let mut p = started("c");
        assert_eq!(resumable_chat(&p, today()), None, "still running");
        p.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Paused);
        assert_eq!(resumable_chat(&p, today()).as_deref(), Some("c"));
        let mut q = started("d");
        q.close_agent_time_lease(Utc::now(), AgentTimeEndReason::Stopped);
        assert_eq!(resumable_chat(&q, today()), None);
    }

    #[test]
    fn seed_message_states_the_rules_without_a_task() {
        let b = start_check(&granted(), today(), Utc::now()).unwrap();
        let m = seed_message(&b, false);
        for needle in ["HOBBES_AGENT_TIME", "\"start\"", "\"propose\"", "\"end\"", "pause or stop", "asks the user", "$2.00"] {
            assert!(m.contains(needle), "missing {needle}: {m}");
        }
        assert!(seed_message(&b, true).contains("resumed"));
    }
}
