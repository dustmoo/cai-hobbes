//! Perception: cheap, pure watchers (no LLM).
//!
//! Each source is captured into a small serde snapshot and successive
//! snapshots are diffed into [`HeartbeatEvent`]s. Snapshots keep only what
//! the diffs read, so the persisted `heartbeat_state` stays small. The first
//! capture of a source has nothing to compare against and emits nothing —
//! [`super::HeartbeatState::observe_planner`] and friends enforce that by
//! storing the snapshot and returning no events.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{CiConclusion, CiRun, EventSource, HeartbeatEvent, Urgency};
use crate::fleet::{AttentionKind, FleetSession, FleetState, FleetStatus};
use crate::todo::model::{AgentState, DecisionSource, Lane, Todo};
use crate::todo::PlannerState;

// ── Event kinds ─────────────────────────────────────────────────────────────
// Stable slugs: persisted, matched by salience rules and by
// `heartbeat_muted_kinds`. Never rename one.

pub const KIND_WORKER_DONE: &str = "worker_done";
pub const KIND_WORKER_NEEDS_YOU: &str = "worker_needs_you";
pub const KIND_WORKER_STUCK: &str = "worker_stuck";
/// A run ended with an open item for the person (`AgentState::Review`).
pub const KIND_WORKER_REVIEW: &str = "worker_review";
pub const KIND_TODO_OVERDUE: &str = "todo_overdue";
pub const KIND_DECISION_RAISED: &str = "decision_raised";
pub const KIND_SESSION_NEEDS_ATTENTION: &str = "session_needs_attention";
pub const KIND_RUN_ENDED: &str = "run_ended";
pub const KIND_CI_FAILED: &str = "ci_failed";
pub const KIND_CI_RECOVERED: &str = "ci_recovered";

/// Closed agent to-dos stay in the planner snapshot this long after their
/// last update, so a Working → Done transition that also closes the to-do
/// is still seen; older ones drop out to keep the snapshot small.
const CLOSED_AGENT_KEEP_DAYS: i64 = 7;

// ── Planner ─────────────────────────────────────────────────────────────────

/// What the planner diff needs to know about one to-do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoSnap {
    pub title: String,
    #[serde(default)]
    pub lane: Lane,
    #[serde(default)]
    pub agent_state: Option<AgentState>,
    /// Past its deadline and still open, as of the snapshot's `today`.
    #[serde(default)]
    pub overdue: bool,
    #[serde(default)]
    pub deadline: Option<NaiveDate>,
    /// `asked_at` of the open worker-asked decision — the decision's
    /// identity. Gate/attention decisions are left out: the Fleet watcher
    /// already reports those as `session_needs_attention`.
    #[serde(default)]
    pub open_decision_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub decision_question: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub linked_session: Option<String>,
}

/// The to-dos a planner diff can say something about: agent-lane work (open,
/// or closed within the last week), open to-dos with a deadline, and open
/// decisions. Keyed by to-do id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerSnapshot {
    pub captured_at: DateTime<Utc>,
    /// The local day overdue was judged against.
    pub today: NaiveDate,
    #[serde(default)]
    pub todos: BTreeMap<String, TodoSnap>,
}

impl PlannerSnapshot {
    /// Capture from live planner state, judging overdue against the local
    /// date of `now`.
    pub fn capture(planner: &PlannerState, now: DateTime<Utc>) -> Self {
        let today = now.with_timezone(&chrono::Local).date_naive();
        Self::capture_on(&planner.todos, now, today)
    }

