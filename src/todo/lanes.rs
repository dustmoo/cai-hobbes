//! The agent lane: `agent_state` derivation, the decision queue, and the
//! lane summaries the Today view, `HOBBES_PLAN_DAY` and `planner_today` show
//! (SYMBIOTIC_PLANNER.md §2, §4).
//!
//! Pure over planner data plus a fleet-free [`WorkerSignal`]; the fleet →
//! signal conversion lives at the edge (`handlers::worker_signal_from_fleet`)
//! so this stays testable without the fleet runtime.
//!
//! **Progress is reported, never judged.** `derive_agent_state` maps only
//! explicit states — a pending gate, a fleet attention flag, the brief's
//! `blocked_on` field, an ended session, a completed to-do. It never reads
//! meaning into free text, and when nothing explicit applies it returns
//! `None` ("leave the state as it is").

use chrono::{DateTime, NaiveDate, Utc};

use super::model::{
    AgentState, Brief, Decision, DecisionDelivery, DecisionSource, Lane, ResolvedDecision, Todo,
    TodoStatus,
};
use super::PlannerState;

/// What the linked worker is doing, as the fleet reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    /// Running a turn (including background subagents).
    Working,
    /// Between turns, nothing pending.
    Idle,
    /// The fleet flags it as needing the user (gate or notification).
    WaitingOnUser,
}

/// A held permission gate, as the fleet reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateSignal {
    pub request_id: String,
    pub tool_name: String,
    pub input_summary: String,
}

/// The linked worker's state, reduced to the fields the derivation may use.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSignal {
    pub status: WorkerStatus,
    /// The session has ended (`SessionEnd` / tab closed).
    pub ended: bool,
    pub gate: Option<GateSignal>,
    /// The fleet notification's message when waiting on the user.
    pub attention_message: Option<String>,
    /// When the current wait began.
    pub attention_at: Option<DateTime<Utc>>,
    /// The latest brief, mapped (headline → did, blocked_on → blocked).
    pub brief: Option<Brief>,
    /// Display name for the decision card.
    pub worker_name: String,
}

/// The explicit agent state a signal implies, or `None` to leave the state
/// as it is. First match wins:
///
/// 1. the to-do is completed → `Done`
/// 2. the worker waits on the person (pending gate or attention flag) → `NeedsYou`
/// 3. an open worker-asked decision → `NeedsYou`
/// 4. the worker is working → `Working`
/// 5. the brief's `blocked` is set (and, by order, nothing is pending) → `Stuck`
/// 6. the session ended → `Done`
/// 7. otherwise → `None`
pub fn derive_agent_state(todo: &Todo, signal: Option<&WorkerSignal>) -> Option<AgentState> {
    if todo.status == TodoStatus::Completed {
        return Some(AgentState::Done);
    }
    if let Some(s) = signal {
        if s.status == WorkerStatus::WaitingOnUser || s.gate.is_some() {
            return Some(AgentState::NeedsYou);
        }
    }
    if todo.has_open_decision() {
        return Some(AgentState::NeedsYou);
    }
    let s = signal?;
    if s.status == WorkerStatus::Working {
        return Some(AgentState::Working);
    }
    if s
        .brief
        .as_ref()
        .and_then(|b| b.blocked.as_deref())
        .is_some_and(|b| !b.trim().is_empty())
    {
        return Some(AgentState::Stuck);
    }
    if s.ended {
        return Some(AgentState::Done);
    }
    None
}

