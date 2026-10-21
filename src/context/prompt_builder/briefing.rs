//! Turn briefing: what changed since the model last ran in this chat.
//!
//! The model only exists while it generates; between turns the harness keeps
//! the thread. At a user-turn boundary that follows a gap (or a timer firing),
//! the request tail opens with a short briefing: how long since the previous
//! turn, which of this chat's timers fired meanwhile (and why the model set
//! them), and the model's own handoff note with its age.
//!
//! The briefing is computed once, when the boundary tail is created, from the
//! session and the tail's own clock, and is persisted inside that tail's body
//! (`ledger.rs`). Retries and later requests replay those bytes; nothing here
//! is recomputed against a newer clock, and nothing reaches the system string.

use chrono::{DateTime, Local, Utc};

use crate::components::chat::Message;
use crate::session::Session;
use crate::timers::TimerStatus;

/// First line of a briefing inside a tail body.
pub(crate) const BRIEFING_HEADER: &str = "[Turn briefing: since your last turn in this chat.]";

/// A gap shorter than this is a live conversation: no elapsed line, and the
/// handoff note isn't repeated.
const MIN_GAP_MINUTES: i64 = 5;
/// A handoff note older than this is flagged as possibly out of date.
const STALE_NOTE_HOURS: i64 = 24;
/// At most this many fired timers are listed (most recent last).
const MAX_TIMERS: usize = 5;
/// Cap on a timer's label / cause / reason as shown in the briefing.
const MAX_TIMER_FIELD_CHARS: usize = 300;

/// When the model last acted in this chat: the latest timestamp among the
/// non-user messages before the current turn (replies and tool calls).
fn previous_turn_at(prior: &[Message]) -> Option<DateTime<Utc>> {
    prior
        .iter()
        .filter(|m| m.author != "User")
        .map(|m| m.created_at)
        .max()
}

/// "47 min", "3 h 5 min", "2 days".
pub(crate) fn human_duration(d: chrono::Duration) -> String {
    let mins = d.num_minutes().max(0);
    if mins < 60 {
        format!("{mins} min")
    } else if mins < 48 * 60 {
        match mins % 60 {
            0 => format!("{} h", mins / 60),
            m => format!("{} h {m} min", mins / 60),
        }
    } else {
        format!("{} days", mins / (24 * 60))
    }
}

/// Clock time of `at` in `now`'s zone: "15:55" on the same day, else
/// "Mon Oct 19 15:55".
fn clock_label(at: DateTime<Utc>, now: DateTime<Local>) -> String {
    let local = at.with_timezone(&now.timezone());
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %b %-d %H:%M").to_string()
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// The briefing for a turn whose user message follows `prior` (the session's
/// messages before it), at `now` (the tail's clock). Empty when there is
/// nothing to say: no previous turn, or a short gap with no fired timer.
pub(crate) fn turn_briefing(session: &Session, prior: &[Message], now: DateTime<Local>) -> String {
    let Some(last) = previous_turn_at(prior) else {
        return String::new();
    };
    let now_utc = now.with_timezone(&Utc);
    let gap = now_utc - last;
    let long_gap = gap.num_minutes() >= MIN_GAP_MINUTES;

    // Timers of this chat that fired after the previous turn. A timer marked
    // fired without a `fired_at` (written before the field existed) counts
    // from its deadline.
    let mut fired: Vec<(DateTime<Utc>, &crate::timers::ScheduledTimer)> = session
        .scheduled_timers
        .iter()
        .filter(|t| t.status == TimerStatus::Fired)
        .filter_map(|t| {
            let at = t.fired_at.unwrap_or(t.fire_at);
            (at > last && at <= now_utc).then_some((at, t))
        })
        .collect();
    fired.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    let skip = fired.len().saturating_sub(MAX_TIMERS);
    let fired = &fired[skip..];

    if !long_gap && fired.is_empty() {
        return String::new();
    }

    let mut lines = vec![BRIEFING_HEADER.to_string()];
    if long_gap {
        lines.push(format!(
            "{} since your last turn here ({}).",
            human_duration(gap),
            clock_label(last, now)
        ));
    }
    for (at, timer) in fired {
        let label = clip(timer.label.as_deref().unwrap_or("(no label)"), MAX_TIMER_FIELD_CHARS);
        let mut line = format!("Timer fired at {}: \"{label}\"", clock_label(*at, now));
        if let Some(cause) = timer.fire_cause.as_deref().filter(|c| !c.trim().is_empty()) {
            line.push_str(&format!(" ({})", clip(cause, MAX_TIMER_FIELD_CHARS)));
        }
        line.push('.');
        if let Some(reason) = timer.reason.as_deref().filter(|r| !r.trim().is_empty()) {
            line.push_str(&format!(" Why you set it: {}", clip(reason, MAX_TIMER_FIELD_CHARS)));
        }
        lines.push(line);
    }
    if let Some(note) = &session.handoff {
        let age = now_utc - note.written_at;
        let stale = if age.num_hours() >= STALE_NOTE_HOURS {
            " (possibly out of date)"
        } else {
            ""
        };
        lines.push(format!(
            "Your handoff note, written {} ago{stale}:",
            human_duration(age)
        ));
        lines.push(format!("Doing: {}", note.doing));
        lines.push(format!("Next: {}", note.expect_next));
        if let Some(change) = note.would_change_mind.as_deref().filter(|c| !c.trim().is_empty()) {
            lines.push(format!("Would change plan if: {change}"));
        }
    }
    lines.join("\n")
}

/// True when a persisted tail carries a briefing. Such a tail is replayed
/// for every provider (not only Claude), so the briefing stays visible for
/// the whole turn and the history stays append-only.
pub(crate) fn carries_briefing(tail_body: &str) -> bool {
    tail_body.lines().any(|l| l == BRIEFING_HEADER)
}
