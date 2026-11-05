//! The Heartbeat runtime: the polling loop spawned from `main.rs`.
//!
//! Every [`TICK_SECS`] while `heartbeat_active`: capture the enabled
//! sources (planner, Fleet; CI every `ci::CI_POLL_MINUTES` on a tokio task),
//! decide salience, ingest, then wake the Hobbes session (`hobbes_session`)
//! if `wake::decide_wake` says so — by queueing a message, marked as an
//! unattended turn (`TurnOrigin::Heartbeat`), for the background chat queue,
//! exactly like an unfocused dispatch. A wake waits while any other turn
//! (typed, voice) is in flight in that session. Day anchors
//! (`anchors.rs`) are checked every poll too. Finished wakes get their cost,
//! outcome and actions read back from the usage log and the session.
//!
//! Everything here runs on the Dioxus side (only it may touch Signals); a
//! tick is synchronous, so no signal guard ever crosses an `.await`.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

use super::anchors::{due_anchors, Anchor, DayView};
use super::diagnose::{self, Diagnosis, DiagnosisTarget};
use super::housekeeping::{self, HousekeepingNote};
use super::wake::{self, TodoInfo, WakeBrief, WakeTrigger, WakeVerdict};
use super::{
    agent_project_ids, check_wake, ci, intention_todo_ids, local_day_start, spent_since, Budget,
    CiRun, FleetSnapshot, HeartbeatEvent, PlannerSnapshot, RulesSalience, Salience, SalienceCtx, WakeRecord,
    HEARTBEAT,
};
use crate::components::chat_queue;
use crate::session::SessionState;
use crate::settings::Settings;

