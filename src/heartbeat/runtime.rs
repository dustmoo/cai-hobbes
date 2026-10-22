//! The Heartbeat runtime: the polling loop spawned from `main.rs`.
//!
//! Every [`TICK_SECS`] while `heartbeat_active`: capture the enabled
//! sources (planner, Fleet; CI every `ci::CI_POLL_MINUTES` on a tokio task),
//! decide salience, ingest, then wake the dedicated Heartbeat session if
//! `wake::decide_wake` says so — by queueing a user message for the
//! background chat queue, exactly like an unfocused dispatch. Finished wakes
//! get their cost and outcome read back from the usage log and the session.
//!
//! Everything here runs on the Dioxus side (only it may touch Signals); a
//! tick is synchronous, so no signal guard ever crosses an `.await`.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

use super::wake::{self, TodoInfo, WakeBrief, WakeTrigger, WakeVerdict};
use super::{
    agent_project_ids, check_wake, ci, intention_todo_ids, local_day_start, spent_since, Budget,
    CiRun, FleetSnapshot, PlannerSnapshot, RulesSalience, Salience, SalienceCtx, WakeRecord,
    HEARTBEAT,
};
use crate::components::chat_queue;
use crate::session::SessionState;
use crate::settings::Settings;

/// Poll interval.
pub const TICK_SECS: u64 = 30;
/// Title of the dedicated session.
pub const SESSION_TITLE: &str = "Heartbeat";
/// At most this many projects are polled for CI per round (projects with
/// open agent work first), to stay gentle on `gh`'s rate limits.
const MAX_CI_PROJECTS: usize = 12;
/// Persist at least this often even when nothing happened, so a restart
/// diffs against recent snapshots.
const SAVE_EVERY_MINUTES: i64 = 10;

/// The app state the runtime reads and writes.
#[derive(Clone, Copy)]
pub struct HeartbeatDeps {
    pub session_state: Signal<SessionState>,
    pub settings: Signal<Settings>,
    pub planner: Signal<crate::todo::PlannerState>,
    /// The Fleet UI mirror (fed by main.rs's drain loop).
    pub fleet: Signal<crate::fleet::FleetState>,
    pub usage_log: Signal<crate::usage_log::UsageLog>,
}

/// Loop-local state that doesn't need persisting.
#[derive(Default)]
struct Pacer {
    last_ci: Option<DateTime<Utc>>,
    ci_task: Option<tokio::task::JoinHandle<Vec<CiRun>>>,
    last_save: Option<DateTime<Utc>>,
}

/// Run forever. `streaming` reports the sessions mid-turn (main.rs's
/// `STREAMING_SESSIONS`, which this module can't reach).
pub async fn run(deps: HeartbeatDeps, streaming: impl Fn() -> HashSet<String>) {
    let mut pacer = Pacer::default();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
        // A finished CI poll is collected here, outside any signal guard.
        let ci_runs = match pacer.ci_task.as_ref().map(|t| t.is_finished()) {
            Some(true) => match pacer.ci_task.take() {
                Some(task) => task.await.ok(),
                None => None,
            },
            _ => None,
        };
        tick(deps, &streaming(), ci_runs, &mut pacer);
    }
}

