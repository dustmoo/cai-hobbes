//! Wakes: when to wake the Heartbeat session, the message a wake delivers,
//! and how a finished wake's cost and outcome are read back. All pure; the
//! runtime (`runtime.rs`) feeds them live state.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Local, Utc};

use super::budget::{check_wake, Budget};
use super::perception::*;
use super::salience::Choice;
use super::state::{HeartbeatState, WakeRecord};
use super::HeartbeatEvent;
use crate::components::chat::Message;
use crate::components::shared::MessageContent;

/// With events queued and no wake for this long, wake for a check-in.
pub const CHECK_IN_MINUTES: i64 = 60;
/// Event lines per wake-message section; the rest are counted.
const MAX_EVENT_LINES: usize = 12;
/// A wake's outcome line is clipped to this.
const OUTCOME_MAX_CHARS: usize = 160;
/// A fired wake whose message never reached the session (the app quit
/// before the queue drained) is closed after this long.
const DELIVERY_TIMEOUT_MINUTES: i64 = 30;
/// A wake left paused on an approval is closed with what it has after this.
const APPROVAL_WAIT_HOURS: i64 = 24;

/// First line of every wake message.
pub const WAKE_HEADER: &str = "[Heartbeat wake]";

// ── When to wake ────────────────────────────────────────────────────────────

