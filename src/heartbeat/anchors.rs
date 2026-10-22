//! Day anchors and the look-ahead (Phase 2b: from reactive to proactive).
//!
//! A [`DayView`] is a small, pure picture of the person's day — workday
//! bounds, upcoming meetings, today's capacity, deadlines and the agent lane
//! — captured from the planner by the runtime. [`due_anchors`] turns it into
//! the wakes that come from the day itself rather than from events:
//!
//! | Anchor | Due from | Lapses at |
//! |---|---|---|
//! | Start of the workday | `planner_workday_start` | 2 hours later (or the workday end) |
//! | Before a meeting | 45 minutes before a firm, busy calendar event | the event's start |
//! | Over capacity | the workday start, while today's plan is over capacity | the workday end |
//! | End of the workday | 30 minutes before `planner_workday_end` | the workday end |
//!
//! Each anchor fires at most once per local day (`HeartbeatState::
//! fired_anchors`). A due anchor still obeys the budget and the busy rule;
//! held back, it retries on later polls until it lapses. Workday anchors
//! skip Saturday and Sunday; meeting anchors run every day.
//!
//! [`look_ahead_section`] renders the same view as the wake message's lead
//! section, "The next few hours".

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc, Weekday};

use super::state::HeartbeatState;
use crate::todo::model::{self, AgentState, BlockSource, Todo};
use crate::todo::PlannerState;

/// A meeting anchor is due this long before the meeting starts.
pub const MEETING_LEAD_MINUTES: i64 = 45;
/// The end-of-day anchor is due this long before the workday ends.
pub const WORKDAY_END_LEAD_MINUTES: i64 = 30;
/// The start-of-day anchor lapses this long after the workday starts.
pub const WORKDAY_START_WINDOW_MINUTES: i64 = 120;
/// How far ahead the look-ahead lists meetings.
pub const LOOK_AHEAD_HOURS: i64 = 4;
/// Meetings kept in a [`DayView`] (the next day's worth).
const CAPTURE_HOURS: i64 = 24;
const MAX_LOOK_AHEAD_MEETINGS: usize = 6;
const MAX_DEADLINES: usize = 5;
const MAX_ATTENTION: usize = 5;
const TITLE_MAX_CHARS: usize = 80;
const ATTENTION_MAX_CHARS: usize = 200;

/// A timed, busy calendar event (an external block on the planner).
#[derive(Debug, Clone, PartialEq)]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub tentative: bool,
}

/// An open to-do due today or tomorrow.
#[derive(Debug, Clone, PartialEq)]
pub struct Deadline {
    pub title: String,
    pub due: NaiveDate,
}

/// What the anchors and the look-ahead read about the day. Pure data, so
/// both are testable without a planner or a time zone.
#[derive(Debug, Clone, PartialEq)]
pub struct DayView {
    /// The local day.
    pub today: NaiveDate,
    /// Monday to Friday: workday anchors only fire on these.
    pub workday: bool,
    pub workday_start: DateTime<Utc>,
    pub workday_end: DateTime<Utc>,
    /// Busy external blocks that end after the capture and start within the
    /// next day, by start.
    pub meetings: Vec<Meeting>,
    /// `Capacity::summary()` for today ("10h 35m planned — over by 4h 35m").
    pub capacity_summary: Option<String>,
    /// Minutes today's plan is over capacity (0 when it fits).
    pub over_by_minutes: u32,
    pub deadlines: Vec<Deadline>,
    /// Open to-dos past their deadline.
    pub overdue: usize,
    /// "2 working · 1 stuck", when the agent lane has anything.
    pub agent_counts: Option<String>,
    /// Agent to-dos that need the person: needs you, stuck, to review.
    pub agent_attention: Vec<String>,
}

