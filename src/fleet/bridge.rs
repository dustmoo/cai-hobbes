//! Hobbes' own chat tabs reporting into the fleet.
//!
//! The fleet core is transport-agnostic: [`super::reduce`] folds
//! [`FleetEvent`]s regardless of producer. External Claude Code sessions
//! arrive over HTTP hooks; Hobbes tabs arrive here — lifecycle points in the
//! chat/stream path synthesize the equivalent events and fold them straight
//! into [`super::FleetShared`], so a Hobbes tab shows up beside terminal
//! sessions (working / needs-approval / idle, banked minutes, day rollup)
//! with [`FleetOrigin::Hobbes`] telling them apart.
//!
//! Gating: reports are dropped unless the supervisor has armed the bridge
//! (`fleet_enabled && pro_active()`, the same condition as the listener —
//! which also means the server's staleness sweep is running whenever Hobbes
//! rows are live). Callers never need settings access.
//!
//! Hobbes rows carry no `transcript_path`, so the brief supervisor skips
//! them — their state already lives in the app. (A later refinement could
//! surface `Session.conversation_summary` as their brief.)

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;

use super::events::FleetEvent;
use super::{store, FleetOrigin};

static BRIDGE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Armed by the fleet supervisor alongside the listener lifecycle.
pub fn set_enabled(on: bool) {
    BRIDGE_ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    BRIDGE_ENABLED.load(Ordering::Relaxed)
}

/// Lifecycle signals a Hobbes chat tab can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HobbesSignal {
    /// A turn is starting: the user sent a prompt or approved a paused tool.
    TurnStarted,
    /// Mid-turn liveness (a tool call dispatched or returned).
    Activity,
    /// The turn paused waiting for the user to approve a tool.
    GateWaiting { tool_name: String },
    /// The turn finished (or was cancelled) — idle.
    TurnCompleted,
    /// The tab was closed.
    Closed,
}

fn to_event(session_id: &str, signal: &HobbesSignal) -> FleetEvent {
    let session_id = session_id.to_string();
    // cwd stays empty: reduce() never clobbers name/cwd on empty, and the
    // display name is patched from the tab title below.
    let cwd = String::new();
    match signal {
        HobbesSignal::TurnStarted => FleetEvent::PromptSubmit { session_id, cwd },
        HobbesSignal::Activity => FleetEvent::ToolActivity {
            session_id,
            cwd,
            tool_name: String::new(),
        },
        HobbesSignal::GateWaiting { tool_name } => FleetEvent::Notification {
            session_id,
            cwd,
            kind: "permission_prompt".to_string(),
            message: format!("Waiting for tool approval: {tool_name}"),
        },
        HobbesSignal::TurnCompleted => FleetEvent::Stop {
            session_id,
            cwd,
            background_tasks: 0,
        },
        HobbesSignal::Closed => FleetEvent::SessionEnd {
            session_id,
            cwd,
            reason: "tab_closed".to_string(),
        },
    }
}

/// Report one lifecycle signal for a Hobbes chat tab. `name` is the tab
/// title (patched onto the row so the fleet shows it instead of a
/// cwd-derived name). No-op while the bridge is disarmed.
pub fn report(session_id: &str, name: &str, signal: HobbesSignal) {
    report_with_brief(session_id, name, signal, None);
}

/// [`report`] plus the session's `ConversationSummary` as JSON: on
/// `TurnCompleted`/`Closed` it becomes the row's re-entry brief via the same
/// tolerant mapper the LLM path uses (`briefs::brief_from_summary_value`) —
/// zero extra LLM calls, and the day rollup / planner context / fleet-status
/// tool pick it up for free.
pub fn report_with_brief(
    session_id: &str,
    name: &str,
    signal: HobbesSignal,
    summary_json: Option<serde_json::Value>,
) {
    if !enabled() || session_id.is_empty() {
        return;
    }
    let ev = to_event(session_id, &signal);
    let now = Utc::now();
    let is_final = matches!(signal, HobbesSignal::Closed);
    let brief = summary_json
        .filter(|_| matches!(signal, HobbesSignal::TurnCompleted | HobbesSignal::Closed))
        .and_then(|v| super::briefs::brief_from_summary_value(&v, now, is_final));
    let shared = super::shared();
    let changed = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        // Revival: a reopened tab (or one that retired for inactivity)
        // rejoins with its stored row, keeping banked time.
        if !state.sessions.contains_key(ev.session_id()) {
            if let Some(mut row) = store::load_session(ev.session_id()) {
                row.ended_at = None;
                row.pending_gate = None;
                state.sessions.insert(row.id.clone(), row);
            }
        }
        let mut changed = super::reduce(&mut state, &ev, now);
        // Patch identity on the live entry and the persisted clones: origin
        // marker plus the human tab title (created rows start "(unknown)"),
        // and the summary-derived brief when one rode along.
        let patch = |s: &mut super::FleetSession| {
            s.origin = FleetOrigin::Hobbes;
            if !name.is_empty() && s.name != name {
                s.name = name.to_string();
            }
            if let Some(b) = &brief {
                s.brief = Some(b.clone());
            }
        };
        if let Some(s) = state.sessions.get_mut(ev.session_id()) {
            patch(s);
        }
        for row in changed.iter_mut() {
            patch(row);
        }
        changed
    };
    store::persist_sessions(&changed);
    shared.poke();
}