/// Apply a worker signal to an agent to-do: derive `agent_state`, and keep
/// the decision in step — a pending gate or attention flag with no open
/// decision synthesizes one from the fleet's own fields (gate tool + input,
/// or the notification message); a synthesized decision whose wait has
/// ended clears. Worker-asked decisions are never touched here — only the
/// person's answer closes them. Human-lane to-dos are left alone. Returns
/// whether anything changed.
pub fn apply_worker_signal(todo: &mut Todo, signal: &WorkerSignal, now: DateTime<Utc>) -> bool {
    if todo.lane != Lane::Agent {
        return false;
    }
    let before = (todo.agent_state, todo.decision.clone());

    let waiting = signal.status == WorkerStatus::WaitingOnUser || signal.gate.is_some();
    let synthesized_open = todo
        .decision
        .as_ref()
        .is_some_and(|d| d.is_open() && d.source != DecisionSource::Worker);
    if waiting && !todo.has_open_decision() {
        // Don't re-raise a wait the person already answered.
        let answered_since = todo
            .decision
            .as_ref()
            .and_then(|d| d.resolved.as_ref())
            .zip(signal.attention_at)
            .is_some_and(|(a, at)| a.at >= at);
        if !answered_since {
            todo.decision = synthesize_decision(signal, now);
        }
    } else if !waiting && synthesized_open {
        todo.decision = None;
    }

    if let Some(state) = derive_agent_state(todo, Some(signal)) {
        todo.agent_state = Some(state);
    }
    let changed = before != (todo.agent_state, todo.decision.clone());
    if changed {
        todo.updated_at = now;
    }
    changed
}

fn synthesize_decision(signal: &WorkerSignal, now: DateTime<Utc>) -> Option<Decision> {
    let (question, options, source) = if let Some(g) = &signal.gate {
        (
            format!("Allow {}? {}", g.tool_name, g.input_summary.trim()),
            vec!["Approve".to_string(), "Deny".to_string()],
            DecisionSource::Gate {
                request_id: g.request_id.clone(),
            },
        )
    } else {
        let msg = signal.attention_message.as_deref().map(str::trim).filter(|m| !m.is_empty());
        let blocked = signal.brief.as_ref().and_then(|b| b.blocked.as_deref());
        let q = msg.or(blocked)?;
        (q.to_string(), Vec::new(), DecisionSource::Attention)
    };
    Some(Decision {
        question,
        options,
        est_minutes: Some(1),
        brief: signal.brief.clone(),
        asked_at: signal.attention_at.unwrap_or(now),
        worker: Some(signal.worker_name.clone()).filter(|w| !w.is_empty()),
        source,
        resolved: None,
    })
}

/// A worker (or the person, through the model) raises a decision on an
/// agent to-do: question, options, estimate and brief. Moves the to-do to
/// `NeedsYou`. Refuses on human to-dos, closed to-dos, and when a decision is
/// already open (one at a time keeps the queue honest).
#[allow(clippy::too_many_arguments)]
pub fn ask_decision(
    todo: &mut Todo,
    question: &str,
    options: Vec<String>,
    est_minutes: Option<u32>,
    brief: Option<Brief>,
    worker: Option<String>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if todo.lane != Lane::Agent {
        return Err(format!(
            "'{}' is in the human lane — decisions belong to agent to-dos.",
            todo.title
        ));
    }
    if todo.status.is_closed() {
        return Err(format!("'{}' is already closed.", todo.title));
    }
    if todo.has_open_decision() {
        return Err(format!("'{}' already has an open decision.", todo.title));
    }
    let question = question.trim();
    if question.is_empty() {
        return Err("A decision needs a one-line 'question'.".into());
    }
    let options: Vec<String> = options
        .into_iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    todo.decision = Some(Decision {
        question: question.to_string(),
        options,
        est_minutes,
        brief,
        asked_at: now,
        worker,
        source: DecisionSource::Worker,
        resolved: None,
    });
    todo.agent_state = Some(AgentState::NeedsYou);
    todo.updated_at = now;
    Ok(())
}