/// Why a wake is due.
#[derive(Debug, Clone, PartialEq)]
pub enum WakeTrigger {
    /// Queued events whose salience decision was WakeNow, oldest first.
    Urgent(Vec<String>),
    /// Events are queued and the Heartbeat hasn't woken for an hour.
    CheckIn,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WakeVerdict {
    /// Nothing calls for a wake.
    Idle,
    /// A wake is due but the session is mid-turn, has a queued message, or
    /// waits on an approval: try again next poll.
    Busy,
    /// Over budget: record a deferred wake; the events stay queued.
    Defer { trigger: WakeTrigger, reason: String },
    Fire(WakeTrigger),
}

/// Queued events a primary decision routed to WakeNow. Read from the state
/// (not one poll's ingest outcome) so a wake held back by the budget or a
/// busy session is still due on the next poll.
pub fn urgent_queued(state: &HeartbeatState) -> Vec<String> {
    state
        .queue
        .iter()
        .filter(|id| state.decision_for(id).is_some_and(|d| d.choice == Choice::WakeNow))
        .cloned()
        .collect()
}

/// When the Heartbeat last actually woke.
pub fn last_fired_at(trail: &[WakeRecord]) -> Option<DateTime<Utc>> {
    trail.iter().filter(|w| !w.deferred).map(|w| w.at).max()
}

/// The wake decision. Urgent events wake at once; otherwise a non-empty
/// queue wakes for a check-in once the last wake (or, before the first
/// wake, the oldest queued event) is [`CHECK_IN_MINUTES`] old. A busy
/// session holds any wake; the budget then defers it.
pub fn decide_wake(
    state: &HeartbeatState,
    budget: &Budget,
    spent_today_usd: f64,
    now: DateTime<Utc>,
    session_busy: bool,
) -> WakeVerdict {
    let urgent = urgent_queued(state);
    let trigger = if !urgent.is_empty() {
        WakeTrigger::Urgent(urgent)
    } else {
        let since = last_fired_at(&state.trail)
            .or_else(|| state.queued().iter().map(|e| e.at).min());
        match since {
            Some(at) if !state.queue.is_empty() && now - at >= Duration::minutes(CHECK_IN_MINUTES) => {
                WakeTrigger::CheckIn
            }
            _ => return WakeVerdict::Idle,
        }
    };
    if session_busy {
        return WakeVerdict::Busy;
    }
    match check_wake(&state.trail, budget, spent_today_usd, now) {
        Ok(()) => WakeVerdict::Fire(trigger),
        Err(reason) => WakeVerdict::Defer { trigger, reason },
    }
}

/// The wake record's trigger: the newest urgent event ("worker_stuck: …"),
/// or "scheduled check-in".
pub fn trigger_label(state: &HeartbeatState, trigger: &WakeTrigger) -> String {
    match trigger {
        WakeTrigger::CheckIn => format!("scheduled check-in ({} queued)", state.queue.len()),
        WakeTrigger::Urgent(ids) => {
            let newest = ids.iter().filter_map(|id| state.event(id)).max_by_key(|e| e.at);
            let mut label = match newest {
                Some(e) => format!("{}: {}", e.kind, e.summary),
                None => "urgent event".to_string(),
            };
            if ids.len() > 1 {
                label.push_str(&format!(" (+{} more)", ids.len() - 1));
            }
            label
        }
    }
}

/// The salience row that made an event wake (see `RulesSalience`).
pub fn rule_label(trigger: &WakeTrigger, state: &HeartbeatState) -> String {
    let kind = match trigger {
        WakeTrigger::CheckIn => {
            return format!("items are queued and you haven't woken for {CHECK_IN_MINUTES}+ minutes, so this is a check-in")
        }
        WakeTrigger::Urgent(ids) => ids
            .iter()
            .filter_map(|id| state.event(id))
            .max_by_key(|e| e.at)
            .map(|e| e.kind.clone())
            .unwrap_or_default(),
    };
    match kind.as_str() {
        KIND_WORKER_NEEDS_YOU | KIND_WORKER_STUCK | KIND_DECISION_RAISED => {
            "an agent needs a person, is stuck, or raised a decision → wake now".to_string()
        }
        KIND_CI_FAILED => "CI failed on a project with open agent work → wake now".to_string(),
        KIND_WORKER_DONE | KIND_RUN_ENDED | KIND_SESSION_NEEDS_ATTENTION => {
            "work you were waiting on or dispatched changed → wake now".to_string()
        }
        _ => "routed to wake now".to_string(),
    }
}

/// "2 wake_now, 3 queue" over the primary decisions of `event_ids`.
pub fn choice_summary(state: &HeartbeatState, event_ids: &[String]) -> String {
    let mut counts: Vec<(Choice, usize)> = Vec::new();
    for id in event_ids {
        if let Some(d) = state.decision_for(id) {
            match counts.iter_mut().find(|(c, _)| *c == d.choice) {
                Some((_, n)) => *n += 1,
                None => counts.push((d.choice, 1)),
            }
        }
    }
    counts.sort_by_key(|(c, _)| std::cmp::Reverse(*c as u8));
    counts
        .iter()
        .map(|(c, n)| format!("{n} {}", c.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "wake 2 of 2 this hour · $0.84 of $5.00 today", counting the wake about
/// to fire.
pub fn budget_line(trail: &[WakeRecord], budget: &Budget, spent_today_usd: f64, now: DateTime<Utc>) -> String {
    let window_start = now - Duration::minutes(60);
    let recent = trail
        .iter()
        .filter(|w| !w.deferred && w.at > window_start && w.at <= now)
        .count();
    format!(
        "wake {} of {} this hour · ${:.2} of ${:.2} today",
        recent + 1,
        budget.wakes_per_hour,
        spent_today_usd,
        budget.daily_usd
    )
}

// ── The wake message ────────────────────────────────────────────────────────

/// What the message knows about a to-do an event points at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TodoInfo {
    pub title: String,
    pub latest_progress: Option<String>,
}

/// Everything a wake message says. Built by the runtime, rendered by
/// [`build_wake_message`].
#[derive(Debug, Clone)]
pub struct WakeBrief<'a> {
    pub why: String,
    pub rule: String,
    /// The drained queue.
    pub events: &'a [HeartbeatEvent],
    /// The previous fired wake: events after it are "what changed", older
    /// ones were carried over (deferred or left from before).
    pub last_wake: Option<DateTime<Utc>>,
    pub todos: &'a HashMap<String, TodoInfo>,
    pub trust: &'a [String],
    pub budget_line: String,
    pub now: DateTime<Local>,
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn clock(at: DateTime<Utc>, now: DateTime<Local>) -> String {
    let local = at.with_timezone(&now.timezone());
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %b %-d %H:%M").to_string()
    }
}

/// Events grouped by to-do (groups and events newest first; events with no
/// to-do last, as "Other"), at most [`MAX_EVENT_LINES`] event lines.
fn event_section(events: &[&HeartbeatEvent], todos: &HashMap<String, TodoInfo>, now: DateTime<Local>) -> Vec<String> {
    let mut groups: Vec<(Option<&str>, Vec<&HeartbeatEvent>)> = Vec::new();
    for e in events {
        let key = e.todo_id.as_deref();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(e),
            None => groups.push((key, vec![e])),
        }
    }
    for (_, v) in &mut groups {
        v.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    }
    groups.sort_by(|(ka, a), (kb, b)| {
        ka.is_none()
            .cmp(&kb.is_none())
            .then_with(|| b[0].at.cmp(&a[0].at))
            .then_with(|| ka.cmp(kb))
    });

    let mut lines = Vec::new();
    let mut shown = 0;
    let total = events.len();
    for (key, evs) in groups {
        if shown >= MAX_EVENT_LINES {
            break;
        }
        let info = key.and_then(|k| todos.get(k));
        lines.push(match (key, info) {
            (Some(_), Some(i)) => format!("- To-do \"{}\":", i.title),
            (Some(k), None) => format!("- To-do {k}:"),
            (None, _) => "- Other:".to_string(),
        });
        for e in evs {
            if shown >= MAX_EVENT_LINES {
                break;
            }
            lines.push(format!("  - {} {} [{}]", clock(e.at, now), e.summary, e.source.as_str()));
            shown += 1;
        }
        if let Some(p) = info.and_then(|i| i.latest_progress.as_deref()).filter(|p| !p.trim().is_empty()) {
            lines.push(format!("  - Latest progress: {}", clip(p, 300)));
        }
    }
    if total > shown {
        lines.push(format!("- …and {} more", total - shown));
    }
    lines
}

/// The wake message: a queued user message in the Heartbeat session. No line
/// starts with "/" (the chat input would read it as a skill command).
pub fn build_wake_message(b: &WakeBrief<'_>) -> String {
    let (fresh, carried): (Vec<&HeartbeatEvent>, Vec<&HeartbeatEvent>) = b
        .events
        .iter()
        .partition(|e| b.last_wake.is_none_or(|w| e.at > w));
    let mut out = vec![WAKE_HEADER.to_string(), format!("Why: {}", b.why), format!("Rule: {}", b.rule)];

    out.push(String::new());
    out.push(if b.last_wake.is_some() {
        "What changed since your last wake:".to_string()
    } else {
        "What changed:".to_string()
    });
    if fresh.is_empty() {
        out.push("- Nothing new.".to_string());
    } else {
        out.extend(event_section(&fresh, b.todos, b.now));
    }
    if !carried.is_empty() {
        out.push(String::new());
        out.push("Still queued from before:".to_string());
        out.extend(event_section(&carried, b.todos, b.now));
    }

    out.push(String::new());
    out.push("What you may do:".to_string());
    out.extend(b.trust.iter().map(|t| format!("- {t}")));

    out.push(String::new());
    out.push(format!("Budget: {}", b.budget_line));

    out.push(String::new());
    out.push("Standing instructions:".to_string());
    out.extend(
        [
            "Act only within the rules above.",
            "Prefer dispatching a worker or raising a decision over doing outside effects yourself.",
            "Write a handoff note (HOBBES_WRITE_HANDOFF) before you finish.",
            "It's fine to decide nothing needs doing; say so in one line.",
        ]
        .iter()
        .map(|s| format!("- {s}")),
    );
    out.join("\n")
}

// ── Finished wakes ──────────────────────────────────────────────────────────

/// Whether the session holds a call waiting on the person's approval.
pub fn has_pending_approval(messages: &[Message]) -> bool {
    messages.iter().any(|m| {
        matches!(
            m.content,
            MessageContent::PermissionRequest(_) | MessageContent::SkillPermissionRequest(_)
        )
    })
}

/// A usage entry, as the attribution reads it.
#[derive(Debug, Clone, Copy)]
pub struct Spend<'a> {
    pub session_id: &'a str,
    pub at: DateTime<Utc>,
    pub cost: Option<f64>,
}