fn tick(deps: HeartbeatDeps, streaming: &HashSet<String>, ci_runs: Option<Vec<CiRun>>, pacer: &mut Pacer) {
    let settings = deps.settings.peek().clone();
    let now = Utc::now();
    let active = super::heartbeat_active(&settings);
    let mut dirty = false;

    let session_id = if active {
        ensure_session(deps, &settings, &mut dirty)
    } else {
        HEARTBEAT.peek().session_id.clone()
    };
    // Finished wakes are read back even when the Heartbeat was just turned
    // off, so the trail doesn't keep a wake open forever.
    if let Some(sid) = &session_id {
        dirty |= complete_finished_wakes(deps, sid, streaming, now);
    }
    if !active {
        if dirty {
            super::save_signal();
        }
        return;
    }

    // ── Perception ──
    let mut events = Vec::new();
    {
        let mut hb = HEARTBEAT.write();
        if settings.heartbeat_source_planner {
            let snap = PlannerSnapshot::capture(&deps.planner.peek(), now);
            events.extend(hb.observe_planner(snap, now));
        } else {
            // Re-enabling starts from a fresh (silent) capture rather than
            // diffing against a stale one.
            hb.planner = None;
        }
        if settings.heartbeat_source_fleet {
            let snap = FleetSnapshot::capture(&deps.fleet.peek());
            events.extend(hb.observe_fleet(snap, now));
        } else {
            hb.fleet = None;
        }
        if settings.heartbeat_source_ci {
            if let Some(runs) = &ci_runs {
                events.extend(hb.observe_ci(runs));
                dirty = true;
            }
        } else {
            hb.ci = None;
        }
    }
    if settings.heartbeat_source_ci {
        maybe_start_ci_poll(deps, pacer, now);
    }

    // ── Salience + journal ──
    if !events.is_empty() {
        let ctx = salience_ctx(deps, &settings, session_id.as_deref());
        // The seam for a shadow decider: wrap in `ShadowSalience::new(
        // RulesSalience, shadow)` and hand `take_shadow_decisions()` to
        // ingest alongside these. There is no shadow yet.
        let salience = RulesSalience;
        let decisions = salience.decide(&events, &ctx);
        tracing::debug!("heartbeat: {} event(s) decided by {}", events.len(), salience.name());
        HEARTBEAT.write().ingest(events, decisions, now);
        dirty = true;
    }

    // ── Wake ──
    if let Some(sid) = &session_id {
        dirty |= maybe_wake(deps, &settings, sid, streaming, now);
    }

    if dirty || pacer.last_save.is_none_or(|t| now - t >= Duration::minutes(SAVE_EVERY_MINUTES)) {
        super::save_signal();
        pacer.last_save = Some(now);
    }
}

// ── The dedicated session ───────────────────────────────────────────────────

/// The Heartbeat session's id, hydrating it into memory if it is only on
/// disk and creating it if it never existed or was deleted. Never touches
/// the active session or the open tabs.
fn ensure_session(deps: HeartbeatDeps, settings: &Settings, dirty: &mut bool) -> Option<String> {
    let mut session_state = deps.session_state;
    if session_state.peek().save_disabled {
        // Startup couldn't read the store: don't create rows we can't save.
        return None;
    }
    if let Some(id) = HEARTBEAT.peek().session_id.clone() {
        if session_state.peek().sessions.contains_key(&id) {
            return Some(id);
        }
        // One indexed row: fine on the UI thread (P-017).
        if let Some(session) = crate::session_store::load_session(&id) {
            session_state.write().sessions.insert(id.clone(), session);
            return Some(id);
        }
        tracing::info!("heartbeat: session {id} is gone; creating a new one");
    }
    // Like every other new tab: the active Composio profile.
    let profile = settings.get_active_profile().map(|p| p.id.clone());
    let id = {
        let mut state = session_state.write();
        // create_session_raw makes the new session active; put it back.
        let previous_active = state.active_session_id.clone();
        let id = state.create_session_raw(profile);
        if !previous_active.is_empty() {
            state.active_session_id = previous_active;
        }
        // A user-set name, so auto-titling never renames it.
        state.update_session_name_raw(&id, SESSION_TITLE.to_string());
        id
    };
    SessionState::save_signal(&session_state, None);
    HEARTBEAT.write().session_id = Some(id.clone());
    *dirty = true;
    tracing::info!("heartbeat: created session {id}");
    Some(id)
}

/// Mid-turn, a message waiting in its queue, or a call waiting on approval.
fn session_busy(deps: HeartbeatDeps, session_id: &str, streaming: &HashSet<String>) -> bool {
    streaming.contains(session_id)
        || chat_queue::CHAT_QUEUE.peek().get(session_id).is_some_and(|q| !q.is_empty())
        || deps
            .session_state
            .peek()
            .sessions
            .get(session_id)
            .is_some_and(|s| wake::has_pending_approval(&s.messages))
}