impl DayView {
    /// Capture from the live planner, in local time.
    pub fn capture(planner: &PlannerState, settings: &crate::settings::Settings, now: DateTime<Utc>) -> Self {
        let today = now.with_timezone(&Local).date_naive();
        let (start_min, end_min) = workday_minutes(&settings.planner_workday_start, &settings.planner_workday_end);
        let workday_start = local_instant(today, start_min).unwrap_or(now);
        let workday_end = local_instant(today, end_min).unwrap_or(now);

        let horizon = now + Duration::hours(CAPTURE_HOURS);
        let mut meetings: Vec<Meeting> = planner
            .blocks
            .iter()
            .filter(|b| b.end > now && b.start < horizon)
            .filter_map(|b| match &b.source {
                BlockSource::External { busy: true, tentative, .. } => Some(Meeting {
                    id: b.id.clone(),
                    title: planner.block_display_title(b),
                    start: b.start,
                    end: b.end,
                    tentative: *tentative,
                }),
                _ => None,
            })
            .collect();
        meetings.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));

        let capacity = planner.capacity_for(today, settings.planner_daily_capacity_minutes);
        let cap = model::measure_capacity_full(
            &planner.todos,
            &planner.blocks,
            &planner.focus_sessions,
            today,
            capacity,
            settings.planner_calendar_counts_against_capacity,
        );

        let tomorrow = today.succ_opt().unwrap_or(today);
        let mut deadlines: Vec<Deadline> = planner
            .todos
            .iter()
            .filter(|t| !t.status.is_closed())
            .filter_map(|t| {
                let due = t.deadline.filter(|d| *d == today || *d == tomorrow)?;
                Some(Deadline { title: t.title.clone(), due })
            })
            .collect();
        deadlines.sort_by(|a, b| a.due.cmp(&b.due).then_with(|| a.title.cmp(&b.title)));

        let lane = crate::todo::lanes::agent_lane(&planner.todos, today);
        let agent_counts = (!lane.is_empty()).then(|| lane.counts_line());
        let agent_attention = lane
            .needs_you
            .iter()
            .chain(lane.stuck.iter())
            .chain(lane.review.iter())
            .filter_map(|t| attention_line(t))
            .collect();

        Self {
            today,
            workday: !matches!(today.weekday(), Weekday::Sat | Weekday::Sun),
            workday_start,
            workday_end,
            meetings,
            capacity_summary: Some(cap.summary()),
            over_by_minutes: cap.over_by(),
            deadlines,
            overdue: crate::todo::views::overdue(&planner.todos, today).len(),
            agent_counts,
            agent_attention,
        }
    }
}

/// One line for an agent to-do that needs the person.
fn attention_line(t: &Todo) -> Option<String> {
    let title = clip(&t.title, TITLE_MAX_CHARS);
    let open = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let line = match t.agent_state? {
        AgentState::NeedsYou => match open(t.decision.as_ref().filter(|d| d.is_open()).map(|d| d.question.as_str())) {
            Some(q) => format!("\"{title}\" needs you: {q}"),
            None => format!("\"{title}\" needs you"),
        },
        AgentState::Stuck => match open(t.open_item.as_deref()) {
            Some(o) => format!("\"{title}\" is stuck: {o}"),
            None => format!("\"{title}\" is stuck"),
        },
        AgentState::Review => match open(t.open_item.as_deref()) {
            Some(o) => format!("\"{title}\" finished with an open item: {o}"),
            None => format!("\"{title}\" finished with an open item"),
        },
        _ => return None,
    };
    Some(clip(&line, ATTENTION_MAX_CHARS))
}

/// "HH:MM" → minutes after midnight.
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 23 && m <= 59).then_some(h * 60 + m)
}

/// Workday bounds in minutes, falling back to 09:00–17:00 when the settings
/// are malformed or inverted (as the Planner timeline does).
pub fn workday_minutes(start: &str, end: &str) -> (u32, u32) {
    match (parse_hhmm(start), parse_hhmm(end)) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => (9 * 60, 17 * 60),
    }
}

fn local_instant(day: NaiveDate, minutes: u32) -> Option<DateTime<Utc>> {
    let naive = day.and_hms_opt(minutes / 60, minutes % 60, 0)?;
    Local.from_local_datetime(&naive).earliest().map(|d| d.with_timezone(&Utc))
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

/// "14:30" in local time.
fn hm(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

// ── Anchors ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    WorkdayStart,
    Meeting,
    OverCapacity,
    WorkdayEnd,
}

/// A wake the day itself calls for.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    /// Stable within the day; [`HeartbeatState::fired_anchors`] keys on it.
    pub key: String,
    pub kind: AnchorKind,
    /// One human line ("Meeting \"Clyde sync\" at 14:30").
    pub label: String,
    pub due_at: DateTime<Utc>,
    pub lapses_at: DateTime<Utc>,
}

