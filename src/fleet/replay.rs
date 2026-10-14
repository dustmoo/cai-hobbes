//! Intra-day replay: reconstruct per-session working spans from the stored
//! `fleet_events` log, for the boost ribbon (concurrency over the day) in
//! the day rollup.
//!
//! Everything here is pure over [`store::StoredEvent`] rows. The event log
//! deliberately omits `PostToolUse` heartbeats (high volume), so a span's
//! liveness is only provable at turn boundaries — the same trust rules as
//! the live reducer apply per event: proven closes (`Stop`,
//! `SubagentStop`, a foreground `Notification`/`PermissionRequest`) end a
//! span at the event; unproven closes (`SessionEnd`, a superseding
//! `UserPromptSubmit`/`SessionStart`, the end of the replay window) cap at
//! the span kind's provable window past its last event. The reconstruction
//! is therefore approximate; [`replayed_minutes_on`] against the banked
//! truth quantifies by how much (the view shows the fidelity, not just the
//! picture).

use chrono::{DateTime, NaiveDate, Utc};

use super::store::StoredEvent;
use super::{
    local_day_bounds_utc, span_seconds_on, BACKGROUND_STALENESS_MINUTES, STALENESS_MINUTES,
};

/// One reconstructed working stretch for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct DaySpan {
    pub session_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// A background phase (turn over, jobs still running) vs a driven turn.
    pub background: bool,
}

/// `background_tasks` from a stored body: the hook payload carries an array,
/// older/synthetic bodies a number; absent means zero.
fn background_tasks(data: &str) -> usize {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return 0;
    };
    match v.get("background_tasks") {
        Some(serde_json::Value::Array(a)) => a.len(),
        Some(n) => n.as_u64().unwrap_or(0) as usize,
        None => 0,
    }
}

