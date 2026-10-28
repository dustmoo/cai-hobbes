//! Housekeeping: what the Heartbeat fixes or tallies by itself, with no
//! wake and no LLM. Everything here is pure; the runtime applies it.
//!
//! - **Runaway focus** ([`runaway_focus`]): a person's focus timer far past
//!   its estimate, or left running well after the workday ended, is paused.
//!   The banked time is capped at the estimate and the real elapsed kept on
//!   the focus row, so "Keep real time" on the card restores it. Pausing is
//!   reversible and the truth is kept, which is why this acts first and
//!   tells after.
//! - **Fleet digest** ([`fleet_digest`]): the Fleet as one line ("5
//!   decisions · 4 active · 31 parked") plus the real decisions, in the wake
//!   brief rather than an event per session. A finished turn with nothing
//!   asked is parked, not "waiting on you".
//! - **Ready to close** ([`ready_to_close`]): agent to-dos whose run ended
//!   cleanly but that are still open — the person accepts them; the
//!   Heartbeat never closes them itself.
//!
//! Agent-state resync (re-deriving `agent_state` from the Fleet every few
//! minutes) lives in the runtime: it is a planner write.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::fleet::{Disposition, FleetState};
use crate::todo::model::{format_minutes, AgentState, Lane};
use crate::todo::PlannerState;

/// A timer is runaway at this multiple of its estimate…
pub const RUNAWAY_FACTOR: u32 = 3;
/// …but never before this many minutes.
pub const RUNAWAY_MIN_MINUTES: u32 = 60;
/// An unestimated timer is runaway after this long.
pub const RUNAWAY_UNESTIMATED_MINUTES: u32 = 180;
/// A timer started in the workday is runaway this long after it ends.
pub const AFTER_HOURS_GRACE_MINUTES: i64 = 60;
/// What an unestimated runaway timer banks.
pub const UNESTIMATED_BANK_MINUTES: u32 = 60;
/// Notes kept on the card.
pub const MAX_NOTES: usize = 50;
pub const NOTE_KEEP_DAYS: i64 = 7;
const MAX_DIGEST_LINES: usize = 8;
const TITLE_MAX_CHARS: usize = 80;

/// One thing housekeeping did, for the card and the next wake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HousekeepingNote {
    pub id: String,
    pub at: DateTime<Utc>,
    /// One line ("Paused the focus timer on …").
    pub text: String,
    /// A runaway-timer pause: the focus row whose real time can be restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_row: Option<String>,
    /// The person kept the real time.
    #[serde(default)]
    pub restored: bool,
    /// Already told to a wake.
    #[serde(default)]
    pub briefed: bool,
}

impl HousekeepingNote {
    pub fn new(at: DateTime<Utc>, text: impl Into<String>, focus_row: Option<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            at,
            text: text.into(),
            focus_row,
            restored: false,
            briefed: false,
        }
    }
}

/// A focus timer to pause.
#[derive(Debug, Clone, PartialEq)]
pub struct Runaway {
    pub todo_id: String,
    pub real_minutes: u32,
    pub bank_minutes: u32,
    /// The note's text.
    pub text: String,
}

/// The person's focus timer, if it has run away. `workday_end` is today's,
/// `None` on a day off (only the estimate rule applies then). Agent-time
/// sittings are left alone: their own budget ends them.
pub fn runaway_focus(planner: &PlannerState, workday_end: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<Runaway> {
    let todo = planner.focused()?;
    let row = planner.open_focus_session_for(&todo.id)?;
    if row.actor.is_agent() {
        return None;
    }
    let real = (now - row.started_at).num_minutes().max(0) as u32;
    let estimate = todo.estimate_minutes.filter(|m| *m > 0);
    let limit = estimate
        .map(|e| (e.saturating_mul(RUNAWAY_FACTOR)).max(RUNAWAY_MIN_MINUTES))
        .unwrap_or(RUNAWAY_UNESTIMATED_MINUTES);
    let after_hours = workday_end.is_some_and(|end| {
        row.started_at < end && now >= end + Duration::minutes(AFTER_HOURS_GRACE_MINUTES)
    }) && real >= RUNAWAY_MIN_MINUTES;
    let over_estimate = real >= limit;
    if !over_estimate && !after_hours {
        return None;
    }
    let bank = estimate.unwrap_or(UNESTIMATED_BANK_MINUTES).min(real);
    let title = clip(&todo.title);
    let why = match (over_estimate, estimate) {
        (true, Some(e)) => format!("it ran {} against a {} estimate", format_minutes(real), format_minutes(e)),
        (true, None) => format!("it ran {} with no estimate", format_minutes(real)),
        (false, _) => format!("it was still running {} after the workday ended", format_minutes(AFTER_HOURS_GRACE_MINUTES as u32)),
    };
    Some(Runaway {
        todo_id: todo.id.clone(),
        real_minutes: real,
        bank_minutes: bank,
        text: format!("Paused the focus timer on \"{title}\": {why}. Banked {}.", format_minutes(bank)),
    })
}

/// The Fleet in one line plus the real decisions, for the wake brief.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetDigest {
    pub decisions: Vec<String>,
    pub active: usize,
    pub parked: usize,
}

