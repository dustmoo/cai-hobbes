//! The weekly review: your time by category against goals, agent leverage
//! and ground covered, judgment vs routing, chore minutes and agent-time
//! proposals (SYMBIOTIC_PLANNER.md §5).
//!
//! **A magic mirror, not a plain one.** The numbers and bars are identical
//! in every feedback style; only the ordering, the wording and the single
//! nudge change ([`review_copy`]). Where a measure has no data it shows "—",
//! never an invented zero.

use chrono::{Datelike, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::settings::{Settings, SettingsManager};
use crate::todo::measures::{self, CategoryProgress, YourTimeWeek};
use crate::todo::model::{self, BlockSource, NudgeOutcome, ProposalKind, ProposalStatus, ReviewStyle, TimeBlock};
use crate::todo::store;
use crate::todo::PlannerState;

use super::persist_block;
use super::today;
use super::your_time::{category_colour, TimeGoalsEditor};
use crate::components::planner_timeline::{fmt_hhmm, local_minutes_to_utc};

// ── Pure: formatting ────────────────────────────────────────────────────────

/// `720` → `"12h 00m"`, `45` → `"45m"`.
pub(crate) fn fmt_hm(minutes: u32) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    }
}

/// Whole hours for the encouraging headline (`"12"`), minutes under an hour.
fn fmt_round_hours(minutes: u32) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        ((minutes + 30) / 60).to_string()
    }
}

/// `["A"]` → `"A"`, `["A","B"]` → `"A and B"`, `["A","B","C"]` → `"A, B and C"`.
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

// ── Pure: the nudge ─────────────────────────────────────────────────────────

/// The one concrete suggestion for next week: two protected blocks for the
/// category furthest from its goal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Nudge {
    pub category: String,
    pub minutes_each: u32,
    /// Local minutes since midnight.
    pub start_min: u32,
}

/// Default slot when the category has no habit to follow (the design's
/// 12:30 walk).
pub(crate) const DEFAULT_NUDGE_START: u32 = 12 * 60 + 30;

/// Pick the nudge: the goal category with the largest shortfall, split over
/// two blocks (rounded to the quarter hour, 15–90 min each), at the
/// category's habitual start when it has one.
pub(crate) fn pick_nudge(week: &YourTimeWeek, habitual_start: impl Fn(&str) -> Option<u32>) -> Option<Nudge> {
    let worst = week
        .categories
        .iter()
        .filter(|c| c.goal_minutes.is_some() && c.remaining() > 0)
        .max_by_key(|c| c.remaining())?;
    let half = worst.remaining().div_ceil(2);
    let minutes_each = (half.div_ceil(15) * 15).clamp(15, 90);
    Some(Nudge {
        category: worst.category.clone(),
        minutes_each,
        start_min: habitual_start(&worst.category).unwrap_or(DEFAULT_NUDGE_START),
    })
}

/// The two days the nudge protects: Tuesday and Thursday of the week after
/// `today`'s.
pub(crate) fn nudge_days(today: NaiveDate) -> [NaiveDate; 2] {
    let next = measures::week_start(today) + chrono::Duration::days(7);
    [next + chrono::Duration::days(1), next + chrono::Duration::days(3)]
}

