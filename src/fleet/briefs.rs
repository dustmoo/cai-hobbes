//! Re-entry brief and day-rollup engine (pure logic).
//!
//! Everything here is LLM-free and side-effect-free: debounce decisions,
//! task collection, prompt framing, and tolerant mapping of the connector's
//! fixed `summarize_conversation` JSON schema onto [`SessionBrief`]. The
//! actual LLM calls happen in the brief supervisor (`main.rs`, Dioxus side —
//! the only place with hydrated API keys) via the defaulted
//! `LlmConnector::generate_fleet_brief` / `generate_fleet_rollup` methods.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{truncate_summary, FleetSession, FleetState, SessionBrief};

/// A Stop marks the session dirty; the brief generates once the session has
/// been quiet this long (SessionEnd rows skip the wait). Coalesces the
/// per-turn Stop spam into one call per work burst.
pub const BRIEF_QUIET_SECS: i64 = 45;
/// Sessions listed in the AI's `planner_today` fleet block.
pub const CTX_MAX_FLEET: usize = 6;
/// Headline clip at generation time (display clips further).
pub const HEADLINE_MAX_CHARS: usize = 200;
const MAX_BULLETS: usize = 5;

/// One unit of brief work for the supervisor.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefTask {
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    pub transcript_path: String,
    pub previous_brief: Option<SessionBrief>,
    pub dirty_at: DateTime<Utc>,
    /// Ended row → final brief, merged into the store (not the live map).
    pub is_final: bool,
}

/// Debounce decision for one session.
pub fn brief_due(s: &FleetSession, now: DateTime<Utc>, quiet_secs: i64) -> bool {
    let Some(dirty_at) = s.brief_dirty_at else {
        return false;
    };
    if s.transcript_path.is_none() {
        return false;
    }
    // Already briefed at or after the dirty mark → clean.
    if s.brief.as_ref().is_some_and(|b| b.generated_at >= dirty_at) {
        return false;
    }
    let is_final = s.ended_at.is_some();
    is_final || (now - dirty_at).num_seconds() >= quiet_secs
}

/// Collect due work: live sessions plus today's ended rows, deduped by id
/// (live wins), ended-first so closing briefs land promptly.
pub fn collect_due(
    live: &FleetState,
    ended_rows: &[FleetSession],
    now: DateTime<Utc>,
) -> Vec<BriefTask> {
    let to_task = |s: &FleetSession| -> Option<BriefTask> {
        brief_due(s, now, BRIEF_QUIET_SECS).then(|| BriefTask {
            session_id: s.id.clone(),
            name: s.name.clone(),
            cwd: s.cwd.clone(),
            transcript_path: s.transcript_path.clone().unwrap_or_default(),
            previous_brief: s.brief.clone(),
            dirty_at: s.brief_dirty_at.unwrap_or(now),
            is_final: s.ended_at.is_some(),
        })
    };
    let mut tasks: Vec<BriefTask> = ended_rows
        .iter()
        .filter(|r| !live.sessions.contains_key(&r.id))
        .filter_map(to_task)
        .collect();
    tasks.extend(live.sessions.values().filter_map(to_task));
    tasks.sort_by_key(|t| (!t.is_final, t.dirty_at));
    tasks
}

/// `(previous_summary, recent_messages)` framing for
/// `LlmConnector::generate_fleet_brief` — the connector's summarize prompt
/// is fixed, so the brief instructions ride the recent-messages slot (the
/// `summarize_tool_result` precedent).
pub fn brief_framing(
    task: &BriefTask,
    digest: &super::transcript::TranscriptDigest,
) -> (String, String) {
    let previous = task
        .previous_brief
        .as_ref()
        .and_then(|b| serde_json::to_string(b).ok())
        .unwrap_or_default();
    let framed = format!(
        "You are writing a re-entry brief for a Claude Code coding-agent session \
         in project '{}' ({}). Below is the tail of its transcript ({} recent \
         turns). In the 'summary' field write ONE short sentence (under 120 \
         characters) stating what was accomplished or is in progress. Put \
         concrete decisions made into entities.key_decisions and anything \
         blocked or awaiting the user into entities.blockers. Do not invent \
         work that is not shown in the transcript.\n--- transcript ---\n{}",
        task.name, task.cwd, digest.turn_count, digest.text
    );
    (previous, framed)
}