// ── Perception helpers ──────────────────────────────────────────────────────

/// Projects polled for CI: every open project with a folder, those with
/// open agent work first, capped. All of them (not only agent-work ones)
/// because a failure elsewhere is still worth a queued mention (salience
/// row 4); the cap and `gh`'s own per-call timeout bound the cost.
fn ci_projects(planner: &crate::todo::PlannerState) -> Vec<(String, String, String)> {
    let agent = agent_project_ids(planner);
    let mut out: Vec<(bool, (String, String, String))> = planner
        .projects
        .iter()
        .filter(|p| !p.status.is_closed())
        .filter_map(|p| {
            let path = p.path.as_deref()?.trim();
            (!path.is_empty()).then(|| (agent.contains(&p.id), (p.id.clone(), p.title.clone(), path.to_string())))
        })
        .collect();
    out.sort_by_key(|(has_agent, _)| !*has_agent);
    out.into_iter().take(MAX_CI_PROJECTS).map(|(_, p)| p).collect()
}

/// Start a CI poll on a tokio task (`gh` runs off the UI thread) when one is
/// due and none is in flight; [`run`] collects it.
fn maybe_start_ci_poll(deps: HeartbeatDeps, pacer: &mut Pacer, now: DateTime<Utc>) {
    if pacer.ci_task.is_some()
        || pacer.last_ci.is_some_and(|t| now - t < Duration::minutes(ci::CI_POLL_MINUTES))
    {
        return;
    }
    pacer.last_ci = Some(now);
    let projects = ci_projects(&deps.planner.peek());
    if projects.is_empty() {
        return;
    }
    pacer.ci_task = Some(tokio::spawn(ci::fetch_all(projects)));
}

fn salience_ctx(deps: HeartbeatDeps, settings: &Settings, session_id: Option<&str>) -> SalienceCtx {
    let mut ctx = SalienceCtx::from_settings(settings);
    {
        let state = deps.session_state.peek();
        ctx.intention_todo_ids =
            intention_todo_ids(state.sessions.values().flat_map(|s| s.scheduled_timers.iter()));
        if let Some(sid) = session_id {
            let messages = state.sessions.get(sid).map(|s| s.messages.as_slice()).unwrap_or(&[]);
            ctx.heartbeat_todo_ids = wake::heartbeat_todo_ids(&deps.fleet.peek(), sid, messages);
        }
    }
    ctx.agent_project_ids = agent_project_ids(&deps.planner.peek());
    ctx.heartbeat_session_id = session_id.map(str::to_string);
    ctx
}

// ── Waking ──────────────────────────────────────────────────────────────────

/// Decide and act on a wake. True when the state changed.
fn maybe_wake(
    deps: HeartbeatDeps,
    settings: &Settings,
    session_id: &str,
    streaming: &HashSet<String>,
    now: DateTime<Utc>,
) -> bool {
    let budget = Budget::from_settings(settings);
    let busy = session_busy(deps, session_id, streaming);
    let verdict = {
        let hb = HEARTBEAT.peek();
        let spent = spent_since(&hb.trail, local_day_start(now));
        wake::decide_wake(&hb, &budget, spent, now, busy)
    };
    match verdict {
        WakeVerdict::Idle | WakeVerdict::Busy => false,
        WakeVerdict::Defer { trigger, reason } => {
            let mut hb = HEARTBEAT.write();
            let label = wake::trigger_label(&hb, &trigger);
            let ids = hb.queue.clone();
            hb.record_wake(WakeRecord::deferred(now, label, ids, reason));
            true
        }
        WakeVerdict::Fire(trigger) => {
            fire_wake(deps, settings, session_id, &budget, trigger, now);
            true
        }
    }
}