impl FleetDigest {
    /// "5 decisions · 4 active · 31 parked"; `None` for an empty Fleet.
    pub fn line(&self) -> Option<String> {
        let n = self.decisions.len();
        if n + self.active + self.parked == 0 {
            return None;
        }
        let s = if n == 1 { "" } else { "s" };
        Some(format!("{n} decision{s} · {} active · {} parked", self.active, self.parked))
    }
}

/// Sort the live Fleet by what each session asks of the person
/// ([`crate::fleet::Disposition`]): a finished turn with nothing asked is
/// parked, not a call for the person. Decisions are listed longest wait
/// first, each with its question. Left out: the Heartbeat's own session,
/// ended sessions, and a dispatched run whose to-do is already closed (its
/// question was overtaken).
pub fn fleet_digest(
    fleet: &FleetState,
    planner: &PlannerState,
    heartbeat_session: Option<&str>,
    now: DateTime<Utc>,
) -> FleetDigest {
    let mut digest = FleetDigest::default();
    let mut decisions: Vec<(i64, String)> = Vec::new();
    for s in fleet.sessions.values().filter(|s| Some(s.id.as_str()) != heartbeat_session) {
        let todo_closed = s
            .dispatched_todo
            .as_deref()
            .and_then(|id| planner.todo(id))
            .is_some_and(|t| t.status.is_closed());
        match s.disposition(now) {
            Disposition::Active => digest.active += 1,
            Disposition::Parked => digest.parked += 1,
            Disposition::Decision if todo_closed => digest.parked += 1,
            Disposition::Decision => {
                let since = s.attention_at.unwrap_or(s.last_event_at);
                let mins = (now - since).num_minutes().max(0);
                let question = clip(&s.question().unwrap_or_else(|| "a permission prompt".to_string()));
                decisions.push((mins, format!("{}: {question} ({})", clip(s.display_name()), format_minutes(mins as u32))));
            }
            Disposition::Ended => {}
        }
    }
    decisions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let more = decisions.len().saturating_sub(MAX_DIGEST_LINES);
    digest.decisions = decisions.into_iter().take(MAX_DIGEST_LINES).map(|(_, l)| l).collect();
    if more > 0 {
        digest.decisions.push(format!("…and {more} more"));
    }
    digest
}

