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
//!
//! A typed answer also overtakes the brief: once the user has typed after a
//! brief was generated, the brief's question is the past, whatever the
//! status says (an Idle row included). The brief is marked dirty at the
//! typed entry, so it stops showing at once and regenerates.

use chrono::{DateTime, Utc};

use super::transcript;
use super::{
    FleetOrigin, FleetSession, FleetShared, FleetStatus, BACKGROUND_STALENESS_MINUTES,
    STALENESS_MINUTES,
};

/// What reconciliation decided for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    /// A lost Stop: show Idle. The open span is kept (the cap bounds it, and
    /// a late real event still proves it) — same stance as the sweep.
    Idle,
    /// The user answered while Hobbes wasn't listening: the alert is stale,
    /// and so is the brief written before the answer.
    ClearAttention { typed_at: DateTime<Utc> },
    /// The user typed after the brief that asks something was generated:
    /// the question is answered; the brief goes dirty at `typed_at`.
    BriefOvertaken { typed_at: DateTime<Utc> },
}

/// Pure verdict for one session given what the transcript shows.
/// `brief_question_at` is the generation time of the brief when it
/// currently asks something ([`FleetSession::blocked_on`]), else `None`.
pub fn reconcile_verdict(
    status: &FleetStatus,
    attention_at: Option<DateTime<Utc>>,
    brief_question_at: Option<DateTime<Utc>>,
    transcript_mtime: Option<DateTime<Utc>>,
    latest_typed_user_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Correction> {
    let overtaken = || {
        let typed = latest_typed_user_ts?;
        (typed > brief_question_at?).then_some(Correction::BriefOvertaken { typed_at: typed })
    };
    match status {
        FleetStatus::Working | FleetStatus::WorkingBackground => {
            // Working with a transcript that hasn't moved in over the
            // staleness window: the Stop was lost. Background phases get the
            // long window — their jobs write nothing until they finish, so a
            // still transcript is their normal state, not a lost Stop.
            let window = if *status == FleetStatus::WorkingBackground {
                BACKGROUND_STALENESS_MINUTES
            } else {
                STALENESS_MINUTES
            };
            let mtime = transcript_mtime?;
            (now - mtime > chrono::Duration::minutes(window)).then_some(Correction::Idle)
        }
        FleetStatus::NeedsAttention(_) => {
            // The user typed after the alert was raised → they answered it.
            match (attention_at, latest_typed_user_ts) {
                (Some(alert), Some(typed)) if typed > alert => {
                    Some(Correction::ClearAttention { typed_at: typed })
                }
                _ => overtaken(),
            }
        }
        FleetStatus::Idle => overtaken(),
        // Hobbes tabs only (no transcript); the sweep idles an overdue wait.
        FleetStatus::Waiting => None,
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

/// Mark the brief overtaken by a typed answer at `typed_at`: dirty from
/// then (so the supervisor re-briefs, and [`FleetSession::blocked_on`]
/// hides the old one at once), and its question cleared outright.
fn overtake_brief(session: &mut FleetSession, typed_at: DateTime<Utc>) {
    session.brief_dirty_at = Some(session.brief_dirty_at.map_or(typed_at, |d| d.max(typed_at)));
    if let Some(b) = session.brief.as_mut() {
        b.blocked_on = None;
    }
}

/// Apply the correction to a session in place (used under the state lock).
pub fn apply_correction(session: &mut FleetSession, correction: Correction) {
    match correction {
        Correction::Idle => {
            session.status = FleetStatus::Idle;
            session.attention_at = None;
        }
        Correction::ClearAttention { typed_at } => {
            session.status = FleetStatus::Idle;
            session.attention_at = None;
            session.pending_gate = None;
            overtake_brief(session, typed_at);
        }
        Correction::BriefOvertaken { typed_at } => overtake_brief(session, typed_at),
    }
}

/// One session's reconciliation inputs, snapshotted under the lock.
struct Candidate {
    id: String,
    path: String,
    status: FleetStatus,
    attention_at: Option<DateTime<Utc>>,
    brief_question_at: Option<DateTime<Utc>>,
}

/// Reconcile every live external session against its transcript. File IO
/// happens outside the lock; corrections apply under it.
pub fn reconcile_all(shared: &FleetShared) {
    // Snapshot candidates without the lock held across IO. Idle rows count
    // only while their brief asks something (an answer there is what we
    // look for); otherwise Idle has nothing to correct.
    let candidates: Vec<Candidate> = {
        let state = shared.state.lock().expect("fleet state lock poisoned");
        state
            .sessions
            .values()
            .filter(|s| s.origin == FleetOrigin::External)
            .filter_map(|s| {
                let brief_question_at = s
                    .blocked_on()
                    .and(s.brief.as_ref())
                    .map(|b| b.generated_at);
                if s.status == FleetStatus::Idle && brief_question_at.is_none() {
                    return None;
                }
                s.transcript_path.clone().map(|path| Candidate {
                    id: s.id.clone(),
                    path,
                    status: s.status.clone(),
                    attention_at: s.attention_at,
                    brief_question_at,
                })
            })
            .collect()
    };
    if candidates.is_empty() {
        return;
    }

    let now = Utc::now();
    let mut corrections: Vec<(String, Correction)> = Vec::new();
    for c in candidates {
        let mtime = mtime_of(&c.path);
        // The tail read is only needed for answer verdicts (an alert or a
        // brief question), and skipped when the file hasn't changed since
        // the earliest of them (nobody typed).
        let since = match (c.attention_at, c.brief_question_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let asks = matches!(c.status, FleetStatus::NeedsAttention(_)) || since.is_some();
        let typed = if asks && !c.status.is_active() {
            match (mtime, since) {
                (Some(m), Some(s)) if m <= s => None,
                _ => transcript::read_tail(&c.path, transcript::TITLE_TAIL_BYTES)
                    .ok()
                    .and_then(|tail| latest_typed_user_ts(&tail)),
            }
        } else {
            None
        };
        if let Some(v) =
            reconcile_verdict(&c.status, c.attention_at, c.brief_question_at, mtime, typed, now)
        {
            corrections.push((c.id, v));
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
            reconcile_verdict(&FleetStatus::Working, None, None, Some(old), None, now),
            Some(Correction::Idle)
        );
        // Working + fresh transcript → leave it (heartbeats just haven't
        // arrived yet or the session is mid-write).
        assert_eq!(
            reconcile_verdict(&FleetStatus::Working, None, None, Some(fresh), None, now),
            None
        );
        // Working + unreadable transcript → no verdict (never guess).
        assert_eq!(reconcile_verdict(&FleetStatus::Working, None, None, None, None, now), None);

        // Background phase + transcript still for an hour → hold: its jobs
        // write nothing until they finish, so stillness proves nothing.
        assert_eq!(
            reconcile_verdict(&FleetStatus::WorkingBackground, None, None, Some(old), None, now),
            None
        );
        // Background phase still past even the long window → lost end.
        let ancient = now - chrono::Duration::minutes(BACKGROUND_STALENESS_MINUTES + 1);
        assert_eq!(
            reconcile_verdict(&FleetStatus::WorkingBackground, None, None, Some(ancient), None, now),
            Some(Correction::Idle)
        );

        // Attention + user typed AFTER the alert → clear.
        let alert = utc("2026-09-03T11:30:00Z");
        let att = FleetStatus::NeedsAttention(super::super::AttentionKind::Gate);
        assert_eq!(
            reconcile_verdict(&att, Some(alert), None, Some(fresh), Some(fresh), now),
            Some(Correction::ClearAttention { typed_at: fresh })
        );
        // Attention + last typed BEFORE the alert → still waiting; hold.
        assert_eq!(
            reconcile_verdict(&att, Some(alert), None, Some(fresh), Some(old), now),
            None
        );
        // Attention without a recorded alert time → hold (pre-upgrade rows).
        assert_eq!(reconcile_verdict(&att, None, None, Some(fresh), Some(fresh), now), None);
        // Idle without a brief question never corrects.
        assert_eq!(
            reconcile_verdict(&FleetStatus::Idle, None, None, Some(old), None, now),
            None
        );
    }

    #[test]
    fn a_typed_answer_overtakes_the_briefs_question() {
        let now = utc("2026-09-03T12:00:00Z");
        let briefed = utc("2026-09-03T11:30:00Z");
        let before = utc("2026-09-03T11:00:00Z");
        let after = utc("2026-09-03T11:45:00Z");
        // Idle rows included: the user answered in the terminal while the
        // hooks were lost, so the row never left Idle.
        assert_eq!(
            reconcile_verdict(&FleetStatus::Idle, None, Some(briefed), Some(after), Some(after), now),
            Some(Correction::BriefOvertaken { typed_at: after })
        );
        assert_eq!(
            reconcile_verdict(&FleetStatus::Idle, None, Some(briefed), Some(after), Some(before), now),
            None,
            "typed before the brief: still the open question"
        );
        // A parked idle prompt whose alert predates the answer clears it;
        // one raised after the answer still overtakes the brief.
        let parked = FleetStatus::NeedsAttention(super::super::AttentionKind::Notification {
            kind: "idle_prompt".into(),
            message: String::new(),
        });
        assert_eq!(
            reconcile_verdict(&parked, Some(utc("2026-09-03T11:50:00Z")), Some(briefed), Some(after), Some(after), now),
            Some(Correction::BriefOvertaken { typed_at: after })
        );

        // Applying it dirties the brief at the answer and drops the question.
        let mut s: FleetSession = serde_json::from_value(serde_json::json!({
            "id": "s", "cwd": "/x", "name": "x", "status": "Idle",
            "last_event_at": "2026-09-03T11:30:00Z",
            "brief": {"headline": "h", "blocked_on": "pick a schema", "generated_at": "2026-09-03T11:30:00Z"}
        }))
        .unwrap();
        assert!(s.asks_person());
        apply_correction(&mut s, Correction::BriefOvertaken { typed_at: after });
        assert_eq!(s.brief_dirty_at, Some(after));
        assert!(s.blocked_on().is_none() && !s.asks_person());
        assert!(s.brief.as_ref().unwrap().blocked_on.is_none());

        // Clearing an alert dirties the brief too.
        let mut g = s.clone();
        g.brief_dirty_at = None;
        g.status = FleetStatus::NeedsAttention(super::super::AttentionKind::Gate);
        apply_correction(&mut g, Correction::ClearAttention { typed_at: after });
        assert_eq!(g.status, FleetStatus::Idle);
        assert_eq!(g.brief_dirty_at, Some(after));
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
