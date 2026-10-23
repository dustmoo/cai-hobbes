//! AI-settable timers/reminders.
//!
//! The AI sets a timer via the `HOBBES_SET_TIMER` built-in tool; the timer is
//! stored on its [`Session`](crate::session::Session) and persisted. A poll-based
//! scheduler (see `main.rs`) fires due timers:
//! - `Notify` → focus the window + show a toast.
//! - `Prompt` → focus, switch to the timer's session, and run the stored prompt
//!   as a new turn (queued if that session is mid-stream).
//!
//! A timer can also be an *intention*: `until` names a condition (a dispatched
//! to-do's worker finishing) that fires it before its deadline. The delay is
//! always the deadline, so an intention never waits forever.
//!
//! This module owns the persisted data model and the pure logic (duration
//! parsing, due and wake-condition detection); the firing/UI lives in the app
//! layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::todo::model::{AgentState, Lane, Todo, TodoStatus};

/// What happens when a timer fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimerMode {
    /// Surface a notification toast and focus the window — nothing runs.
    #[default]
    Notify,
    /// Inject the stored prompt and run an AI turn in the timer's session.
    Prompt,
}

/// Lifecycle of a scheduled timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimerStatus {
    #[default]
    Pending,
    Fired,
    Cancelled,
}

/// A timer the AI scheduled, persisted on its session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduledTimer {
    pub id: String,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub fire_at: DateTime<Utc>,
    pub mode: TimerMode,
    /// Short human label shown in the UI / notification.
    #[serde(default)]
    pub label: Option<String>,
    /// The prompt to run when `mode == Prompt`.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub status: TimerStatus,
    /// Why the model set it, in its own words: shown back to it in the wake
    /// briefing when the timer fires, so it acts on its earlier reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Fire early when this condition is met (an intention: "when the worker
    /// finishes, or at `fire_at`, whichever comes first").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<WakeCondition>,
    /// When it fired (set with `status = Fired`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired_at: Option<DateTime<Utc>>,
    /// Why it fired, for the wake briefing: "deadline reached", or the
    /// condition that was met ("worker for 'Run the suite' finished: done").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_cause: Option<String>,
}

/// Something the harness can observe that fires an intention before its
/// deadline. Phase 1 conditions need no perception layer: they read planner
/// and fleet state Hobbes already holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeCondition {
    /// The to-do's dispatched worker stopped working: done, needs you or
    /// stuck, or the to-do was completed.
    WorkerDone { todo_id: String },
}

impl ScheduledTimer {
    /// A pending timer whose fire time has arrived.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.status == TimerStatus::Pending && self.fire_at <= now
    }

    /// One-line summary for tool responses and the timers list. A condition
    /// names its to-do by the title `todo_title` resolves, else by id.
    pub fn summary_with(&self, todo_title: impl Fn(&str) -> Option<String>) -> String {
        let when = self.fire_at.format("%Y-%m-%d %H:%M:%S UTC");
        let label = self.label.as_deref().unwrap_or("(no label)");
        let mode = match self.mode {
            TimerMode::Notify => "notify",
            TimerMode::Prompt => "prompt",
        };
        let mut out = format!("[{}] {} — fires {} ({})", &self.id, label, when, mode);
        if let Some(cond) = &self.until {
            out.push_str(", ");
            out.push_str(&cond.describe(&todo_title));
        }
        if let Some(reason) = &self.reason {
            out.push_str(" — reason: ");
            out.push_str(reason);
        }
        out
    }

    /// Mark it fired now, recording why (one of the `CAUSE_*` strings or a
    /// [`condition_cause`]).
    pub fn fire(&mut self, now: DateTime<Utc>, cause: impl Into<String>) {
        self.status = TimerStatus::Fired;
        self.fired_at = Some(now);
        self.fire_cause = Some(cause.into());
    }
}

