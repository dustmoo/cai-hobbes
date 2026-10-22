//! The Heartbeat's persisted state: last snapshots, the event journal,
//! salience decisions, the wake queue, the wake trail and the person's
//! reactions (future training labels). Every method is pure; persistence
//! lives in `mod.rs`.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::perception::{
    ci_events, fleet_events, merge_ci_snapshot, planner_events, FleetSnapshot, PlannerSnapshot,
};
use super::salience::{Choice, Decision};
use super::{CiRun, HeartbeatEvent};

/// Events kept in the journal (newest win).
pub const MAX_EVENTS: usize = 500;
/// Events older than this are dropped.
pub const EVENT_MAX_AGE_DAYS: i64 = 7;
/// Wake records kept.
pub const MAX_TRAIL: usize = 200;
/// Wake records older than this are dropped.
pub const TRAIL_MAX_AGE_DAYS: i64 = 7;
/// Reactions kept. Labels outlive their events (each carries the event's
/// kind and summary), so this cap is the only bound.
pub const MAX_REACTIONS: usize = 1000;

/// One wake attempt: run, or deferred by the budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeRecord {
    pub id: String,
    pub at: DateTime<Utc>,
    /// What prompted it ("worker_stuck: …", "scheduled check").
    pub trigger: String,
    #[serde(default)]
    pub event_ids: Vec<String>,
    /// e.g. "2 wake_now, 3 queue".
    #[serde(default)]
    pub choice_summary: String,
    #[serde(default)]
    pub deferred: bool,
    #[serde(default)]
    pub defer_reason: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// What the wake did, filled in when it finishes.
    #[serde(default)]
    pub outcome: Option<String>,
    /// What the wake actually did, one short line each ("Dispatched 'Clyde
    /// prep'", "Raised: defer the Hetzner cutover?"), read from its tool calls
    /// when it finishes. Empty = it decided nothing needed doing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
}

impl WakeRecord {
    pub fn fired(
        at: DateTime<Utc>,
        trigger: impl Into<String>,
        event_ids: Vec<String>,
        choice_summary: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            at,
            trigger: trigger.into(),
            event_ids,
            choice_summary: choice_summary.into(),
            deferred: false,
            defer_reason: None,
            cost_usd: None,
            outcome: None,
            actions: Vec::new(),
        }
    }

    pub fn deferred(
        at: DateTime<Utc>,
        trigger: impl Into<String>,
        event_ids: Vec<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            deferred: true,
            defer_reason: Some(reason.into()),
            ..Self::fired(at, trigger, event_ids, "")
        }
    }
}

/// What a reaction points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ReactionTarget {
    Event(String),
    Wake(String),
}

/// The person's reaction — a label for training a future decider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReactionKind {
    /// "Stop telling me about these" — carries the muted kind slug.
    MutedKind(String),
    Dismissed,
    Useful,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    pub id: String,
    pub at: DateTime<Utc>,
    pub target: ReactionTarget,
    pub reaction: ReactionKind,
    /// Copied from the target at reaction time so the label survives the
    /// event (or wake) being pruned: the event kind, or "wake".
    #[serde(default)]
    pub target_kind: Option<String>,
    /// The event summary or wake trigger.
    #[serde(default)]
    pub target_summary: Option<String>,
    /// The primary decision the event got.
    #[serde(default)]
    pub decided: Option<Choice>,
}

/// What [`HeartbeatState::ingest`] did with a batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IngestOutcome {
    /// Events that asked to wake now (also queued, so a deferred wake keeps
    /// them).
    pub wake_now: Vec<String>,
    /// Events newly added to the queue (including `wake_now`).
    pub queued: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatState {
    #[serde(default)]
    pub planner: Option<PlannerSnapshot>,
    #[serde(default)]
    pub fleet: Option<FleetSnapshot>,
    #[serde(default)]
    pub ci: Option<Vec<CiRun>>,
    /// The journal, oldest first.
    #[serde(default)]
    pub events: Vec<HeartbeatEvent>,
    /// Every decision made on a journalled event, primary and shadow.
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// Event ids waiting for the next wake, oldest first.
    #[serde(default)]
    pub queue: Vec<String>,
    /// Wake attempts, oldest first.
    #[serde(default)]
    pub trail: Vec<WakeRecord>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// The dedicated Heartbeat chat session wakes are delivered to, once
    /// created (not an open tab unless the user opens it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// "Clear trail": the card hides wake records at or before this time.
    /// The records stay for the budget (the hourly cap and today's spend
    /// count them) until they age out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_cleared_at: Option<DateTime<Utc>>,
}