/// A finished wake's (cost, outcome), or `None` while it is still running.
///
/// The wake is running while the session is busy (streaming / queued) and,
/// for up to a day, while a call waits on approval. Cost is the session's
/// spend from `wake.at` up to the next fired wake (`until`); the outcome is
/// the first line of the last reply after the wake, clipped. A wake whose
/// message never arrived is closed after [`DELIVERY_TIMEOUT_MINUTES`].
pub fn wake_completion(
    wake: &WakeRecord,
    until: Option<DateTime<Utc>>,
    session_id: &str,
    messages: &[Message],
    usage: &[Spend<'_>],
    session_busy: bool,
    now: DateTime<Utc>,
) -> Option<(f64, String)> {
    if session_busy {
        return None;
    }
    let after: Vec<&Message> = messages.iter().filter(|m| m.created_at >= wake.at).collect();
    let delivered = after.iter().any(|m| m.author == "User");
    let age = now - wake.at;
    if !delivered && age < Duration::minutes(DELIVERY_TIMEOUT_MINUTES) {
        return None;
    }
    let waiting = has_pending_approval(messages);
    if delivered && waiting && age < Duration::hours(APPROVAL_WAIT_HOURS) {
        return None;
    }
    let cost: f64 = usage
        .iter()
        .filter(|u| u.session_id == session_id && u.at >= wake.at && until.is_none_or(|t| u.at < t))
        .filter_map(|u| u.cost)
        .sum();
    let reply = after.iter().rev().find_map(|m| match &m.content {
        MessageContent::Text { content, .. } if m.author != "User" => content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| clip(l, OUTCOME_MAX_CHARS)),
        _ => None,
    });
    let outcome = match reply {
        Some(r) => r,
        None if !delivered => "not delivered (the wake message never reached the session)".to_string(),
        None if waiting => "waiting on your approval".to_string(),
        None => "no reply".to_string(),
    };
    Some((cost, outcome))
}