/// Every anchor the day has, due or not.
pub fn day_anchors(day: &DayView) -> Vec<Anchor> {
    let mut out = Vec::new();
    if day.workday {
        out.push(Anchor {
            key: "workday_start".into(),
            kind: AnchorKind::WorkdayStart,
            label: format!("Start of the workday ({})", hm(day.workday_start)),
            due_at: day.workday_start,
            lapses_at: (day.workday_start + Duration::minutes(WORKDAY_START_WINDOW_MINUTES)).min(day.workday_end),
        });
    }
    for m in day.meetings.iter().filter(|m| !m.tentative) {
        out.push(Anchor {
            key: format!("meeting:{}@{}", m.id, m.start.to_rfc3339()),
            kind: AnchorKind::Meeting,
            label: format!("Meeting \"{}\" at {}", clip(&m.title, TITLE_MAX_CHARS), hm(m.start)),
            due_at: m.start - Duration::minutes(MEETING_LEAD_MINUTES),
            lapses_at: m.start,
        });
    }
    if day.workday && day.over_by_minutes > 0 {
        out.push(Anchor {
            key: "over_capacity".into(),
            kind: AnchorKind::OverCapacity,
            label: format!(
                "Today's plan is over capacity (over by {})",
                model::format_minutes(day.over_by_minutes)
            ),
            due_at: day.workday_start,
            lapses_at: day.workday_end,
        });
    }
    if day.workday {
        out.push(Anchor {
            key: "workday_end".into(),
            kind: AnchorKind::WorkdayEnd,
            label: format!("The workday ends at {}", hm(day.workday_end)),
            due_at: day.workday_end - Duration::minutes(WORKDAY_END_LEAD_MINUTES),
            lapses_at: day.workday_end,
        });
    }
    out
}

/// The anchors due now: inside their window and not yet fired today, in
/// due order.
pub fn due_anchors(state: &HeartbeatState, day: &DayView, now: DateTime<Utc>) -> Vec<Anchor> {
    let mut out: Vec<Anchor> = day_anchors(day)
        .into_iter()
        .filter(|a| a.due_at <= now && now < a.lapses_at)
        .filter(|a| !state.anchor_fired(&a.key, day.today))
        .collect();
    out.sort_by(|a, b| a.due_at.cmp(&b.due_at).then_with(|| a.key.cmp(&b.key)));
    out
}

/// The next anchor still to fire today (due now or later), for the card's
/// "next check-in".
pub fn next_anchor(state: &HeartbeatState, day: &DayView, now: DateTime<Utc>) -> Option<Anchor> {
    day_anchors(day)
        .into_iter()
        .filter(|a| now < a.lapses_at && !state.anchor_fired(&a.key, day.today))
        .min_by_key(|a| a.due_at)
}

// ── The look-ahead ──────────────────────────────────────────────────────────