/// Open agent to-dos whose run ended cleanly: done, waiting to be accepted.
pub fn ready_to_close(planner: &PlannerState) -> Vec<String> {
    let mut out: Vec<String> = planner
        .todos
        .iter()
        .filter(|t| t.lane == Lane::Agent && !t.status.is_closed() && t.agent_state == Some(AgentState::Done))
        .map(|t| format!("\"{}\"", clip(&t.title)))
        .collect();
    out.sort();
    if out.len() > MAX_DIGEST_LINES {
        let more = out.len() - MAX_DIGEST_LINES;
        out.truncate(MAX_DIGEST_LINES);
        out.push(format!("…and {more} more"));
    }
    out
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= TITLE_MAX_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(TITLE_MAX_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{FocusActor, Todo};

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, 23, h, m, 0).unwrap()
    }

    fn focused(estimate: Option<u32>, start: DateTime<Utc>, actor: FocusActor) -> PlannerState {
        let mut state = PlannerState::default();
        let mut todo = Todo::new("Zeblok node check", 0.0);
        todo.estimate_minutes = estimate;
        let id = todo.id.clone();
        state.upsert_todo(todo);
        state.start_focus(&id, start, actor);
        state
    }

    #[test]
    fn a_timer_is_runaway_past_three_times_its_estimate_with_a_floor() {
        let s = focused(Some(30), t(13, 0), FocusActor::Person);
        // 3 × 30m = 90m.
        assert!(runaway_focus(&s, None, t(14, 29)).is_none());
        let r = runaway_focus(&s, None, t(14, 30)).expect("90m of 30m");
        assert_eq!((r.real_minutes, r.bank_minutes), (90, 30));
        assert!(r.text.contains("ran 1h 30m against a 30m estimate"), "{}", r.text);

        // A 10m estimate still gets the 60m floor.
        let s = focused(Some(10), t(13, 0), FocusActor::Person);
        assert!(runaway_focus(&s, None, t(13, 59)).is_none());
        assert_eq!(runaway_focus(&s, None, t(14, 0)).unwrap().bank_minutes, 10);

        // Unestimated: three hours, banking an hour.
        let s = focused(None, t(9, 0), FocusActor::Person);
        assert!(runaway_focus(&s, None, t(11, 59)).is_none());
        assert_eq!(runaway_focus(&s, None, t(12, 0)).unwrap().bank_minutes, 60);
    }

    #[test]
    fn a_timer_left_running_after_hours_is_runaway() {
        let end = Some(t(17, 0));
        let s = focused(Some(120), t(16, 30), FocusActor::Person);
        assert!(runaway_focus(&s, end, t(17, 59)).is_none(), "inside the grace");
        let r = runaway_focus(&s, end, t(18, 0)).expect("an hour past the workday");
        assert_eq!((r.real_minutes, r.bank_minutes), (90, 90), "under the estimate: banks it all");
        assert!(r.text.contains("after the workday ended"), "{}", r.text);

        // Evening focus started after hours is the person's choice.
        let s = focused(Some(120), t(19, 0), FocusActor::Person);
        assert!(runaway_focus(&s, end, t(20, 30)).is_none());
        // A day off has no workday end.
        let s = focused(Some(120), t(16, 30), FocusActor::Person);
        assert!(runaway_focus(&s, None, t(18, 0)).is_none());
    }

    #[test]
    fn agent_sittings_are_left_alone() {
        let s = focused(Some(10), t(9, 0), FocusActor::Agent { session_id: None });
        assert!(runaway_focus(&s, None, t(17, 0)).is_none());
    }

    fn row(id: &str, status: serde_json::Value, attention_at: &str) -> crate::fleet::FleetSession {
        serde_json::from_value(serde_json::json!({
            "id": id, "cwd": "/x", "name": id, "status": status,
            "last_event_at": attention_at, "attention_at": attention_at
        }))
        .unwrap()
    }

    #[test]
    fn the_digest_counts_parked_apart_from_real_decisions() {
        let idle_prompt = serde_json::json!({"NeedsAttention": {"Notification": {"kind": "idle_prompt", "message": "Claude is waiting for your input"}}});
        let mut fleet = FleetState::default();
        for (id, status, at) in [
            ("app-pack-wrapped", idle_prompt.clone(), "2026-10-23T09:00:00Z"),
            ("ended-long-ago", idle_prompt.clone(), "2026-10-23T08:00:00Z"),
            ("app-pack-hotfix", idle_prompt.clone(), "2026-10-23T14:00:00Z"),
            ("tribes", idle_prompt.clone(), "2026-10-23T18:00:00Z"),
            ("overtaken", idle_prompt.clone(), "2026-10-23T18:00:00Z"),
            ("working", serde_json::json!("Working"), "2026-10-23T20:00:00Z"),
            ("hb", idle_prompt.clone(), "2026-10-23T08:00:00Z"),
        ] {
            fleet.sessions.insert(id.into(), row(id, status, at));
        }
        let ask = |s: &mut crate::fleet::FleetSession, q: &str| {
            s.brief = Some(crate::fleet::SessionBrief {
                headline: "h".into(),
                bullets: vec![],
                blocked_on: Some(q.into()),
                generated_at: t(18, 0),
                final_brief: false,
            });
        };
        ask(fleet.sessions.get_mut("app-pack-hotfix").unwrap(), "go-ahead on the eager-mode hotfix");
        ask(fleet.sessions.get_mut("tribes").unwrap(), "say push for Fediverse slice 0");
        ask(fleet.sessions.get_mut("overtaken").unwrap(), "upgrade PHP?");
        // The overtaken run's to-do is already done.
        let mut planner = PlannerState::default();
        let mut todo = Todo::new("PHP 8.2.35", 0.0);
        todo.mark_completed(t(19, 0));
        fleet.sessions.get_mut("overtaken").unwrap().dispatched_todo = Some(todo.id.clone());
        planner.upsert_todo(todo);

        let d = fleet_digest(&fleet, &planner, Some("hb"), t(20, 30));
        assert_eq!(d.line().as_deref(), Some("2 decisions · 1 active · 2 parked"));
        assert_eq!(
            d.decisions,
            vec![
                "app-pack-hotfix: go-ahead on the eager-mode hotfix (6h 30m)",
                "tribes: say push for Fediverse slice 0 (2h 30m)",
            ]
        );
        assert_eq!(FleetDigest::default().line(), None);
    }

    #[test]
    fn ready_to_close_lists_open_done_agent_todos_only() {
        let mut state = PlannerState::default();
        for (title, lane, st, close) in [
            ("SMTP DOTENV", Lane::Agent, Some(AgentState::Done), false),
            ("Inbox FYI batch", Lane::Agent, Some(AgentState::Done), false),
            ("Closed already", Lane::Agent, Some(AgentState::Done), true),
            ("Still working", Lane::Agent, Some(AgentState::Working), false),
            ("Mine", Lane::Human, None, false),
        ] {
            let mut todo = Todo::new(title, 0.0);
            todo.set_lane(lane);
            todo.agent_state = st;
            if close {
                todo.mark_completed(Utc::now());
            }
            state.upsert_todo(todo);
        }
        assert_eq!(ready_to_close(&state), vec!["\"Inbox FYI batch\"", "\"SMTP DOTENV\""]);
    }
}
