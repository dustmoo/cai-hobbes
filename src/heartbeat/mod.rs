//! The Heartbeat: a harness that stays awake between chats (Phase 2).
//!
//! Perception (cheap watchers, no LLM) turns planner, Fleet and CI changes
//! into events; salience routes each to ignore / note / queue / wake; the
//! budget caps wakes; a wake hands the dedicated Heartbeat session a
//! briefing. Design: the "Hobbes Heartbeat" doc.
//!
//! This module is the pure engine; the runtime (polling, the Heartbeat
//! session, the UI) is wired in from `main.rs`. One poll looks like:
//!
//! ```text
//! let evs = state.observe_planner(PlannerSnapshot::capture(&planner, now), now);
//! //   + observe_fleet / observe_ci for the enabled sources
//! let decisions = salience.decide(&evs, &ctx);
//! let out = state.ingest(evs, decisions, now);
//! if !out.wake_now.is_empty() {
//!     match check_wake(&state.trail, &budget, spent_since(&state.trail, local_day_start(now)), now) {
//!         Ok(()) => { /* drain_queue → briefing → record_wake(WakeRecord::fired(..)) */ }
//!         Err(reason) => { state.record_wake(WakeRecord::deferred(.., reason)); } // stays queued
//!     }
//! }
//! ```

// The runtime pass wires this in and removes the allow.
#![allow(dead_code)]

pub mod budget;
pub mod ci;
pub mod perception;
pub mod salience;
pub mod state;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// The engine's public surface for the runtime pass (unused until then).
#[allow(unused_imports)]
pub use budget::{check_wake, local_day_start, spent_since, Budget};
#[allow(unused_imports)]
pub use perception::{
    ci_events, fleet_events, merge_ci_snapshot, planner_events, FleetSnapshot, PlannerSnapshot,
};
#[allow(unused_imports)]
pub use salience::{
    agent_project_ids, intention_todo_ids, Choice, Decision, RulesSalience, Salience, SalienceCtx,
    ShadowSalience,
};
#[allow(unused_imports)]
pub use state::{
    HeartbeatState, IngestOutcome, Reaction, ReactionKind, ReactionTarget, WakeRecord,
};

// ── Availability ────────────────────────────────────────────────────────────

/// Flip to gate the Heartbeat behind Pro.
pub const HEARTBEAT_PRO_ONLY: bool = false;

/// Whether this build/licence may run the Heartbeat at all.
pub fn heartbeat_available() -> bool {
    !HEARTBEAT_PRO_ONLY || crate::entitlement::pro_active()
}

/// Available and switched on in settings.
pub fn heartbeat_active(settings: &crate::settings::Settings) -> bool {
    heartbeat_available() && settings.heartbeat_enabled
}

// ── Events ──────────────────────────────────────────────────────────────────

/// Where an event came from. New sources (email, calendar) add a variant;
/// `Other` catches variants written by a newer build so state still loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Planner,
    Fleet,
    Ci,
    #[serde(other)]
    Other,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSource::Planner => "planner",
            EventSource::Fleet => "fleet",
            EventSource::Ci => "ci",
            EventSource::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    High,
}

/// One noticed change. `kind` is a stable slug (see the `KIND_*` constants
/// in [`perception`]); `summary` is one human line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatEvent {
    pub id: String,
    pub at: DateTime<Utc>,
    pub source: EventSource,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    /// The session the event is about (a to-do's linked worker, a fleet
    /// session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The chat session that dispatched the work, when known — lets a
    /// briefing find "changes to work you dispatched".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_from: Option<String>,
    #[serde(default)]
    pub urgency: Urgency,
}

impl HeartbeatEvent {
    pub fn new(
        source: EventSource,
        kind: impl Into<String>,
        summary: impl Into<String>,
        urgency: Urgency,
        at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            at,
            source,
            kind: kind.into(),
            summary: summary.into(),
            todo_id: None,
            session_id: None,
            project_id: None,
            dispatched_from: None,
            urgency,
        }
    }
}

// ── CI input (produced by `ci`) ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiConclusion {
    Success,
    Failure,
    Cancelled,
    #[serde(other)]
    Other,
}

/// The newest completed run of one workflow on one branch of a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiRun {
    pub project_id: String,
    pub project_title: String,
    pub workflow: String,
    pub branch: String,
    pub conclusion: CiConclusion,
    pub url: String,
    pub finished_at: DateTime<Utc>,
}

