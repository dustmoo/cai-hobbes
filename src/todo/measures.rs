//! Measures for the symbiotic planner (SYMBIOTIC_PLANNER.md §5): weekly
//! your-time progress against goals, leverage, ground covered, the judgment
//! ratio, chore load, and the evidence-only suggestions (review style, scope
//! widening).
//!
//! All pure. Where a figure has no data source yet the function returns
//! `None` rather than inventing a number — the review shows "not measured
//! yet", never a fabricated zero.

// Public API whose consumers are wave 2 (the weekly review screen and the Today header); the
// tool handlers use only part of it so far.
#![allow(dead_code)]

use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};

use super::model::{
    BlockSource, FocusSession, HumanKind, Lane, ResolvedDecision, ReviewSignals, ReviewStyle,
    TimeBlock, TimeGoal, Todo, TodoStatus, TrustLedger,
};

/// A half-open UTC interval.
type Span = (DateTime<Utc>, DateTime<Utc>);

/// The local Monday starting the week that contains `date`.
pub fn week_start(date: NaiveDate) -> NaiveDate {
    date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64)
}

/// The 7 local days of `date`'s week, Monday first.
pub fn week_days(date: NaiveDate) -> [NaiveDate; 7] {
    let start = week_start(date);
    std::array::from_fn(|i| start + chrono::Duration::days(i as i64))
}

/// UTC bounds of the local days `[from, to)`.
fn local_span_utc(from: NaiveDate, to_exclusive: NaiveDate) -> Span {
    use chrono::TimeZone;
    let at = |d: NaiveDate| {
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            .earliest()
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight")))
    };
    (at(from), at(to_exclusive))
}

fn in_week(t: DateTime<Utc>, week_of: NaiveDate) -> bool {
    let d = t.with_timezone(&chrono::Local).date_naive();
    let start = week_start(week_of);
    d >= start && d < start + chrono::Duration::days(7)
}