/// Map the connector's summarize schema (`summary`, `current_task`,
/// `entities.{key_decisions,key_topics,blockers}`) onto a [`SessionBrief`].
/// Tolerant of shape drift: any non-null object yields a brief.
pub fn brief_from_summary_value(
    v: &serde_json::Value,
    now: DateTime<Utc>,
    is_final: bool,
) -> Option<SessionBrief> {
    if v.is_null() {
        return None;
    }
    let str_at = |ptr: &str| -> Option<String> {
        v.pointer(ptr)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let list_at = |ptr: &str| -> Vec<String> {
        v.pointer(ptr)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let headline = str_at("/summary")
        .or_else(|| str_at("/current_task"))
        .unwrap_or_else(|| truncate_summary(&v.to_string(), HEADLINE_MAX_CHARS));
    let mut bullets = list_at("/entities/key_decisions");
    bullets.extend(list_at("/entities/key_topics"));
    bullets.truncate(MAX_BULLETS);
    let blocked_on = list_at("/entities/blockers").into_iter().next();

    Some(SessionBrief {
        headline: truncate_summary(&headline, HEADLINE_MAX_CHARS),
        bullets,
        blocked_on,
        generated_at: now,
        final_brief: is_final,
    })
}

// ── Day rollup ──────────────────────────────────────────────────────────────

/// A cached end-of-day narrative (session_store `meta` table, keyed by date).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayRollup {
    pub date: NaiveDate,
    pub narrative: String,
    pub generated_at: DateTime<Utc>,
    pub session_count: usize,
    pub total_minutes: u32,
}

pub fn rollup_meta_key(date: NaiveDate) -> String {
    format!("fleet_rollup_{}", date.format("%Y-%m-%d"))
}

pub use crate::todo::model::format_minutes;

/// Rich per-session lines for the rollup prompt: name — time — headline
/// (+ blocked), minutes-desc, capped.
pub fn rollup_lines(
    rows: &[FleetSession],
    date: NaiveDate,
    now: DateTime<Utc>,
    cap: usize,
) -> Vec<String> {
    let mut with_minutes: Vec<(&FleetSession, u32)> =
        rows.iter().map(|s| (s, s.minutes_on(date, now))).collect();
    with_minutes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    with_minutes
        .into_iter()
        .take(cap)
        .map(|(s, m)| {
            let mut line = format!("{} — {}", truncate_summary(&s.name, 40), format_minutes(m));
            if let Some(b) = &s.brief {
                line.push_str(&format!(" — {}", truncate_summary(&b.headline, 140)));
                if let Some(blocked) = &b.blocked_on {
                    line.push_str(&format!("; blocked: {}", truncate_summary(blocked, 80)));
                }
            }
            line
        })
        .collect()
}

/// Headline-only lines for the AI's `planner_today` fleet block.
pub fn fleet_context_lines(
    rows: &[FleetSession],
    date: NaiveDate,
    now: DateTime<Utc>,
    cap: usize,
) -> Vec<String> {
    rollup_lines(rows, date, now, cap)
}

/// Sessions included in a `HOBBES_FLEET_STATUS` report.
pub const STATUS_MAX_SESSIONS: usize = 20;

/// The `HOBBES_FLEET_STATUS` payload: live + ended-today sessions (deduped,
/// live wins), optionally filtered by a case-insensitive name/cwd substring.
/// Pure — callers hand in the snapshot and the day rows.
pub fn status_report(
    live: &FleetState,
    ended_today: &[FleetSession],
    filter: Option<&str>,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> serde_json::Value {
    let needle = filter.map(str::to_lowercase).filter(|f| !f.is_empty());
    let matches = |s: &FleetSession| {
        needle.as_ref().is_none_or(|n| {
            s.name.to_lowercase().contains(n) || s.cwd.to_lowercase().contains(n)
        })
    };
    let mut rows: Vec<&FleetSession> = ended_today
        .iter()
        .filter(|r| !live.sessions.contains_key(&r.id))
        .chain(live.sessions.values())
        .filter(|s| matches(s))
        .collect();
    // Attention first, then live-before-ended, then most recent.
    rows.sort_by(|a, b| {
        b.status
            .needs_attention()
            .cmp(&a.status.needs_attention())
            .then(a.ended_at.is_some().cmp(&b.ended_at.is_some()))
            .then(b.last_event_at.cmp(&a.last_event_at))
    });
    let total = rows.len();
    let sessions: Vec<serde_json::Value> = rows
        .into_iter()
        .take(STATUS_MAX_SESSIONS)
        .map(|s| {
            let (status, attention): (&str, Option<String>) = match &s.status {
                super::FleetStatus::Working => ("working", None),
                super::FleetStatus::WorkingBackground => ("working_background", None),
                super::FleetStatus::Idle if s.ended_at.is_some() => ("ended", None),
                super::FleetStatus::Idle => ("idle", None),
                super::FleetStatus::NeedsAttention(kind) => (
                    "needs_attention",
                    Some(match kind {
                        super::AttentionKind::Gate => "waiting on a permission approval".into(),
                        super::AttentionKind::Notification { message, .. } => {
                            truncate_summary(message, 160)
                        }
                    }),
                ),
            };
            let mut v = serde_json::json!({
                "name": s.name,
                "origin": match s.origin {
                    super::FleetOrigin::External => "claude_code",
                    super::FleetOrigin::Hobbes => "hobbes_tab",
                },
                "status": status,
                "minutes_today": s.minutes_on(today, now),
                "last_event_minutes_ago": (now - s.last_event_at).num_minutes().max(0),
            });
            if s.origin == super::FleetOrigin::External && !s.cwd.is_empty() {
                v["cwd"] = serde_json::json!(s.cwd);
            }
            if let Some(a) = attention {
                v["attention"] = serde_json::json!(a);
            }
            if let Some(b) = &s.brief {
                let mut brief = serde_json::json!({
                    "headline": truncate_summary(&b.headline, HEADLINE_MAX_CHARS),
                });
                if !b.bullets.is_empty() {
                    brief["details"] = serde_json::json!(b.bullets);
                }
                if let Some(blocked) = &b.blocked_on {
                    brief["blocked_on"] = serde_json::json!(truncate_summary(blocked, 160));
                }
                v["brief"] = brief;
            }
            if let Some(g) = &s.pending_gate {
                v["pending_approval"] = serde_json::json!({
                    "tool": g.tool_name,
                    "input": truncate_summary(&g.input_summary, 160),
                });
            }
            v
        })
        .collect();

    let mut report = serde_json::json!({
        "date": today.format("%Y-%m-%d").to_string(),
        "session_count": total,
        "agent_minutes_today": super::live_minutes_on(live, today, now)
            .saturating_add(ended_today.iter().map(|s| s.minutes_on(today, now)).sum()),
        "sessions": sessions,
    });
    if total > STATUS_MAX_SESSIONS {
        report["note"] =
            serde_json::json!(format!("showing {STATUS_MAX_SESSIONS} of {total} sessions"));
    }
    report
}

/// The one-shot framing for `LlmConnector::generate_fleet_rollup`.
pub fn rollup_framing(date: NaiveDate, lines: &[String], total_minutes: u32) -> String {
    format!(
        "Write a brief end-of-day review for {} of the user's coding-agent \
         sessions. In the 'summary' field write 2-4 sentences: what got done, \
         what is blocked, and what to pick up tomorrow. Do not invent work not \
         listed. Sessions:\n{}\nTotal agent time: {}.",
        date.format("%A, %-d %B %Y"),
        lines.join("\n"),
        format_minutes(total_minutes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::transcript::TranscriptDigest;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn session(id: &str) -> FleetSession {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap();
        let mut state = FleetState::default();
        super::super::reduce(
            &mut state,
            &super::super::events::FleetEvent::Stop {
                session_id: id.into(),
                cwd: format!("/dev/{id}"),
                background_tasks: 0,
            },
            now,
        );
        state.sessions[id].clone()
    }

    fn brief_at(t: &str) -> SessionBrief {
        SessionBrief {
            headline: "did things".into(),
            bullets: vec![],
            blocked_on: None,
            generated_at: utc(t),
            final_brief: false,
        }
    }

    #[test]
    fn brief_due_matrix() {
        let t0 = utc("2026-09-01T10:00:00Z");
        let mut s = session("s1");
        // Not dirty → not due.
        assert!(!brief_due(&s, t0, 45));
        s.brief_dirty_at = Some(t0);
        // Dirty but no transcript path → not due.
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
        s.transcript_path = Some("/tmp/t.jsonl".into());
        // Dirty, path, quiet not elapsed → not due.
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(30), 45));
        // Quiet elapsed → due.
        assert!(brief_due(&s, t0 + chrono::Duration::seconds(46), 45));
        // Ended overrides the quiet period.
        s.ended_at = Some(t0);
        assert!(brief_due(&s, t0, 45));
        s.ended_at = None;
        // Brief generated after the dirty mark → clean.
        s.brief = Some(brief_at("2026-09-01T10:00:01Z"));
        assert!(!brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
        // Brief older than the dirty mark → due again.
        s.brief = Some(brief_at("2026-09-01T09:00:00Z"));
        assert!(brief_due(&s, t0 + chrono::Duration::seconds(60), 45));
    }

    #[test]
    fn collect_due_dedupes_live_over_ended_and_finals_first() {
        let now = utc("2026-09-01T12:00:00Z");
        let dirty = now - chrono::Duration::seconds(120);

        let mut live = FleetState::default();
        let mut a = session("a");
        a.transcript_path = Some("/t/a.jsonl".into());
        a.brief_dirty_at = Some(dirty);
        live.sessions.insert("a".into(), a.clone());

        // Stale ended copy of "a" (should be shadowed by the live row) plus a
        // genuinely ended "b".
        let mut a_ended = a.clone();
        a_ended.ended_at = Some(dirty);
        let mut b = session("b");
        b.transcript_path = Some("/t/b.jsonl".into());
        b.brief_dirty_at = Some(dirty);
        b.ended_at = Some(now - chrono::Duration::seconds(10));

        let tasks = collect_due(&live, &[a_ended, b], now);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].session_id, "b");
        assert!(tasks[0].is_final);
        assert_eq!(tasks[1].session_id, "a");
        assert!(!tasks[1].is_final);
    }

    #[test]
    fn brief_from_full_schema() {
        let v = serde_json::json!({
            "summary": "Refactored the fleet store and added brief plumbing.",
            "current_task": "briefs",
            "entities": {
                "key_decisions": ["no migration needed", "merge via seq guard"],
                "key_topics": ["fleet", "briefs", "store", "extra", "more", "overflow"],
                "blockers": ["waiting on user review"]
            }
        });
        let b = brief_from_summary_value(&v, utc("2026-09-01T10:00:00Z"), true).unwrap();
        assert_eq!(b.headline, "Refactored the fleet store and added brief plumbing.");
        assert_eq!(b.bullets.len(), MAX_BULLETS);
        assert_eq!(b.bullets[0], "no migration needed");
        assert_eq!(b.blocked_on.as_deref(), Some("waiting on user review"));
        assert!(b.final_brief);
    }

    #[test]
    fn brief_falls_back_current_task_then_stringified() {
        let v = serde_json::json!({"summary": "", "current_task": "fixing tests"});
        let b = brief_from_summary_value(&v, utc("2026-09-01T10:00:00Z"), false).unwrap();
        assert_eq!(b.headline, "fixing tests");

        let odd = serde_json::json!({"unexpected": "shape"});
        let b = brief_from_summary_value(&odd, utc("2026-09-01T10:00:00Z"), false).unwrap();
        assert!(b.headline.contains("unexpected"));
        assert!(brief_from_summary_value(&serde_json::Value::Null, utc("2026-09-01T10:00:00Z"), false).is_none());
    }

    #[test]
    fn rollup_lines_sort_clip_and_cap() {
        let day: NaiveDate = "2026-09-01".parse().unwrap();
        let now = utc("2026-09-01T20:00:00Z");
        let mut small = session("small");
        small.day_minutes.insert(day, 5);
        let mut big = session("big");
        big.day_minutes.insert(day, 90);
        big.brief = Some(SessionBrief {
            headline: "Shipped the thing".into(),
            bullets: vec![],
            blocked_on: Some("code review".into()),
            generated_at: now,
            final_brief: false,
        });
        let mut extra = session("extra");
        extra.day_minutes.insert(day, 1);

        let lines = rollup_lines(&[small.clone(), big.clone(), extra.clone()], day, now, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("big — 1h 30m — Shipped the thing; blocked: code review"));
        assert!(lines[1].starts_with("small — 5m"));
    }

    #[test]
    fn brief_framing_carries_project_digest_and_previous() {
        let mut s = session("s1");
        s.transcript_path = Some("/t/s1.jsonl".into());
        s.brief_dirty_at = Some(utc("2026-09-01T10:00:00Z"));
        s.brief = Some(brief_at("2026-09-01T09:00:00Z"));
        let task = collect_due(
            &FleetState {
                sessions: [("s1".to_string(), s)].into_iter().collect(),
            },
            &[],
            utc("2026-09-01T10:01:00Z"),
        )
        .remove(0);
        let digest = TranscriptDigest {
            text: "USER: hi\nASSISTANT: done".into(),
            turn_count: 1,
        };
        let (prev, framed) = brief_framing(&task, &digest);
        assert!(prev.contains("did things"), "previous brief rides the summary slot");
        assert!(framed.contains("'s1'"));
        assert!(framed.contains("/dev/s1"));
        assert!(framed.contains("USER: hi"));
        assert!(framed.contains("1 recent"));
    }

    #[test]
    fn rollup_meta_key_and_framing() {
        let day: NaiveDate = "2026-09-01".parse().unwrap();
        assert_eq!(rollup_meta_key(day), "fleet_rollup_2026-09-01");
        let f = rollup_framing(day, &["a — 5m".into()], 65);
        assert!(f.contains("a — 5m"));
        assert!(f.contains("1h 5m"));
        assert!(f.contains("September 2026"));
    }

}