// ── Shared state + persistence ──────────────────────────────────────────────

/// The live Heartbeat state, shared by the runtime and the UI. Hydrate it
/// once at startup from [`load`].
pub static HEARTBEAT: GlobalSignal<HeartbeatState> = Signal::global(HeartbeatState::default);

/// `meta` key holding the JSON-serialized [`HeartbeatState`].
pub const META_KEY: &str = "heartbeat_state";

/// Read the persisted state; missing or unreadable state starts fresh.
pub fn load() -> HeartbeatState {
    match crate::session_store::meta_get(META_KEY) {
        Some(raw) => from_json(&raw),
        None => HeartbeatState::default(),
    }
}

/// Parse persisted JSON, falling back to a fresh state (logged) on error.
pub fn from_json(raw: &str) -> HeartbeatState {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        tracing::warn!("heartbeat: unreadable persisted state, starting fresh: {e}");
        HeartbeatState::default()
    })
}

/// Generation of the newest save requested / written. Background writes
/// can finish out of order; an older generation never overwrites a newer.
static SAVE_REQUESTED: AtomicU64 = AtomicU64::new(0);
static SAVE_WRITTEN: Mutex<u64> = Mutex::new(0);

/// Serialize on the calling thread, write on a blocking task (P-009 spirit:
/// only the bytes cross the await). Needs a Tokio runtime.
pub fn save_async(state: &HeartbeatState) {
    let json = match serde_json::to_string(state) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("heartbeat: serialize failed: {e}");
            return;
        }
    };
    let generation = SAVE_REQUESTED.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::task::spawn_blocking(move || write_generation(generation, &json));
}

/// Save [`HEARTBEAT`]: the read guard is released before anything is
/// spawned.
pub fn save_signal() {
    let state = HEARTBEAT.read();
    save_async(&state);
}

fn write_generation(generation: u64, json: &str) {
    let mut written = SAVE_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if generation < *written {
        return;
    }
    if let Err(e) = crate::session_store::meta_set(META_KEY, json) {
        tracing::warn!("heartbeat: save failed: {e}");
        return;
    }
    *written = generation;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gating() {
        let mut s = crate::settings::Settings::default();
        assert!(!s.heartbeat_enabled);
        assert!(heartbeat_available(), "not Pro-gated in Phase 2");
        assert!(!heartbeat_active(&s));
        s.heartbeat_enabled = true;
        assert!(heartbeat_active(&s));
    }

    #[test]
    fn settings_defaults_and_serde() {
        let s = crate::settings::Settings::default();
        assert_eq!(s.heartbeat_wakes_per_hour, 2);
        assert_eq!(s.heartbeat_daily_usd, 5.0);
        assert!(s.heartbeat_source_planner && s.heartbeat_source_fleet && s.heartbeat_source_ci);
        assert!(s.heartbeat_muted_kinds.is_empty());
        // Settings written before the Heartbeat existed load with defaults.
        let mut v = serde_json::to_value(&s).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.retain(|k, _| !k.starts_with("heartbeat_"));
        let back: crate::settings::Settings = serde_json::from_value(v).unwrap();
        assert_eq!(back.heartbeat_wakes_per_hour, 2);
        assert_eq!(back.heartbeat_daily_usd, 5.0);
        assert!(back.heartbeat_source_ci);
        assert!(!back.heartbeat_enabled);
    }

    #[test]
    fn event_serde_is_stable() {
        let mut e = HeartbeatEvent::new(EventSource::Ci, "ci_failed", "CI failed", Urgency::High, Utc::now());
        e.project_id = Some("p".into());
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["source"], "ci");
        assert_eq!(v["urgency"], "high");
        assert!(v.get("todo_id").is_none(), "empty links are omitted");
        let back: HeartbeatEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
        let c: CiConclusion = serde_json::from_str("\"skipped\"").unwrap();
        assert_eq!(c, CiConclusion::Other);
    }

    #[test]
    fn corrupt_state_starts_fresh() {
        assert_eq!(from_json("not json"), HeartbeatState::default());
    }

    #[test]
    fn budget_from_settings() {
        let b = Budget::from_settings(&crate::settings::Settings::default());
        assert_eq!(b, Budget { wakes_per_hour: 2, daily_usd: 5.0 });
    }
}
