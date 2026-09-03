//! Reconciliation: correcting fleet state against the transcript on disk.
//!
//! The hook transport is edge-triggered and lossy — events fired while
//! Hobbes is closed (rebuilds, restarts) vanish silently, freezing cards at
//! their last-received state. The transcript file is the truth that never
//! misses: its mtime says whether the session moved, and its typed-user
//! entries say whether the user answered an alert. The pure verdict function
//! carries all the logic; the driver stats/reads files and applies
//! corrections under the state lock. Runs once after hydration and every
//! sweep pass — Hobbes-origin rows are skipped (no transcript; their
//! producer is in-process).

use chrono::{DateTime, Utc};

use super::transcript;
use super::{FleetOrigin, FleetSession, FleetShared, FleetStatus, STALENESS_MINUTES};

/// What reconciliation decided for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    /// A lost Stop: show Idle. The open span is kept (the cap bounds it, and
    /// a late real event still proves it) — same stance as the sweep.
    Idle,
    /// The user answered while Hobbes wasn't listening: the alert is stale.
    ClearAttention,
}

/// Pure verdict for one session given what the transcript shows.
pub fn reconcile_verdict(
    status: &FleetStatus,
    attention_at: Option<DateTime<Utc>>,
    transcript_mtime: Option<DateTime<Utc>>,
    latest_typed_user_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Correction> {
    match status {
        FleetStatus::Working | FleetStatus::WorkingBackground => {
            // Working with a transcript that hasn't moved in over the
            // staleness window: the Stop was lost.
            let mtime = transcript_mtime?;
            (now - mtime > chrono::Duration::minutes(STALENESS_MINUTES))
                .then_some(Correction::Idle)
        }
        FleetStatus::NeedsAttention(_) => {
            // The user typed after the alert was raised → they answered it.
            let alert = attention_at?;
            let typed = latest_typed_user_ts?;
            (typed > alert).then_some(Correction::ClearAttention)
        }
        FleetStatus::Idle => None,
    }
}

/// The most recent typed-user entry timestamp in a transcript tail.
pub fn latest_typed_user_ts(tail: &str) -> Option<DateTime<Utc>> {
    let mut latest = None;
    for raw in tail.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false) {
            continue;
        }
        if matches!(v.get("promptSource").and_then(|p| p.as_str()), Some(s) if s != "typed") {
            continue;
        }
        if v.get("toolUseResult").is_some() {
            continue;
        }
        if let Some(ts) = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| t.parse::<DateTime<Utc>>().ok())
        {
            if latest.is_none_or(|l| ts > l) {
                latest = Some(ts);
            }
        }
    }
    latest
}

fn mtime_of(path: &str) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

/// Apply the correction to a session in place (used under the state lock).
pub fn apply_correction(session: &mut FleetSession, correction: Correction) {
    match correction {
        Correction::Idle => {
            session.status = FleetStatus::Idle;
            session.attention_at = None;
        }
        Correction::ClearAttention => {
            session.status = FleetStatus::Idle;
            session.attention_at = None;
            session.pending_gate = None;
        }
    }
}