    /// [`Self::capture`] with an explicit `today` (deterministic for tests).
    pub fn capture_on(todos: &[Todo], now: DateTime<Utc>, today: NaiveDate) -> Self {
        let keep_closed_since = now - Duration::days(CLOSED_AGENT_KEEP_DAYS);
        let mut out = BTreeMap::new();
        for t in todos {
            let closed = t.status.is_closed();
            let worker_decision = t
                .decision
                .as_ref()
                .filter(|d| d.is_open() && d.source == DecisionSource::Worker);
            let relevant = match t.lane {
                Lane::Agent => !closed || t.updated_at >= keep_closed_since,
                Lane::Human => !closed && t.deadline.is_some(),
            } || worker_decision.is_some();
            if !relevant {
                continue;
            }
            out.insert(
                t.id.clone(),
                TodoSnap {
                    title: t.title.clone(),
                    lane: t.lane,
                    agent_state: t.agent_state,
                    overdue: t.is_overdue(today),
                    deadline: t.deadline,
                    open_decision_at: worker_decision.map(|d| d.asked_at),
                    decision_question: worker_decision.map(|d| d.question.clone()),
                    project_id: t.project_id.clone(),
                    linked_session: t.linked_fleet_session.clone(),
                },
            );
        }
        Self { captured_at: now, today, todos: out }
    }
}

fn todo_event(
    id: &str,
    t: &TodoSnap,
    kind: &str,
    summary: String,
    urgency: Urgency,
    now: DateTime<Utc>,
) -> HeartbeatEvent {
    let mut ev = HeartbeatEvent::new(EventSource::Planner, kind, summary, urgency, now);
    ev.todo_id = Some(id.to_string());
    ev.session_id = t.linked_session.clone();
    ev.project_id = t.project_id.clone();
    ev
}

/// Diff two planner snapshots:
/// - an agent-lane to-do whose `agent_state` moved into Done / NeedsYou /
///   Stuck / Review → `worker_done` (Normal) / `worker_needs_you` (High) /
///   `worker_stuck` (High) / `worker_review` (Normal);
/// - a to-do that became overdue → `todo_overdue` (Normal), once — it stays
///   overdue in every later snapshot, so it never fires again;
/// - a new open worker decision → `decision_raised` (High). It supersedes a
///   simultaneous `worker_needs_you` on the same to-do (the decision is the
///   richer event).
///
/// To-dos new to the snapshot only raise decision/overdue events (their
/// agent state has no "before").
pub fn planner_events(
    prev: &PlannerSnapshot,
    cur: &PlannerSnapshot,
    now: DateTime<Utc>,
) -> Vec<HeartbeatEvent> {
    let mut out = Vec::new();
    for (id, c) in &cur.todos {
        let p = prev.todos.get(id);

        let decision_new = c.open_decision_at.is_some()
            && p.is_none_or(|p| p.open_decision_at != c.open_decision_at);
        if decision_new {
            let q = c.decision_question.as_deref().unwrap_or("").trim();
            let summary = if q.is_empty() {
                format!("Decision needed on \"{}\"", c.title)
            } else {
                format!("Decision needed on \"{}\": {}", c.title, q)
            };
            out.push(todo_event(id, c, KIND_DECISION_RAISED, summary, Urgency::High, now));
        }

        if let Some(p) = p {
            if c.lane == Lane::Agent && c.agent_state != p.agent_state {
                let ev = match c.agent_state {
                    Some(AgentState::Done) => Some((
                        KIND_WORKER_DONE,
                        format!("Agent finished \"{}\"", c.title),
                        Urgency::Normal,
                    )),
                    Some(AgentState::NeedsYou) if !decision_new => Some((
                        KIND_WORKER_NEEDS_YOU,
                        format!("Agent working on \"{}\" needs you", c.title),
                        Urgency::High,
                    )),
                    Some(AgentState::Stuck) => Some((
                        KIND_WORKER_STUCK,
                        format!("Agent working on \"{}\" is stuck", c.title),
                        Urgency::High,
                    )),
                    Some(AgentState::Review) => Some((
                        KIND_WORKER_REVIEW,
                        format!("Agent finished \"{}\" with an open item to review", c.title),
                        Urgency::Normal,
                    )),
                    _ => None,
                };
                if let Some((kind, summary, urgency)) = ev {
                    out.push(todo_event(id, c, kind, summary, urgency, now));
                }
            }
        }

        if c.overdue && !p.is_some_and(|p| p.overdue) {
            let summary = match c.deadline {
                Some(d) => format!("\"{}\" is overdue (due {})", c.title, d),
                None => format!("\"{}\" is overdue", c.title),
            };
            out.push(todo_event(id, c, KIND_TODO_OVERDUE, summary, Urgency::Normal, now));
        }
    }
    out
}

