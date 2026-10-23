//! Salience: route each event to ignore / note / queue / wake-now.
//!
//! [`RulesSalience`] is the default — deterministic and cheap. The
//! [`Salience`] trait exists so a learned model can sit behind the same
//! call, first in shadow (see [`ShadowSalience`]).

use std::collections::HashSet;
#[cfg(test)]
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::perception::*;
use super::{EventSource, HeartbeatEvent, Urgency};

/// What to do with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    /// Drop it (still journalled, so a reaction can label it).
    Ignore,
    /// Journal it; briefings may mention it, nothing waits on it.
    Note,
    /// Hold it for the next wake.
    Queue,
    /// Wake now (budget permitting; otherwise it stays queued).
    WakeNow,
}

impl Choice {
    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Ignore => "ignore",
            Choice::Note => "note",
            Choice::Queue => "queue",
            Choice::WakeNow => "wake_now",
        }
    }
}

/// One salience verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub event_id: String,
    pub choice: Choice,
    /// 0.0–1.0. Rules use 1.0 for an explicit table row, 0.5 for the
    /// urgency fallback.
    pub confidence: f32,
    /// Which decider made it ([`Salience::name`]).
    pub by: String,
    /// A shadow decider's verdict: logged for comparison, never acted on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadow: bool,
}

/// What the rules need beyond the events themselves.
#[derive(Debug, Clone, Default)]
pub struct SalienceCtx {
    /// Event kinds the person muted (`heartbeat_muted_kinds`).
    pub muted_kinds: HashSet<String>,
    /// Sources switched off (`heartbeat_source_*`).
    pub muted_sources: HashSet<EventSource>,
    /// To-dos an open timer intention waits on (`WakeCondition::WorkerDone`).
    pub intention_todo_ids: HashSet<String>,
    /// To-dos the Heartbeat itself dispatched.
    pub heartbeat_todo_ids: HashSet<String>,
    /// The dedicated Heartbeat chat session, once it exists — runs
    /// dispatched from it count as the Heartbeat's own.
    pub heartbeat_session_id: Option<String>,
    /// Projects with open agent-lane work (see [`agent_project_ids`]).
    pub agent_project_ids: HashSet<String>,
    /// `dispatch_auto_continue`: a finished run already hands its result
    /// back to the chat that dispatched it, the Heartbeat session included.
    pub dispatch_auto_continue: bool,
}

impl SalienceCtx {
    /// Mutes from settings; the caller fills the planner/timer sets.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let mut muted_sources = HashSet::new();
        if !settings.heartbeat_source_planner {
            muted_sources.insert(EventSource::Planner);
        }
        if !settings.heartbeat_source_fleet {
            muted_sources.insert(EventSource::Fleet);
        }
        if !settings.heartbeat_source_ci {
            muted_sources.insert(EventSource::Ci);
        }
        Self {
            muted_kinds: settings
                .heartbeat_muted_kinds
                .iter()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect(),
            muted_sources,
            dispatch_auto_continue: settings.dispatch_auto_continue,
            ..Default::default()
        }
    }

    /// A run the Heartbeat session dispatched, whose result the auto-continue
    /// already delivers to it: a wake on top would be a second turn.
    fn continued_into_heartbeat(&self, ev: &HeartbeatEvent) -> bool {
        self.dispatch_auto_continue
            && self.heartbeat_session_id.is_some()
            && ev.dispatched_from == self.heartbeat_session_id
    }

    /// Work the Heartbeat is waiting on or launched itself.
    fn awaited(&self, ev: &HeartbeatEvent) -> bool {
        ev.todo_id
            .as_ref()
            .is_some_and(|t| self.intention_todo_ids.contains(t) || self.heartbeat_todo_ids.contains(t))
            || (self.heartbeat_session_id.is_some() && ev.dispatched_from == self.heartbeat_session_id)
    }
}

/// To-dos that a pending timer intention is waiting on.
pub fn intention_todo_ids<'a>(
    timers: impl IntoIterator<Item = &'a crate::timers::ScheduledTimer>,
) -> HashSet<String> {
    timers
        .into_iter()
        .filter(|t| t.status == crate::timers::TimerStatus::Pending)
        .filter_map(|t| match t.until.as_ref()? {
            crate::timers::WakeCondition::WorkerDone { todo_id } => Some(todo_id.clone()),
        })
        .collect()
}