impl HeartbeatState {
    // ── Perception ──

    /// Diff the planner against the stored snapshot and store the new one.
    /// The first capture returns no events.
    pub fn observe_planner(&mut self, cur: PlannerSnapshot, now: DateTime<Utc>) -> Vec<HeartbeatEvent> {
        let evs = match &self.planner {
            Some(prev) => planner_events(prev, &cur, now),
            None => Vec::new(),
        };
        self.planner = Some(cur);
        evs
    }

    /// Fleet counterpart of [`Self::observe_planner`].
    pub fn observe_fleet(&mut self, cur: FleetSnapshot, now: DateTime<Utc>) -> Vec<HeartbeatEvent> {
        let evs = match &self.fleet {
            Some(prev) => fleet_events(prev, &cur, now),
            None => Vec::new(),
        };
        self.fleet = Some(cur);
        evs
    }

    /// CI counterpart; stores the merged snapshot (see `merge_ci_snapshot`).
    pub fn observe_ci(&mut self, cur: &[CiRun]) -> Vec<HeartbeatEvent> {
        let (evs, next) = match &self.ci {
            Some(prev) => (ci_events(prev, cur), merge_ci_snapshot(prev, cur)),
            None => (Vec::new(), cur.to_vec()),
        };
        self.ci = Some(next);
        evs
    }

    // ── Journal ──

    /// Journal `events` with their `decisions`, queue the ones a primary
    /// decision routed to Queue / WakeNow, then prune. Shadow decisions are
    /// logged only. Events without a decision are journalled as notes;
    /// duplicate event ids are skipped.
    pub fn ingest(
        &mut self,
        events: Vec<HeartbeatEvent>,
        decisions: Vec<Decision>,
        now: DateTime<Utc>,
    ) -> IngestOutcome {
        let mut known: HashSet<String> = self.events.iter().map(|e| e.id.clone()).collect();
        let mut fresh: HashSet<String> = HashSet::new();
        for ev in events {
            if known.insert(ev.id.clone()) {
                fresh.insert(ev.id.clone());
                self.events.push(ev);
            }
        }
        let mut out = IngestOutcome::default();
        let mut queued: HashSet<String> = self.queue.iter().cloned().collect();
        for d in decisions {
            if !fresh.contains(&d.event_id) {
                continue;
            }
            if !d.shadow && matches!(d.choice, Choice::Queue | Choice::WakeNow) {
                if d.choice == Choice::WakeNow {
                    out.wake_now.push(d.event_id.clone());
                }
                if queued.insert(d.event_id.clone()) {
                    self.queue.push(d.event_id.clone());
                    out.queued.push(d.event_id.clone());
                }
            }
            self.decisions.push(d);
        }
        self.prune(now);
        out
    }

    /// The queued events, oldest first, without draining.
    pub fn queued(&self) -> Vec<&HeartbeatEvent> {
        self.queue.iter().filter_map(|id| self.event(id)).collect()
    }

    /// Take the queue (call once a wake actually runs).
    pub fn drain_queue(&mut self) -> Vec<HeartbeatEvent> {
        let ids = std::mem::take(&mut self.queue);
        ids.iter().filter_map(|id| self.event(id).cloned()).collect()
    }

    pub fn event(&self, id: &str) -> Option<&HeartbeatEvent> {
        self.events.iter().find(|e| e.id == id)
    }

    /// The primary (acted-on) decision for an event.
    pub fn decision_for(&self, event_id: &str) -> Option<&Decision> {
        self.decisions
            .iter()
            .rev()
            .find(|d| !d.shadow && d.event_id == event_id)
    }

    /// Events after `since`, oldest first.
    #[cfg(test)]
    pub fn events_since(&self, since: DateTime<Utc>) -> Vec<&HeartbeatEvent> {
        self.events.iter().filter(|e| e.at > since).collect()
    }

    /// Events after `since` about work `session_id` carries or dispatched
    /// (its `session_id` or `dispatched_from`) — the briefing's "changes to
    /// work you dispatched".
    pub fn events_for_session_since(&self, session_id: &str, since: DateTime<Utc>) -> Vec<&HeartbeatEvent> {
        self.events
            .iter()
            .filter(|e| e.at > since)
            .filter(|e| {
                e.session_id.as_deref() == Some(session_id)
                    || e.dispatched_from.as_deref() == Some(session_id)
            })
            .collect()
    }

    // ── Trail ──