/// Reconcile every live external session against its transcript. File IO
/// happens outside the lock; corrections apply under it.
pub fn reconcile_all(shared: &FleetShared) {
    // Snapshot candidates (id, path, status, attention_at) without the lock
    // held across IO.
    let candidates: Vec<(String, String, FleetStatus, Option<DateTime<Utc>>)> = {
        let state = shared.state.lock().expect("fleet state lock poisoned");
        state
            .sessions
            .values()
            .filter(|s| s.origin == FleetOrigin::External)
            .filter(|s| !matches!(s.status, FleetStatus::Idle))
            .filter_map(|s| {
                s.transcript_path.clone().map(|p| {
                    (s.id.clone(), p, s.status.clone(), s.attention_at)
                })
            })
            .collect()
    };
    if candidates.is_empty() {
        return;
    }

    let now = Utc::now();
    let mut corrections: Vec<(String, Correction)> = Vec::new();
    for (id, path, status, attention_at) in candidates {
        let mtime = mtime_of(&path);
        // The tail read is only needed for attention verdicts.
        let typed = if matches!(status, FleetStatus::NeedsAttention(_)) {
            // Skip the read when the file clearly hasn't changed since the
            // alert (mtime older than the alert → nobody typed).
            match (mtime, attention_at) {
                (Some(m), Some(a)) if m <= a => None,
                _ => transcript::read_tail(&path, transcript::TITLE_TAIL_BYTES)
                    .ok()
                    .and_then(|tail| latest_typed_user_ts(&tail)),
            }
        } else {
            None
        };
        if let Some(c) = reconcile_verdict(&status, attention_at, mtime, typed, now) {
            corrections.push((id, c));
        }
    }
    if corrections.is_empty() {
        return;
    }

    let changed: Vec<FleetSession> = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        corrections
            .into_iter()
            .filter_map(|(id, c)| {
                state.sessions.get_mut(&id).map(|s| {
                    apply_correction(s, c);
                    s.clone()
                })
            })
            .collect()
    };
    if !changed.is_empty() {
        tracing::info!("fleet: reconciled {} stale session(s) from transcripts", changed.len());
        super::store::persist_sessions(&changed);
        shared.poke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn verdict_matrix() {
        let now = utc("2026-09-03T12:00:00Z");
        let old = utc("2026-09-03T11:00:00Z");
        let fresh = utc("2026-09-03T11:58:00Z");

        // Working + stale transcript → Idle (lost Stop).
        assert_eq!(
            reconcile_verdict(&FleetStatus::Working, None, Some(old), None, now),
            Some(Correction::Idle)
        );
        // Working + fresh transcript → leave it (heartbeats just haven't
        // arrived yet or the session is mid-write).
        assert_eq!(
            reconcile_verdict(&FleetStatus::Working, None, Some(fresh), None, now),
            None
        );
        // Working + unreadable transcript → no verdict (never guess).
        assert_eq!(reconcile_verdict(&FleetStatus::Working, None, None, None, now), None);

        // Attention + user typed AFTER the alert → clear.
        let alert = utc("2026-09-03T11:30:00Z");
        let att = FleetStatus::NeedsAttention(super::super::AttentionKind::Gate);
        assert_eq!(
            reconcile_verdict(&att, Some(alert), Some(fresh), Some(fresh), now),
            Some(Correction::ClearAttention)
        );
        // Attention + last typed BEFORE the alert → still waiting; hold.
        assert_eq!(
            reconcile_verdict(&att, Some(alert), Some(fresh), Some(old), now),
            None
        );
        // Attention without a recorded alert time → hold (pre-upgrade rows).
        assert_eq!(reconcile_verdict(&att, None, Some(fresh), Some(fresh), now), None);
        // Idle never corrects.
        assert_eq!(
            reconcile_verdict(&FleetStatus::Idle, None, Some(old), None, now),
            None
        );
    }

    #[test]
    fn typed_user_timestamps_parse_and_filter() {
        let tail = [
            serde_json::json!({"type":"user","promptSource":"typed",
                "timestamp":"2026-09-03T11:00:00Z","message":{"content":"a"}})
            .to_string(),
            // Synthetic + tool-result lines don't count as the user typing.
            serde_json::json!({"type":"user","promptSource":"system",
                "timestamp":"2026-09-03T11:50:00Z","message":{"content":"n"}})
            .to_string(),
            serde_json::json!({"type":"user","toolUseResult":{},
                "timestamp":"2026-09-03T11:55:00Z","message":{"content":"r"}})
            .to_string(),
            serde_json::json!({"type":"user","promptSource":"typed",
                "timestamp":"2026-09-03T11:40:00Z","message":{"content":"b"}})
            .to_string(),
        ]
        .join("\n");
        assert_eq!(
            latest_typed_user_ts(&tail),
            Some(utc("2026-09-03T11:40:00Z"))
        );
        assert!(latest_typed_user_ts("not json").is_none());
    }
}