fn fire_wake(
    deps: HeartbeatDeps,
    settings: &Settings,
    session_id: &str,
    budget: &Budget,
    trigger: WakeTrigger,
    now: DateTime<Utc>,
) {
    let todos: HashMap<String, TodoInfo> = {
        let hb = HEARTBEAT.peek();
        let planner = deps.planner.peek();
        hb.queued()
            .iter()
            .filter_map(|e| e.todo_id.as_deref())
            .filter_map(|id| {
                planner.todo(id).map(|t| {
                    (id.to_string(), TodoInfo { title: t.title.clone(), latest_progress: t.latest_progress.clone() })
                })
            })
            .collect()
    };
    let trust = super::trust::trust_summary(settings.planner_enabled, settings.agent_time_enabled());
    let text = {
        let mut hb = HEARTBEAT.write();
        let spent = spent_since(&hb.trail, local_day_start(now));
        // decide_wake already checked; re-checking here keeps the invariant
        // local should the two ever drift.
        if let Err(reason) = check_wake(&hb.trail, budget, spent, now) {
            let label = wake::trigger_label(&hb, &trigger);
            let ids = hb.queue.clone();
            hb.record_wake(WakeRecord::deferred(now, label, ids, reason));
            return;
        }
        let why = wake::trigger_label(&hb, &trigger);
        let rule = wake::rule_label(&trigger, &hb);
        let ids = hb.queue.clone();
        let choices = wake::choice_summary(&hb, &ids);
        let last_wake = wake::last_fired_at(&hb.trail);
        let budget_line = wake::budget_line(&hb.trail, budget, spent, now);
        let events = hb.drain_queue();
        let text = wake::build_wake_message(&WakeBrief {
            why: why.clone(),
            rule,
            events: &events,
            last_wake,
            todos: &todos,
            trust: &trust,
            budget_line,
            now: now.with_timezone(&chrono::Local),
        });
        hb.record_wake(WakeRecord::fired(now, why, ids, choices));
        text
    };
    // The background chat queue sends it: its drain runs for any hydrated
    // session, focused or not (see todo::dispatch::dispatch_to_chat).
    chat_queue::queue_push(
        &mut chat_queue::CHAT_QUEUE.write(),
        session_id,
        chat_queue::QueuedMessage::new(text, Vec::new()),
    );
    chat_queue::request_drain();
    tracing::info!("heartbeat: woke session {session_id}");
}

/// Fill in cost and outcome for fired wakes whose turn has finished. True
/// when any was completed.
fn complete_finished_wakes(
    deps: HeartbeatDeps,
    session_id: &str,
    streaming: &HashSet<String>,
    now: DateTime<Utc>,
) -> bool {
    let open: Vec<(WakeRecord, Option<DateTime<Utc>>)> = {
        let hb = HEARTBEAT.peek();
        let fired: Vec<&WakeRecord> = hb.trail.iter().filter(|w| !w.deferred).collect();
        fired
            .iter()
            .enumerate()
            .filter(|(_, w)| w.cost_usd.is_none() && w.outcome.is_none())
            .map(|(i, w)| ((*w).clone(), fired.get(i + 1).map(|n| n.at)))
            .collect()
    };
    if open.is_empty() {
        return false;
    }
    let busy = session_busy(deps, session_id, streaming);
    let done: Vec<(String, f64, String)> = {
        let state = deps.session_state.peek();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        let log = deps.usage_log.peek();
        let oldest = open.iter().map(|(w, _)| w.at).min().unwrap_or(now);
        let spend: Vec<wake::Spend<'_>> = log
            .entries
            .iter()
            .filter(|e| e.session_id == session_id && e.timestamp >= oldest)
            .map(|e| wake::Spend { session_id: &e.session_id, at: e.timestamp, cost: e.cost })
            .collect();
        open.iter()
            .filter_map(|(w, until)| {
                // Only the newest wake can still be running; older ones
                // ended when the next fired.
                let busy = busy && until.is_none();
                wake::wake_completion(w, *until, session_id, &session.messages, &spend, busy, now)
                    .map(|(cost, outcome)| (w.id.clone(), cost, outcome))
            })
            .collect()
    };
    if done.is_empty() {
        return false;
    }
    let mut hb = HEARTBEAT.write();
    for (id, cost, outcome) in done {
        hb.complete_wake(&id, Some(cost), Some(outcome));
    }
    true
}