/// Pre-create a fleet row for a headless run HOBBES_DISPATCH is about to
/// launch: the card appears instantly (Idle, dispatched chip, todo title as
/// its name) and the run's first hook event simply finds it — `reduce`'s
/// entry().or_insert never replaces an existing row, and events only
/// overwrite cwd/name when non-empty. Not gated on the bridge arm: dispatch
/// is explicit user-driven action, and the row must exist for the fleet to
/// attribute the run.
pub fn precreate_dispatched_session(session_id: &str, cwd: &str, title: &str, todo_id: &str) {
    let now = Utc::now();
    let shared = super::shared();
    let row = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        let session = state
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| super::FleetSession::new(session_id, cwd, now));
        session.session_title = Some(title.to_string());
        session.dispatched_todo = Some(todo_id.to_string());
        session.clone()
    };
    store::persist_sessions(std::slice::from_ref(&row));
    shared.poke();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::FleetStatus;

    // Tests drive the pure pieces (to_event + the reduce/patch composition on
    // a local FleetState) — the armed/disarmed static is process-global, so
    // report() itself is exercised only for the disarmed no-op.

    fn fold(state: &mut crate::fleet::FleetState, id: &str, name: &str, sig: HobbesSignal) {
        let ev = to_event(id, &sig);
        let now = Utc::now();
        let mut changed = crate::fleet::reduce(state, &ev, now);
        let patch = |s: &mut crate::fleet::FleetSession| {
            s.origin = FleetOrigin::Hobbes;
            if !name.is_empty() && s.name != name {
                s.name = name.to_string();
            }
        };
        if let Some(s) = state.sessions.get_mut(ev.session_id()) {
            patch(s);
        }
        changed.iter_mut().for_each(patch);
    }

    #[test]
    fn hobbes_tab_lifecycle_maps_onto_fleet_statuses() {
        let mut state = crate::fleet::FleetState::default();
        fold(&mut state, "tab1", "Case Study Metrics", HobbesSignal::TurnStarted);
        {
            let s = &state.sessions["tab1"];
            assert_eq!(s.status, FleetStatus::Working);
            assert_eq!(s.name, "Case Study Metrics");
            assert_eq!(s.origin, FleetOrigin::Hobbes);
            assert!(s.working_since.is_some());
            assert!(s.cwd.is_empty());
        }
        fold(&mut state, "tab1", "Case Study Metrics", HobbesSignal::Activity);
        assert_eq!(state.sessions["tab1"].status, FleetStatus::Working);

        fold(
            &mut state,
            "tab1",
            "Case Study Metrics",
            HobbesSignal::GateWaiting { tool_name: "GMAIL_SEND".into() },
        );
        {
            let s = &state.sessions["tab1"];
            assert!(s.status.needs_attention());
            assert!(s.pending_gate.is_none(), "in-app approvals hold no HTTP gate");
        }
        // Approving resumes the turn.
        fold(&mut state, "tab1", "Case Study Metrics", HobbesSignal::TurnStarted);
        assert_eq!(state.sessions["tab1"].status, FleetStatus::Working);

        fold(&mut state, "tab1", "Case Study Metrics", HobbesSignal::TurnCompleted);
        {
            let s = &state.sessions["tab1"];
            assert_eq!(s.status, FleetStatus::Idle);
            assert!(s.working_since.is_none());
            // No transcript → the brief supervisor never picks it up.
            assert!(s.brief_dirty_at.is_none());
            assert!(s.transcript_path.is_none());
        }

        fold(&mut state, "tab1", "Case Study Metrics", HobbesSignal::Closed);
        assert!(state.sessions.is_empty(), "closed tabs leave the live map");
    }

    #[test]
    fn renamed_tab_updates_the_row_name() {
        let mut state = crate::fleet::FleetState::default();
        fold(&mut state, "tab1", "New Chat", HobbesSignal::TurnStarted);
        fold(&mut state, "tab1", "Quarterly Report", HobbesSignal::Activity);
        assert_eq!(state.sessions["tab1"].name, "Quarterly Report");
    }

    #[test]
    fn summary_value_becomes_the_rows_brief() {
        // The mapper contract: a ConversationSummary serialized to JSON has
        // exactly the shape brief_from_summary_value consumes.
        let mut cs = crate::session::ConversationSummary::default();
        cs.summary = "Drafted the Puget proposal.".to_string();
        cs.entities
            .other_entities
            .insert("blockers".into(), serde_json::json!(["waiting on legal"]));
        let v = serde_json::to_value(&cs).unwrap();
        let brief =
            crate::fleet::briefs::brief_from_summary_value(&v, Utc::now(), true).unwrap();
        assert_eq!(brief.headline, "Drafted the Puget proposal.");
        assert_eq!(brief.blocked_on.as_deref(), Some("waiting on legal"));
        assert!(brief.final_brief);
    }

    #[test]
    fn disarmed_bridge_reports_nothing() {
        set_enabled(false);
        report("tabX", "X", HobbesSignal::TurnStarted);
        assert!(!crate::fleet::shared()
            .snapshot()
            .sessions
            .contains_key("tabX"));
    }

    #[test]
    fn origin_serde_defaults_external() {
        let legacy = r#"{"id":"x","cwd":"/a","name":"a","status":"Idle",
            "last_event_at":"2026-08-25T10:00:00Z","working_since":null}"#;
        let s: crate::fleet::FleetSession = serde_json::from_str(legacy).unwrap();
        assert_eq!(s.origin, FleetOrigin::External);
    }
}