    /// Append a wake record and return its id. A deferred record identical
    /// to the previous one (same reason, same events) refreshes its time
    /// instead, so a capped budget doesn't flood the trail.
    pub fn record_wake(&mut self, record: WakeRecord) -> String {
        if record.deferred {
            if let Some(last) = self.trail.last_mut() {
                if last.deferred
                    && last.defer_reason == record.defer_reason
                    && last.event_ids == record.event_ids
                {
                    last.at = record.at;
                    return last.id.clone();
                }
            }
        }
        let id = record.id.clone();
        self.trail.push(record);
        cap_front(&mut self.trail, MAX_TRAIL);
        id
    }

    /// Fill in a finished wake's cost and outcome. False if unknown.
    pub fn complete_wake(&mut self, id: &str, cost_usd: Option<f64>, outcome: Option<String>) -> bool {
        match self.trail.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                w.cost_usd = cost_usd;
                w.outcome = outcome;
                true
            }
            None => false,
        }
    }

    // ── Reactions ──

    /// Record the person's reaction, snapshotting the target's kind,
    /// summary and decision so the label outlives it. Returns its id.
    pub fn record_reaction(
        &mut self,
        target: ReactionTarget,
        reaction: ReactionKind,
        now: DateTime<Utc>,
    ) -> String {
        let (target_kind, target_summary, decided) = match &target {
            ReactionTarget::Event(id) => match self.event(id) {
                Some(e) => (
                    Some(e.kind.clone()),
                    Some(e.summary.clone()),
                    self.decision_for(id).map(|d| d.choice),
                ),
                None => (None, None, None),
            },
            ReactionTarget::Wake(id) => (
                Some("wake".to_string()),
                self.trail.iter().find(|w| &w.id == id).map(|w| w.trigger.clone()),
                None,
            ),
        };
        let r = Reaction {
            id: uuid::Uuid::new_v4().to_string(),
            at: now,
            target,
            reaction,
            target_kind,
            target_summary,
            decided,
        };
        let id = r.id.clone();
        self.reactions.push(r);
        cap_front(&mut self.reactions, MAX_REACTIONS);
        id
    }

    // ── Pruning ──

    /// Drop events older than a week and beyond the cap, then the decisions
    /// and queue entries that pointed at them; cap the trail and reactions.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(EVENT_MAX_AGE_DAYS);
        self.events.retain(|e| e.at >= cutoff);
        cap_front(&mut self.events, MAX_EVENTS);
        let live: HashSet<&str> = self.events.iter().map(|e| e.id.as_str()).collect();
        self.decisions.retain(|d| live.contains(d.event_id.as_str()));
        self.queue.retain(|id| live.contains(id.as_str()));
        let trail_cutoff = now - Duration::days(TRAIL_MAX_AGE_DAYS);
        self.trail.retain(|w| w.at >= trail_cutoff);
        cap_front(&mut self.trail, MAX_TRAIL);
        cap_front(&mut self.reactions, MAX_REACTIONS);
    }

    /// Hide the current wake trail from the card ("Clear trail").
    pub fn clear_trail(&mut self, now: DateTime<Utc>) {
        self.trail_cleared_at = Some(now);
    }

    /// The wake records the card shows: those after the last clear.
    pub fn visible_trail(&self) -> Vec<&WakeRecord> {
        self.trail
            .iter()
            .filter(|w| self.trail_cleared_at.is_none_or(|c| w.at > c))
            .collect()
    }
}

