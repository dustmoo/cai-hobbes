//! AI-settable timers/reminders.
//!
//! The AI sets a timer via the `HOBBES_SET_TIMER` built-in tool; the timer is
//! stored on its [`Session`](crate::session::Session) and persisted. A poll-based
//! scheduler (see `main.rs`) fires due timers:
//! - `Notify` → focus the window + show a toast.
//! - `Prompt` → focus, switch to the timer's session, and run the stored prompt
//!   as a new turn (queued if that session is mid-stream).
//!
//! This module owns the persisted data model and the pure logic (duration
//! parsing, due detection); the firing/UI lives in the app layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

impl ScheduledTimer {
    /// A pending timer whose fire time has arrived.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.status == TimerStatus::Pending && self.fire_at <= now
    }

    /// One-line summary for tool responses and the timers list.
    pub fn summary(&self) -> String {
        let when = self.fire_at.format("%Y-%m-%d %H:%M:%S UTC");
        let label = self.label.as_deref().unwrap_or("(no label)");
        let mode = match self.mode {
            TimerMode::Notify => "notify",
            TimerMode::Prompt => "prompt",
        };
        format!("[{}] {} — fires {} ({})", &self.id, label, when, mode)
    }
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
    }
}