/// Projects with at least one open agent-lane to-do.
pub fn agent_project_ids(planner: &crate::todo::PlannerState) -> HashSet<String> {
    planner
        .todos
        .iter()
        .filter(|t| t.lane == crate::todo::model::Lane::Agent && !t.status.is_closed())
        .filter_map(|t| t.project_id.clone())
        .collect()
}

/// A salience decider.
pub trait Salience {
    /// One decision per event, in order.
    fn decide(&self, events: &[HeartbeatEvent], ctx: &SalienceCtx) -> Vec<Decision>;
    fn name(&self) -> &'static str;
}

/// The default, deterministic decider. Rows are checked in order; the first
/// match wins:
///
/// | # | Event                                         | Choice   |
/// |---|-----------------------------------------------|----------|
/// | 1 | source or kind muted                          | Ignore   |
/// | 2 | `worker_needs_you`, `worker_stuck`, `decision_raised` | WakeNow |
/// | 3 | `ci_failed` on a project with open agent work | WakeNow  |
/// | 4 | `ci_failed` elsewhere                         | Queue    |
/// | 4b | `run_ended` from the Heartbeat session while auto-continue hands the result back | Note |
/// | 5 | `worker_done` / `run_ended` / `session_needs_attention` the Heartbeat awaits (pending intention on the to-do, a to-do it dispatched, or a run from its own session) | WakeNow |
/// | 6 | `worker_done`, `session_needs_attention`      | Queue    |
/// | 7 | `run_ended` (the dispatching chat is continued anyway) | Note |
/// | 8 | `todo_overdue`, `ci_recovered`                | Queue    |
/// | 9 | anything else: Low → Note, Normal/High → Queue (confidence 0.5) | — |
///
/// Unknown kinds never wake on their own: a new source earns a row first.
#[derive(Debug, Clone, Copy, Default)]
pub struct RulesSalience;

impl RulesSalience {
    pub const NAME: &'static str = "rules";

    pub fn decide_one(&self, ev: &HeartbeatEvent, ctx: &SalienceCtx) -> (Choice, f32) {
        if ctx.muted_sources.contains(&ev.source) || ctx.muted_kinds.contains(&ev.kind) {
            return (Choice::Ignore, 1.0);
        }
        let choice = match ev.kind.as_str() {
            KIND_WORKER_NEEDS_YOU | KIND_WORKER_STUCK | KIND_DECISION_RAISED => Choice::WakeNow,
            KIND_CI_FAILED => {
                let agent_work = ev
                    .project_id
                    .as_ref()
                    .is_some_and(|p| ctx.agent_project_ids.contains(p));
                if agent_work { Choice::WakeNow } else { Choice::Queue }
            }
            KIND_RUN_ENDED if ctx.continued_into_heartbeat(ev) => Choice::Note,
            KIND_WORKER_DONE | KIND_RUN_ENDED | KIND_SESSION_NEEDS_ATTENTION if ctx.awaited(ev) => {
                Choice::WakeNow
            }
            KIND_WORKER_DONE | KIND_SESSION_NEEDS_ATTENTION => Choice::Queue,
            KIND_RUN_ENDED => Choice::Note,
            KIND_TODO_OVERDUE | KIND_CI_RECOVERED => Choice::Queue,
            _ => {
                let c = if ev.urgency == Urgency::Low { Choice::Note } else { Choice::Queue };
                return (c, 0.5);
            }
        };
        (choice, 1.0)
    }
}

impl Salience for RulesSalience {
    fn decide(&self, events: &[HeartbeatEvent], ctx: &SalienceCtx) -> Vec<Decision> {
        events
            .iter()
            .map(|ev| {
                let (choice, confidence) = self.decide_one(ev, ctx);
                Decision {
                    event_id: ev.id.clone(),
                    choice,
                    confidence,
                    by: Self::NAME.to_string(),
                    shadow: false,
                }
            })
            .collect()
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }
}