/// Record the person's answer to an open decision. Returns the log entry;
/// delivery to the worker happens at the edge, which then calls
/// [`record_delivery`]. Gate decisions are refused: a permission gate is the
/// person's own Approve/Deny in the Fleet or decision-queue UI
/// (`FleetShared::resolve_gate`), never something the model answers.
pub fn resolve_decision(
    state: &mut PlannerState,
    todo_id: &str,
    choice: &str,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ResolvedDecision, String> {
    let todo = state
        .todo_mut(todo_id)
        .ok_or_else(|| format!("unknown todo id '{}'", todo_id))?;
    let title = todo.title.clone();
    let decision = todo
        .decision
        .as_mut()
        .filter(|d| d.is_open())
        .ok_or_else(|| format!("'{}' has no open decision.", title))?;
    if matches!(decision.source, DecisionSource::Gate { .. }) {
        return Err(format!(
            "'{}' is waiting on a permission gate. Only the user can approve or deny it, \
             in the Fleet or the decision queue — it cannot be answered by a tool.",
            title
        ));
    }
    if choice.trim().is_empty() {
        return Err("Provide the 'choice'.".into());
    }
    let choice = decision.match_choice(choice);
    let note = note.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    decision.resolved = Some(super::model::DecisionAnswer {
        choice: choice.clone(),
        note: note.clone(),
        at: now,
        delivery: None,
    });
    let entry = ResolvedDecision {
        id: format!("dc_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        todo_id: todo_id.to_string(),
        question: decision.question.clone(),
        choice,
        note,
        worker: decision.worker.clone(),
        asked_at: decision.asked_at,
        resolved_at: now,
        source: decision.source.clone(),
        delivery: None,
    };
    todo.updated_at = now;
    state.decision_log.push(entry.clone());
    Ok(entry)
}

/// The message a Hobbes chat worker receives with the answer — first person,
/// as the user handing it back.
pub fn answer_message(d: &ResolvedDecision) -> String {
    let mut out = format!("Decision on \"{}\": {}.", d.question, d.choice);
    if let Some(n) = &d.note {
        out.push_str(&format!(" {}", n));
    }
    out.push_str(" Carry on from here.");
    out
}

/// Record how the answer reached the worker. Delivered → `Working` (the
/// worker has what it needs); not delivered → `Queued` (it must be resumed
/// with the answer), and the answer is surfaced on the to-do's progress line
/// so it isn't lost. Returns the updated log entry to persist.
pub fn record_delivery(
    state: &mut PlannerState,
    decision_id: &str,
    delivery: DecisionDelivery,
    now: DateTime<Utc>,
) -> Option<ResolvedDecision> {
    let entry = state.decision_log.iter_mut().find(|d| d.id == decision_id)?;
    entry.delivery = Some(delivery.clone());
    let entry = entry.clone();
    if let Some(todo) = state.todo_mut(&entry.todo_id) {
        if let Some(ans) = todo.decision.as_mut().and_then(|d| d.resolved.as_mut()) {
            ans.delivery = Some(delivery.clone());
        }
        match delivery {
            DecisionDelivery::ChatQueued { .. } => todo.agent_state = Some(AgentState::Working),
            DecisionDelivery::NotDelivered { .. } => {
                todo.agent_state = Some(AgentState::Queued);
                todo.latest_progress = Some(format!(
                    "Decided: {} → {} (not yet delivered to the worker; resume it with this answer)",
                    entry.question, entry.choice
                ));
            }
        }
        todo.updated_at = now;
    }
    Some(entry)
}

/// Agent to-dos waiting on the person, oldest question first.
pub fn needs_you(todos: &[Todo]) -> Vec<&Todo> {
    let mut v: Vec<&Todo> = todos
        .iter()
        .filter(|t| t.lane == Lane::Agent && !t.status.is_closed())
        .filter(|t| t.agent_state == Some(AgentState::NeedsYou) || t.has_open_decision())
        .collect();
    v.sort_by_key(|t| t.decision.as_ref().map(|d| d.asked_at).unwrap_or(t.updated_at));
    v
}

/// The agent lane, bucketed for the Today view and the tools.
#[derive(Debug, Default)]
pub struct AgentLane<'a> {
    /// Agent to-dos no worker is running yet (`Queued` or no state, unlinked).
    pub to_dispatch: Vec<&'a Todo>,
    pub working: Vec<&'a Todo>,
    pub needs_you: Vec<&'a Todo>,
    pub stuck: Vec<&'a Todo>,
    /// Agent to-dos that reached `Done` (or were completed) on `today`.
    pub done_today: Vec<&'a Todo>,
}

impl AgentLane<'_> {
    pub fn is_empty(&self) -> bool {
        self.to_dispatch.is_empty()
            && self.working.is_empty()
            && self.needs_you.is_empty()
            && self.stuck.is_empty()
            && self.done_today.is_empty()
    }

    /// `"2 working · 1 needs you · 1 done today"` — zero buckets omitted.
    pub fn counts_line(&self) -> String {
        let parts: Vec<String> = [
            (self.working.len(), "working"),
            (self.needs_you.len(), "needs you"),
            (self.stuck.len(), "stuck"),
            (self.to_dispatch.len(), "to dispatch"),
            (self.done_today.len(), "done today"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, l)| format!("{} {}", n, l))
        .collect();
        parts.join(" · ")
    }
}