// ── Fleet ───────────────────────────────────────────────────────────────────

/// A fleet session's status, flattened for the snapshot (stable serde,
/// independent of `FleetStatus`'s shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetPhase {
    Working,
    Background,
    Waiting,
    Idle,
    NeedsAttention,
}

impl FleetPhase {
    pub fn from_status(s: &FleetStatus) -> Self {
        match s {
            FleetStatus::Working => FleetPhase::Working,
            FleetStatus::WorkingBackground => FleetPhase::Background,
            FleetStatus::Waiting => FleetPhase::Waiting,
            FleetStatus::Idle => FleetPhase::Idle,
            FleetStatus::NeedsAttention(_) => FleetPhase::NeedsAttention,
        }
    }

    /// Mid-run: working, or paused on the person mid-run.
    fn running(self) -> bool {
        matches!(
            self,
            FleetPhase::Working | FleetPhase::Background | FleetPhase::NeedsAttention
        )
    }
}

/// What the fleet diff needs to know about one live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetSessionSnap {
    pub name: String,
    pub phase: FleetPhase,
    /// Why it needs attention ("permission for Bash", the notification
    /// message), when it does.
    #[serde(default)]
    pub attention: Option<String>,
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub dispatched_todo: Option<String>,
    #[serde(default)]
    pub dispatched_from: Option<String>,
}

impl FleetSessionSnap {
    pub fn from_session(s: &FleetSession) -> Self {
        let attention = match &s.status {
            FleetStatus::NeedsAttention(AttentionKind::Gate) => Some(match &s.pending_gate {
                Some(g) => format!("waiting on permission for {}", g.tool_name),
                None => "waiting on a permission prompt".to_string(),
            }),
            // What it asks (a permission prompt's own message, or the
            // brief's question for a parked turn) says more than "waiting
            // for your input".
            FleetStatus::NeedsAttention(AttentionKind::Notification { message, kind }) => {
                let m = message.trim();
                Some(crate::fleet::truncate_summary(
                    &s.question().unwrap_or_else(|| if m.is_empty() { kind.clone() } else { m.to_string() }),
                    120,
                ))
            }
            _ => None,
        };
        // A finished turn with nothing asked (Claude Code's idle prompt) is
        // parked, not a call for the person: it reads as idle, so a
        // dispatched run that parks reports `run_ended`, not attention.
        let phase = match FleetPhase::from_status(&s.status) {
            FleetPhase::NeedsAttention if !s.asks_person() => FleetPhase::Idle,
            phase => phase,
        };
        Self {
            name: s.display_name().to_string(),
            phase,
            attention,
            ended: s.ended_at.is_some(),
            dispatched_todo: s.dispatched_todo.clone(),
            dispatched_from: s.dispatched_from.clone(),
        }
    }

    fn dispatched(&self) -> bool {
        self.dispatched_todo.is_some() || self.dispatched_from.is_some()
    }
}

/// The live fleet map, keyed by session id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    #[serde(default)]
    pub sessions: BTreeMap<String, FleetSessionSnap>,
}

impl FleetSnapshot {
    pub fn capture(fleet: &FleetState) -> Self {
        Self {
            sessions: fleet
                .sessions
                .iter()
                .map(|(id, s)| (id.clone(), FleetSessionSnap::from_session(s)))
                .collect(),
        }
    }
}

fn fleet_event(
    id: &str,
    s: &FleetSessionSnap,
    kind: &str,
    summary: String,
    urgency: Urgency,
    now: DateTime<Utc>,
) -> HeartbeatEvent {
    let mut ev = HeartbeatEvent::new(EventSource::Fleet, kind, summary, urgency, now);
    ev.session_id = Some(id.to_string());
    ev.todo_id = s.dispatched_todo.clone();
    ev.dispatched_from = s.dispatched_from.clone();
    ev
}