/// To-dos the Heartbeat dispatched: Fleet runs launched from its session,
/// and to-dos named by a `HOBBES_DISPATCH` call that completed in it.
pub fn heartbeat_todo_ids(fleet: &crate::fleet::FleetState, session_id: &str, messages: &[Message]) -> HashSet<String> {
    let mut out: HashSet<String> = fleet
        .sessions
        .values()
        .filter(|s| s.dispatched_from.as_deref() == Some(session_id))
        .filter_map(|s| s.dispatched_todo.clone())
        .collect();
    for m in messages {
        if let MessageContent::ToolCall(tc) = &m.content {
            if tc.tool_name == "HOBBES_DISPATCH" && tc.status == crate::components::shared::ToolCallStatus::Completed {
                let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
                if let Some(t) = args.get("todo_id").and_then(|v| v.as_str()) {
                    out.insert(t.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::salience::Decision;
    use crate::heartbeat::{EventSource, Urgency};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 15, 0, 0).unwrap()
    }

    const B: Budget = Budget { wakes_per_hour: 2, daily_usd: 5.0 };

    fn ev(kind: &str, mins_ago: i64) -> HeartbeatEvent {
        HeartbeatEvent::new(EventSource::Planner, kind, format!("{kind} happened"), Urgency::Normal, now() - Duration::minutes(mins_ago))
    }

    fn queue(state: &mut HeartbeatState, e: &HeartbeatEvent, choice: Choice) {
        let d = Decision { event_id: e.id.clone(), choice, confidence: 1.0, by: "rules".into(), shadow: false };
        state.ingest(vec![e.clone()], vec![d], now());
    }

    fn fired(mins_ago: i64) -> WakeRecord {
        WakeRecord::fired(now() - Duration::minutes(mins_ago), "t", vec![], "")
    }

    #[test]
    fn wake_now_fires_and_check_in_waits_an_hour() {
        let mut s = HeartbeatState::default();
        assert_eq!(decide_wake(&s, &B, 0.0, now(), false), WakeVerdict::Idle);

        // A queued (not urgent) event: no wake until it (or the last wake)
        // is an hour old.
        let q = ev(KIND_TODO_OVERDUE, 10);
        queue(&mut s, &q, Choice::Queue);
        assert_eq!(decide_wake(&s, &B, 0.0, now(), false), WakeVerdict::Idle);
        assert_eq!(
            decide_wake(&s, &B, 0.0, now() + Duration::minutes(50), false),
            WakeVerdict::Fire(WakeTrigger::CheckIn)
        );
        s.trail.push(fired(30));
        assert_eq!(decide_wake(&s, &B, 0.0, now(), false), WakeVerdict::Idle);
        assert_eq!(
            decide_wake(&s, &B, 0.0, now() + Duration::minutes(30), false),
            WakeVerdict::Fire(WakeTrigger::CheckIn)
        );
        // Deferred attempts don't reset the check-in clock.
        s.trail.push(WakeRecord::deferred(now(), "t", vec![], "hourly cap"));
        assert_eq!(
            decide_wake(&s, &B, 0.0, now() + Duration::minutes(30), false),
            WakeVerdict::Fire(WakeTrigger::CheckIn)
        );

        // An urgent event wakes at once, whatever the clock.
        let u = ev(KIND_WORKER_STUCK, 0);
        queue(&mut s, &u, Choice::WakeNow);
        assert_eq!(decide_wake(&s, &B, 0.0, now(), false), WakeVerdict::Fire(WakeTrigger::Urgent(vec![u.id.clone()])));

        // A noted event never wakes.
        let mut quiet = HeartbeatState::default();
        queue(&mut quiet, &ev(KIND_RUN_ENDED, 120), Choice::Note);
        assert_eq!(decide_wake(&quiet, &B, 0.0, now(), false), WakeVerdict::Idle);
    }

    #[test]
    fn busy_session_holds_and_budget_defers() {
        let mut s = HeartbeatState::default();
        let u = ev(KIND_DECISION_RAISED, 0);
        queue(&mut s, &u, Choice::WakeNow);
        assert_eq!(decide_wake(&s, &B, 0.0, now(), true), WakeVerdict::Busy);

        s.trail.push(fired(10));
        s.trail.push(fired(20));
        match decide_wake(&s, &B, 0.0, now(), false) {
            WakeVerdict::Defer { trigger, reason } => {
                assert_eq!(trigger, WakeTrigger::Urgent(vec![u.id.clone()]));
                assert!(reason.contains("hourly cap"), "{reason}");
            }
            v => panic!("expected a deferral, got {v:?}"),
        }
        // Busy wins over the budget: nothing is recorded while mid-turn.
        assert_eq!(decide_wake(&s, &B, 0.0, now(), true), WakeVerdict::Busy);

        let mut s2 = HeartbeatState::default();
        queue(&mut s2, &u, Choice::WakeNow);
        match decide_wake(&s2, &B, 5.0, now(), false) {
            WakeVerdict::Defer { reason, .. } => assert!(reason.contains("daily budget"), "{reason}"),
            v => panic!("expected a deferral, got {v:?}"),
        }
        // Still queued, so it fires once the budget allows.
        assert_eq!(urgent_queued(&s2), vec![u.id.clone()]);
        assert!(matches!(decide_wake(&s2, &B, 1.0, now(), false), WakeVerdict::Fire(_)));
    }

    #[test]
    fn labels_and_budget_line() {
        let mut s = HeartbeatState::default();
        let a = ev(KIND_WORKER_STUCK, 5);
        let b = ev(KIND_DECISION_RAISED, 1);
        let c = ev(KIND_TODO_OVERDUE, 3);
        queue(&mut s, &a, Choice::WakeNow);
        queue(&mut s, &b, Choice::WakeNow);
        queue(&mut s, &c, Choice::Queue);
        let t = WakeTrigger::Urgent(urgent_queued(&s));
        assert_eq!(trigger_label(&s, &t), "decision_raised: decision_raised happened (+1 more)");
        assert!(rule_label(&t, &s).contains("raised a decision"));
        assert!(trigger_label(&s, &WakeTrigger::CheckIn).starts_with("scheduled check-in"));
        assert_eq!(choice_summary(&s, &s.queue.clone()), "2 wake_now, 1 queue");

        let trail = [fired(10), fired(90), WakeRecord::deferred(now(), "t", vec![], "x")];
        assert_eq!(budget_line(&trail, &B, 0.84, now()), "wake 2 of 2 this hour · $0.84 of $5.00 today");
    }

    fn local_now() -> DateTime<Local> {
        now().with_timezone(&Local)
    }

    #[test]
    fn wake_message_groups_by_todo_newest_first() {
        let mut stuck = ev(KIND_WORKER_STUCK, 2);
        stuck.todo_id = Some("t1".into());
        let mut earlier = ev(KIND_WORKER_DONE, 20);
        earlier.todo_id = Some("t1".into());
        let ci = ev(KIND_CI_FAILED, 5);
        let mut overdue = ev(KIND_TODO_OVERDUE, 10);
        overdue.todo_id = Some("t2".into());
        let mut old = ev(KIND_TODO_OVERDUE, 120);
        old.todo_id = Some("t3".into());
        let events = vec![old.clone(), earlier.clone(), overdue.clone(), ci.clone(), stuck.clone()];
        let todos = HashMap::from([
            ("t1".to_string(), TodoInfo { title: "Ship v0.13".into(), latest_progress: Some("tests green".into()) }),
            ("t2".to_string(), TodoInfo { title: "Docs".into(), latest_progress: None }),
        ]);
        let trust = vec!["Read freely.".to_string()];
        let brief = WakeBrief {
            why: "worker_stuck: x".into(),
            rule: "stuck → wake now".into(),
            events: &events,
            last_wake: Some(now() - Duration::minutes(60)),
            todos: &todos,
            trust: &trust,
            budget_line: "wake 1 of 2 this hour · $0.00 of $5.00 today".into(),
            now: local_now(),
        };
        let msg = build_wake_message(&brief);
        let hm = |e: &HeartbeatEvent| clock(e.at, local_now());
        let expected = format!(
            "[Heartbeat wake]\nWhy: worker_stuck: x\nRule: stuck → wake now\n\n\
             What changed since your last wake:\n\
             - To-do \"Ship v0.13\":\n\
             \x20 - {} worker_stuck happened [planner]\n\
             \x20 - {} worker_done happened [planner]\n\
             \x20 - Latest progress: tests green\n\
             - To-do \"Docs\":\n\
             \x20 - {} todo_overdue happened [planner]\n\
             - Other:\n\
             \x20 - {} ci_failed happened [planner]\n\n\
             Still queued from before:\n\
             - To-do t3:\n\
             \x20 - {} todo_overdue happened [planner]\n\n\
             What you may do:\n- Read freely.\n\n\
             Budget: wake 1 of 2 this hour · $0.00 of $5.00 today\n\n\
             Standing instructions:\n\
             - Act only within the rules above.\n\
             - Prefer dispatching a worker or raising a decision over doing outside effects yourself.\n\
             - Write a handoff note (HOBBES_WRITE_HANDOFF) before you finish.\n\
             - It's fine to decide nothing needs doing; say so in one line.",
            hm(&stuck), hm(&earlier), hm(&overdue), hm(&ci), hm(&old),
        );
        assert_eq!(msg, expected);
        assert!(msg.lines().all(|l| !l.trim_start().starts_with('/')));
    }

    #[test]
    fn wake_message_caps_events_and_handles_first_wake() {
        let events: Vec<_> = (0..20).map(|i| ev(KIND_CI_FAILED, i)).collect();
        let brief = WakeBrief {
            why: "w".into(),
            rule: "r".into(),
            events: &events,
            last_wake: None,
            todos: &HashMap::new(),
            trust: &[],
            budget_line: String::new(),
            now: local_now(),
        };
        let msg = build_wake_message(&brief);
        assert!(msg.contains("\nWhat changed:\n"), "{msg}");
        assert!(!msg.contains("Still queued"));
        assert_eq!(msg.lines().filter(|l| l.starts_with("  - ")).count(), MAX_EVENT_LINES);
        assert!(msg.contains("- …and 8 more"));

        let empty = WakeBrief { events: &[], ..brief };
        assert!(build_wake_message(&empty).contains("- Nothing new."));
    }

    fn msg_at(author: &str, content: MessageContent, at: DateTime<Utc>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: at,
            usage: None,
        }
    }

    fn text(s: &str) -> MessageContent {
        MessageContent::Text { content: s.into(), thought_signature: None, thought_summary: None, signature_provider: None }
    }

    fn tool(name: &str, args: &str, status: crate::components::shared::ToolCallStatus) -> crate::components::shared::ToolCall {
        crate::components::shared::ToolCall {
            execution_id: "x".into(),
            server_name: "hobbes-planner".into(),
            tool_name: name.into(),
            arguments: args.into(),
            status,
            response: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn completion_attributes_cost_and_outcome() {
        let wake = fired(10);
        let t = wake.at;
        let msgs = vec![
            msg_at("Hobbes", text("earlier reply"), t - Duration::minutes(5)),
            msg_at("User", text("[Heartbeat wake]"), t),
            msg_at("Hobbes", text("\n  Dispatched the fix to a worker.\nMore detail."), t + Duration::minutes(2)),
        ];
        let spend = [
            Spend { session_id: "hb", at: t - Duration::minutes(1), cost: Some(9.0) },
            Spend { session_id: "hb", at: t + Duration::minutes(1), cost: Some(0.25) },
            Spend { session_id: "hb", at: t + Duration::minutes(2), cost: Some(0.5) },
            Spend { session_id: "hb", at: t + Duration::minutes(3), cost: None },
            Spend { session_id: "other", at: t + Duration::minutes(1), cost: Some(3.0) },
            Spend { session_id: "hb", at: t + Duration::minutes(8), cost: Some(1.0) },
        ];
        // Still streaming: not finished.
        assert_eq!(wake_completion(&wake, None, "hb", &msgs, &spend, true, now()), None);
        let (cost, outcome) = wake_completion(&wake, None, "hb", &msgs, &spend, false, now()).unwrap();
        assert!((cost - 1.75).abs() < 1e-9, "{cost}");
        assert_eq!(outcome, "Dispatched the fix to a worker.");
        // Spend after the next wake belongs to that wake.
        let (cost, _) =
            wake_completion(&wake, Some(t + Duration::minutes(5)), "hb", &msgs, &spend, false, now()).unwrap();
        assert!((cost - 0.75).abs() < 1e-9, "{cost}");

        // A long reply is clipped.
        let long = vec![msg_at("User", text("w"), t), msg_at("Hobbes", text(&"x".repeat(400)), t)];
        let (_, outcome) = wake_completion(&wake, None, "hb", &long, &[], false, now()).unwrap();
        assert_eq!(outcome.chars().count(), OUTCOME_MAX_CHARS);
        assert!(outcome.ends_with('…'));
    }

    #[test]
    fn completion_waits_for_delivery_and_approvals() {
        let wake = fired(10);
        let t = wake.at;
        // Not delivered yet: wait, then close as undelivered.
        assert_eq!(wake_completion(&wake, None, "hb", &[], &[], false, now()), None);
        let (cost, outcome) =
            wake_completion(&wake, None, "hb", &[], &[], false, now() + Duration::minutes(30)).unwrap();
        assert_eq!(cost, 0.0);
        assert!(outcome.starts_with("not delivered"));

        // Paused on an approval: open for a day, then closed.
        let held = tool("gmail_send_email", "{}", crate::components::shared::ToolCallStatus::Running);
        let msgs = vec![
            msg_at("User", text("w"), t),
            msg_at("Hobbes", MessageContent::PermissionRequest(held), t + Duration::minutes(1)),
        ];
        assert!(has_pending_approval(&msgs));
        assert_eq!(wake_completion(&wake, None, "hb", &msgs, &[], false, now()), None);
        let (_, outcome) = wake_completion(&wake, None, "hb", &msgs, &[], false, now() + Duration::hours(24)).unwrap();
        assert_eq!(outcome, "waiting on your approval");
    }

    #[test]
    fn todos_the_heartbeat_dispatched() {
        let mut fleet = crate::fleet::FleetState::default();
        let row = |id: &str, todo: &str, from: &str| -> crate::fleet::FleetSession {
            serde_json::from_str(&format!(
                r#"{{"id":"{id}","cwd":"/a","name":"{id}","status":"Idle",
                    "last_event_at":"2026-10-20T10:00:00Z","working_since":null,
                    "dispatched_todo":"{todo}","dispatched_from":"{from}"}}"#
            ))
            .unwrap()
        };
        let run = row("run-1", "t-fleet", "hb");
        let other = row("run-2", "t-other", "chat");
        fleet.sessions.insert("run-1".into(), run);
        fleet.sessions.insert("run-2".into(), other);
        use crate::components::shared::ToolCallStatus as S;
        let msgs = vec![
            msg_at("Hobbes", MessageContent::ToolCall(tool("HOBBES_DISPATCH", r#"{"todo_id":"t-chat"}"#, S::Completed)), now()),
            msg_at("Hobbes", MessageContent::ToolCall(tool("HOBBES_DISPATCH", r#"{"todo_id":"t-err"}"#, S::Error)), now()),
            msg_at("Hobbes", MessageContent::PermissionRequest(tool("HOBBES_DISPATCH", r#"{"todo_id":"t-held"}"#, S::Running)), now()),
        ];
        let ids = heartbeat_todo_ids(&fleet, "hb", &msgs);
        assert_eq!(ids, HashSet::from(["t-fleet".to_string(), "t-chat".to_string()]));
    }
}