impl WakeCondition {
    /// The clause appended to a timer summary, e.g. "or sooner when the
    /// worker for 'Run the suite' finishes".
    pub fn describe(&self, todo_title: impl Fn(&str) -> Option<String>) -> String {
        match self {
            WakeCondition::WorkerDone { todo_id } => match todo_title(todo_id) {
                Some(title) => format!("or sooner when the worker for '{}' finishes", title),
                None => format!("or sooner when the worker for to-do {} finishes", todo_id),
            },
        }
    }
}

/// `fire_cause` for a timer whose deadline arrived while Hobbes was running.
pub const CAUSE_DEADLINE: &str = "deadline reached";
/// `fire_cause` for a timer whose deadline passed while Hobbes was closed.
pub const CAUSE_MISSED: &str = "deadline passed while Hobbes was closed";

/// Longest `reason` kept; the model's own words, but not an essay.
pub const MAX_REASON_CHARS: usize = 500;

/// Normalise a timer reason: trimmed, empty → `None`, capped at
/// [`MAX_REASON_CHARS`] characters (ending in "…" when cut).
pub fn clean_reason(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_REASON_CHARS {
        return Some(trimmed.to_string());
    }
    let mut cut: String = trimmed.chars().take(MAX_REASON_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

/// Whether `cond` is met, given its to-do as the planner holds it now
/// (`None` = no such to-do). Returns the human cause when it is.
pub fn condition_cause(cond: &WakeCondition, todo: Option<&Todo>) -> Option<String> {
    match cond {
        WakeCondition::WorkerDone { todo_id } => {
            let Some(todo) = todo else {
                return Some(format!("to-do {} was deleted", todo_id));
            };
            if let Some(state @ (AgentState::Done | AgentState::NeedsYou | AgentState::Stuck)) =
                todo.agent_state
            {
                return Some(format!("worker for '{}' finished: {}", todo.title, state.label()));
            }
            match todo.status {
                TodoStatus::Completed => Some(format!("to-do '{}' completed", todo.title)),
                // Nobody will finish a cancelled to-do; waiting for the full
                // deadline would only delay the follow-up.
                TodoStatus::Cancelled => Some(format!("to-do '{}' was cancelled", todo.title)),
                TodoStatus::Open | TodoStatus::InProgress => None,
            }
        }
    }
}

/// Why `timer` should fire now, if it should: its condition was met, or its
/// deadline arrived. A met condition wins over a simultaneous deadline (it
/// says more). Non-pending timers never wake.
pub fn wake_cause<'a>(
    timer: &ScheduledTimer,
    todo_by_id: impl Fn(&str) -> Option<&'a Todo>,
    now: DateTime<Utc>,
) -> Option<String> {
    if timer.status != TimerStatus::Pending {
        return None;
    }
    if let Some(cond) = &timer.until {
        let todo = match cond {
            WakeCondition::WorkerDone { todo_id } => todo_by_id(todo_id),
        };
        if let Some(cause) = condition_cause(cond, todo) {
            return Some(cause);
        }
    }
    timer.is_due(now).then(|| CAUSE_DEADLINE.to_string())
}

/// Check that `until_worker_done` names a to-do whose worker is running, so
/// the intention can actually be met. `Ok` carries the to-do's title; `Err`
/// is the message for the model.
pub fn validate_worker_target(todo_id: &str, todo: Option<&Todo>) -> Result<String, String> {
    let Some(todo) = todo else {
        return Err(format!(
            "No to-do with id '{}'. until_worker_done needs a dispatched agent to-do: \
             dispatch it first (HOBBES_DISPATCH), or omit until_worker_done for a plain timer.",
            todo_id
        ));
    };
    if todo.lane != Lane::Agent || todo.linked_fleet_session.is_none() {
        return Err(format!(
            "To-do '{}' has no dispatched worker to wait on. Dispatch it first \
             (HOBBES_DISPATCH), or omit until_worker_done for a plain timer.",
            todo.title
        ));
    }
    let cond = WakeCondition::WorkerDone { todo_id: todo_id.to_string() };
    if let Some(cause) = condition_cause(&cond, Some(todo)) {
        return Err(format!(
            "Nothing to wait for: {}. Act on it now, or omit until_worker_done for a plain timer.",
            cause
        ));
    }
    Ok(todo.title.clone())
}

/// Minimum and maximum delays we'll accept (guards against `0s` and absurd waits).
pub const MIN_DELAY_SECS: i64 = 5;
pub const MAX_DELAY_SECS: i64 = 7 * 24 * 3_600; // 7 days

/// Parse a human delay into seconds.
///
/// Accepts a bare integer (seconds) or unit groups `d`/`h`/`m`/`s`, e.g.
/// `"600"`, `"10m"`, `"1h30m"`, `"45s"`. Whitespace is ignored. Returns `None`
/// for anything unparseable, zero, or negative.
pub fn parse_duration_secs(input: &str) -> Option<i64> {
    let s: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() {
        return None;
    }

    // Bare integer → seconds.
    if let Ok(n) = s.parse::<i64>() {
        return (n > 0).then_some(n);
    }

    // Unit groups: <number><unit> repeated.
    let mut total: i64 = 0;
    let mut num = String::new();
    let mut saw_unit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit_secs = match c.to_ascii_lowercase() {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        if num.is_empty() {
            return None; // unit with no preceding number, e.g. "m"
        }
        let n: i64 = num.parse().ok()?;
        total = total.checked_add(n.checked_mul(unit_secs)?)?;
        num.clear();
        saw_unit = true;
    }
    // Trailing digits with no unit (e.g. "10m20") are invalid in unit mode.
    if !num.is_empty() || !saw_unit || total <= 0 {
        return None;
    }
    Some(total)
}

/// Hover text for a tab waiting on its own timer.
pub fn waiting_title(fire_at: DateTime<Utc>) -> String {
    format!(
        "Waiting on a timer · {}",
        fire_at.with_timezone(&chrono::Local).format("%-I:%M %p")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_seconds() {
        assert_eq!(parse_duration_secs("600"), Some(600));
        assert_eq!(parse_duration_secs(" 30 "), Some(30));
    }

    #[test]
    fn parses_unit_groups() {
        assert_eq!(parse_duration_secs("10m"), Some(600));
        assert_eq!(parse_duration_secs("2h"), Some(7_200));
        assert_eq!(parse_duration_secs("45s"), Some(45));
        assert_eq!(parse_duration_secs("1h30m"), Some(5_400));
        assert_eq!(parse_duration_secs("1d"), Some(86_400));
        assert_eq!(parse_duration_secs("1h 30m"), Some(5_400)); // whitespace ignored
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(parse_duration_secs("0"), None);
        assert_eq!(parse_duration_secs("-5"), None);
        assert_eq!(parse_duration_secs("abc"), None);
        assert_eq!(parse_duration_secs("m"), None);
        assert_eq!(parse_duration_secs("10x"), None);
        assert_eq!(parse_duration_secs("10m20"), None); // trailing unitless number
    }

    fn timer(fire_at: DateTime<Utc>, status: TimerStatus) -> ScheduledTimer {
        ScheduledTimer {
            id: "t1".to_string(),
            session_id: "s1".to_string(),
            created_at: fire_at,
            fire_at,
            mode: TimerMode::Notify,
            label: None,
            prompt: None,
            status,
            reason: None,
            until: None,
            fired_at: None,
            fire_cause: None,
        }
    }

    #[test]
    fn is_due_only_when_pending_and_elapsed() {
        let now = DateTime::parse_from_rfc3339("2026-06-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let past = now - chrono::Duration::seconds(1);
        let future = now + chrono::Duration::seconds(1);

        assert!(timer(past, TimerStatus::Pending).is_due(now));
        assert!(!timer(future, TimerStatus::Pending).is_due(now)); // not yet
        assert!(!timer(past, TimerStatus::Fired).is_due(now)); // already fired
        assert!(!timer(past, TimerStatus::Cancelled).is_due(now)); // cancelled
    }

    #[test]
    fn timer_serde_roundtrip_with_defaults() {
        // Older persisted timers without the optional fields still load.
        let json = serde_json::json!({
            "id": "t9",
            "session_id": "s2",
            "created_at": "2026-06-17T12:00:00Z",
            "fire_at": "2026-06-17T12:10:00Z",
            "mode": "prompt"
        });
        let t: ScheduledTimer = serde_json::from_value(json).unwrap();
        assert_eq!(t.mode, TimerMode::Prompt);
        assert_eq!(t.status, TimerStatus::Pending);
        assert!(t.label.is_none());
        assert!(t.reason.is_none() && t.until.is_none() && t.fire_cause.is_none());
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A dispatched agent to-do with a running worker.
    fn worker_todo(state: Option<AgentState>) -> Todo {
        let mut t = Todo::new("Run the suite", 1.0);
        t.id = "tdo_1".into();
        t.lane = Lane::Agent;
        t.linked_fleet_session = Some("w1".into());
        t.agent_state = state;
        t
    }

    fn intention(fire_at: DateTime<Utc>) -> ScheduledTimer {
        let mut t = timer(fire_at, TimerStatus::Pending);
        t.until = Some(WakeCondition::WorkerDone { todo_id: "tdo_1".into() });
        t
    }

    #[test]
    fn worker_done_causes() {
        let cond = WakeCondition::WorkerDone { todo_id: "tdo_1".into() };
        let cause = |todo: &Todo| condition_cause(&cond, Some(todo));

        assert_eq!(
            cause(&worker_todo(Some(AgentState::Done))).as_deref(),
            Some("worker for 'Run the suite' finished: done")
        );
        assert_eq!(
            cause(&worker_todo(Some(AgentState::NeedsYou))).as_deref(),
            Some("worker for 'Run the suite' finished: needs you")
        );
        assert_eq!(
            cause(&worker_todo(Some(AgentState::Stuck))).as_deref(),
            Some("worker for 'Run the suite' finished: stuck")
        );
        let mut completed = worker_todo(Some(AgentState::Working));
        completed.status = TodoStatus::Completed;
        assert_eq!(cause(&completed).as_deref(), Some("to-do 'Run the suite' completed"));
        let mut cancelled = worker_todo(Some(AgentState::Working));
        cancelled.status = TodoStatus::Cancelled;
        assert_eq!(cause(&cancelled).as_deref(), Some("to-do 'Run the suite' was cancelled"));
        assert_eq!(condition_cause(&cond, None).as_deref(), Some("to-do tdo_1 was deleted"));
    }

    #[test]
    fn worker_done_not_met_while_working_or_queued() {
        let cond = WakeCondition::WorkerDone { todo_id: "tdo_1".into() };
        for state in [Some(AgentState::Working), Some(AgentState::Queued), None] {
            assert_eq!(condition_cause(&cond, Some(&worker_todo(state))), None, "{state:?}");
        }
    }

    #[test]
    fn wake_cause_prefers_condition_then_deadline() {
        let later = now() + chrono::Duration::minutes(10);
        let earlier = now() - chrono::Duration::seconds(1);
        let working = worker_todo(Some(AgentState::Working));
        let done = worker_todo(Some(AgentState::Done));

        // Condition not met, deadline ahead → nothing.
        assert_eq!(wake_cause(&intention(later), |_| Some(&working), now()), None);
        // Condition met before the deadline → fires early with its cause.
        assert_eq!(
            wake_cause(&intention(later), |_| Some(&done), now()).as_deref(),
            Some("worker for 'Run the suite' finished: done")
        );
        // Deadline passed, condition not met → deadline.
        assert_eq!(
            wake_cause(&intention(earlier), |_| Some(&working), now()).as_deref(),
            Some(CAUSE_DEADLINE)
        );
        // Missing to-do → fires as deleted.
        assert_eq!(
            wake_cause(&intention(later), |_| None, now()).as_deref(),
            Some("to-do tdo_1 was deleted")
        );
        // Plain timers only use the deadline; settled timers never wake.
        assert_eq!(wake_cause(&timer(later, TimerStatus::Pending), |_| None, now()), None);
        assert_eq!(
            wake_cause(&timer(earlier, TimerStatus::Pending), |_| None, now()).as_deref(),
            Some(CAUSE_DEADLINE)
        );
        let mut fired = intention(later);
        fired.status = TimerStatus::Fired;
        assert_eq!(wake_cause(&fired, |_| Some(&done), now()), None);
    }

    #[test]
    fn fire_records_time_and_cause() {
        let mut t = intention(now());
        t.fire(now(), CAUSE_MISSED);
        assert_eq!(t.status, TimerStatus::Fired);
        assert_eq!(t.fired_at, Some(now()));
        assert_eq!(t.fire_cause.as_deref(), Some("deadline passed while Hobbes was closed"));
    }

    #[test]
    fn summary_shows_condition_and_reason() {
        let mut t = intention(now());
        t.label = Some("Check the suite".into());
        t.reason = Some("Report whether it passed.".into());
        assert_eq!(
            t.summary_with(|_| None),
            "[t1] Check the suite — fires 2026-10-21 12:00:00 UTC (notify), \
             or sooner when the worker for to-do tdo_1 finishes — reason: Report whether it passed."
        );
        assert!(t
            .summary_with(|_| Some("Run the suite".into()))
            .contains("or sooner when the worker for 'Run the suite' finishes"));
        // A plain timer's summary is unchanged.
        assert_eq!(
            timer(now(), TimerStatus::Pending).summary_with(|_| None),
            "[t1] (no label) — fires 2026-10-21 12:00:00 UTC (notify)"
        );
    }

    #[test]
    fn clean_reason_trims_and_caps() {
        assert_eq!(clean_reason(None), None);
        assert_eq!(clean_reason(Some("   ")), None);
        assert_eq!(clean_reason(Some("  why  ")).as_deref(), Some("why"));
        let capped = clean_reason(Some(&"é".repeat(MAX_REASON_CHARS + 50))).unwrap();
        assert_eq!(capped.chars().count(), MAX_REASON_CHARS);
        assert!(capped.ends_with('…'));
        let exact = "a".repeat(MAX_REASON_CHARS);
        assert_eq!(clean_reason(Some(&exact)), Some(exact));
    }

    #[test]
    fn validate_worker_target_requires_a_running_worker() {
        assert_eq!(
            validate_worker_target("tdo_1", Some(&worker_todo(Some(AgentState::Working)))),
            Ok("Run the suite".to_string())
        );
        let missing = validate_worker_target("tdo_x", None).unwrap_err();
        assert!(missing.contains("No to-do with id 'tdo_x'") && missing.contains("omit until_worker_done"));

        let mut human = worker_todo(Some(AgentState::Working));
        human.lane = Lane::Human;
        assert!(validate_worker_target("tdo_1", Some(&human)).unwrap_err().contains("Dispatch it first"));
        let mut unlinked = worker_todo(Some(AgentState::Queued));
        unlinked.linked_fleet_session = None;
        assert!(validate_worker_target("tdo_1", Some(&unlinked)).unwrap_err().contains("no dispatched worker"));

        let finished = validate_worker_target("tdo_1", Some(&worker_todo(Some(AgentState::Done)))).unwrap_err();
        assert!(finished.contains("Nothing to wait for: worker for 'Run the suite' finished: done"), "{finished}");
    }
}