/// Poll interval.
pub const TICK_SECS: u64 = 30;
/// At most this many projects are polled for CI per round (projects with
/// open agent work first), to stay gentle on `gh`'s rate limits.
const MAX_CI_PROJECTS: usize = 12;
/// Persist at least this often even when nothing happened, so a restart
/// diffs against recent snapshots.
const SAVE_EVERY_MINUTES: i64 = 10;
/// Agent states are re-derived from the Fleet this often (the Planner's own
/// sync only runs while the Planner is on screen).
const AGENT_SYNC_MINUTES: i64 = 2;

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
    last_agent_sync: Option<DateTime<Utc>>,
    last_rotation_check: Option<DateTime<Utc>>,
    ended_rows: crate::components::planner_view::agent_lane::EndedRowCache,
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

    // Rotate an oversized Hobbes session at a quiet moment
    // (`session_rotation`). Here because this tick runs whether or not the
    // Heartbeat is on; before `ensure_session`, so a rotated session's
    // successor is adopted on this same tick.
    if pacer
        .last_rotation_check
        .is_none_or(|t| now - t >= Duration::minutes(crate::session_rotation::CHECK_EVERY_MINUTES))
    {
        pacer.last_rotation_check = Some(now);
        crate::session_rotation::check_all(deps.session_state, &settings, streaming);
    }

    let session_id = if active {
        crate::hobbes_session::ensure_session(deps.session_state, &settings)
    } else {
        crate::system_sessions::current()
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
            let fleet_events = hb.observe_fleet(snap, now);
            // The person's own sessions needing attention are the Fleet's
            // news; only dispatched runs and linked workers are kept.
            events.extend(super::perception::filter_fleet_noise(fleet_events, &worker_sessions(&deps.planner.peek())));
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
    if settings.planner_enabled && settings.heartbeat_source_planner {
        dirty |= housekeep(deps, &settings, pacer, now);
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

// ── The Hobbes session ──────────────────────────────────────────────────────

/// Mid-turn (a typed, voice or wake turn alike — the Hobbes session hosts
/// all three), a message popped from its queue but not streaming yet, a
/// message waiting in its queue, or a call waiting on approval in the latest
/// turn (or a recent wake's own; `wake::approval_blocks_wake`). A wake
/// waits for all of these, so it never interleaves with or aborts another
/// turn.
fn session_busy(deps: HeartbeatDeps, session_id: &str, streaming: &HashSet<String>) -> bool {
    streaming.contains(session_id)
        || crate::session_rotation::TURN_ACTIVITY.peek().contains(session_id)
        || chat_queue::DISPATCHING.peek().contains(session_id)
        || chat_queue::CHAT_QUEUE.peek().get(session_id).is_some_and(|q| !q.is_empty())
        || deps
            .session_state
            .peek()
            .sessions
            .get(session_id)
            .is_some_and(|s| {
                wake::approval_blocks_wake(
                    &s.messages,
                    Utc::now(),
                    Duration::hours(crate::session_rotation::APPROVAL_STALE_HOURS),
                )
            })
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

/// Session → to-do for open agent to-dos linked to a worker session.
fn worker_sessions(planner: &crate::todo::PlannerState) -> HashMap<String, String> {
    planner
        .todos
        .iter()
        .filter(|t| t.lane == crate::todo::model::Lane::Agent && !t.status.is_closed())
        .filter_map(|t| Some((t.linked_fleet_session.clone()?, t.id.clone())))
        .collect()
}

fn salience_ctx(deps: HeartbeatDeps, settings: &Settings, session_id: Option<&str>) -> SalienceCtx {
    let mut ctx = SalienceCtx::from_settings(settings);
    {
        let state = deps.session_state.peek();
        ctx.intention_todo_ids =
            intention_todo_ids(state.sessions.values().flat_map(|s| s.scheduled_timers.iter()));
        if let Some(sid) = session_id {
            let session = state.sessions.get(sid);
            let messages = session.map(|s| s.messages.as_slice()).unwrap_or(&[]);
            let mut lineage = crate::system_sessions::lineage();
            if !lineage.iter().any(|id| id == sid) {
                lineage.push(sid.to_string());
            }
            ctx.heartbeat_todo_ids = wake::heartbeat_todo_ids(&deps.fleet.peek(), &lineage, messages);
            // Chat workers it dispatched, before a rotation too (the worker
            // list is carried over; the HOBBES_DISPATCH calls are not).
            if let Some(s) = session.filter(|s| !s.dispatched_workers.is_empty()) {
                ctx.heartbeat_todo_ids.extend(
                    deps.planner
                        .peek()
                        .todos
                        .iter()
                        .filter(|t| t.linked_fleet_session.as_ref().is_some_and(|w| s.dispatched_workers.contains(w)))
                        .map(|t| t.id.clone()),
                );
            }
            ctx.heartbeat_archived_ids = lineage.into_iter().filter(|id| id != sid).collect();
            ctx.relayed_todo_ids =
                crate::todo::relay::relayed_todo_ids(&state, &deps.planner.peek(), settings, sid);
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
    let day = day_view(deps, settings, now);
    let verdict = {
        let hb = HEARTBEAT.peek();
        let anchors: Vec<Anchor> = day.as_ref().map(|d| due_anchors(&hb, d, now)).unwrap_or_default();
        let spent = spent_since(&hb.trail, local_day_start(now));
        wake::decide_wake(&hb, &budget, spent, now, busy, &anchors)
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
            fire_wake(deps, settings, session_id, &budget, trigger, day.as_ref(), now);
            true
        }
    }
}

/// Today's [`DayView`] when the planner is on and the Heartbeat watches it;
/// `None` turns day anchors and the look-ahead off.
fn day_view(deps: HeartbeatDeps, settings: &Settings, now: DateTime<Utc>) -> Option<DayView> {
    (settings.planner_enabled && settings.heartbeat_source_planner).then(|| {
        let mut day = DayView::capture(&deps.planner.peek(), settings, now);
        let heartbeat_session = crate::system_sessions::current();
        day.fleet =
            housekeeping::fleet_digest(&deps.fleet.peek(), &deps.planner.peek(), heartbeat_session.as_deref(), now);
        day
    })
}

// ── Housekeeping ────────────────────────────────────────────────────────────

/// What the Heartbeat fixes without a wake (`housekeeping.rs`): re-derive
/// agent states from the Fleet, and pause a runaway focus timer. True when
/// the Heartbeat state changed.
fn housekeep(deps: HeartbeatDeps, settings: &Settings, pacer: &mut Pacer, now: DateTime<Utc>) -> bool {
    use crate::components::planner_view::{agent_lane, persist_focus_sessions, persist_todo};
    let mut planner = deps.planner;
    if pacer.last_agent_sync.is_none_or(|t| now - t >= Duration::minutes(AGENT_SYNC_MINUTES)) {
        pacer.last_agent_sync = Some(now);
        let live = deps.fleet.peek().clone();
        let changed = agent_lane::sync_agent_states_cached(&mut planner, &live, &mut pacer.ended_rows, now);
        if changed > 0 {
            tracing::info!("heartbeat: re-synced {changed} agent to-do state(s)");
        }
    }

    let workday_end = super::anchors::workday_end_today(settings, now);
    let Some(runaway) = housekeeping::runaway_focus(&planner.peek(), workday_end, now) else {
        return false;
    };
    let paused = planner.write().pause_runaway_focus(&runaway.todo_id, runaway.bank_minutes, now);
    persist_todo(&planner, &runaway.todo_id);
    persist_focus_sessions(&mut planner);
    let Some((row_id, clamped)) = paused else {
        return false;
    };
    tracing::info!(
        "heartbeat: paused a runaway focus timer ({}m real, {}m banked)",
        runaway.real_minutes,
        runaway.bank_minutes
    );
    // "Keep real time" only when there's real time to keep.
    let focus_row = clamped.then_some(row_id);
    HEARTBEAT.write().record_housekeeping(HousekeepingNote::new(now, runaway.text, focus_row));
    true
}

/// "Keep real time" on a runaway-timer note: bank the real elapsed after
/// all. Called from the card.
pub fn restore_runaway(mut planner: Signal<crate::todo::PlannerState>, note_id: &str) {
    use crate::components::planner_view::{persist_focus_sessions, persist_todo};
    let Some(row_id) = HEARTBEAT.peek().restorable_focus_row(note_id) else {
        return;
    };
    let todo_id = planner.write().restore_unclamped_focus(&row_id, Utc::now());
    if let Some(id) = &todo_id {
        persist_todo(&planner, id);
        persist_focus_sessions(&mut planner);
    }
    HEARTBEAT.write().settle_housekeeping_restore(note_id, todo_id.is_some());
    super::save_signal();
}

/// Tails for the workers a wake's events point at. A Hobbes chat worker
/// shows its last messages (hydrated, or one indexed row from the store,
/// peeked: reading it must not hydrate it, so no restart-cut call is ended
/// and journaled for a row this never saves); a
/// Claude Code run shows the end of its transcript (a small tail read; the
/// tick is synchronous, so no guard is held across it).
fn diagnoses(
    deps: HeartbeatDeps,
    targets: &[DiagnosisTarget],
    todos: &HashMap<String, TodoInfo>,
) -> Vec<Diagnosis> {
    let mut out = Vec::new();
    for t in targets {
        let todo_title = t.todo_id.as_ref().and_then(|id| todos.get(id)).map(|i| i.title.clone());
        // A Hobbes chat worker.
        let chat = {
            let state = deps.session_state.peek();
            state.sessions.get(&t.session_id).map(|s| (s.name.clone(), diagnose::chat_tail(&s.messages)))
        };
        let chat = chat.or_else(|| {
            crate::session_store::peek_session(&t.session_id).map(|s| (s.name.clone(), diagnose::chat_tail(&s.messages)))
        });
        if let Some((name, tail)) = chat {
            if let Some(tail) = tail {
                out.push(Diagnosis { subject: todo_title.unwrap_or(name), state: t.state, source: "chat", tail });
            }
            continue;
        }
        // A Claude Code run: the live row, or the stored one once it ended.
        let row = deps
            .fleet
            .peek()
            .sessions
            .get(&t.session_id)
            .cloned()
            .or_else(|| crate::fleet::store::load_session(&t.session_id));
        let Some(row) = row else { continue };
        let Some(path) = row.transcript_path.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        let tail = match crate::fleet::transcript::read_tail(path, diagnose::TRANSCRIPT_TAIL_BYTES) {
            Ok(text) => diagnose::transcript_tail(&text),
            Err(e) => {
                tracing::debug!("heartbeat: transcript unreadable for {}: {e}", t.session_id);
                None
            }
        };
        if let Some(tail) = tail {
            out.push(Diagnosis {
                subject: todo_title.unwrap_or_else(|| row.display_name().to_string()),
                state: t.state,
                source: "Claude Code transcript",
                tail,
            });
        }
    }
    out
}

fn fire_wake(
    deps: HeartbeatDeps,
    settings: &Settings,
    session_id: &str,
    budget: &Budget,
    trigger: WakeTrigger,
    day: Option<&DayView>,
    now: DateTime<Utc>,
) {
    let (todos, targets): (HashMap<String, TodoInfo>, Vec<DiagnosisTarget>) = {
        let hb = HEARTBEAT.peek();
        let planner = deps.planner.peek();
        let queued: Vec<HeartbeatEvent> = hb.queued().into_iter().cloned().collect();
        let todos = queued
            .iter()
            .filter_map(|e| e.todo_id.as_deref())
            .filter_map(|id| {
                planner.todo(id).map(|t| {
                    (id.to_string(), TodoInfo { title: t.title.clone(), latest_progress: t.latest_progress.clone() })
                })
            })
            .collect();
        (todos, diagnose::diagnosis_targets(&queued))
    };
    let diagnoses = diagnoses(deps, &targets, &todos);
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
        // Every due anchor rides this wake, whatever triggered it.
        let anchors: Vec<Anchor> = day.map(|d| due_anchors(&hb, d, now)).unwrap_or_default();
        let ids = hb.queue.clone();
        let choices = wake::choice_summary(&hb, &ids);
        let last_wake = wake::last_fired_at(&hb.trail);
        let budget_line = wake::budget_line(&hb.trail, budget, spent, now);
        let events = hb.drain_queue();
        let housekeeping = hb.take_unbriefed_housekeeping();
        let text = wake::build_wake_message(&WakeBrief {
            ahead: day,
            why: why.clone(),
            rule,
            anchors: &anchors,
            events: &events,
            last_wake,
            todos: &todos,
            diagnoses: &diagnoses,
            trust: &trust,
            budget_line,
            now: now.with_timezone(&chrono::Local),
            housekeeping: &housekeeping,
        });
        hb.record_wake(WakeRecord::fired(now, why, ids, choices));
        if let Some(d) = day {
            hb.mark_anchors_fired(anchors.iter().map(|a| a.key.as_str()), d.today, now);
        }
        text
    };
    // The background chat queue sends it: its drain runs for any hydrated
    // session, focused or not (see todo::dispatch::dispatch_to_chat). The
    // turn is marked unattended, so the Heartbeat's scope rules apply to
    // every call it makes; what it has for the person is spoken when its
    // turn ends (voice::controller::on_turn_end).
    chat_queue::queue_push(
        &mut chat_queue::CHAT_QUEUE.write(),
        session_id,
        chat_queue::QueuedMessage::new(text, Vec::new(), chat_queue::QueueOrigin::HeartbeatWake),
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
    let done: Vec<(String, f64, String, Vec<String>)> = {
        let state = deps.session_state.peek();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        let planner = deps.planner.peek();
        let title = |id: &str| planner.todo(id).map(|t| t.title.clone());
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
                wake::wake_completion(w, *until, session_id, &session.messages, &spend, busy, now).map(
                    |(cost, outcome)| {
                        // Only the wake's own turn (`wake::wake_window_end`).
                        let until = wake::wake_window_end(&session.messages, w.at, *until);
                        let actions = wake::wake_actions(&session.messages, w.at, until, title);
                        (w.id.clone(), cost, outcome, actions)
                    },
                )
            })
            .collect()
    };
    if done.is_empty() {
        return false;
    }
    let mut hb = HEARTBEAT.write();
    for (id, cost, outcome, actions) in done {
        hb.complete_wake(&id, Some(cost), Some(outcome), actions);
    }
    true
}