/// Keep the newest `max` (the tail) of an oldest-first list.
fn cap_front<T>(v: &mut Vec<T>, max: usize) {
    if v.len() > max {
        v.drain(..v.len() - max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::perception::*;
    use crate::heartbeat::{EventSource, Urgency};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 15, 0, 0).unwrap()
    }

    fn ev(kind: &str, at: DateTime<Utc>) -> HeartbeatEvent {
        HeartbeatEvent::new(EventSource::Planner, kind, format!("{kind} happened"), Urgency::Normal, at)
    }

    fn dec(e: &HeartbeatEvent, choice: Choice) -> Decision {
        Decision { event_id: e.id.clone(), choice, confidence: 1.0, by: "rules".into(), shadow: false }
    }

    #[test]
    fn ingest_queues_and_reports_wakes() {
        let mut s = HeartbeatState::default();
        let a = ev("a", now());
        let b = ev("b", now());
        let c = ev("c", now());
        let mut shadow = dec(&c, Choice::WakeNow);
        shadow.shadow = true;
        let out = s.ingest(
            vec![a.clone(), b.clone(), c.clone()],
            vec![dec(&a, Choice::WakeNow), dec(&b, Choice::Queue), dec(&c, Choice::Note), shadow],
            now(),
        );
        assert_eq!(out.wake_now, vec![a.id.clone()]);
        assert_eq!(out.queued, vec![a.id.clone(), b.id.clone()]);
        assert_eq!(s.events.len(), 3);
        assert_eq!(s.decisions.len(), 4);
        assert_eq!(s.decision_for(&c.id).unwrap().choice, Choice::Note);
        assert_eq!(s.queued().len(), 2);

        // Re-ingesting the same event is a no-op.
        let again = s.ingest(vec![a.clone()], vec![dec(&a, Choice::WakeNow)], now());
        assert_eq!(again, IngestOutcome::default());
        assert_eq!(s.events.len(), 3);

        let drained = s.drain_queue();
        assert_eq!(drained.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(s.queue.is_empty());
        assert!(s.drain_queue().is_empty());
    }

    #[test]
    fn first_capture_is_silent_then_diffs() {
        let mut s = HeartbeatState::default();
        let mut t = crate::todo::model::Todo::new("ship", 1.0);
        t.id = "a".into();
        t.set_lane(crate::todo::model::Lane::Agent);
        t.agent_state = Some(crate::todo::model::AgentState::Working);
        let today = now().date_naive();
        assert!(s
            .observe_planner(PlannerSnapshot::capture_on(std::slice::from_ref(&t), now(), today), now())
            .is_empty());
        t.agent_state = Some(crate::todo::model::AgentState::Stuck);
        let evs = s.observe_planner(PlannerSnapshot::capture_on(&[t], now(), today), now());
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, KIND_WORKER_STUCK);

        let mut live = crate::fleet::FleetState::default();
        assert!(s.observe_fleet(FleetSnapshot::capture(&live), now()).is_empty());
        live.coverage_open = None;
        assert!(s.observe_fleet(FleetSnapshot::capture(&live), now()).is_empty());

        let run = CiRun {
            project_id: "p".into(),
            project_title: "P".into(),
            workflow: "CI".into(),
            branch: "main".into(),
            conclusion: crate::heartbeat::CiConclusion::Failure,
            url: String::new(),
            finished_at: now(),
        };
        assert!(s.observe_ci(std::slice::from_ref(&run)).is_empty());
        assert!(s.observe_ci(&[run]).is_empty());
    }

    #[test]
    fn session_and_time_queries() {
        let mut s = HeartbeatState::default();
        let mut mine = ev("worker_done", now());
        mine.session_id = Some("sess".into());
        let mut dispatched = ev("run_ended", now());
        dispatched.session_id = Some("run-1".into());
        dispatched.dispatched_from = Some("sess".into());
        let mut old = ev("worker_done", now() - Duration::hours(3));
        old.session_id = Some("sess".into());
        let other = ev("todo_overdue", now());
        s.ingest(vec![old, mine, dispatched, other], vec![], now());
        let since = now() - Duration::hours(1);
        assert_eq!(s.events_for_session_since("sess", since).len(), 2);
        assert_eq!(s.events_for_session_since("nobody", since).len(), 0);
        assert_eq!(s.events_since(since).len(), 3);
        assert_eq!(s.events_since(now() - Duration::hours(4)).len(), 4);
    }

    #[test]
    fn pruning_caps_and_ages() {
        let mut s = HeartbeatState::default();
        let stale = ev("old", now() - Duration::days(8));
        s.events.push(stale.clone());
        s.decisions.push(dec(&stale, Choice::Queue));
        s.queue.push(stale.id.clone());
        let fresh: Vec<_> = (0..MAX_EVENTS + 20).map(|i| ev(&format!("e{i}"), now())).collect();
        let decisions: Vec<_> = fresh.iter().map(|e| dec(e, Choice::Queue)).collect();
        s.ingest(fresh.clone(), decisions, now());
        assert_eq!(s.events.len(), MAX_EVENTS);
        assert_eq!(s.events[0].id, fresh[20].id, "oldest dropped first");
        assert!(s.event(&stale.id).is_none());
        assert_eq!(s.decisions.len(), MAX_EVENTS);
        assert_eq!(s.queue.len(), MAX_EVENTS);
        assert!(!s.queue.contains(&stale.id));

        for i in 0..MAX_TRAIL + 5 {
            s.record_wake(WakeRecord::fired(now(), format!("w{i}"), vec![], ""));
        }
        assert_eq!(s.trail.len(), MAX_TRAIL);
        assert_eq!(s.trail[0].trigger, "w5");
    }

    #[test]
    fn clearing_hides_the_trail_but_keeps_it_for_the_budget() {
        let mut s = HeartbeatState::default();
        s.record_wake(WakeRecord::fired(now() - Duration::minutes(20), "a", vec![], ""));
        s.record_wake(WakeRecord::fired(now() - Duration::minutes(10), "b", vec![], ""));
        s.clear_trail(now());
        assert!(s.visible_trail().is_empty());
        assert_eq!(s.trail.len(), 2, "kept for the hourly cap and today's spend");
        s.record_wake(WakeRecord::fired(now() + Duration::minutes(5), "c", vec![], ""));
        assert_eq!(s.visible_trail().len(), 1);
        // Records age out after a week regardless.
        s.prune(now() + Duration::days(TRAIL_MAX_AGE_DAYS) + Duration::minutes(1));
        assert_eq!(s.trail.len(), 1);
    }

    #[test]
    fn deferred_wakes_coalesce_and_wakes_complete() {
        let mut s = HeartbeatState::default();
        let a = s.record_wake(WakeRecord::deferred(now(), "t", vec!["e".into()], "hourly cap"));
        let b = s.record_wake(WakeRecord::deferred(now() + Duration::minutes(1), "t", vec!["e".into()], "hourly cap"));
        assert_eq!(a, b);
        assert_eq!(s.trail.len(), 1);
        assert_eq!(s.trail[0].at, now() + Duration::minutes(1));
        let c = s.record_wake(WakeRecord::deferred(now(), "t", vec!["e".into()], "daily budget"));
        assert_ne!(a, c);
        let w = s.record_wake(WakeRecord::fired(now(), "t", vec!["e".into()], "1 wake_now"));
        assert_eq!(s.trail.len(), 3);
        assert!(s.complete_wake(&w, Some(0.12), Some("drafted a reply".into())));
        assert_eq!(s.trail[2].cost_usd, Some(0.12));
        assert!(!s.complete_wake("missing", None, None));
    }

    #[test]
    fn reactions_snapshot_their_target() {
        let mut s = HeartbeatState::default();
        let e = ev("todo_overdue", now());
        s.ingest(vec![e.clone()], vec![dec(&e, Choice::Queue)], now());
        let w = s.record_wake(WakeRecord::fired(now(), "worker_stuck: x", vec![], ""));
        s.record_reaction(ReactionTarget::Event(e.id.clone()), ReactionKind::MutedKind("todo_overdue".into()), now());
        s.record_reaction(ReactionTarget::Wake(w), ReactionKind::Useful, now());
        s.record_reaction(ReactionTarget::Event("gone".into()), ReactionKind::Dismissed, now());
        assert_eq!(s.reactions.len(), 3);
        let r = &s.reactions[0];
        assert_eq!(r.target_kind.as_deref(), Some("todo_overdue"));
        assert_eq!(r.target_summary.as_deref(), Some("todo_overdue happened"));
        assert_eq!(r.decided, Some(Choice::Queue));
        assert_eq!(s.reactions[1].target_kind.as_deref(), Some("wake"));
        assert_eq!(s.reactions[1].target_summary.as_deref(), Some("worker_stuck: x"));
        assert_eq!(s.reactions[2].target_kind, None);

        // Labels survive their event being pruned.
        s.prune(now() + Duration::days(8));
        assert!(s.events.is_empty());
        assert_eq!(s.reactions.len(), 3);
    }

    #[test]
    fn serde_round_trip_and_defaults() {
        let mut s = HeartbeatState::default();
        let e = ev("worker_done", now());
        s.ingest(vec![e.clone()], vec![dec(&e, Choice::Queue)], now());
        s.record_wake(WakeRecord::deferred(now(), "t", vec![e.id.clone()], "hourly cap"));
        s.record_reaction(ReactionTarget::Event(e.id.clone()), ReactionKind::Useful, now());
        s.ci = Some(vec![]);
        s.fleet = Some(FleetSnapshot::default());
        let json = serde_json::to_string(&s).unwrap();
        let back: HeartbeatState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        // An empty object (and unknown future fields) load as defaults.
        let empty: HeartbeatState = serde_json::from_str(r#"{"future_field": 1}"#).unwrap();
        assert_eq!(empty, HeartbeatState::default());
        // A minimal event / wake from an older build loads.
        let minimal: HeartbeatState = serde_json::from_str(&format!(
            r#"{{"events":[{{"id":"x","at":"{0}","source":"email","kind":"mail","summary":"s"}}],
                "trail":[{{"id":"w","at":"{0}","trigger":"t"}}]}}"#,
            now().to_rfc3339()
        ))
        .unwrap();
        assert_eq!(minimal.events[0].source, EventSource::Other);
        assert_eq!(minimal.events[0].urgency, Urgency::Normal);
        assert!(!minimal.trail[0].deferred);
    }
}