/// Runs two deciders: acts on `primary`, records `shadow` for comparison.
///
/// [`Salience::decide`] returns the primary's decisions unchanged; the
/// shadow's (marked `shadow: true`) accumulate until
/// [`Self::take_shadow_decisions`] hands them to
/// [`super::HeartbeatState::ingest`], which logs them without acting.
///
/// Test-only until a shadow exists: this is the seam the runtime wraps
/// `RulesSalience` in (see `runtime.rs`); drop the `cfg(test)` then.
///
/// The intended shadow is **Laya**, a ModernBERT decision model. It is not
/// implemented here: Laya needs fine-tuning on our own labelled decisions
/// first, which is why every decision and the person's reactions to events
/// and wakes are logged in `HeartbeatState`.
#[cfg(test)]
pub struct ShadowSalience<P: Salience, S: Salience> {
    pub primary: P,
    pub shadow: S,
    recorded: Mutex<Vec<Decision>>,
}

#[cfg(test)]
impl<P: Salience, S: Salience> ShadowSalience<P, S> {
    pub fn new(primary: P, shadow: S) -> Self {
        Self { primary, shadow, recorded: Mutex::new(Vec::new()) }
    }

    /// The shadow decisions recorded since the last call.
    pub fn take_shadow_decisions(&self) -> Vec<Decision> {
        std::mem::take(&mut *self.recorded.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
impl<P: Salience, S: Salience> Salience for ShadowSalience<P, S> {
    fn decide(&self, events: &[HeartbeatEvent], ctx: &SalienceCtx) -> Vec<Decision> {
        let shadow = self.shadow.decide(events, ctx).into_iter().map(|mut d| {
            d.shadow = true;
            d
        });
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(shadow);
        self.primary.decide(events, ctx)
    }

    fn name(&self) -> &'static str {
        self.primary.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev(source: EventSource, kind: &str, urgency: Urgency) -> HeartbeatEvent {
        HeartbeatEvent::new(source, kind, "x", urgency, Utc::now())
    }

    fn choice(e: &HeartbeatEvent, ctx: &SalienceCtx) -> Choice {
        RulesSalience.decide_one(e, ctx).0
    }

    #[test]
    fn rules_table() {
        let ctx = SalienceCtx::default();
        let p = EventSource::Planner;
        assert_eq!(choice(&ev(p, KIND_WORKER_NEEDS_YOU, Urgency::High), &ctx), Choice::WakeNow);
        assert_eq!(choice(&ev(p, KIND_WORKER_STUCK, Urgency::High), &ctx), Choice::WakeNow);
        assert_eq!(choice(&ev(p, KIND_DECISION_RAISED, Urgency::High), &ctx), Choice::WakeNow);
        assert_eq!(choice(&ev(p, KIND_WORKER_DONE, Urgency::Normal), &ctx), Choice::Queue);
        assert_eq!(choice(&ev(p, KIND_TODO_OVERDUE, Urgency::Normal), &ctx), Choice::Queue);
        let f = EventSource::Fleet;
        assert_eq!(choice(&ev(f, KIND_RUN_ENDED, Urgency::Low), &ctx), Choice::Note);
        assert_eq!(choice(&ev(f, KIND_SESSION_NEEDS_ATTENTION, Urgency::High), &ctx), Choice::Queue);
        let c = EventSource::Ci;
        assert_eq!(choice(&ev(c, KIND_CI_RECOVERED, Urgency::Normal), &ctx), Choice::Queue);
        assert_eq!(choice(&ev(c, KIND_CI_FAILED, Urgency::High), &ctx), Choice::Queue);
        // Unknown kinds fall back on urgency and never wake.
        assert_eq!(RulesSalience.decide_one(&ev(c, "mystery", Urgency::High), &ctx), (Choice::Queue, 0.5));
        assert_eq!(choice(&ev(c, "mystery", Urgency::Low), &ctx), Choice::Note);
    }

    #[test]
    fn ci_failed_wakes_on_projects_with_agent_work() {
        let mut ctx = SalienceCtx::default();
        ctx.agent_project_ids.insert("p".into());
        let mut e = ev(EventSource::Ci, KIND_CI_FAILED, Urgency::High);
        e.project_id = Some("p".into());
        assert_eq!(choice(&e, &ctx), Choice::WakeNow);
        e.project_id = Some("other".into());
        assert_eq!(choice(&e, &ctx), Choice::Queue);
    }

    #[test]
    fn awaited_work_wakes() {
        let mut e = ev(EventSource::Planner, KIND_WORKER_DONE, Urgency::Normal);
        e.todo_id = Some("t".into());
        let mut ctx = SalienceCtx::default();
        ctx.intention_todo_ids.insert("t".into());
        assert_eq!(choice(&e, &ctx), Choice::WakeNow);

        let mut ctx = SalienceCtx::default();
        ctx.heartbeat_todo_ids.insert("t".into());
        assert_eq!(choice(&e, &ctx), Choice::WakeNow);

        let mut r = ev(EventSource::Fleet, KIND_RUN_ENDED, Urgency::Low);
        r.dispatched_from = Some("hb".into());
        let mut ctx = SalienceCtx::default();
        assert_eq!(choice(&r, &ctx), Choice::Note);
        ctx.heartbeat_session_id = Some("hb".into());
        assert_eq!(choice(&r, &ctx), Choice::WakeNow);
        // With auto-continue the result is already handed back: no second turn.
        ctx.dispatch_auto_continue = true;
        assert_eq!(choice(&r, &ctx), Choice::Note);
        ctx.dispatch_auto_continue = false;
        // No Heartbeat session: a run with no parent is not "awaited".
        r.dispatched_from = None;
        ctx.heartbeat_session_id = None;
        assert_eq!(choice(&r, &ctx), Choice::Note);
    }

    #[test]
    fn mutes_win() {
        let mut ctx = SalienceCtx::default();
        ctx.muted_kinds.insert(KIND_WORKER_STUCK.into());
        assert_eq!(choice(&ev(EventSource::Planner, KIND_WORKER_STUCK, Urgency::High), &ctx), Choice::Ignore);
        let mut ctx = SalienceCtx::default();
        ctx.muted_sources.insert(EventSource::Ci);
        assert_eq!(choice(&ev(EventSource::Ci, KIND_CI_FAILED, Urgency::High), &ctx), Choice::Ignore);
    }

    #[test]
    fn ctx_from_settings() {
        let s = crate::settings::Settings {
            heartbeat_source_fleet: false,
            heartbeat_muted_kinds: vec![" todo_overdue ".into(), "".into()],
            ..Default::default()
        };
        let ctx = SalienceCtx::from_settings(&s);
        assert_eq!(ctx.muted_sources, HashSet::from([EventSource::Fleet]));
        assert_eq!(ctx.muted_kinds, HashSet::from(["todo_overdue".to_string()]));
    }

    #[test]
    fn decide_preserves_order_and_names() {
        let evs = vec![
            ev(EventSource::Planner, KIND_WORKER_STUCK, Urgency::High),
            ev(EventSource::Planner, KIND_TODO_OVERDUE, Urgency::Normal),
        ];
        let ds = RulesSalience.decide(&evs, &SalienceCtx::default());
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].event_id, evs[0].id);
        assert_eq!(ds[1].choice, Choice::Queue);
        assert!(ds.iter().all(|d| d.by == "rules" && !d.shadow));
    }

    struct AlwaysIgnore;
    impl Salience for AlwaysIgnore {
        fn decide(&self, events: &[HeartbeatEvent], _: &SalienceCtx) -> Vec<Decision> {
            events
                .iter()
                .map(|e| Decision {
                    event_id: e.id.clone(),
                    choice: Choice::Ignore,
                    confidence: 0.9,
                    by: "ignore".into(),
                    shadow: false,
                })
                .collect()
        }
        fn name(&self) -> &'static str {
            "ignore"
        }
    }

    #[test]
    fn shadow_returns_primary_and_records_shadow() {
        let s = ShadowSalience::new(RulesSalience, AlwaysIgnore);
        let evs = vec![ev(EventSource::Planner, KIND_WORKER_STUCK, Urgency::High)];
        let ds = s.decide(&evs, &SalienceCtx::default());
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].choice, Choice::WakeNow);
        assert!(!ds[0].shadow);
        assert_eq!(s.name(), "rules");
        let shadow = s.take_shadow_decisions();
        assert_eq!(shadow.len(), 1);
        assert_eq!(shadow[0].choice, Choice::Ignore);
        assert!(shadow[0].shadow);
        assert_eq!(shadow[0].by, "ignore");
        assert!(s.take_shadow_decisions().is_empty());
    }
}