/// "The next few hours" for the wake message: meetings in the next
/// [`LOOK_AHEAD_HOURS`] (local times), today's plan against capacity,
/// deadlines today and tomorrow, and the agent lane with what needs the
/// person. `None` (the planner is off) says so in one line.
pub fn look_ahead_section(day: Option<&DayView>, now: DateTime<Local>) -> Vec<String> {
    let mut out = vec!["The next few hours:".to_string()];
    let Some(day) = day else {
        out.push("- The planner is off, so there is no calendar or plan to look at.".to_string());
        return out;
    };
    let tz = now.timezone();
    let at = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
    let now_utc = now.with_timezone(&Utc);
    let horizon = now_utc + Duration::hours(LOOK_AHEAD_HOURS);
    let soon: Vec<&Meeting> = day
        .meetings
        .iter()
        .filter(|m| m.end > now_utc && m.start < horizon)
        .collect();
    if soon.is_empty() {
        out.push(format!("- Calendar: nothing in the next {LOOK_AHEAD_HOURS} hours."));
    } else {
        out.push(format!("- Calendar (next {LOOK_AHEAD_HOURS} hours):"));
        for m in soon.iter().take(MAX_LOOK_AHEAD_MEETINGS) {
            let mut line = format!("  - {}–{} {}", at(m.start), at(m.end), clip(&m.title, TITLE_MAX_CHARS));
            if m.start <= now_utc {
                line.push_str(" (now)");
            }
            if m.tentative {
                line.push_str(" (tentative)");
            }
            out.push(line);
        }
        if soon.len() > MAX_LOOK_AHEAD_MEETINGS {
            out.push(format!("  - …and {} more", soon.len() - MAX_LOOK_AHEAD_MEETINGS));
        }
    }
    if let Some(c) = &day.capacity_summary {
        out.push(format!("- Today's plan: {c}"));
    }
    if !day.deadlines.is_empty() {
        let names = |due: NaiveDate| -> Vec<String> {
            day.deadlines
                .iter()
                .filter(|d| d.due == due)
                .take(MAX_DEADLINES)
                .map(|d| format!("\"{}\"", clip(&d.title, TITLE_MAX_CHARS)))
                .collect()
        };
        let mut parts = Vec::new();
        let today = names(day.today);
        if !today.is_empty() {
            parts.push(format!("today {}", today.join(", ")));
        }
        let tomorrow = names(day.today.succ_opt().unwrap_or(day.today));
        if !tomorrow.is_empty() {
            parts.push(format!("tomorrow {}", tomorrow.join(", ")));
        }
        out.push(format!("- Deadlines: {}", parts.join("; ")));
    }
    if day.overdue > 0 {
        let s = if day.overdue == 1 { "" } else { "s" };
        out.push(format!("- Overdue: {} to-do{s}", day.overdue));
    }
    match &day.agent_counts {
        Some(c) => {
            out.push(format!("- Agent lane: {c}"));
            out.extend(day.agent_attention.iter().take(MAX_ATTENTION).map(|l| format!("  - {l}")));
        }
        None => out.push("- Agent lane: empty.".to_string()),
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, h, m, 0).unwrap()
    }

    /// Thursday 22 Oct, workday 09:00–17:00 UTC, a 14:30 meeting.
    pub fn day() -> DayView {
        DayView {
            today: NaiveDate::from_ymd_opt(2026, 10, 22).unwrap(),
            workday: true,
            workday_start: t(9, 0),
            workday_end: t(17, 0),
            meetings: vec![Meeting {
                id: "b1".into(),
                title: "Clyde sync".into(),
                start: t(14, 30),
                end: t(15, 0),
                tentative: false,
            }],
            capacity_summary: Some("10h 35m planned — over by 4h 35m".into()),
            over_by_minutes: 275,
            deadlines: vec![],
            overdue: 0,
            agent_counts: None,
            agent_attention: vec![],
        }
    }

    fn kinds(v: &[Anchor]) -> Vec<AnchorKind> {
        v.iter().map(|a| a.kind).collect()
    }

    #[test]
    fn next_anchor_is_the_earliest_still_to_fire() {
        let mut state = HeartbeatState::default();
        // 13:00: the workday-start window has lapsed; over-capacity is due
        // since 09:00 and unfired, so it comes first.
        assert_eq!(next_anchor(&state, &day(), t(13, 0)).map(|a| a.kind), Some(AnchorKind::OverCapacity));
        // Once it fires, the 14:30 meeting's prep at 13:45 is next.
        let fired: Vec<Anchor> = due_anchors(&state, &day(), t(13, 0));
        state.mark_anchors_fired(fired.iter().map(|a| a.key.as_str()), day().today, t(13, 0));
        let next = next_anchor(&state, &day(), t(13, 0)).unwrap();
        assert_eq!((next.kind, next.due_at), (AnchorKind::Meeting, t(13, 45)));
        // After the workday, nothing.
        assert!(next_anchor(&state, &day(), t(17, 30)).is_none());
    }

    #[test]
    fn anchors_fall_due_in_their_windows() {
        let s = HeartbeatState::default();
        let d = day();
        assert!(due_anchors(&s, &d, t(8, 59)).is_empty(), "before the workday");
        assert_eq!(
            kinds(&due_anchors(&s, &d, t(9, 0))),
            vec![AnchorKind::OverCapacity, AnchorKind::WorkdayStart]
        );
        // The start anchor lapses after two hours; over capacity holds all day.
        assert_eq!(kinds(&due_anchors(&s, &d, t(11, 0))), vec![AnchorKind::OverCapacity]);
        // 45 minutes before the meeting, until it starts.
        assert_eq!(
            kinds(&due_anchors(&s, &d, t(13, 45))),
            vec![AnchorKind::OverCapacity, AnchorKind::Meeting]
        );
        assert!(due_anchors(&s, &d, t(13, 44)).iter().all(|a| a.kind != AnchorKind::Meeting));
        assert!(due_anchors(&s, &d, t(14, 30)).iter().all(|a| a.kind != AnchorKind::Meeting), "lapsed at the start");
        // 30 minutes before the end, until the end.
        assert!(kinds(&due_anchors(&s, &d, t(16, 30))).contains(&AnchorKind::WorkdayEnd));
        assert!(due_anchors(&s, &d, t(17, 0)).is_empty(), "everything lapses with the day");
    }

    #[test]
    fn each_anchor_fires_once_per_day() {
        let mut s = HeartbeatState::default();
        let d = day();
        let due = due_anchors(&s, &d, t(13, 50));
        s.mark_anchors_fired(due.iter().map(|a| a.key.as_str()), d.today, t(13, 50));
        assert!(due_anchors(&s, &d, t(13, 55)).is_empty());
        // The end-of-day anchor is still to come.
        assert_eq!(kinds(&due_anchors(&s, &d, t(16, 40))), vec![AnchorKind::WorkdayEnd]);
        // Tomorrow is a new day.
        let mut tomorrow = day();
        tomorrow.today = tomorrow.today.succ_opt().unwrap();
        let shift = Duration::days(1);
        tomorrow.workday_start += shift;
        tomorrow.workday_end += shift;
        tomorrow.meetings.clear();
        assert_eq!(due_anchors(&s, &tomorrow, t(9, 0) + shift).len(), 2);
    }

    #[test]
    fn weekends_tentative_and_fitting_days_have_fewer_anchors() {
        let s = HeartbeatState::default();
        let mut d = day();
        d.over_by_minutes = 0;
        assert_eq!(kinds(&due_anchors(&s, &d, t(9, 0))), vec![AnchorKind::WorkdayStart]);
        d.workday = false;
        assert!(due_anchors(&s, &d, t(9, 0)).is_empty());
        assert_eq!(kinds(&due_anchors(&s, &d, t(14, 0))), vec![AnchorKind::Meeting], "meetings count every day");
        d.meetings[0].tentative = true;
        assert!(due_anchors(&s, &d, t(14, 0)).is_empty());
    }

    #[test]
    fn anchor_keys_and_labels() {
        let a = day_anchors(&day());
        let m = a.iter().find(|a| a.kind == AnchorKind::Meeting).unwrap();
        assert_eq!(m.key, format!("meeting:b1@{}", t(14, 30).to_rfc3339()));
        assert_eq!(m.label, format!("Meeting \"Clyde sync\" at {}", hm(t(14, 30))));
        let o = a.iter().find(|a| a.kind == AnchorKind::OverCapacity).unwrap();
        assert_eq!(o.label, "Today's plan is over capacity (over by 4h 35m)");
    }

    #[test]
    fn workday_parsing_falls_back() {
        assert_eq!(workday_minutes("08:30", "16:00"), (510, 960));
        assert_eq!(workday_minutes("nope", "16:00"), (540, 1020));
        assert_eq!(workday_minutes("18:00", "09:00"), (540, 1020));
    }

    #[test]
    fn look_ahead_renders_the_day() {
        let now = t(14, 0).with_timezone(&Local);
        let at = |x: DateTime<Utc>| x.with_timezone(&Local).format("%H:%M").to_string();
        let mut d = day();
        d.meetings.push(Meeting { id: "b2".into(), title: "Standup".into(), start: t(13, 45), end: t(14, 15), tentative: false });
        d.meetings.push(Meeting { id: "b3".into(), title: "Maybe lunch".into(), start: t(17, 0), end: t(17, 30), tentative: true });
        d.meetings.push(Meeting { id: "b4".into(), title: "Tomorrow".into(), start: t(20, 0), end: t(21, 0), tentative: false });
        d.meetings.sort_by_key(|m| m.start);
        d.deadlines = vec![
            Deadline { title: "Invoice".into(), due: d.today },
            Deadline { title: "Deck".into(), due: d.today.succ_opt().unwrap() },
        ];
        d.overdue = 2;
        d.agent_counts = Some("1 working · 1 stuck".into());
        d.agent_attention = vec!["\"Clyde prep\" is stuck: no access to the shared drive".into()];
        let expected = vec![
            "The next few hours:".to_string(),
            "- Calendar (next 4 hours):".to_string(),
            format!("  - {}–{} Standup (now)", at(t(13, 45)), at(t(14, 15))),
            format!("  - {}–{} Clyde sync", at(t(14, 30)), at(t(15, 0))),
            format!("  - {}–{} Maybe lunch (tentative)", at(t(17, 0)), at(t(17, 30))),
            "- Today's plan: 10h 35m planned — over by 4h 35m".to_string(),
            "- Deadlines: today \"Invoice\"; tomorrow \"Deck\"".to_string(),
            "- Overdue: 2 to-dos".to_string(),
            "- Agent lane: 1 working · 1 stuck".to_string(),
            "  - \"Clyde prep\" is stuck: no access to the shared drive".to_string(),
        ];
        assert_eq!(look_ahead_section(Some(&d), now), expected);

        let mut quiet = day();
        quiet.meetings.clear();
        quiet.capacity_summary = None;
        let lines = look_ahead_section(Some(&quiet), now);
        assert_eq!(lines[1], "- Calendar: nothing in the next 4 hours.");
        assert_eq!(lines.last().unwrap(), "- Agent lane: empty.");
        assert!(look_ahead_section(None, now)[1].contains("planner is off"));
    }

    #[test]
    fn capture_reads_the_planner() {
        let now = Utc::now();
        let mut planner = PlannerState::default();
        let block = |id: &str, mins: i64, busy: bool| model::TimeBlock {
            id: id.into(),
            todo_id: None,
            title: format!("event {id}"),
            start: now + Duration::minutes(mins),
            end: now + Duration::minutes(mins + 30),
            source: BlockSource::External {
                uid: id.into(),
                subscription_id: None,
                url: None,
                busy,
                tentative: false,
            },
        };
        planner.blocks.push(block("later", 90, true));
        planner.blocks.push(block("soon", 30, true));
        planner.blocks.push(block("free", 40, false));
        planner.blocks.push(block("past", -120, true));
        let mut own = block("own", 10, true);
        own.source = BlockSource::Manual;
        planner.blocks.push(own);
        let today = now.with_timezone(&Local).date_naive();
        let mut due = Todo::new("pay rent", 30.0);
        due.deadline = Some(today);
        planner.todos.push(due);
        let mut stuck = Todo::new("Clyde prep", 30.0);
        stuck.set_lane(model::Lane::Agent);
        stuck.agent_state = Some(AgentState::Stuck);
        stuck.open_item = Some("no drive access".into());
        planner.todos.push(stuck);

        let d = DayView::capture(&planner, &crate::settings::Settings::default(), now);
        assert_eq!(d.today, today);
        assert_eq!(d.meetings.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["soon", "later"]);
        assert_eq!(d.deadlines, vec![Deadline { title: "pay rent".into(), due: today }]);
        assert!(d.capacity_summary.is_some());
        assert!(d.agent_counts.as_deref().unwrap().contains("1 stuck"));
        assert_eq!(d.agent_attention, vec!["\"Clyde prep\" is stuck: no drive access".to_string()]);
        assert!(d.workday_start < d.workday_end);
    }
}