/// Bucket the agent lane. `today` is a local day.
pub fn agent_lane(todos: &[Todo], today: NaiveDate) -> AgentLane<'_> {
    let mut lane = AgentLane::default();
    for t in todos.iter().filter(|t| t.lane == Lane::Agent) {
        if t.status == TodoStatus::Cancelled {
            continue;
        }
        let closed_today = t
            .completed_at
            .is_some_and(|c| c.with_timezone(&chrono::Local).date_naive() == today);
        if t.status == TodoStatus::Completed || t.agent_state == Some(AgentState::Done) {
            if closed_today || (t.status != TodoStatus::Completed
                && t.updated_at.with_timezone(&chrono::Local).date_naive() == today)
            {
                lane.done_today.push(t);
            }
            continue;
        }
        if t.has_open_decision() {
            lane.needs_you.push(t);
            continue;
        }
        match t.agent_state {
            Some(AgentState::NeedsYou) => lane.needs_you.push(t),
            Some(AgentState::Working) => lane.working.push(t),
            Some(AgentState::Stuck) => lane.stuck.push(t),
            Some(AgentState::Queued) | None => {
                if t.linked_fleet_session.is_some() && t.agent_state.is_none() {
                    lane.working.push(t);
                } else {
                    lane.to_dispatch.push(t);
                }
            }
            Some(AgentState::Done) => {}
        }
    }
    lane.needs_you
        .sort_by_key(|t| t.decision.as_ref().map(|d| d.asked_at).unwrap_or(t.updated_at));
    lane
}

/// `"about 2 min"`-style estimate for decision cards and tool output.
pub fn est_label(est: Option<u32>) -> String {
    match est {
        Some(m) if m > 0 => format!("about {} min", m),
        _ => "about 1 min".to_string(),
    }
}

