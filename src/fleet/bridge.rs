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
    if !enabled() || session_id.is_empty() {
        return;
    }
    let ev = to_event(session_id, &signal);
    let now = Utc::now();
    let shared = super::shared();
    let changed = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        let mut changed = super::reduce(&mut state, &ev, now);
        // Patch identity on the live entry and the persisted clones: origin
        // marker plus the human tab title (created rows start "(unknown)").
        let patch = |s: &mut super::FleetSession| {
            s.origin = FleetOrigin::Hobbes;
            if !name.is_empty() && s.name != name {
                s.name = name.to_string();
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