fn merge(mut v: Vec<Span>) -> Vec<Span> {
    v.sort_by_key(|(s, _)| *s);
    let mut out: Vec<Span> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some((_, ce)) if s <= *ce => *ce = (*ce).max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn minutes(spans: &[Span]) -> u32 {
    spans
        .iter()
        .map(|(s, e)| (*e - *s).num_minutes().max(0) as u32)
        .fold(0u32, u32::saturating_add)
}

fn clip_to(
    spans: &[Span],
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
) -> Vec<Span> {
    spans
        .iter()
        .filter_map(|(s, e)| {
            let (s, e) = ((*s).max(lo), (*e).min(hi));
            (e > s).then_some((s, e))
        })
        .collect()
}

/// Subtract `cut` (merged) from `spans` (merged).
fn subtract(
    spans: &[Span],
    cut: &[Span],
) -> Vec<Span> {
    let mut out = Vec::new();
    for &(s, e) in spans {
        let mut cur = s;
        for &(cs, ce) in cut {
            if ce <= cur || cs >= e {
                continue;
            }
            if cs > cur {
                out.push((cur, cs));
            }
            cur = cur.max(ce);
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

/// The your-time category a to-do counts toward: its `time_category`, else
/// the first tag naming a goal's category. `None` for anything that isn't a
/// human your-time to-do.
pub fn todo_category<'a>(todo: &'a Todo, goals: &'a [TimeGoal]) -> Option<&'a str> {
    if todo.lane != Lane::Human || todo.kind != Some(HumanKind::YourTime) {
        return None;
    }
    if let Some(c) = todo.time_category.as_deref().filter(|c| !c.trim().is_empty()) {
        return Some(c.trim());
    }
    todo.tags
        .iter()
        .find_map(|tag| goals.iter().find(|g| g.matches(tag)).map(|g| g.category.as_str()))
}

/// One category's week.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryProgress {
    /// The goal's spelling when one exists, else the block/to-do's.
    pub category: String,
    pub colour: Option<String>,
    /// `None` when the category has time but no goal.
    pub goal_minutes: Option<u32>,
    /// Protected your-time block minutes in the week (overlaps merged).
    pub protected_minutes: u32,
    /// Person focus minutes on your-time to-dos of the category that fall
    /// outside its protected blocks (so nothing counts twice).
    pub focus_minutes: u32,
}

impl CategoryProgress {
    /// Protected plus focus: the category's time this week.
    pub fn minutes(&self) -> u32 {
        self.protected_minutes.saturating_add(self.focus_minutes)
    }

    /// Minutes still to go against the goal (0 when met or no goal).
    pub fn remaining(&self) -> u32 {
        self.goal_minutes.unwrap_or(0).saturating_sub(self.minutes())
    }

    pub fn goal_met(&self) -> bool {
        self.goal_minutes.is_some_and(|g| self.minutes() >= g)
    }
}

/// The week's your-time picture.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct YourTimeWeek {
    pub week_start: NaiveDate,
    /// Goals first (in goal order), then categories with time but no goal.
    pub categories: Vec<CategoryProgress>,
}

impl YourTimeWeek {
    pub fn total_minutes(&self) -> u32 {
        self.categories.iter().map(CategoryProgress::minutes).fold(0, u32::saturating_add)
    }

    pub fn goal_minutes(&self) -> u32 {
        self.categories
            .iter()
            .filter_map(|c| c.goal_minutes)
            .fold(0, u32::saturating_add)
    }

    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
    }

    /// `"Reading 2h/4h · Outside 30m/4h"`, minutes rounded down to `step`
    /// (pass 1 for exact figures).
    pub fn compact(&self, step: u32) -> String {
        let step = step.max(1);
        self.categories
            .iter()
            .map(|c| {
                let m = super::model::format_minutes(c.minutes() / step * step);
                match c.goal_minutes {
                    Some(g) => format!("{} {}/{}", c.category, m, super::model::format_minutes(g)),
                    None => format!("{} {}", c.category, m),
                }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// **Weekly your-time progress.** Per category, for the local week holding
/// `week_of`: protected `YourTime` block minutes (merged per category) plus
/// person focus minutes on your-time to-dos of that category that fall
/// outside those blocks, against the goals. Closed focus sessions only (a
/// live one attributes nothing until it closes, like capacity).
pub fn your_time_week(
    goals: &[TimeGoal],
    blocks: &[TimeBlock],
    todos: &[Todo],
    sessions: &[FocusSession],
    week_of: NaiveDate,
) -> YourTimeWeek {
    let start = week_start(week_of);
    let (lo, hi) = local_span_utc(start, start + chrono::Duration::days(7));

    let key = |c: &str| c.trim().to_lowercase();
    let mut block_spans: HashMap<String, Vec<Span>> = HashMap::new();
    let mut display: HashMap<String, String> = HashMap::new();
    for b in blocks {
        if let BlockSource::YourTime { category } = &b.source {
            if b.end > b.start {
                block_spans.entry(key(category)).or_default().push((b.start, b.end));
                display.entry(key(category)).or_insert_with(|| category.trim().to_string());
            }
        }
    }

    let category_of: HashMap<&str, &str> = todos
        .iter()
        .filter_map(|t| todo_category(t, goals).map(|c| (t.id.as_str(), c)))
        .collect();
    let mut focus_spans: HashMap<String, Vec<Span>> = HashMap::new();
    for s in sessions.iter().filter(|s| !s.actor.is_agent()) {
        let Some(cat) = category_of.get(s.todo_id.as_str()) else {
            continue;
        };
        let Some(ended) = s.ended_at else { continue };
        let end = ended.min(s.started_at + chrono::Duration::minutes(s.minutes as i64));
        if end > s.started_at {
            focus_spans.entry(key(cat)).or_default().push((s.started_at, end));
            display.entry(key(cat)).or_insert_with(|| cat.to_string());
        }
    }

    let progress = |k: &str, label: String, goal: Option<&TimeGoal>| {
        let blocks = clip_to(&merge(block_spans.get(k).cloned().unwrap_or_default()), lo, hi);
        let focus = clip_to(&merge(focus_spans.get(k).cloned().unwrap_or_default()), lo, hi);
        CategoryProgress {
            category: label,
            colour: goal.and_then(|g| g.colour.clone()),
            goal_minutes: goal.map(|g| g.weekly_target_minutes),
            protected_minutes: minutes(&blocks),
            focus_minutes: minutes(&subtract(&focus, &blocks)),
        }
    };

    let mut categories: Vec<CategoryProgress> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for g in goals {
        let k = key(&g.category);
        if seen.contains(&k) {
            continue;
        }
        categories.push(progress(&k, g.category.clone(), Some(g)));
        seen.push(k);
    }
    let mut extra: Vec<String> = block_spans
        .keys()
        .chain(focus_spans.keys())
        .filter(|k| !seen.contains(k))
        .cloned()
        .collect();
    extra.sort();
    extra.dedup();
    for k in extra {
        let p = progress(&k, display.get(&k).cloned().unwrap_or_else(|| k.clone()), None);
        if p.minutes() > 0 {
            categories.push(p);
        }
    }
    YourTimeWeek {
        week_start: start,
        categories,
    }
}

/// Protected your-time minutes on one local day (merged across
/// categories), and the blocks themselves, time-sorted.
pub fn protected_on(blocks: &[TimeBlock], date: NaiveDate) -> (u32, Vec<&TimeBlock>) {
    let mut on: Vec<&TimeBlock> = blocks
        .iter()
        .filter(|b| b.is_protected() && b.start.with_timezone(&chrono::Local).date_naive() == date)
        .collect();
    on.sort_by_key(|b| b.start);
    let spans = merge(on.iter().filter(|b| b.end > b.start).map(|b| (b.start, b.end)).collect());
    (minutes(&spans), on)
}

// ── Review measures ─────────────────────────────────────────────────────────

/// Leverage: agent minutes per engaged human minute over the week — the
/// exocortex figure (`fleet::exocortex_multiplier`) summed over days. The
/// caller supplies the week's agent minutes (fleet + in-app agent focus)
/// and engaged minutes (fleet coverage). `None` under 5 engaged minutes or
/// with no agent work, like the daily figure.
pub fn weekly_leverage(agent_minutes: u32, engaged_minutes: u32) -> Option<f64> {
    crate::fleet::exocortex_multiplier(agent_minutes, engaged_minutes).map(|(m, _)| m)
}

/// Ground covered in the week: agent outcomes reached and decisions cleared
/// — so busyness isn't mistaken for progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroundCovered {
    /// Agent-lane to-dos completed in the week.
    pub outcomes_reached: u32,
    /// Decisions the person answered in the week.
    pub decisions_cleared: u32,
}

pub fn ground_covered(
    todos: &[Todo],
    decisions: &[ResolvedDecision],
    week_of: NaiveDate,
) -> GroundCovered {
    GroundCovered {
        outcomes_reached: todos
            .iter()
            .filter(|t| t.lane == Lane::Agent && t.status == TodoStatus::Completed)
            .filter(|t| t.completed_at.is_some_and(|c| in_week(c, week_of)))
            .count() as u32,
        decisions_cleared: decisions.iter().filter(|d| in_week(d.resolved_at, week_of)).count()
            as u32,
    }
}

/// Judgment calls (decisions the person answered) against routing calls
/// surfaced to the person (permission prompts they clicked through), for
/// the week. Routing should trend to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JudgmentRatio {
    pub judgment: u32,
    pub routing: u32,
    /// Routing calls a trust rule handled without asking (not surfaced).
    pub routing_auto: u32,
}

/// Count from the decision log and the trust-decision log
/// (`context::trust_store`). `None` when neither has anything in the week
/// (nothing measured, not "zero").
pub fn judgment_ratio(
    decisions: &[ResolvedDecision],
    trust: &[crate::context::trust_store::TrustDecision],
    week_of: NaiveDate,
) -> Option<JudgmentRatio> {
    use crate::context::trust_store::DecisionKind;
    let judgment = decisions.iter().filter(|d| in_week(d.resolved_at, week_of)).count() as u32;
    let mut routing = 0u32;
    let mut routing_auto = 0u32;
    for t in trust.iter().filter(|t| in_week(t.ts, week_of)) {
        match t.decision {
            DecisionKind::RuleHit => routing_auto += 1,
            _ => routing += 1,
        }
    }
    (judgment + routing + routing_auto > 0).then_some(JudgmentRatio {
        judgment,
        routing,
        routing_auto,
    })
}

/// Routing calls a trust rule resolved without asking on one local day —
/// the decision queue's "N handled by your rules today".
pub fn routing_rule_hits_on(
    trust: &[crate::context::trust_store::TrustDecision],
    date: NaiveDate,
) -> u32 {
    use crate::context::trust_store::DecisionKind;
    trust
        .iter()
        .filter(|t| t.decision == DecisionKind::RuleHit)
        .filter(|t| t.ts.with_timezone(&chrono::Local).date_naive() == date)
        .count() as u32
}

/// Chore minutes in the week: person focus minutes on chore to-dos, plus,
/// for chores completed in the week with no session rows (pre-sessions
/// data), their `actual_minutes`. `None` when no to-do is marked a chore
/// yet — chore load isn't measured until chores are.
pub fn chore_minutes(todos: &[Todo], sessions: &[FocusSession], week_of: NaiveDate) -> Option<u32> {
    let chores: Vec<&Todo> = todos
        .iter()
        .filter(|t| t.lane == Lane::Human && t.kind == Some(HumanKind::Chore))
        .collect();
    if chores.is_empty() {
        return None;
    }
    let days = week_days(week_of);
    let mut total = 0u32;
    for t in chores {
        let rows: Vec<&FocusSession> = sessions.iter().filter(|s| s.todo_id == t.id).collect();
        if rows.is_empty() {
            if t.status == TodoStatus::Completed && t.completed_at.is_some_and(|c| in_week(c, week_of)) {
                total = total.saturating_add(t.actual_minutes);
            }
            continue;
        }
        for s in rows.into_iter().filter(|s| !s.actor.is_agent()) {
            for d in days {
                total = total.saturating_add(super::model::session_minutes_on(s, d));
            }
        }
    }
    Some(total)
}

/// Suggest a gentler review style when the current one's nudges are mostly
/// dismissed or its reviews skipped. **Suggests only** — the caller shows
/// "Try a gentler style?" and the person decides; acting on blunt nudges
/// keeps them (no suggestion).
///
/// Rule: at least 3 signals under the current style, and dismissals plus
/// skipped reviews are at least twice the nudges acted on.
pub fn suggest_review_style(current: ReviewStyle, signals: &ReviewSignals) -> Option<ReviewStyle> {
    let c = signals.counts(current);
    let negative = c.dismissed.saturating_add(signals.skipped(current));
    if c.acted + negative < 3 || negative < c.acted.saturating_mul(2).max(1) {
        return None;
    }
    current.softer()
}

/// Suggest widening agent time's scope when the evidence is strong:
/// ≥ 10 decided proposals, ≥ 90% accepted, no out-of-scope attempts, no
/// broken promises. Returns the suggestion text; never applies anything.
pub fn suggest_scope_widening(ledger: &TrustLedger) -> Option<String> {
    let decided = ledger.proposals_accepted + ledger.proposals_edited + ledger.proposals_dismissed;
    let rate = ledger.acceptance_rate()?;
    if decided < 10 || rate < 0.9 || ledger.out_of_scope_attempts > 0 || ledger.promises_broken > 0 {
        return None;
    }
    Some(format!(
        "The last {} agent-time proposals were {:.0}% accepted with nothing out of scope. \
         Allow agent time to file accepted research digests straight into Notes?",
        decided,
        rate * 100.0
    ))
}

/// The weekly review's numbers in one place. Fields with no data source yet
/// are `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct WeekReview {
    pub your_time: YourTimeWeek,
    /// `None` without fleet engagement data (non-Pro, or nothing engaged).
    pub leverage: Option<f64>,
    pub ground_covered: GroundCovered,
    pub judgment: Option<JudgmentRatio>,
    pub chore_minutes: Option<u32>,
    pub trust: TrustLedger,
    pub style_suggestion: Option<ReviewStyle>,
    pub scope_suggestion: Option<String>,
}

/// Assemble the review from planner state plus the edge-supplied figures
/// (`agent_minutes`/`engaged_minutes` from the fleet, `trust` from
/// `trust_store::recent`).
pub fn week_review(
    planner: &super::PlannerState,
    style: ReviewStyle,
    week_of: NaiveDate,
    agent_and_engaged_minutes: Option<(u32, u32)>,
    trust: &[crate::context::trust_store::TrustDecision],
) -> WeekReview {
    WeekReview {
        your_time: your_time_week(
            &planner.time_goals,
            &planner.blocks,
            &planner.todos,
            &planner.focus_sessions,
            week_of,
        ),
        leverage: agent_and_engaged_minutes.and_then(|(a, e)| weekly_leverage(a, e)),
        ground_covered: ground_covered(&planner.todos, &planner.decision_log, week_of),
        judgment: judgment_ratio(&planner.decision_log, trust, week_of),
        chore_minutes: chore_minutes(&planner.todos, &planner.focus_sessions, week_of),
        trust: planner.trust_ledger,
        style_suggestion: suggest_review_style(style, &planner.review_signals),
        scope_suggestion: suggest_scope_widening(&planner.trust_ledger),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{FocusActor, FocusEndReason, NudgeOutcome};

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Local wall-clock `h:m` on `d` as UTC.
    fn at(d: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn yt_block(id: &str, cat: &str, d: NaiveDate, h1: u32, h2: u32) -> TimeBlock {
        TimeBlock {
            id: id.into(),
            todo_id: None,
            title: cat.into(),
            start: at(d, h1, 0),
            end: at(d, h2, 0),
            source: BlockSource::YourTime { category: cat.into() },
        }
    }

    fn person_session(todo: &str, start: DateTime<Utc>, mins: u32) -> FocusSession {
        let mut s = FocusSession::open(todo, start, FocusActor::Person);
        s.close(start + chrono::Duration::minutes(mins as i64), FocusEndReason::Completed);
        s
    }

    #[test]
    fn week_starts_monday() {
        // 2026-10-07 is a Wednesday.
        assert_eq!(week_start(date("2026-10-07")), date("2026-10-05"));
        assert_eq!(week_start(date("2026-10-05")), date("2026-10-05"));
        assert_eq!(week_start(date("2026-10-11")), date("2026-10-05"));
        assert_eq!(week_days(date("2026-10-07"))[6], date("2026-10-11"));
    }

    #[test]
    fn your_time_sums_blocks_and_focus_without_double_counting() {
        let wed = date("2026-10-07");
        let goals = vec![
            TimeGoal { colour: Some("#8DBB7A".into()), ..TimeGoal::new("Outside", 240) },
            TimeGoal::new("Reading", 180),
        ];
        let blocks = vec![
            yt_block("b1", "outside", wed, 12, 13),          // 60, case-insensitive match
            yt_block("b2", "Outside", wed, 12, 13),          // duplicate slot merges
            yt_block("b3", "Gaming", wed, 20, 22),           // no goal: still reported
            yt_block("old", "Outside", date("2026-09-30"), 12, 14), // previous week
        ];
        let mut book = Todo::new("Read Dune", 0.0);
        book.kind = Some(HumanKind::YourTime);
        book.tags = vec!["reading".into()]; // category via tag
        let mut walk = Todo::new("Walk", 0.0);
        walk.kind = Some(HumanKind::YourTime);
        walk.time_category = Some("Outside".into());
        let sessions = vec![
            person_session(&book.id, at(wed, 21, 0), 45),
            // 30 min of this walk sit inside the 12–13 Outside block: only the
            // 30 outside it count as focus.
            person_session(&walk.id, at(wed, 12, 30), 60),
            // Agent sessions never count as your time.
            {
                let mut s = FocusSession::open(&book.id, at(wed, 9, 0), FocusActor::Agent { session_id: None });
                s.close(at(wed, 10, 0), FocusEndReason::Completed);
                s
            },
        ];
        let w = your_time_week(&goals, &blocks, &[book, walk], &sessions, wed);
        assert_eq!(w.week_start, date("2026-10-05"));
        assert_eq!(w.categories.len(), 3);
        let outside = &w.categories[0];
        assert_eq!((outside.protected_minutes, outside.focus_minutes), (60, 30));
        assert_eq!(outside.goal_minutes, Some(240));
        assert_eq!(outside.remaining(), 150);
        assert_eq!(outside.colour.as_deref(), Some("#8DBB7A"));
        let reading = &w.categories[1];
        assert_eq!(reading.minutes(), 45);
        let gaming = &w.categories[2];
        assert_eq!((gaming.category.as_str(), gaming.minutes(), gaming.goal_minutes), ("Gaming", 120, None));
        assert_eq!(w.total_minutes(), 255);
        assert_eq!(w.goal_minutes(), 420);
        assert_eq!(w.compact(15), "Outside 1h 30m/4h · Reading 45m/3h · Gaming 2h");
    }

    #[test]
    fn protected_on_merges_across_categories() {
        let d = date("2026-10-07");
        let blocks = vec![yt_block("a", "Reading", d, 19, 21), yt_block("b", "Gaming", d, 20, 22)];
        let (m, on) = protected_on(&blocks, d);
        assert_eq!(m, 180);
        assert_eq!(on.len(), 2);
    }

    #[test]
    fn ground_covered_and_chores() {
        let wed = date("2026-10-07");
        let mut out = Todo::new("PR open", 0.0);
        out.set_lane(Lane::Agent);
        out.mark_completed(at(wed, 10, 0));
        let mut chore = Todo::new("Pay bill", 0.0);
        chore.kind = Some(HumanKind::Chore);
        chore.actual_minutes = 7;
        chore.mark_completed(at(wed, 15, 0));
        let mut filed = Todo::new("Expenses", 0.0);
        filed.kind = Some(HumanKind::Chore);
        let sessions = vec![person_session(&filed.id, at(wed, 16, 0), 20)];
        let todos = vec![out, chore, filed];
        let g = ground_covered(&todos, &[], wed);
        assert_eq!(g.outcomes_reached, 1);
        assert_eq!(chore_minutes(&todos, &sessions, wed), Some(27));
        assert_eq!(chore_minutes(&[Todo::new("x", 0.0)], &[], wed), None, "no chores = not measured");
    }

    #[test]
    fn judgment_ratio_is_none_without_data() {
        assert_eq!(judgment_ratio(&[], &[], date("2026-10-07")), None);
        use crate::context::trust_store::{DecisionKind, TrustDecision};
        let mut hit = TrustDecision::new(DecisionKind::RuleHit, "s", "t", String::new(), None, None, Some("r".into()));
        hit.ts = Utc::now();
        let mut clicked = TrustDecision::new(DecisionKind::Approved, "s", "t", String::new(), None, None, None);
        clicked.ts = Utc::now();
        let today = chrono::Local::now().date_naive();
        let r = judgment_ratio(&[], &[hit.clone(), clicked], today).unwrap();
        assert_eq!((r.judgment, r.routing, r.routing_auto), (0, 1, 1));
        assert_eq!(routing_rule_hits_on(&[hit], today), 1);
    }

    #[test]
    fn leverage_reuses_the_exocortex_floor() {
        assert_eq!(weekly_leverage(0, 100), None);
        assert_eq!(weekly_leverage(100, 4), None);
        assert_eq!(weekly_leverage(300, 100), Some(3.0));
    }

    #[test]
    fn review_style_only_ever_suggests_softer() {
        let mut s = ReviewSignals::default();
        assert_eq!(suggest_review_style(ReviewStyle::Direct, &s), None, "no evidence");
        s.record(ReviewStyle::Direct, NudgeOutcome::Acted);
        s.record(ReviewStyle::Direct, NudgeOutcome::Acted);
        s.record(ReviewStyle::Direct, NudgeOutcome::Acted);
        assert_eq!(suggest_review_style(ReviewStyle::Direct, &s), None, "acting keeps blunt nudges");
        for _ in 0..5 {
            s.record(ReviewStyle::Direct, NudgeOutcome::Dismissed);
        }
        s.record(ReviewStyle::Direct, NudgeOutcome::Skipped);
        assert_eq!(suggest_review_style(ReviewStyle::Direct, &s), Some(ReviewStyle::Balanced));
        for _ in 0..4 {
            s.record(ReviewStyle::Encouraging, NudgeOutcome::Dismissed);
        }
        assert_eq!(suggest_review_style(ReviewStyle::Encouraging, &s), None, "already gentlest");
        s.reset(ReviewStyle::Direct);
        assert_eq!(s.counts(ReviewStyle::Direct), Default::default());
    }

    #[test]
    fn scope_widening_needs_strong_clean_evidence() {
        let mut l = TrustLedger { proposals_accepted: 9, ..Default::default() };
        assert!(suggest_scope_widening(&l).is_none(), "under 10 decided");
        l.proposals_accepted = 20;
        assert!(suggest_scope_widening(&l).unwrap().contains("20"));
        l.out_of_scope_attempts = 1;
        assert!(suggest_scope_widening(&l).is_none());
        l.out_of_scope_attempts = 0;
        l.proposals_dismissed = 5;
        assert!(suggest_scope_widening(&l).is_none(), "80% accepted");
    }
}