/// One decision as a compact line: `[id] question (options; about N min; worker)`.
pub fn decision_line(t: &Todo) -> Option<String> {
    let d = t.decision.as_ref().filter(|d| d.is_open())?;
    let mut extras = Vec::new();
    if !d.options.is_empty() {
        let opts: Vec<String> = d
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}) {}", i + 1, o))
            .collect();
        extras.push(opts.join(" / "));
    }
    extras.push(est_label(d.est_minutes));
    if let Some(w) = &d.worker {
        extras.push(w.clone());
    }
    if matches!(d.source, DecisionSource::Gate { .. }) {
        extras.push("permission gate — the user approves in the Fleet".into());
    }
    Some(format!("[{}] {} ({})", t.id, d.question, extras.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_todo() -> Todo {
        let mut t = Todo::new("Ship release", 0.0);
        t.set_lane(Lane::Agent);
        t.linked_fleet_session = Some("w1".into());
        t
    }

    fn signal(status: WorkerStatus) -> WorkerSignal {
        WorkerSignal {
            status,
            ended: false,
            gate: None,
            attention_message: None,
            attention_at: None,
            brief: None,
            worker_name: "puget-app-packs".into(),
        }
    }

    #[test]
    fn set_lane_keeps_fields_coherent() {
        let mut t = Todo::new("x", 0.0);
        t.kind = Some(super::super::model::HumanKind::Chore);
        t.set_lane(Lane::Agent);
        assert_eq!(t.kind, None);
        assert_eq!(t.agent_state, Some(AgentState::Queued));
        t.set_lane(Lane::Human);
        assert_eq!(t.agent_state, None);
    }

    #[test]
    fn derivation_maps_only_explicit_states() {
        let t = agent_todo();
        assert_eq!(derive_agent_state(&t, None), None, "no signal, nothing to say");
        assert_eq!(derive_agent_state(&t, Some(&signal(WorkerStatus::Idle))), None, "idle is not judged");
        assert_eq!(
            derive_agent_state(&t, Some(&signal(WorkerStatus::Working))),
            Some(AgentState::Working)
        );
        assert_eq!(
            derive_agent_state(&t, Some(&signal(WorkerStatus::WaitingOnUser))),
            Some(AgentState::NeedsYou)
        );

        let mut blocked = signal(WorkerStatus::Idle);
        blocked.brief = Some(Brief {
            did: "Built 4 of 5".into(),
            blocked: Some("driver check fails".into()),
            next: None,
        });
        assert_eq!(derive_agent_state(&t, Some(&blocked)), Some(AgentState::Stuck));
        // Blocked text in the headline alone is NOT read as stuck.
        let mut headline_only = signal(WorkerStatus::Idle);
        headline_only.brief = Some(Brief { did: "blocked on review".into(), blocked: None, next: None });
        assert_eq!(derive_agent_state(&t, Some(&headline_only)), None);

        // A pending gate outranks a blocked brief: something IS pending.
        let mut gated = blocked.clone();
        gated.gate = Some(GateSignal { request_id: "g1".into(), tool_name: "Bash".into(), input_summary: "ls".into() });
        assert_eq!(derive_agent_state(&t, Some(&gated)), Some(AgentState::NeedsYou));

        let mut ended = signal(WorkerStatus::Idle);
        ended.ended = true;
        assert_eq!(derive_agent_state(&t, Some(&ended)), Some(AgentState::Done));

        let mut done = agent_todo();
        done.mark_completed(Utc::now());
        assert_eq!(derive_agent_state(&done, Some(&signal(WorkerStatus::Working))), Some(AgentState::Done));
    }

    #[test]
    fn gate_synthesizes_a_decision_and_clears_when_answered_elsewhere() {
        let mut t = agent_todo();
        let now = Utc::now();
        let mut gated = signal(WorkerStatus::WaitingOnUser);
        gated.gate = Some(GateSignal { request_id: "g1".into(), tool_name: "Bash".into(), input_summary: "npm publish".into() });
        assert!(apply_worker_signal(&mut t, &gated, now));
        assert_eq!(t.agent_state, Some(AgentState::NeedsYou));
        let d = t.decision.clone().unwrap();
        assert!(d.question.contains("Bash") && d.question.contains("npm publish"));
        assert_eq!(d.options, vec!["Approve", "Deny"]);
        assert!(matches!(d.source, DecisionSource::Gate { .. }));

        // Approved in the Fleet: the worker resumes; the synthesized
        // decision clears itself.
        assert!(apply_worker_signal(&mut t, &signal(WorkerStatus::Working), now));
        assert!(t.decision.is_none());
        assert_eq!(t.agent_state, Some(AgentState::Working));
        assert!(!apply_worker_signal(&mut t, &signal(WorkerStatus::Working), now), "idempotent");
    }

    #[test]
    fn worker_asked_decisions_survive_signals_and_human_todos_are_ignored() {
        let mut t = agent_todo();
        let now = Utc::now();
        ask_decision(&mut t, "Ship four now?", vec!["Yes".into(), "Wait".into()], Some(3), None, Some("w".into()), now).unwrap();
        apply_worker_signal(&mut t, &signal(WorkerStatus::Idle), now);
        assert!(t.has_open_decision());
        assert_eq!(t.agent_state, Some(AgentState::NeedsYou));
        assert!(ask_decision(&mut t, "again?", vec![], None, None, None, now).is_err(), "one at a time");

        let mut human = Todo::new("mine", 0.0);
        human.linked_fleet_session = Some("w1".into());
        assert!(!apply_worker_signal(&mut human, &signal(WorkerStatus::Working), now));
        assert!(human.agent_state.is_none());
        assert!(ask_decision(&mut human, "q", vec![], None, None, None, now).is_err());
    }

    #[test]
    fn resolve_records_matches_options_and_refuses_gates() {
        let now = Utc::now();
        let mut state = PlannerState::default();
        let mut t = agent_todo();
        ask_decision(&mut t, "Keep scrubber in Phase 2?", vec!["Keep in Phase 2".into(), "Move to Phase 3".into()], Some(2), None, Some("Scott Marsh".into()), now).unwrap();
        let id = t.id.clone();
        state.todos.push(t);

        let entry = resolve_decision(&mut state, &id, "2", Some("budget"), now).unwrap();
        assert_eq!(entry.choice, "Move to Phase 3");
        assert_eq!(state.decision_log.len(), 1);
        assert!(!state.todo(&id).unwrap().has_open_decision());
        assert!(answer_message(&entry).contains("Move to Phase 3"));
        assert!(resolve_decision(&mut state, &id, "1", None, now).is_err(), "already answered");

        let updated = record_delivery(&mut state, &entry.id, DecisionDelivery::NotDelivered { reason: "headless".into() }, now).unwrap();
        assert!(updated.delivery.is_some());
        let t = state.todo(&id).unwrap();
        assert_eq!(t.agent_state, Some(AgentState::Queued));
        assert!(t.latest_progress.as_deref().unwrap().contains("Move to Phase 3"));

        // Gate decisions are the user's own Approve/Deny.
        let mut g = agent_todo();
        let mut gated = signal(WorkerStatus::WaitingOnUser);
        gated.gate = Some(GateSignal { request_id: "g".into(), tool_name: "Bash".into(), input_summary: "rm".into() });
        apply_worker_signal(&mut g, &gated, now);
        let gid = g.id.clone();
        state.todos.push(g);
        let err = resolve_decision(&mut state, &gid, "Approve", None, now).unwrap_err();
        assert!(err.contains("permission gate"), "{err}");

        // Free-text answers pass through when they match no option.
        let mut f = agent_todo();
        ask_decision(&mut f, "Which name?", vec![], None, None, None, now).unwrap();
        let fid = f.id.clone();
        state.todos.push(f);
        assert_eq!(resolve_decision(&mut state, &fid, "Monica Leigh", None, now).unwrap().choice, "Monica Leigh");
    }

    #[test]
    fn agent_lane_buckets() {
        let today = chrono::Local::now().date_naive();
        let now = Utc::now();
        let mut queued = Todo::new("q", 0.0);
        queued.set_lane(Lane::Agent);
        let mut working = agent_todo();
        working.agent_state = Some(AgentState::Working);
        let mut needs = agent_todo();
        ask_decision(&mut needs, "?", vec![], None, None, None, now).unwrap();
        let mut stuck = agent_todo();
        stuck.agent_state = Some(AgentState::Stuck);
        let mut done = agent_todo();
        done.mark_completed(now);
        let human = Todo::new("h", 0.0);
        let todos = vec![queued, working, needs, stuck, done, human];
        let lane = agent_lane(&todos, today);
        assert_eq!(lane.to_dispatch.len(), 1);
        assert_eq!(lane.working.len(), 1);
        assert_eq!(lane.needs_you.len(), 1);
        assert_eq!(lane.stuck.len(), 1);
        assert_eq!(lane.done_today.len(), 1);
        assert_eq!(lane.counts_line(), "1 working · 1 needs you · 1 stuck · 1 to dispatch · 1 done today");
        assert_eq!(needs_you(&todos).len(), 1);
        assert!(decision_line(lane.needs_you[0]).unwrap().contains("about 1 min"));
    }
}