/// Diff two fleet snapshots:
/// - a session that newly needs attention → `session_needs_attention`
///   (High for a dispatched run, else Normal);
/// - a dispatched run that was running (working, background, or paused on
///   the person) and now isn't — ended, gone from the live map (SessionEnd
///   removes it), or idle → `run_ended` (Low: the dispatching chat's own
///   continuation already hands the result back).
pub fn fleet_events(
    prev: &FleetSnapshot,
    cur: &FleetSnapshot,
    now: DateTime<Utc>,
) -> Vec<HeartbeatEvent> {
    let mut out = Vec::new();
    for (id, c) in &cur.sessions {
        let was_attention = prev
            .sessions
            .get(id)
            .is_some_and(|p| p.phase == FleetPhase::NeedsAttention);
        if c.phase == FleetPhase::NeedsAttention && !was_attention {
            let why = c.attention.as_deref().unwrap_or("needs you");
            let urgency = if c.dispatched() { Urgency::High } else { Urgency::Normal };
            out.push(fleet_event(
                id,
                c,
                KIND_SESSION_NEEDS_ATTENTION,
                format!("Fleet: {} — {}", c.name, why),
                urgency,
                now,
            ));
        }
    }
    for (id, p) in &prev.sessions {
        if !p.dispatched() || !p.phase.running() || p.ended {
            continue;
        }
        let finished = match cur.sessions.get(id) {
            None => true,
            Some(c) => c.ended || c.phase == FleetPhase::Idle,
        };
        if finished {
            let s = cur.sessions.get(id).unwrap_or(p);
            out.push(fleet_event(
                id,
                s,
                KIND_RUN_ENDED,
                format!("Fleet: dispatched run {} ended", s.name),
                Urgency::Low,
                now,
            ));
        }
    }
    out
}

/// A `session_needs_attention` event about a session nobody dispatched and
/// no to-do is linked to: the person's own terminal or tab. The Fleet already
/// shows those, so the Heartbeat neither journals nor queues them.
pub fn is_fleet_noise(ev: &HeartbeatEvent) -> bool {
    ev.kind == KIND_SESSION_NEEDS_ATTENTION && ev.todo_id.is_none() && ev.dispatched_from.is_none()
}

/// Drop [`is_fleet_noise`] events from a fleet diff. `workers` maps the
/// session linked to an open agent to-do (a Hobbes chat worker, or a run
/// linked after the fact) to that to-do: such a session's attention event is
/// kept and tagged with the to-do, so it reads as worker news.
pub fn filter_fleet_noise(
    events: Vec<HeartbeatEvent>,
    workers: &HashMap<String, String>,
) -> Vec<HeartbeatEvent> {
    events
        .into_iter()
        .filter_map(|mut ev| {
            if ev.kind == KIND_SESSION_NEEDS_ATTENTION && ev.todo_id.is_none() {
                if let Some(todo) = ev.session_id.as_ref().and_then(|s| workers.get(s)) {
                    ev.todo_id = Some(todo.clone());
                }
            }
            (!is_fleet_noise(&ev)).then_some(ev)
        })
        .collect()
}

// ── CI ──────────────────────────────────────────────────────────────────────

type CiKey<'a> = (&'a str, &'a str, &'a str);

fn ci_key(r: &CiRun) -> CiKey<'_> {
    (&r.project_id, &r.workflow, &r.branch)
}

fn ci_event(r: &CiRun, kind: &str, summary: String, urgency: Urgency) -> HeartbeatEvent {
    let mut ev = HeartbeatEvent::new(EventSource::Ci, kind, summary, urgency, r.finished_at);
    ev.project_id = Some(r.project_id.clone());
    ev
}

/// Diff two CI polls, keyed by (project, workflow, branch):
/// - a failure where the previous run wasn't one (or the key is new) →
///   `ci_failed` (High);
/// - a success after a failure → `ci_recovered` (Normal);
/// - the same run again, or failure → failure → nothing.
///
/// Cancelled / other conclusions never raise an event; use
/// [`merge_ci_snapshot`] to store the next `prev` so they don't hide a
/// failure → success recovery.
pub fn ci_events(prev: &[CiRun], cur: &[CiRun]) -> Vec<HeartbeatEvent> {
    let before: HashMap<CiKey<'_>, &CiRun> = prev.iter().map(|r| (ci_key(r), r)).collect();
    let mut out = Vec::new();
    for c in cur {
        let p = before.get(&ci_key(c)).copied();
        if p.is_some_and(|p| p.finished_at == c.finished_at && p.conclusion == c.conclusion) {
            continue;
        }
        let was_failing = p.is_some_and(|p| p.conclusion == CiConclusion::Failure);
        match c.conclusion {
            CiConclusion::Failure if !was_failing => out.push(ci_event(
                c,
                KIND_CI_FAILED,
                format!("CI failed: {} on {} ({}) — {}", c.workflow, c.branch, c.project_title, c.url),
                Urgency::High,
            )),
            CiConclusion::Success if was_failing => out.push(ci_event(
                c,
                KIND_CI_RECOVERED,
                format!("CI recovered: {} on {} ({})", c.workflow, c.branch, c.project_title),
                Urgency::Normal,
            )),
            _ => {}
        }
    }
    out
}