// ── Pure: the copy per feedback style ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewRow {
    pub category: String,
    pub colour: String,
    pub minutes: u32,
    pub goal: Option<u32>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewCopy {
    pub title: &'static str,
    pub headline: String,
    pub sub: String,
    pub rows: Vec<ReviewRow>,
    /// (key, line): "Win", "Gap", "Next"…
    pub lines: Vec<(&'static str, String)>,
    pub nudge: Option<String>,
    pub yes: &'static str,
    pub no: &'static str,
}

fn ratio(c: &CategoryProgress) -> f64 {
    match c.goal_minutes {
        Some(g) if g > 0 => c.minutes() as f64 / g as f64,
        _ => f64::MAX,
    }
}

/// The review's words for one style. Rows carry the same numbers in every
/// style — only their order and labels differ.
pub(crate) fn review_copy(
    style: ReviewStyle,
    week: &YourTimeWeek,
    colour_of: impl Fn(&CategoryProgress) -> String,
    nudge: Option<&Nudge>,
) -> ReviewCopy {
    let goal_cats: Vec<&CategoryProgress> = week.categories.iter().filter(|c| c.goal_minutes.is_some()).collect();
    let spent: u32 = goal_cats.iter().map(|c| c.minutes()).sum();
    let goal = week.goal_minutes();
    let pct = if goal > 0 { spent * 100 / goal } else { 0 };
    let met: Vec<&CategoryProgress> = goal_cats.iter().copied().filter(|c| c.goal_met()).collect();
    let mut gaps: Vec<&CategoryProgress> = goal_cats.iter().copied().filter(|c| c.remaining() > 0).collect();
    gaps.sort_by_key(|c| std::cmp::Reverse(c.remaining()));
    let over = goal_cats
        .iter()
        .copied()
        .filter(|c| c.minutes() > c.goal_minutes.unwrap_or(0))
        .max_by_key(|c| c.minutes() - c.goal_minutes.unwrap_or(0));
    let days = "Tuesday and Thursday";

    let row = |c: &CategoryProgress, label: String| ReviewRow {
        category: c.category.clone(),
        colour: colour_of(c),
        minutes: c.minutes(),
        goal: c.goal_minutes,
        label,
    };
    let mut ordered: Vec<&CategoryProgress> = week.categories.iter().collect();

    match style {
        ReviewStyle::Direct => {
            ordered.sort_by_key(|c| match c.goal_minutes {
                Some(g) => c.minutes() as i64 - g as i64,
                None => i64::MAX,
            });
            let rows = ordered
                .iter()
                .map(|c| {
                    let label = match c.goal_minutes {
                        Some(g) if c.minutes() < g => format!("\u{2212}{}", model::format_minutes(g - c.minutes())),
                        Some(g) if c.minutes() > g => format!("+{} over", model::format_minutes(c.minutes() - g)),
                        Some(_) => "on goal".to_string(),
                        None => fmt_hm(c.minutes()),
                    };
                    row(c, label)
                })
                .collect();
            let mut lines = Vec::new();
            match gaps.first() {
                Some(g) => {
                    let mut gap = format!(
                        "{} got {} of {}.",
                        g.category,
                        model::format_minutes(g.minutes()),
                        model::format_minutes(g.goal_minutes.unwrap_or(0))
                    );
                    if let Some(o) = over {
                        gap.push_str(&format!(
                            " It lost to {}, which ran {} over.",
                            o.category,
                            model::format_minutes(o.minutes() - o.goal_minutes.unwrap_or(0))
                        ));
                    }
                    lines.push(("Gap", gap));
                    if let Some(g2) = gaps.get(1) {
                        lines.push(("Also", format!("{} is {} short.", g2.category, model::format_minutes(g2.remaining()))));
                    }
                }
                None => lines.push(("Gap", "None this week \u{2014} every category reached its goal.".to_string())),
            }
            let short = goal.saturating_sub(spent);
            ReviewCopy {
                title: "Where you fell short",
                headline: if short > 0 { format!("{} short", model::format_minutes(short)) } else { "Goal met".to_string() },
                sub: format!("{} of your {} goal", fmt_hm(spent), model::format_minutes(goal)),
                rows,
                lines,
                nudge: nudge.map(|n| {
                    let mut s = format!(
                        "Block {} of {} at {} on {}",
                        model::format_minutes(n.minutes_each),
                        n.category,
                        fmt_hhmm(n.start_min),
                        days
                    );
                    if let Some(o) = over {
                        s.push_str(&format!(
                            ", and keep {} to its {}",
                            o.category,
                            model::format_minutes(o.goal_minutes.unwrap_or(0))
                        ));
                    }
                    s.push('.');
                    s
                }),
                yes: "Block them",
                no: "Skip",
            }
        }
        ReviewStyle::Balanced => {
            ordered.sort_by(|a, b| ratio(b).partial_cmp(&ratio(a)).unwrap_or(std::cmp::Ordering::Equal));
            let rows = ordered
                .iter()
                .map(|c| {
                    let label = match c.goal_minutes {
                        Some(g) => format!("{} / {}", fmt_hm(c.minutes()), fmt_hm(g)),
                        None => fmt_hm(c.minutes()),
                    };
                    row(c, label)
                })
                .collect();
            let mut lines = Vec::new();
            if !met.is_empty() {
                let names: Vec<&str> = met.iter().map(|c| c.category.as_str()).collect();
                let verb = if names.len() == 1 { "hit its goal" } else { "hit their goals" };
                lines.push(("Win", format!("{} {}.", join_and(&names), verb)));
            } else if let Some(best) = goal_cats
                .iter()
                .filter(|c| c.minutes() > 0)
                .max_by(|a, b| ratio(a).partial_cmp(&ratio(b)).unwrap_or(std::cmp::Ordering::Equal))
            {
                lines.push((
                    "Win",
                    format!(
                        "{} came closest: {} of {}.",
                        best.category,
                        model::format_minutes(best.minutes()),
                        model::format_minutes(best.goal_minutes.unwrap_or(0))
                    ),
                ));
            }
            if let Some(g) = gaps.first() {
                lines.push((
                    "Gap",
                    format!(
                        "{} is {} short of {}.",
                        g.category,
                        model::format_minutes(g.remaining()),
                        model::format_minutes(g.goal_minutes.unwrap_or(0))
                    ),
                ));
            }
            ReviewCopy {
                title: "Your time this week",
                headline: fmt_hm(spent),
                sub: format!("of {} goal \u{b7} {}%", model::format_minutes(goal), pct),
                rows,
                lines,
                nudge: nudge.map(|n| {
                    format!(
                        "Protect {} of {} at {} on {} next week?",
                        model::format_minutes(n.minutes_each),
                        n.category,
                        fmt_hhmm(n.start_min),
                        days
                    )
                }),
                yes: "Protect them",
                no: "Not this week",
            }
        }
        ReviewStyle::Encouraging => {
            ordered.sort_by(|a, b| {
                b.goal_met()
                    .cmp(&a.goal_met())
                    .then(ratio(b).partial_cmp(&ratio(a)).unwrap_or(std::cmp::Ordering::Equal))
            });
            let rows = ordered
                .iter()
                .map(|c| {
                    let label = match c.goal_minutes {
                        Some(_) if c.goal_met() => "goal met".to_string(),
                        Some(_) if c.remaining() <= 60 => format!("{} to go", model::format_minutes(c.remaining())),
                        Some(_) => "room to grow".to_string(),
                        None => fmt_hm(c.minutes()),
                    };
                    row(c, label)
                })
                .collect();
            let mut lines = Vec::new();
            let near: Vec<&str> = gaps
                .iter()
                .filter(|c| c.remaining() <= 60 && c.minutes() > 0)
                .map(|c| c.category.as_str())
                .collect();
            if !met.is_empty() {
                let names: Vec<&str> = met.iter().map(|c| c.category.as_str()).collect();
                let mut win = format!("You kept {}", join_and(&names));
                if !near.is_empty() {
                    win.push_str(&format!(" and nearly all of {}", join_and(&near)));
                }
                win.push('.');
                lines.push(("Win", win));
            } else if spent > 0 {
                lines.push(("Win", format!("You made room for {} of your own time this week.", fmt_hm(spent))));
            }
            if let Some(g) = gaps.iter().find(|c| !near.contains(&c.category.as_str())).or(gaps.first()) {
                lines.push(("Next", format!("{} has the most room. One block is a good start.", g.category)));
            }
            ReviewCopy {
                title: "What you made room for",
                headline: format!("{} of {} hours", fmt_round_hours(spent), fmt_round_hours(goal)),
                sub: "protected for you this week".to_string(),
                rows,
                lines,
                nudge: nudge.map(|n| {
                    format!(
                        "Want to try two {} {} blocks next week? Small wins count.",
                        model::format_minutes(n.minutes_each),
                        n.category
                    )
                }),
                yes: "Let's try",
                no: "Maybe later",
            }
        }
    }
}

/// One scale for every bar and goal marker: the largest minutes or goal,
/// rounded up to whole hours (at least 1h), and the hour tick step.
pub(crate) fn bar_scale(rows: &[ReviewRow]) -> (u32, u32) {
    let top = rows.iter().map(|r| r.minutes.max(r.goal.unwrap_or(0))).max().unwrap_or(0);
    let hours = top.div_ceil(60).max(1);
    let step = hours.div_ceil(6).max(1);
    let hours = hours.div_ceil(step) * step;
    (hours * 60, step)
}

fn pct_of(v: u32, scale: u32) -> f64 {
    if scale == 0 {
        0.0
    } else {
        ((v.min(scale) as f64 / scale as f64) * 1000.0).round() / 10.0
    }
}

// ── The view ────────────────────────────────────────────────────────────────

const PANEL: &str = "flex min-w-0 flex-col gap-3 rounded-lg border border-faint bg-section p-4";
const EYEBROW: &str = "text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const BIG: &str = "text-[26px] font-semibold leading-none text-fg tabular-nums";
const SUB: &str = "text-xs text-fg-muted";

fn week_label(start: NaiveDate) -> String {
    let end = start + chrono::Duration::days(6);
    if start.month() == end.month() {
        format!("Week of {} \u{2013} {}", start.format("%-d"), end.format("%-d %B"))
    } else {
        format!("Week of {} \u{2013} {}", start.format("%-d %B"), end.format("%-d %B"))
    }
}

#[component]
pub(crate) fn WeeklyReview() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let mut week_of = use_signal(|| measures::week_start(today()));
    // Opens on its own when there are no goals yet ("set goals" lands here).
    let mut editing_goals = use_signal(|| planner.peek().time_goals.is_empty());
    // Weeks whose nudge was acted on / dismissed in this view (and what happened).
    let mut nudge_done = use_signal(std::collections::HashMap::<NaiveDate, String>::new);
    let mut style_offer_dismissed = use_signal(|| false);

    // Fleet figures cost a store query per day: computed when the week
    // changes, not on every fleet poke.
    let leverage = use_memo(move || {
        let start = week_of();
        if !crate::entitlement::pro_active() {
            return None;
        }
        let fleet = fleet_state.peek();
        let now = Utc::now();
        let real_today = today();
        let state = planner.peek();
        let (mut agent, mut engaged_secs) = (0u32, 0u32);
        for d in measures::week_days(start).into_iter().filter(|d| *d <= real_today) {
            agent = agent
                .saturating_add(crate::fleet::fleet_agent_minutes_on(&fleet, d, now))
                .saturating_add(model::agent_minutes_on(&state.focus_sessions, d));
            engaged_secs = engaged_secs.saturating_add(crate::fleet::engaged_seconds_on(&fleet, d, now));
        }
        Some((agent, engaged_secs / 60))
    });
    let trust = use_memo(move || {
        let _ = week_of();
        crate::context::trust_store::recent(2000)
    });

    let style = settings.read().planner_review_style;
    let start = week_of();
    let state = planner.read();
    let review = measures::week_review(&state, style, start, *leverage.read(), &trust.read());
    let goals = state.time_goals.clone();
    let habitual = |cat: &str| {
        use chrono::Timelike;
        state
            .blocks
            .iter()
            .filter(|b| b.your_time_category().is_some_and(|c| c.eq_ignore_ascii_case(cat)))
            .max_by_key(|b| b.start)
            .map(|b| {
                let t = b.start.with_timezone(&chrono::Local).time();
                t.hour() * 60 + t.minute()
            })
    };
    let nudge = pick_nudge(&review.your_time, habitual);
    let copy = review_copy(style, &review.your_time, |c| category_colour(&goals, &c.category), nudge.as_ref());
    let (scale, step) = bar_scale(&copy.rows);
    let ticks: Vec<u32> = (0..=scale / 60).step_by(step as usize).collect();

    // Trend series, oldest first, ending at the viewed week.
    let weeks: Vec<NaiveDate> = (0..6).rev().map(|i| start - chrono::Duration::days(7 * i)).collect();
    let week_labels: Vec<String> = weeks.iter().map(|w| w.format("%-d %b").to_string()).collect();
    let routing: Vec<Option<measures::JudgmentRatio>> =
        weeks.iter().map(|w| measures::judgment_ratio(&state.decision_log, &trust.read(), *w)).collect();
    let chores: Vec<Option<u32>> =
        weeks.iter().map(|w| measures::chore_minutes(&state.todos, &state.focus_sessions, *w)).collect();
    // Agent-time proposals decided this week, by kind.
    let in_week = |p: &model::Proposal| {
        p.resolved_at
            .is_some_and(|t| measures::week_start(t.with_timezone(&chrono::Local).date_naive()) == start)
    };
    let kinds = [
        (ProposalKind::Research, "Research digests"),
        (ProposalKind::Todo, "Suggested to-dos"),
        (ProposalKind::Skill, "Draft skills"),
        (ProposalKind::Note, "Notes"),
        (ProposalKind::Other, "Other"),
        (ProposalKind::Offer, "Offers to take a task"),
    ];
    // Notice proposals by kind (accepted vs dismissed), then delivered
    // results by job (kept vs needs work or discarded).
    let decided = |p: &&model::Proposal| in_week(p) && !p.removed;
    let mut prop_rows: Vec<(&str, u32, u32)> = kinds
        .iter()
        .map(|(k, label)| {
            let notice = state.proposals.iter().filter(decided).filter(|p| p.job == model::Job::Notice && p.kind == *k);
            let (acc, dis) = notice.fold((0u32, 0u32), |(a, d), p| match p.status {
                ProposalStatus::Accepted | ProposalStatus::Edited => (a + 1, d),
                ProposalStatus::Dismissed => (a, d + 1),
                ProposalStatus::NeedsWork | ProposalStatus::Open => (a, d),
            });
            (*label, acc, dis)
        })
        .filter(|(_, a, d)| a + d > 0)
        .collect();
    let jobs = [
        (model::Job::Prepare, "Prepared"),
        (model::Job::Finish, "Finished work"),
        (model::Job::Tend, "Upkeep"),
    ];
    prop_rows.extend(
        jobs.iter()
            .map(|(job, label)| {
                let results = state.proposals.iter().filter(decided).filter(|p| p.job == *job);
                let (kept, not_kept) = results.fold((0u32, 0u32), |(k, n), p| match p.status {
                    ProposalStatus::Accepted | ProposalStatus::Edited => (k + 1, n),
                    ProposalStatus::NeedsWork | ProposalStatus::Dismissed => (k, n + 1),
                    ProposalStatus::Open => (k, n),
                });
                (*label, kept, not_kept)
            })
            .filter(|(_, a, d)| a + d > 0),
    );
    drop(state);

    let has_goals = !goals.is_empty();
    let real_week = measures::week_start(today());
    let done_note = nudge_done.read().get(&start).cloned();
    let style_offer = review.style_suggestion.filter(|_| !*style_offer_dismissed.read());

    let prop_acc: u32 = prop_rows.iter().map(|(_, a, _)| a).sum();
    let prop_total: u32 = prop_rows.iter().map(|(_, a, d)| a + d).sum();
    let prop_scale = prop_rows.iter().map(|(_, a, d)| a + d).max().unwrap_or(1).max(1);

    let mut set_style = move |s: ReviewStyle| {
        if settings.peek().planner_review_style == s {
            return;
        }
        settings.write().planner_review_style = s;
        if let Err(e) = settings_manager.read().save(&settings.peek()) {
            tracing::error!("planner: failed to save review style: {}", e);
        }
    };
    let mut record = move |outcome: NudgeOutcome| {
        let style = settings.peek().planner_review_style;
        planner.write().review_signals.record(style, outcome);
        if let Err(e) = store::save_review_signals(&planner.peek().review_signals) {
            tracing::error!("planner: failed to save review signals: {}", e);
        }
    };

    let routing_now = routing.last().copied().flatten();
    let routing_first = routing.iter().flatten().next().copied();
    let routing_scale = routing
        .iter()
        .flatten()
        .map(|r| r.judgment + r.routing)
        .max()
        .unwrap_or(1)
        .max(1);
    let chores_now = chores.last().copied().flatten();
    let chore_first = chores.iter().flatten().next().copied();
    let chore_scale = chores.iter().flatten().copied().max().unwrap_or(1).max(1);

    rsx! {
        div {
            class: "flex-1 min-w-0 min-h-0 overflow-y-auto bg-app",
            div {
                class: "flex flex-col gap-3 px-6 pt-6 pb-10",
                div {
                    class: "flex flex-wrap items-baseline gap-2.5",
                    h1 { class: "text-xl font-semibold", "Weekly review" }
                    span { class: "text-sm text-fg-muted", "{week_label(start)}" }
                    div { class: "flex-1" }
                    button {
                        class: "h-6 w-6 rounded border border-faint text-[13px] text-fg-muted hover:bg-input hover:text-fg",
                        title: "Previous week",
                        onclick: move |_| week_of.set(start - chrono::Duration::days(7)),
                        "\u{2039}"
                    }
                    button {
                        class: "h-6 w-6 rounded border border-faint text-[13px] text-fg-muted hover:bg-input hover:text-fg disabled:opacity-40",
                        title: "Next week",
                        disabled: start >= real_week,
                        onclick: move |_| week_of.set(start + chrono::Duration::days(7)),
                        "\u{203a}"
                    }
                }
                if let Some(s) = style_offer {
                    div {
                        class: "flex items-center gap-2 rounded-md border border-faint bg-card px-3 py-1.5 text-xs text-fg",
                        span { class: "min-w-0 flex-1",
                            "These reviews' nudges haven't been landing. Try the {s.as_str()} style? The numbers stay the same."
                        }
                        button {
                            class: "rounded bg-btn-primary px-2 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                            onclick: move |_| {
                                set_style(s);
                                style_offer_dismissed.set(true);
                            },
                            "Switch"
                        }
                        button {
                            class: "rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input",
                            onclick: move |_| style_offer_dismissed.set(true),
                            "Not now"
                        }
                    }
                }
                div {
                    class: "grid gap-3",
                    style: "grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));",
                    // ── Your time ──────────────────────────────────────
                    div {
                        class: "{PANEL}",
                        style: "grid-column: 1 / -1;",
                        div {
                            class: "flex flex-wrap items-center gap-2",
                            span { class: "{EYEBROW}", "{copy.title}" }
                            div { class: "flex-1" }
                            span { class: "{SUB}", "Feedback style" }
                            div {
                                class: "inline-flex overflow-hidden rounded-lg border border-faint",
                                for (s, label) in [(ReviewStyle::Direct, "Direct"), (ReviewStyle::Balanced, "Balanced"), (ReviewStyle::Encouraging, "Encouraging")] {
                                    button {
                                        key: "{label}",
                                        class: if style == s {
                                            "px-2.5 py-1 text-[11px] font-medium bg-btn-primary text-fg"
                                        } else {
                                            "px-2.5 py-1 text-[11px] text-fg-muted hover:bg-input hover:text-fg"
                                        },
                                        "aria-pressed": if style == s { "true" } else { "false" },
                                        onclick: move |_| set_style(s),
                                        "{label}"
                                    }
                                }
                            }
                            button {
                                class: "ml-1 text-[11px] text-primary-300 hover:text-fg",
                                onclick: move |_| editing_goals.toggle(),
                                if has_goals { "Edit goals" } else { "Set goals" }
                            }
                        }
                        if *editing_goals.read() {
                            TimeGoalsEditor { on_close: move |_| editing_goals.set(false) }
                        }
                        if !has_goals && copy.rows.is_empty() {
                            p { class: "text-sm text-fg-muted",
                                "No your-time goals yet. Say what the rest of the week is for \u{2014} Reading, Outside, Gaming \u{2014} and protected blocks on the timeline will count toward them."
                            }
                        } else {
                            div {
                                class: "flex flex-wrap items-baseline gap-2.5",
                                span { class: "{BIG}", "{copy.headline}" }
                                span { class: "{SUB}", "{copy.sub}" }
                                div { class: "flex-1" }
                                span { class: "font-mono text-[11px] text-fg-muted", "goal marked \u{2503}" }
                            }
                            div {
                                class: "flex flex-col gap-3.5",
                                for r in copy.rows.iter() {
                                    div {
                                        key: "{r.category}",
                                        class: "grid grid-cols-[120px_minmax(0,1fr)_100px] items-center gap-x-3 text-xs text-fg",
                                        span { class: "inline-flex min-w-0 items-center gap-2",
                                            span { class: "h-2 w-2 shrink-0 rounded-full", style: "background: {r.colour};" }
                                            span { class: "truncate", "{r.category}" }
                                        }
                                        div {
                                            class: "relative h-2.5 rounded-full bg-input",
                                            span {
                                                class: "absolute inset-y-0 left-0 rounded-full",
                                                style: "width: {pct_of(r.minutes, scale)}%; background: {r.colour};",
                                            }
                                            if let Some(g) = r.goal {
                                                span {
                                                    class: "absolute -top-1 -bottom-1 w-0.5 rounded bg-fg",
                                                    style: "left: {pct_of(g, scale)}%;",
                                                    title: "Goal {model::format_minutes(g)}",
                                                }
                                            }
                                        }
                                        span { class: "text-right font-mono text-[11px] tabular-nums text-fg-muted", "{r.label}" }
                                    }
                                }
                                div {
                                    class: "grid grid-cols-[120px_minmax(0,1fr)_100px] gap-x-3 -mt-2",
                                    span {}
                                    div {
                                        class: "relative h-3 font-mono text-[10px] text-fg-muted",
                                        for h in ticks.iter().copied() {
                                            span {
                                                key: "{h}",
                                                class: "absolute -translate-x-1/2",
                                                style: "left: {pct_of(h * 60, scale)}%;",
                                                if h == 0 { "0" } else { "{h}h" }
                                            }
                                        }
                                    }
                                    span {}
                                }
                            }
                            div {
                                class: "flex flex-col gap-1.5",
                                for (k, t) in copy.lines.iter().cloned() {
                                    div {
                                        key: "{k}",
                                        class: "flex gap-2 text-[12.5px] leading-[18px] text-fg",
                                        span { class: "w-11 shrink-0 pt-px font-mono text-[10.5px] uppercase text-fg-muted", "{k}" }
                                        span { "{t}" }
                                    }
                                }
                            }
                            if let Some(note) = done_note {
                                div { class: "rounded-lg border border-faint px-3 py-2 text-xs text-fg-muted", "{note}" }
                            } else if let (Some(text), Some(n)) = (copy.nudge.clone(), nudge.clone()) {
                                div {
                                    class: "flex flex-wrap items-center gap-3 rounded-lg border border-subtle bg-card px-3 py-2.5",
                                    span {
                                        class: "h-2.5 w-2.5 shrink-0 rounded-full",
                                        style: "background: {category_colour(&goals, &n.category)};",
                                    }
                                    span { class: "min-w-0 flex-1 text-[12.5px] leading-[17px] text-fg", "{text}" }
                                    button {
                                        class: "rounded bg-btn-primary px-2.5 py-1 text-[11px] text-fg hover:bg-btn-primary-hover",
                                        onclick: {
                                            let n = n.clone();
                                            move |_| {
                                                let blocks: Vec<TimeBlock> = nudge_days(today())
                                                    .into_iter()
                                                    .map(|d| TimeBlock {
                                                        id: uuid::Uuid::new_v4().to_string(),
                                                        todo_id: None,
                                                        title: n.category.clone(),
                                                        start: local_minutes_to_utc(d, n.start_min),
                                                        end: local_minutes_to_utc(d, n.start_min + n.minutes_each),
                                                        source: BlockSource::YourTime { category: n.category.clone() },
                                                    })
                                                    .collect();
                                                for b in &blocks {
                                                    persist_block(b);
                                                }
                                                planner.write().blocks.extend(blocks);
                                                record(NudgeOutcome::Acted);
                                                nudge_done.write().insert(
                                                    start,
                                                    format!(
                                                        "Protected {} of {} on Tuesday and Thursday next week, at {}.",
                                                        model::format_minutes(n.minutes_each),
                                                        n.category,
                                                        fmt_hhmm(n.start_min)
                                                    ),
                                                );
                                            }
                                        },
                                        "{copy.yes}"
                                    }
                                    button {
                                        class: "rounded border border-faint px-2.5 py-1 text-[11px] text-fg hover:bg-input",
                                        onclick: move |_| {
                                            record(NudgeOutcome::Dismissed);
                                            nudge_done.write().insert(start, "Okay \u{2014} no change for next week.".to_string());
                                        },
                                        "{copy.no}"
                                    }
                                }
                            }
                        }
                    }
                    // ── Agent leverage ─────────────────────────────────
                    div {
                        class: "{PANEL}",
                        span { class: "{EYEBROW}", "Agent leverage" }
                        div {
                            class: "flex items-baseline gap-2.5",
                            match review.leverage {
                                Some(m) => rsx! { span { class: "{BIG}", "{m:.1}\u{d7}" } },
                                None => rsx! { span { class: "{BIG} text-fg-muted", "\u{2014}" } },
                            }
                            span { class: "{SUB}", "agent minutes per engaged minute" }
                        }
                        if let (Some(_), Some((agent, engaged))) = (review.leverage, *leverage.read()) {
                            {
                                let top = agent.max(engaged).max(1);
                                rsx! {
                                    div {
                                        class: "flex flex-col gap-2",
                                        div {
                                            class: "grid grid-cols-[64px_minmax(0,1fr)_64px] items-center gap-x-2.5 text-xs text-fg",
                                            span { "Agents" }
                                            div { class: "relative h-2.5 rounded-full bg-input",
                                                span { class: "absolute inset-y-0 left-0 rounded-full bg-[#EA8A2E]", style: "width: {pct_of(agent, top)}%;" }
                                            }
                                            span { class: "text-right font-mono text-[11px] text-fg-muted", "{fmt_hm(agent)}" }
                                        }
                                        div {
                                            class: "grid grid-cols-[64px_minmax(0,1fr)_64px] items-center gap-x-2.5 text-xs text-fg",
                                            span { "You" }
                                            div { class: "relative h-2.5 rounded-full bg-input",
                                                span { class: "absolute inset-y-0 left-0 rounded-full bg-primary-400", style: "width: {pct_of(engaged, top)}%;" }
                                            }
                                            span { class: "text-right font-mono text-[11px] text-fg-muted", "{fmt_hm(engaged)}" }
                                        }
                                    }
                                }
                            }
                        } else {
                            p { class: "{SUB}", "Needs fleet time this week to measure." }
                        }
                        div { class: "h-px bg-[var(--color-border-muted)]" }
                        span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Ground covered" }
                        div {
                            class: "flex gap-5",
                            div { class: "flex flex-col",
                                span { class: "text-xl font-semibold tabular-nums", "{review.ground_covered.outcomes_reached}" }
                                span { class: "{SUB}", "outcomes reached" }
                            }
                            div { class: "flex flex-col",
                                span { class: "text-xl font-semibold tabular-nums", "{review.ground_covered.decisions_cleared}" }
                                span { class: "{SUB}", "decisions cleared" }
                            }
                        }
                    }
                    // ── Judgment vs routing ────────────────────────────
                    div {
                        class: "{PANEL}",
                        span { class: "{EYEBROW}", "Judgment vs routing" }
                        div {
                            class: "flex flex-wrap items-baseline gap-2.5",
                            match routing_now {
                                Some(r) => rsx! {
                                    span { class: "{BIG}", "{r.routing}" }
                                    span { class: "{SUB}",
                                        "routing calls reached you"
                                        if let Some(f) = routing_first.filter(|f| f.routing > r.routing) {
                                            " \u{b7} down from {f.routing}"
                                        }
                                    }
                                },
                                None => rsx! {
                                    span { class: "{BIG} text-fg-muted", "\u{2014}" }
                                    span { class: "{SUB}", "nothing measured this week" }
                                },
                            }
                        }
                        div { class: "flex gap-3 text-[10.5px] text-fg-muted",
                            span { class: "inline-flex items-center gap-1", span { class: "h-2 w-2 rounded-full bg-primary-400" } "judgment" }
                            span { class: "inline-flex items-center gap-1", span { class: "h-2 w-2 rounded-full bg-gray-400" } "routing" }
                        }
                        div {
                            class: "flex h-[120px] items-end gap-2",
                            for (i, r) in routing.iter().enumerate() {
                                div {
                                    key: "{i}",
                                    class: "flex flex-1 flex-col items-center justify-end gap-1",
                                    span { class: "font-mono text-[10px] text-fg-muted",
                                        match r { Some(r) => r.routing.to_string(), None => "\u{2014}".to_string() }
                                    }
                                    div {
                                        class: "flex w-full max-w-[30px] flex-col",
                                        if let Some(r) = r {
                                            span { class: "rounded-t-sm bg-gray-400", style: "height: {(r.routing as f64 / routing_scale as f64 * 90.0).round()}px;" }
                                            span { class: "bg-primary-400", style: "height: {(r.judgment as f64 / routing_scale as f64 * 90.0).round()}px;" }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "flex gap-2 font-mono text-[10px] text-fg-muted",
                            for (i, w) in week_labels.iter().enumerate() {
                                span { key: "{i}", class: "flex-1 text-center", "{w}" }
                            }
                        }
                    }
                    // ── Chore minutes ──────────────────────────────────
                    div {
                        class: "{PANEL}",
                        span { class: "{EYEBROW}", "Chore minutes" }
                        div {
                            class: "flex flex-wrap items-baseline gap-2.5",
                            match chores_now {
                                Some(m) => rsx! {
                                    span { class: "{BIG}", "{m}m" }
                                    span { class: "{SUB}",
                                        "this week"
                                        if let Some(f) = chore_first.filter(|f| *f > m && *f > 0) {
                                            " \u{b7} down {(f - m) * 100 / f}% since {week_labels[0]}"
                                        }
                                    }
                                },
                                None => rsx! {
                                    span { class: "{BIG} text-fg-muted", "\u{2014}" }
                                    span { class: "{SUB}", "not measured yet \u{2014} mark chores with !chore" }
                                },
                            }
                        }
                        div {
                            class: "flex h-[120px] items-end gap-2",
                            for (i, c) in chores.iter().enumerate() {
                                div {
                                    key: "{i}",
                                    class: "flex flex-1 flex-col items-center justify-end gap-1",
                                    span { class: "font-mono text-[10px] text-fg-muted",
                                        match c { Some(m) => format!("{m}m"), None => "\u{2014}".to_string() }
                                    }
                                    if let Some(m) = c {
                                        span {
                                            class: if i == chores.len() - 1 { "w-full max-w-[30px] rounded-t-sm bg-primary-300" } else { "w-full max-w-[30px] rounded-t-sm bg-gray-600" },
                                            style: "height: {(*m as f64 / chore_scale as f64 * 90.0).round()}px;",
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "flex gap-2 font-mono text-[10px] text-fg-muted",
                            for (i, w) in week_labels.iter().enumerate() {
                                span { key: "{i}", class: "flex-1 text-center", "{w}" }
                            }
                        }
                    }
                    // ── Agent-time proposals ───────────────────────────
                    div {
                        class: "{PANEL}",
                        span { class: "{EYEBROW}", "Agent-time proposals" }
                        if prop_total == 0 {
                            div {
                                class: "flex items-baseline gap-2.5",
                                span { class: "{BIG} text-fg-muted", "\u{2014}" }
                                span { class: "{SUB}", "none decided this week" }
                            }
                        } else {
                            div {
                                class: "flex items-baseline gap-2.5",
                                span { class: "{BIG}", "{prop_acc} of {prop_total}" }
                                span { class: "{SUB}", "accepted this week" }
                            }
                            div { class: "flex gap-3 text-[10.5px] text-fg-muted",
                                span { class: "inline-flex items-center gap-1", span { class: "h-2 w-2 rounded-full bg-primary-400" } "accepted" }
                                span { class: "inline-flex items-center gap-1", span { class: "h-2 w-2 rounded-full bg-gray-600" } "dismissed" }
                            }
                            div {
                                class: "flex flex-col gap-3",
                                for (label, a, d) in prop_rows.iter().copied() {
                                    div {
                                        key: "{label}",
                                        class: "flex flex-col gap-1",
                                        div { class: "flex text-xs text-fg",
                                            span { "{label}" }
                                            div { class: "flex-1" }
                                            span { class: "font-mono text-[11px] text-fg-muted", "{a} of {a + d}" }
                                        }
                                        div { class: "flex h-2.5 overflow-hidden rounded-full bg-input",
                                            span { class: "bg-primary-400", style: "width: {pct_of(a, prop_scale)}%;" }
                                            span { class: "bg-gray-600", style: "width: {pct_of(d, prop_scale)}%;" }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "mt-auto flex items-center gap-1.5 text-[11px] text-fg-muted",
                            Icon { width: 11, height: 11, icon: fi_icons::FiShield }
                            {
                                let l = review.trust;
                                match l.acceptance_rate() {
                                    Some(r) => format!("All time: {:.0}% accepted", r * 100.0),
                                    None => "No proposals decided yet".to_string(),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(name: &str, minutes: u32, goal: Option<u32>) -> CategoryProgress {
        CategoryProgress {
            category: name.into(),
            colour: None,
            goal_minutes: goal,
            protected_minutes: minutes,
            focus_minutes: 0,
        }
    }

    /// The WeekTone board's week: Experimenting 5h10/6h, Gaming 5h20/5h,
    /// Outside 1h30/4h.
    fn week() -> YourTimeWeek {
        YourTimeWeek {
            week_start: "2026-09-28".parse().unwrap(),
            categories: vec![
                cat("Experimenting", 310, Some(360)),
                cat("Gaming", 320, Some(300)),
                cat("Outside", 90, Some(240)),
            ],
        }
    }

    fn copy(style: ReviewStyle) -> ReviewCopy {
        let w = week();
        let n = pick_nudge(&w, |_| None);
        review_copy(style, &w, |_| "#000000".into(), n.as_ref())
    }

    #[test]
    fn numbers_are_identical_across_styles_only_order_and_words_change() {
        let mut sets: Vec<Vec<(String, u32, Option<u32>)>> = [ReviewStyle::Direct, ReviewStyle::Balanced, ReviewStyle::Encouraging]
            .iter()
            .map(|s| {
                let mut v: Vec<_> = copy(*s).rows.into_iter().map(|r| (r.category, r.minutes, r.goal)).collect();
                v.sort();
                v
            })
            .collect();
        let first = sets.remove(0);
        assert!(sets.iter().all(|s| *s == first));
        // Order differs: Direct leads with the gap, Balanced/Encouraging with the win.
        assert_eq!(copy(ReviewStyle::Direct).rows[0].category, "Outside");
        assert_eq!(copy(ReviewStyle::Balanced).rows[0].category, "Gaming");
        assert_eq!(copy(ReviewStyle::Encouraging).rows[0].category, "Gaming");
    }

    #[test]
    fn direct_names_the_gap_and_the_drain() {
        let c = copy(ReviewStyle::Direct);
        assert_eq!(c.title, "Where you fell short");
        assert_eq!(c.headline, "3h short");
        assert_eq!(c.sub, "12h 00m of your 15h goal");
        assert_eq!(c.rows[0].label, "\u{2212}2h 30m");
        assert!(c.rows.iter().any(|r| r.label == "+20m over"));
        assert_eq!(c.lines[0], ("Gap", "Outside got 1h 30m of 4h. It lost to Gaming, which ran 20m over.".to_string()));
        assert_eq!(c.nudge.as_deref(), Some("Block 1h 15m of Outside at 12:30 on Tuesday and Thursday, and keep Gaming to its 5h."));
        assert_eq!((c.yes, c.no), ("Block them", "Skip"));
    }

    #[test]
    fn balanced_is_one_win_one_gap_one_step() {
        let c = copy(ReviewStyle::Balanced);
        assert_eq!(c.headline, "12h 00m");
        assert_eq!(c.sub, "of 15h goal \u{b7} 80%");
        assert_eq!(c.rows[0].label, "5h 20m / 5h 00m");
        assert_eq!(c.lines, vec![
            ("Win", "Gaming hit its goal.".to_string()),
            ("Gap", "Outside is 2h 30m short of 4h.".to_string()),
        ]);
        assert_eq!(c.nudge.as_deref(), Some("Protect 1h 15m of Outside at 12:30 on Tuesday and Thursday next week?"));
        assert_eq!((c.yes, c.no), ("Protect them", "Not this week"));
    }

    #[test]
    fn encouraging_leads_with_what_you_made_room_for() {
        let c = copy(ReviewStyle::Encouraging);
        assert_eq!(c.title, "What you made room for");
        assert_eq!(c.headline, "12 of 15 hours");
        let labels: Vec<&str> = c.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["goal met", "50m to go", "room to grow"]);
        assert_eq!(c.lines[0], ("Win", "You kept Gaming and nearly all of Experimenting.".to_string()));
        assert_eq!(c.lines[1], ("Next", "Outside has the most room. One block is a good start.".to_string()));
        assert!(c.nudge.unwrap().ends_with("Small wins count."));
        assert_eq!((c.yes, c.no), ("Let's try", "Maybe later"));
    }

    #[test]
    fn no_gap_means_no_nudge() {
        let w = YourTimeWeek { week_start: week().week_start, categories: vec![cat("Gaming", 320, Some(300))] };
        assert!(pick_nudge(&w, |_| None).is_none());
        let c = review_copy(ReviewStyle::Direct, &w, |_| "#000".into(), None);
        assert_eq!(c.headline, "Goal met");
        assert!(c.nudge.is_none());
    }

    #[test]
    fn nudge_follows_habit_and_lands_next_tuesday_and_thursday() {
        let n = pick_nudge(&week(), |c| (c == "Outside").then_some(7 * 60)).unwrap();
        assert_eq!(n.category, "Outside");
        assert_eq!(n.minutes_each, 75);
        assert_eq!(n.start_min, 7 * 60);
        let wed: NaiveDate = "2026-10-07".parse().unwrap();
        assert_eq!(nudge_days(wed), ["2026-10-13".parse().unwrap(), "2026-10-15".parse().unwrap()]);
    }

    #[test]
    fn one_scale_for_bars_and_goal_markers() {
        let rows = copy(ReviewStyle::Balanced).rows;
        assert_eq!(bar_scale(&rows), (360, 1));
        assert_eq!(pct_of(240, 360), 66.7);
        let big = vec![ReviewRow { category: "x".into(), colour: String::new(), minutes: 13 * 60, goal: None, label: String::new() }];
        assert_eq!(bar_scale(&big), (15 * 60, 3));
        assert_eq!(fmt_hm(45), "45m");
        assert_eq!(fmt_hm(720), "12h 00m");
    }
}