/// A `Notification` body's kind, for the permission/idle distinction.
fn notification_kind(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| {
            v.get("kind")
                .or_else(|| v.get("notification_type"))
                .and_then(|k| k.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

struct OpenSpan {
    start: DateTime<Utc>,
    last_event: DateTime<Utc>,
    background: bool,
}

impl OpenSpan {
    fn provable_end(&self, bound: DateTime<Utc>) -> DateTime<Utc> {
        let window = if self.background {
            BACKGROUND_STALENESS_MINUTES
        } else {
            STALENESS_MINUTES
        };
        bound
            .min(self.last_event + chrono::Duration::minutes(window))
            .max(self.start)
    }
}

/// Fold one session's time-ordered events into closed spans.
struct SpanBuilder {
    session_id: String,
    open: Option<OpenSpan>,
    spans: Vec<DaySpan>,
}

impl SpanBuilder {
    fn new(session_id: &str) -> Self {
        Self { session_id: session_id.to_string(), open: None, spans: Vec::new() }
    }

    fn push(&mut self, start: DateTime<Utc>, end: DateTime<Utc>, background: bool) {
        if end > start {
            self.spans.push(DaySpan {
                session_id: self.session_id.clone(),
                start,
                end,
                background,
            });
        }
    }

    /// Close at exactly `t` — the event proves work ran until then.
    fn close_proven(&mut self, t: DateTime<Utc>) {
        if let Some(open) = self.open.take() {
            self.push(open.start, t.max(open.start), open.background);
        }
    }

    /// Close without proof — cap at the span's provable window.
    fn close_unproven(&mut self, t: DateTime<Utc>) {
        if let Some(open) = self.open.take() {
            let end = open.provable_end(t);
            self.push(open.start, end, open.background);
        }
    }

    fn open(&mut self, t: DateTime<Utc>, background: bool) {
        self.open = Some(OpenSpan { start: t, last_event: t, background });
    }

    fn feed(&mut self, ev: &StoredEvent) {
        let t = ev.created_at;
        match ev.event.as_str() {
            "UserPromptSubmit" | "SessionStart" => {
                // A superseding open is unproven (the previous phase went
                // quiet without a boundary event).
                self.close_unproven(t);
                self.open(t, false);
            }
            "Stop" => {
                self.close_proven(t);
                if background_tasks(&ev.data) > 0 {
                    self.open(t, true);
                }
            }
            "SubagentStop" => {
                let bt = background_tasks(&ev.data);
                match (&mut self.open, bt) {
                    (Some(open), 0) => {
                        if open.background {
                            // Last background job done: the phase ends here.
                            self.close_proven(t);
                        } else {
                            // Mid-turn subagent completion: liveness.
                            open.last_event = t;
                        }
                    }
                    (Some(open), _) => open.last_event = t,
                    // Background work proving itself with no span open (the
                    // Stop predates the window, or was lost): heal one open.
                    (None, n) if n > 0 => self.open(t, true),
                    (None, _) => {}
                }
            }
            "Notification" | "PermissionRequest" => {
                let permissionish = ev.event == "PermissionRequest"
                    || notification_kind(&ev.data).contains("permission");
                match &mut self.open {
                    // An idle nudge during a background phase doesn't end it
                    // (mirror of the live reducer); a permission ask does.
                    Some(open) if open.background && !permissionish => open.last_event = t,
                    Some(_) => self.close_proven(t),
                    None => {}
                }
            }
            "GateDecision" => {
                // The user answered a gate: the turn resumes.
                if let Some(open) = &mut self.open {
                    open.last_event = t;
                } else {
                    self.open(t, false);
                }
            }
            // PostToolUse isn't stored; unknown names are liveness at most.
            _ => {
                if let Some(open) = &mut self.open {
                    open.last_event = t;
                }
            }
        }
    }

    fn finish(mut self, bound: DateTime<Utc>) -> Vec<DaySpan> {
        self.close_unproven(bound);
        self.spans
    }
}

/// Reconstruct every session's spans from a day's stored events. `bound`
/// closes still-open spans — pass `now` for today, end-of-day for past days.
pub fn replay_day_spans(events: &[StoredEvent], bound: DateTime<Utc>) -> Vec<DaySpan> {
    use std::collections::HashMap;
    let mut builders: HashMap<String, SpanBuilder> = HashMap::new();
    for ev in events {
        builders
            .entry(ev.session_id.clone())
            .or_insert_with(|| SpanBuilder::new(&ev.session_id))
            .feed(ev);
    }
    let mut spans: Vec<DaySpan> = builders
        .into_values()
        .flat_map(|b| b.finish(bound))
        .collect();
    spans.sort_by_key(|s| s.start);
    spans
}

/// Average concurrency per bucket across local day `date`: each bucket holds
/// the sum of span-overlap fractions (2.0 = two sessions working the whole
/// bucket). Buckets are `bucket_minutes` wide; the last one may be short in
/// wall-clock terms on DST days — overlap math handles it.
pub fn concurrency_curve(spans: &[DaySpan], date: NaiveDate, bucket_minutes: u32) -> Vec<f32> {
    let bucket_minutes = bucket_minutes.max(1);
    let (day_start, day_end) = local_day_bounds_utc(date);
    let bucket = chrono::Duration::minutes(i64::from(bucket_minutes));
    let n = ((day_end - day_start).num_minutes().max(0) as u32).div_ceil(bucket_minutes);
    let mut curve = vec![0f32; n as usize];
    for span in spans {
        let (s, e) = (span.start.max(day_start), span.end.min(day_end));
        if e <= s {
            continue;
        }
        let first = ((s - day_start).num_minutes() / i64::from(bucket_minutes)) as usize;
        for (i, slot) in curve.iter_mut().enumerate().skip(first) {
            let b0 = day_start + bucket * (i as i32);
            if b0 >= e {
                break;
            }
            let b1 = (b0 + bucket).min(day_end);
            let overlap = (e.min(b1) - s.max(b0)).num_seconds().max(0) as f32;
            let width = (b1 - b0).num_seconds().max(1) as f32;
            *slot += overlap / width;
        }
    }
    curve
}

/// Replayed agent minutes attributable to local day `date` — compare against
/// the banked truth (`fleet_agent_minutes_on`) to quantify reconstruction
/// fidelity. Spans from the same session never overlap (the builder closes
/// before it opens), so a plain sum is exact per session.
pub fn replayed_minutes_on(spans: &[DaySpan], date: NaiveDate) -> u32 {
    spans
        .iter()
        .fold(0u32, |acc, s| {
            acc.saturating_add(span_seconds_on(s.start, s.end, date))
        })
        / 60
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn local_at(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn ev(id: &str, event: &str, at: DateTime<Utc>, data: &str) -> StoredEvent {
        StoredEvent {
            session_id: id.into(),
            event: event.into(),
            created_at: at,
            data: data.into(),
        }
    }

    #[test]
    fn prompt_to_stop_is_an_exact_span_and_background_opens_after() {
        let day = date("2026-09-04");
        let events = vec![
            ev("s1", "UserPromptSubmit", local_at(day, 9, 0), "{}"),
            ev(
                "s1",
                "Stop",
                local_at(day, 9, 30),
                r#"{"background_tasks": [{"id":"a"}]}"#,
            ),
            ev(
                "s1",
                "SubagentStop",
                local_at(day, 11, 30),
                r#"{"background_tasks": []}"#,
            ),
        ];
        let spans = replay_day_spans(&events, local_at(day, 12, 0));
        assert_eq!(spans.len(), 2);
        assert!(!spans[0].background);
        assert_eq!((spans[0].start, spans[0].end), (local_at(day, 9, 0), local_at(day, 9, 30)));
        assert!(spans[1].background);
        assert_eq!((spans[1].start, spans[1].end), (local_at(day, 9, 30), local_at(day, 11, 30)));
        assert_eq!(replayed_minutes_on(&spans, day), 150);
    }

    #[test]
    fn unproven_closes_cap_at_the_span_kind_window() {
        let day = date("2026-09-04");
        // A turn that never stopped (lost Stop), superseded hours later.
        let events = vec![
            ev("s1", "UserPromptSubmit", local_at(day, 9, 0), "{}"),
            ev("s1", "UserPromptSubmit", local_at(day, 13, 0), "{}"),
            ev("s1", "Stop", local_at(day, 13, 5), "{}"),
        ];
        let spans = replay_day_spans(&events, local_at(day, 14, 0));
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0].end,
            local_at(day, 9, 0) + chrono::Duration::minutes(STALENESS_MINUTES),
            "foreground cap"
        );
        // A background phase left open at the bound caps at the long window.
        let events = vec![ev(
            "s1",
            "Stop",
            local_at(day, 9, 0),
            r#"{"background_tasks": [1]}"#,
        )];
        let spans = replay_day_spans(&events, local_at(day, 23, 0));
        assert_eq!(spans.len(), 1);
        assert!(spans[0].background);
        assert_eq!(
            spans[0].end,
            local_at(day, 9, 0) + chrono::Duration::minutes(BACKGROUND_STALENESS_MINUTES)
        );
    }

    #[test]
    fn idle_nudges_hold_background_phases_but_permission_asks_end_foreground() {
        let day = date("2026-09-04");
        let events = vec![
            ev("s1", "Stop", local_at(day, 9, 0), r#"{"background_tasks": [1]}"#),
            ev(
                "s1",
                "Notification",
                local_at(day, 9, 20),
                r#"{"kind": "idle_prompt"}"#,
            ),
            ev("s1", "SubagentStop", local_at(day, 10, 0), r#"{"background_tasks": []}"#),
        ];
        let spans = replay_day_spans(&events, local_at(day, 11, 0));
        assert_eq!(spans.len(), 1, "idle nudge doesn't split the background phase");
        assert_eq!(spans[0].end, local_at(day, 10, 0));

        let events = vec![
            ev("s2", "UserPromptSubmit", local_at(day, 9, 0), "{}"),
            ev(
                "s2",
                "PermissionRequest",
                local_at(day, 9, 10),
                r#"{"tool_name": "Bash"}"#,
            ),
        ];
        let spans = replay_day_spans(&events, local_at(day, 11, 0));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].end, local_at(day, 9, 10), "gate ends the driven span exactly");
    }

    #[test]
    fn concurrency_curve_counts_overlap_fractions() {
        let day = date("2026-09-04");
        let spans = vec![
            DaySpan {
                session_id: "a".into(),
                start: local_at(day, 9, 0),
                end: local_at(day, 10, 0),
                background: false,
            },
            DaySpan {
                session_id: "b".into(),
                start: local_at(day, 9, 30),
                end: local_at(day, 10, 0),
                background: true,
            },
        ];
        let curve = concurrency_curve(&spans, day, 30);
        assert_eq!(curve.len(), 48);
        let bucket_at = |h: u32, m: u32| ((h * 60 + m) / 30) as usize;
        assert_eq!(curve[bucket_at(9, 0)], 1.0);
        assert_eq!(curve[bucket_at(9, 30)], 2.0);
        assert_eq!(curve[bucket_at(10, 0)], 0.0);
        assert_eq!(curve[bucket_at(8, 30)], 0.0);
    }

    #[test]
    fn subagent_stop_with_remaining_tasks_heals_a_missing_background_span() {
        let day = date("2026-09-04");
        let events = vec![
            ev("s1", "SubagentStop", local_at(day, 9, 0), r#"{"background_tasks": [1, 2]}"#),
            ev("s1", "SubagentStop", local_at(day, 9, 40), r#"{"background_tasks": []}"#),
        ];
        let spans = replay_day_spans(&events, local_at(day, 10, 0));
        assert_eq!(spans.len(), 1);
        assert!(spans[0].background);
        assert_eq!(replayed_minutes_on(&spans, day), 40);
    }
}