/// The CI snapshot to keep after a poll: `cur`, except that where `cur`'s
/// run is cancelled / other the previous decisive (success or failure) run
/// is kept, so failure → cancelled → success still reads as a recovery.
/// Keys missing from `cur` keep their previous run (a quiet poll forgets
/// nothing).
pub fn merge_ci_snapshot(prev: &[CiRun], cur: &[CiRun]) -> Vec<CiRun> {
    let decisive = |r: &CiRun| matches!(r.conclusion, CiConclusion::Success | CiConclusion::Failure);
    let before: HashMap<CiKey<'_>, &CiRun> = prev.iter().map(|r| (ci_key(r), r)).collect();
    let mut out: Vec<CiRun> = cur
        .iter()
        .map(|c| match before.get(&ci_key(c)) {
            Some(p) if !decisive(c) && decisive(p) => (*p).clone(),
            _ => c.clone(),
        })
        .collect();
    let seen: std::collections::HashSet<CiKey<'_>> = cur.iter().map(ci_key).collect();
    out.extend(prev.iter().filter(|p| !seen.contains(&ci_key(p))).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 15, 0, 0).unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn agent_todo(id: &str, state: AgentState) -> Todo {
        let mut t = Todo::new(format!("task {id}"), 1.0);
        t.id = id.to_string();
        t.set_lane(Lane::Agent);
        t.agent_state = Some(state);
        t.project_id = Some("proj".into());
        t.linked_fleet_session = Some("sess-1".into());
        t
    }

    fn snap(todos: &[Todo], today: NaiveDate) -> PlannerSnapshot {
        PlannerSnapshot::capture_on(todos, t0(), today)
    }

    fn kinds(evs: &[HeartbeatEvent]) -> Vec<&str> {
        evs.iter().map(|e| e.kind.as_str()).collect()
    }

    #[test]
    fn agent_state_transitions_raise_worker_events() {
        for (to, kind, urgency) in [
            (AgentState::Done, KIND_WORKER_DONE, Urgency::Normal),
            (AgentState::NeedsYou, KIND_WORKER_NEEDS_YOU, Urgency::High),
            (AgentState::Stuck, KIND_WORKER_STUCK, Urgency::High),
        ] {
            let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
            let cur = snap(&[agent_todo("a", to)], day(20));
            let evs = planner_events(&prev, &cur, t0());
            assert_eq!(kinds(&evs), vec![kind]);
            let e = &evs[0];
            assert_eq!(e.urgency, urgency);
            assert_eq!(e.source, EventSource::Planner);
            assert_eq!(e.todo_id.as_deref(), Some("a"));
            assert_eq!(e.session_id.as_deref(), Some("sess-1"));
            assert_eq!(e.project_id.as_deref(), Some("proj"));
        }
    }

    #[test]
    fn unchanged_or_uninteresting_agent_state_is_silent() {
        let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
        assert!(planner_events(&prev, &prev.clone(), t0()).is_empty());
        let cur = snap(&[agent_todo("a", AgentState::Queued)], day(20));
        assert!(planner_events(&prev, &cur, t0()).is_empty());
    }

    #[test]
    fn closed_done_agent_todo_is_still_seen() {
        let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
        let mut done = agent_todo("a", AgentState::Done);
        done.status = crate::todo::model::TodoStatus::Completed;
        done.updated_at = t0();
        let cur = snap(&[done], day(20));
        assert_eq!(kinds(&planner_events(&prev, &cur, t0())), vec![KIND_WORKER_DONE]);
    }

    #[test]
    fn overdue_fires_once() {
        let mut t = Todo::new("pay rent", 1.0);
        t.id = "h".into();
        t.deadline = Some(day(20));
        let s20 = snap(std::slice::from_ref(&t), day(20));
        let s21 = snap(std::slice::from_ref(&t), day(21));
        let s22 = snap(std::slice::from_ref(&t), day(22));
        let evs = planner_events(&s20, &s21, t0());
        assert_eq!(kinds(&evs), vec![KIND_TODO_OVERDUE]);
        assert!(evs[0].summary.contains("2026-10-20"));
        assert!(planner_events(&s21, &s22, t0()).is_empty());
    }

    #[test]
    fn completed_todo_is_not_overdue() {
        let mut t = Todo::new("pay rent", 1.0);
        t.id = "h".into();
        t.deadline = Some(day(20));
        let prev = snap(std::slice::from_ref(&t), day(20));
        t.status = crate::todo::model::TodoStatus::Completed;
        let cur = snap(&[t], day(21));
        assert!(cur.todos.is_empty());
        assert!(planner_events(&prev, &cur, t0()).is_empty());
    }

    fn with_decision(mut t: Todo, asked_at: DateTime<Utc>, source: DecisionSource) -> Todo {
        t.decision = Some(crate::todo::model::Decision {
            question: "Ship it?".into(),
            options: vec!["yes".into(), "no".into()],
            est_minutes: None,
            brief: None,
            asked_at,
            worker: None,
            source,
            resolved: None,
        });
        t
    }

    #[test]
    fn new_decision_supersedes_needs_you() {
        let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
        let t = with_decision(agent_todo("a", AgentState::NeedsYou), t0(), DecisionSource::Worker);
        let cur = snap(std::slice::from_ref(&t), day(20));
        let evs = planner_events(&prev, &cur, t0());
        assert_eq!(kinds(&evs), vec![KIND_DECISION_RAISED]);
        assert!(evs[0].summary.contains("Ship it?"));
        // Same decision next poll: nothing.
        assert!(planner_events(&cur, &snap(std::slice::from_ref(&t), day(20)), t0()).is_empty());
        // A second, later decision is new.
        let t2 = with_decision(t, t0() + Duration::minutes(5), DecisionSource::Worker);
        assert_eq!(
            kinds(&planner_events(&cur, &snap(&[t2], day(20)), t0())),
            vec![KIND_DECISION_RAISED]
        );
    }

    #[test]
    fn gate_decisions_are_left_to_fleet() {
        let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
        let t = with_decision(
            agent_todo("a", AgentState::NeedsYou),
            t0(),
            DecisionSource::Gate { request_id: "r".into() },
        );
        let evs = planner_events(&prev, &snap(&[t], day(20)), t0());
        assert_eq!(kinds(&evs), vec![KIND_WORKER_NEEDS_YOU]);
    }

    #[test]
    fn snapshot_keeps_only_relevant_todos() {
        let plain = Todo::new("no deadline", 1.0);
        let mut old_done = agent_todo("old", AgentState::Done);
        old_done.status = crate::todo::model::TodoStatus::Completed;
        old_done.updated_at = t0() - Duration::days(30);
        let s = snap(&[plain, old_done, agent_todo("a", AgentState::Working)], day(20));
        assert_eq!(s.todos.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    // ── Fleet ──

    fn fs(phase: FleetPhase, dispatched: bool) -> FleetSessionSnap {
        FleetSessionSnap {
            name: "worker".into(),
            phase,
            attention: (phase == FleetPhase::NeedsAttention).then(|| "waiting on permission for Bash".into()),
            ended: false,
            dispatched_todo: dispatched.then(|| "todo-1".into()),
            dispatched_from: dispatched.then(|| "chat-1".into()),
        }
    }

    fn fleet(sessions: &[(&str, FleetSessionSnap)]) -> FleetSnapshot {
        FleetSnapshot {
            sessions: sessions.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        }
    }

    #[test]
    fn session_newly_needing_attention_fires_once() {
        let prev = fleet(&[("s", fs(FleetPhase::Working, false))]);
        let cur = fleet(&[("s", fs(FleetPhase::NeedsAttention, false))]);
        let evs = fleet_events(&prev, &cur, t0());
        assert_eq!(kinds(&evs), vec![KIND_SESSION_NEEDS_ATTENTION]);
        assert_eq!(evs[0].urgency, Urgency::Normal);
        assert_eq!(evs[0].session_id.as_deref(), Some("s"));
        assert!(evs[0].summary.contains("permission for Bash"));
        assert!(fleet_events(&cur, &cur.clone(), t0()).is_empty());
    }

    #[test]
    fn dispatched_attention_is_high_and_carries_links() {
        let prev = fleet(&[]);
        let cur = fleet(&[("s", fs(FleetPhase::NeedsAttention, true))]);
        let evs = fleet_events(&prev, &cur, t0());
        assert_eq!(evs[0].urgency, Urgency::High);
        assert_eq!(evs[0].todo_id.as_deref(), Some("todo-1"));
        assert_eq!(evs[0].dispatched_from.as_deref(), Some("chat-1"));
    }

    #[test]
    fn review_transition_raises_worker_review() {
        let prev = snap(&[agent_todo("a", AgentState::Working)], day(20));
        let cur = snap(&[agent_todo("a", AgentState::Review)], day(20));
        let evs = planner_events(&prev, &cur, t0());
        assert_eq!(kinds(&evs), vec![KIND_WORKER_REVIEW]);
        assert_eq!(evs[0].urgency, Urgency::Normal);
    }

    #[test]
    fn undispatched_attention_is_noise_unless_a_worker() {
        let prev = fleet(&[]);
        let cur = fleet(&[
            ("mine", fs(FleetPhase::NeedsAttention, false)),
            ("chat-worker", fs(FleetPhase::NeedsAttention, false)),
            ("run", fs(FleetPhase::NeedsAttention, true)),
        ]);
        let evs = fleet_events(&prev, &cur, t0());
        assert_eq!(evs.len(), 3);
        assert!(evs.iter().filter(|e| is_fleet_noise(e)).count() == 2);
        let workers = HashMap::from([("chat-worker".to_string(), "todo-9".to_string())]);
        let kept = filter_fleet_noise(evs, &workers);
        let mut ids: Vec<_> = kept.iter().map(|e| e.session_id.clone().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec!["chat-worker".to_string(), "run".to_string()]);
        let worker = kept.iter().find(|e| e.session_id.as_deref() == Some("chat-worker")).unwrap();
        assert_eq!(worker.todo_id.as_deref(), Some("todo-9"));
        assert!(kept.iter().all(|e| !is_fleet_noise(e)));
    }

    #[test]
    fn dispatched_run_ending_fires_run_ended() {
        let prev = fleet(&[("s", fs(FleetPhase::Working, true))]);
        // Gone from the live map (SessionEnd).
        assert_eq!(kinds(&fleet_events(&prev, &fleet(&[]), t0())), vec![KIND_RUN_ENDED]);
        // Went idle.
        let idle = fleet(&[("s", fs(FleetPhase::Idle, true))]);
        let evs = fleet_events(&prev, &idle, t0());
        assert_eq!(kinds(&evs), vec![KIND_RUN_ENDED]);
        assert_eq!(evs[0].urgency, Urgency::Low);
        // Idle → idle again: nothing.
        assert!(fleet_events(&idle, &idle.clone(), t0()).is_empty());
    }

    #[test]
    fn undispatched_session_ending_is_silent() {
        let prev = fleet(&[("s", fs(FleetPhase::Working, false))]);
        assert!(fleet_events(&prev, &fleet(&[]), t0()).is_empty());
        // An idle dispatched session retiring off the board isn't a run end.
        let idle = fleet(&[("s", fs(FleetPhase::Idle, true))]);
        assert!(fleet_events(&idle, &fleet(&[]), t0()).is_empty());
    }

    #[test]
    fn fleet_capture_flattens_status() {
        let s: FleetSession = serde_json::from_value(serde_json::json!({
            "id": "s", "cwd": "/x/repo", "name": "repo",
            "status": {"NeedsAttention": {"Notification": {"kind": "idle_prompt", "message": "Waiting for input"}}},
            "last_event_at": t0(), "dispatched_todo": "todo-1"
        }))
        .unwrap();
        let mut live = FleetState::default();
        live.sessions.insert("s".into(), s);
        let snap = FleetSnapshot::capture(&live);
        let c = &snap.sessions["s"];
        assert_eq!(c.phase, FleetPhase::Idle, "a bare idle prompt is parked");
        assert_eq!(c.dispatched_todo.as_deref(), Some("todo-1"));

        // With a question in its brief it does need the person.
        let s = live.sessions.get_mut("s").unwrap();
        s.brief = Some(crate::fleet::SessionBrief {
            headline: "Built the hotfix".into(),
            bullets: vec![],
            blocked_on: Some("go-ahead to ship the eager-mode hotfix".into()),
            generated_at: t0(),
            final_brief: false,
        });
        let snap = FleetSnapshot::capture(&live);
        let c = &snap.sessions["s"];
        assert_eq!(c.phase, FleetPhase::NeedsAttention);
        assert_eq!(c.attention.as_deref(), Some("go-ahead to ship the eager-mode hotfix"));
    }

    // ── CI ──

    fn run(branch: &str, conclusion: CiConclusion, mins: i64) -> CiRun {
        CiRun {
            project_id: "p".into(),
            project_title: "Hobbes".into(),
            workflow: "CI".into(),
            branch: branch.into(),
            conclusion,
            url: "https://example.test/run".into(),
            finished_at: t0() + Duration::minutes(mins),
        }
    }

    #[test]
    fn ci_new_failure_and_recovery() {
        let ok = vec![run("main", CiConclusion::Success, 0)];
        let bad = vec![run("main", CiConclusion::Failure, 10)];
        let evs = ci_events(&ok, &bad);
        assert_eq!(kinds(&evs), vec![KIND_CI_FAILED]);
        assert_eq!(evs[0].urgency, Urgency::High);
        assert_eq!(evs[0].project_id.as_deref(), Some("p"));
        assert_eq!(evs[0].at, bad[0].finished_at);
        let fixed = vec![run("main", CiConclusion::Success, 20)];
        let evs = ci_events(&bad, &fixed);
        assert_eq!(kinds(&evs), vec![KIND_CI_RECOVERED]);
        assert_eq!(evs[0].urgency, Urgency::Normal);
    }

    #[test]
    fn ci_repeats_are_silent() {
        let bad = vec![run("main", CiConclusion::Failure, 10)];
        assert!(ci_events(&bad, &bad.clone()).is_empty());
        // A newer run that still fails is not a new failure.
        assert!(ci_events(&bad, &[run("main", CiConclusion::Failure, 20)]).is_empty());
        let ok = vec![run("main", CiConclusion::Success, 0)];
        assert!(ci_events(&ok, &[run("main", CiConclusion::Success, 30)]).is_empty());
    }

    #[test]
    fn ci_failure_on_a_new_branch_fires() {
        let ok = vec![run("main", CiConclusion::Success, 0)];
        let cur = vec![ok[0].clone(), run("feat/x", CiConclusion::Failure, 5)];
        assert_eq!(kinds(&ci_events(&ok, &cur)), vec![KIND_CI_FAILED]);
    }

    #[test]
    fn merge_keeps_decisive_runs_through_cancellations() {
        let bad = vec![run("main", CiConclusion::Failure, 10)];
        let cancelled = vec![run("main", CiConclusion::Cancelled, 15)];
        assert!(ci_events(&bad, &cancelled).is_empty());
        let kept = merge_ci_snapshot(&bad, &cancelled);
        assert_eq!(kept, bad);
        let fixed = vec![run("main", CiConclusion::Success, 20)];
        assert_eq!(kinds(&ci_events(&kept, &fixed)), vec![KIND_CI_RECOVERED]);
        // Keys missing from a poll are remembered.
        let other = vec![run("dev", CiConclusion::Success, 1)];
        assert_eq!(merge_ci_snapshot(&bad, &other).len(), 2);
    }
}
