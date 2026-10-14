//! Your time: the category palette, the Today header's "Your time today"
//! readout, and the small goals editor (SYMBIOTIC_PLANNER.md §2, §4).
//!
//! The readout measures against the person's own intention — the daily
//! share of their weekly goals — never against a raw total (§5).

use chrono::{DateTime, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::todo::categories;
use crate::todo::measures;
use crate::todo::model::{self, TimeBlock, TimeGoal};
use crate::todo::store;
use crate::todo::PlannerState;

/// The default your-time palette offered when creating a goal:
/// (category, colour). Text colour follows from [`text_on`].
pub(crate) const DEFAULT_PALETTE: [(&str, &str); 5] = [
    ("Reading", "#E2BCC3"),
    ("Outside", "#8DBB7A"),
    ("Experimenting", "#9787DC"),
    ("Gaming", "#2C7A71"),
    ("Shows & movies", "#A4554C"),
];

/// Fallback colour for a category with no goal and no palette match.
pub(crate) const FALLBACK_COLOUR: &str = "#9787DC";

fn parse_hex(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| v as f64 / 255.0);
    Some((c(0)?, c(2)?, c(4)?))
}

/// Relative luminance (WCAG) of a `#rrggbb` colour.
fn luminance(hex: &str) -> Option<f64> {
    let (r, g, b) = parse_hex(hex)?;
    let lin = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b))
}

/// Text colour on a category fill: dark on light fills, white on dark ones
/// (Reading/Outside/Experimenting → dark; Gaming/Shows & movies → white).
pub(crate) fn text_on(hex: &str) -> &'static str {
    match luminance(hex) {
        Some(l) if l > 0.2 => "#141414",
        _ => "#FFFFFF",
    }
}

/// A category's colour: its goal's colour, else the default palette's entry
/// for that name, else the fallback.
pub(crate) fn category_colour(goals: &[TimeGoal], category: &str) -> String {
    if let Some(c) = goals
        .iter()
        .find(|g| g.matches(category))
        .and_then(|g| g.colour.clone())
        .filter(|c| parse_hex(c).is_some())
    {
        return c;
    }
    DEFAULT_PALETTE
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(category.trim()))
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| FALLBACK_COLOUR.to_string())
}

/// The first palette colour no goal uses yet (cycles when all are taken).
pub(crate) fn next_palette_colour(goals: &[TimeGoal]) -> &'static str {
    DEFAULT_PALETTE
        .iter()
        .map(|(_, c)| *c)
        .find(|c| !goals.iter().any(|g| g.colour.as_deref().is_some_and(|gc| gc.eq_ignore_ascii_case(c))))
        .unwrap_or(DEFAULT_PALETTE[goals.len() % DEFAULT_PALETTE.len()].1)
}

/// The day's share of the weekly goals, in minutes (rounded).
pub(crate) fn daily_share(goals: &[TimeGoal]) -> u32 {
    let week: u32 = goals.iter().map(|g| g.weekly_target_minutes).sum();
    (week + 3) / 7
}

/// The palette's category names, for [`categories::category_options`].
pub(crate) fn palette_names() -> [&'static str; 5] {
    DEFAULT_PALETTE.map(|(n, _)| n)
}

/// The one your-time category list (goals, then in use, then suggestions)
/// from the planner's in-memory state.
pub(crate) fn category_list(state: &PlannerState) -> Vec<categories::CategoryOption> {
    categories::category_options(
        &state.time_goals,
        &state.blocks,
        &palette_names(),
        categories::in_use_since(Utc::now()),
    )
}

/// Re-file every block and your-time to-do in `from` under `to`, then write
/// the touched rows through off the UI thread (P-017). Returns how many
/// blocks moved.
pub(crate) fn apply_recategorise(planner: &mut Signal<PlannerState>, from: &str, to: &str) -> usize {
    let (blocks, todos) = {
        let mut state = planner.write();
        let st = &mut *state;
        let r = categories::recategorise(&mut st.blocks, &mut st.todos, from, to, Utc::now());
        let blocks: Vec<TimeBlock> = st.blocks.iter().filter(|b| r.blocks.contains(&b.id)).cloned().collect();
        let todos: Vec<model::Todo> = st.todos.iter().filter(|t| r.todos.contains(&t.id)).cloned().collect();
        (blocks, todos)
    };
    let moved = blocks.len();
    if !blocks.is_empty() || !todos.is_empty() {
        tokio::task::spawn_blocking(move || {
            for b in &blocks {
                if let Err(e) = store::save_block(b) {
                    tracing::error!("planner: failed to save block {}: {}", b.id, e);
                }
            }
            for t in &todos {
                if let Err(e) = store::save_todo(t) {
                    tracing::error!("planner: failed to save todo {}: {}", t.id, e);
                }
            }
        });
    }
    moved
}

/// The weekly target a category gets when promoted to a goal from the
/// review (the goals editor's own default).
pub(crate) const PROMOTED_GOAL_MINUTES: u32 = 3 * 60;

/// Make an in-use category a goal: [`PROMOTED_GOAL_MINUTES`] a week, its
/// palette colour when that's free, else the next unused one. No-op when it
/// already is one.
pub(crate) fn promote_to_goal(planner: &mut Signal<PlannerState>, category: &str) {
    let goal = {
        let state = planner.peek();
        if state.time_goals.iter().any(|g| g.matches(category)) {
            return;
        }
        let palette = DEFAULT_PALETTE
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(category.trim()))
            .map(|(_, c)| *c)
            .filter(|c| {
                !state
                    .time_goals
                    .iter()
                    .any(|g| g.colour.as_deref().is_some_and(|gc| gc.eq_ignore_ascii_case(c)))
            });
        TimeGoal {
            colour: Some(palette.unwrap_or_else(|| next_palette_colour(&state.time_goals)).to_string()),
            sort_order: state.time_goals.iter().map(|g| g.sort_order).fold(0.0f64, f64::max) + 1.0,
            ..TimeGoal::new(category.trim(), PROMOTED_GOAL_MINUTES)
        }
    };
    persist_goal(&goal);
    planner.write().time_goals.push(goal);
}

/// Menu rows for the your-time category list: goals, then categories in use
/// without a goal, then suggestions, then a field to type a new one. Used by
/// the timeline's empty-slot menu, a selected block's category picker and
/// the review's "Merge into…".
#[component]
pub(crate) fn CategoryMenuItems(
    on_pick: EventHandler<String>,
    /// Ticked in the list (a block's current category).
    #[props(default)]
    current: Option<String>,
    /// Left out of the list (the category being merged away).
    #[props(default)]
    exclude: Option<String>,
    #[props(default = "New category\u{2026}".to_string())]
    placeholder: String,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut typed = use_signal(String::new);
    let (options, goals) = {
        let state = planner.read();
        (category_list(&state), state.time_goals.clone())
    };
    let same = |a: &str, b: &Option<String>| b.as_deref().is_some_and(|b| a.eq_ignore_ascii_case(b.trim()));
    let options: Vec<categories::CategoryOption> = options.into_iter().filter(|o| !same(&o.name, &exclude)).collect();
    let (main, suggestions): (Vec<_>, Vec<_>) = options
        .into_iter()
        .partition(|o| o.source != categories::CategorySource::Suggestion);
    let mut submit = move || {
        let raw = typed.peek().trim().to_string();
        if raw.is_empty() {
            return;
        }
        let name = {
            let state = planner.peek();
            categories::canonical(&state.time_goals, &state.blocks, &raw)
        };
        typed.set(String::new());
        on_pick.call(name);
    };
    rsx! {
        for o in main {
            CategoryMenuRow {
                key: "yt-{o.name}",
                colour: category_colour(&goals, &o.name),
                ticked: same(&o.name, &current),
                option: o.clone(),
                on_pick: on_pick,
            }
        }
        if !suggestions.is_empty() {
            div {
                class: "px-2.5 pt-1.5 pb-0.5 text-[10px] uppercase tracking-[0.08em] text-fg-muted",
                "Suggestions"
            }
        }
        for o in suggestions {
            CategoryMenuRow {
                key: "yt-{o.name}",
                colour: category_colour(&goals, &o.name),
                ticked: same(&o.name, &current),
                option: o.clone(),
                on_pick: on_pick,
            }
        }
        div {
            class: "px-2 pt-1 pb-1",
            input {
                class: "h-6 w-full rounded border border-faint bg-input px-2 text-[11px] text-fg outline-none",
                placeholder: "{placeholder}",
                value: "{typed}",
                onmousedown: move |evt| evt.stop_propagation(),
                onclick: move |evt| evt.stop_propagation(),
                oninput: move |e| typed.set(e.value()),
                onkeydown: move |e| {
                    // Keys typed here are text, not the rail's `[` / `]`.
                    e.stop_propagation();
                    if e.key() == Key::Enter {
                        submit();
                    }
                },
            }
        }
    }
}

#[component]
fn CategoryMenuRow(
    option: categories::CategoryOption,
    colour: String,
    ticked: bool,
    on_pick: EventHandler<String>,
) -> Element {
    let o = option;
    let suggestion = o.source == categories::CategorySource::Suggestion;
    rsx! {
        button {
            class: if suggestion {
                "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-fg-muted text-left hover:bg-input hover:text-fg"
            } else {
                "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-fg text-left hover:bg-input"
            },
            onclick: {
                let name = o.name.clone();
                move |evt: MouseEvent| {
                    evt.stop_propagation();
                    on_pick.call(name.clone());
                }
            },
            span { class: "h-2 w-2 shrink-0 rounded-full", style: "background: {colour};" }
            span { class: "flex-1 truncate", "{o.name}" }
            if ticked {
                Icon { width: 10, height: 10, icon: fi_icons::FiCheck }
            } else if let Some(m) = o.goal_minutes {
                span { class: "font-mono text-[10px] text-fg-muted", "{model::format_minutes(m)}/wk" }
            } else if o.source == categories::CategorySource::InUse {
                span { class: "font-mono text-[10px] text-fg-muted", "no goal" }
            }
        }
    }
}

/// One category chip in the readout.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct YourTimeChip {
    pub category: String,
    pub colour: String,
    pub minutes: u32,
    /// Every block of this category today has ended.
    pub done: bool,
}

/// "Your time today": protected minutes against the daily share.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct YourTimeToday {
    pub protected_minutes: u32,
    pub goal_minutes: u32,
    pub chips: Vec<YourTimeChip>,
}

impl YourTimeToday {
    /// `"of 3h · goal met"` / `"of 3h · 30m short"` / `"no goals set"`.
    pub fn goal_note(&self) -> String {
        if self.goal_minutes == 0 {
            return "no goals set".to_string();
        }
        let of = model::format_minutes(self.goal_minutes);
        if self.protected_minutes >= self.goal_minutes {
            format!("of {of} \u{b7} goal met")
        } else {
            format!(
                "of {of} \u{b7} {} short",
                model::format_minutes(self.goal_minutes - self.protected_minutes)
            )
        }
    }

    /// Bar segments as (colour, percent of the bar). The bar's scale is the
    /// larger of the goal and what's protected, so a met goal fills it.
    pub fn segments(&self) -> Vec<(String, f64)> {
        let scale = self.goal_minutes.max(self.protected_minutes).max(1) as f64;
        self.chips
            .iter()
            .map(|c| (c.colour.clone(), (c.minutes as f64 / scale * 1000.0).round() / 10.0))
            .collect()
    }
}

/// Build the readout for `date` from the day's protected blocks.
pub(crate) fn your_time_today(
    goals: &[TimeGoal],
    blocks: &[TimeBlock],
    date: NaiveDate,
    now: DateTime<Utc>,
) -> YourTimeToday {
    let (protected_minutes, on) = measures::protected_on(blocks, date);
    let mut chips: Vec<YourTimeChip> = Vec::new();
    for b in on {
        let Some(cat) = b.your_time_category() else { continue };
        let mins = (b.end - b.start).num_minutes().max(0) as u32;
        match chips.iter_mut().find(|c| c.category.eq_ignore_ascii_case(cat.trim())) {
            Some(c) => {
                c.minutes += mins;
                c.done &= b.end <= now;
            }
            None => chips.push(YourTimeChip {
                category: cat.trim().to_string(),
                colour: category_colour(goals, cat),
                minutes: mins,
                done: b.end <= now,
            }),
        }
    }
    YourTimeToday {
        protected_minutes,
        goal_minutes: daily_share(goals),
        chips,
    }
}

/// The "Your time today" header readout.
#[component]
pub(crate) fn YourTimeReadout(on_edit_goals: EventHandler<()>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let readout = {
        let state = planner.read();
        your_time_today(&state.time_goals, &state.blocks, super::today(), Utc::now())
    };
    let note = readout.goal_note();
    let label = format!("{} protected", model::format_minutes(readout.protected_minutes));
    let segments = readout.segments();
    let no_goals = readout.goal_minutes == 0;
    rsx! {
        div {
            class: "flex w-[330px] max-w-full shrink-0 flex-col gap-1.5 rounded-lg border border-primary-700/80 bg-section px-3 py-1.5",
            div {
                class: "flex items-baseline gap-2",
                span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Your time today" }
                div { class: "flex-1" }
                span { class: "font-mono text-[11px] tabular-nums text-fg", "{label}" }
                if no_goals {
                    button {
                        class: "font-mono text-[10px] text-primary-300 hover:text-fg",
                        title: "Set weekly your-time goals",
                        onclick: move |_| on_edit_goals.call(()),
                        "set goals"
                    }
                } else {
                    span { class: "font-mono text-[10px] tabular-nums text-fg-muted", "{note}" }
                }
            }
            div {
                class: "flex h-[5px] overflow-hidden rounded-full bg-input",
                for (i, (colour, pct)) in segments.into_iter().enumerate() {
                    span { key: "{i}", style: "width: {pct}%; background: {colour};" }
                }
            }
            if readout.chips.is_empty() {
                span { class: "text-[10.5px] text-fg-muted truncate",
                    "Nothing protected yet \u{2014} click an empty slot on the timeline \u{2192} Your time"
                }
            } else {
                div {
                    class: "flex flex-wrap gap-1",
                    for c in readout.chips.iter() {
                        span {
                            key: "{c.category}",
                            class: "inline-flex h-[18px] items-center gap-1 rounded-full border border-faint px-1.5 text-[10.5px] text-fg whitespace-nowrap",
                            span { class: "h-[7px] w-[7px] shrink-0 rounded-full", style: "background: {c.colour};" }
                            if c.done {
                                "{c.category} {model::format_minutes(c.minutes)} \u{b7} done"
                            } else {
                                "{c.category} {model::format_minutes(c.minutes)}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Parse an hours field ("4", "2.5", "1h 30m", "90m") into minutes.
pub(crate) fn parse_weekly_target(raw: &str) -> Option<u32> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Ok(h) = s.parse::<f64>() {
        return (0.0..=168.0).contains(&h).then(|| (h * 60.0).round() as u32);
    }
    let mut total = 0u32;
    let mut any = false;
    for part in s.split_whitespace() {
        if let Some(h) = part.strip_suffix('h') {
            total += (h.parse::<f64>().ok()? * 60.0).round() as u32;
            any = true;
        } else if let Some(m) = part.strip_suffix('m') {
            total += m.parse::<u32>().ok()?;
            any = true;
        } else {
            return None;
        }
    }
    (any && total <= 168 * 60).then_some(total)
}

/// Hours as the editor shows them: `240` → `"4"`, `150` → `"2.5"`.
pub(crate) fn hours_field(minutes: u32) -> String {
    if minutes.is_multiple_of(60) {
        (minutes / 60).to_string()
    } else {
        let h = minutes as f64 / 60.0;
        let s = format!("{h:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn persist_goal(goal: &TimeGoal) {
    if let Err(e) = store::save_time_goal(goal) {
        tracing::error!("planner: failed to save time goal {}: {}", goal.id, e);
    }
}

/// Add, edit and remove your-time categories: name, weekly target, colour.
/// Every change persists at once (single-row writes, like the rest of the
/// Planner).
#[component]
pub(crate) fn TimeGoalsEditor(on_close: EventHandler<()>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut new_name = use_signal(String::new);
    let mut new_hours = use_signal(|| "3".to_string());
    let mut new_colour = use_signal(|| Option::<String>::None);

    let mut goals: Vec<TimeGoal> = planner.read().time_goals.clone();
    goals.sort_by(|a, b| a.sort_order.partial_cmp(&b.sort_order).unwrap_or(std::cmp::Ordering::Equal));
    let suggested = next_palette_colour(&goals).to_string();
    let chosen = new_colour.read().clone().unwrap_or_else(|| suggested.clone());
    // Categories already on blocks without a goal (Break, say) come first:
    // anything Hobbes filed time under can be promoted to a goal here.
    let in_use_names: Vec<String> = {
        let state = planner.read();
        categories::in_use_without_goal(&state.time_goals, &state.blocks, categories::in_use_since(chrono::Utc::now()))
    };
    let unused_names: Vec<&str> = DEFAULT_PALETTE
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !goals.iter().any(|g| g.matches(n)))
        .filter(|n| !in_use_names.iter().any(|u| u.eq_ignore_ascii_case(n)))
        .collect();

    let mut add = move || {
        let name = {
            let state = planner.peek();
            categories::canonical(&state.time_goals, &state.blocks, &new_name.peek())
        };
        if name.is_empty() {
            return;
        }
        let Some(minutes) = parse_weekly_target(&new_hours.peek()) else { return };
        if planner.peek().time_goals.iter().any(|g| g.matches(&name)) {
            return;
        }
        let colour = new_colour.peek().clone().unwrap_or_else(|| next_palette_colour(&planner.peek().time_goals).to_string());
        let sort_order = planner.peek().time_goals.iter().map(|g| g.sort_order).fold(0.0f64, f64::max) + 1.0;
        let goal = TimeGoal {
            colour: Some(colour),
            sort_order,
            ..TimeGoal::new(name, minutes)
        };
        persist_goal(&goal);
        planner.write().time_goals.push(goal);
        new_name.set(String::new());
        new_colour.set(None);
    };

    rsx! {
        div {
            class: "flex flex-col gap-2 rounded-lg border border-subtle bg-card p-3",
            div {
                class: "flex items-center gap-2",
                span { class: "text-[11px] uppercase tracking-[0.08em] text-fg-muted", "Your-time goals" }
                span { class: "text-[11px] text-fg-muted", "\u{b7} hours a week, per category" }
                div { class: "flex-1" }
                button {
                    class: "rounded p-0.5 text-fg-muted hover:bg-input hover:text-fg",
                    title: "Close",
                    onclick: move |_| on_close.call(()),
                    Icon { width: 12, height: 12, icon: fi_icons::FiX }
                }
            }
            for g in goals.iter().cloned() {
                GoalRow { key: "{g.id}", goal: g.clone() }
            }
            if goals.is_empty() {
                p { class: "text-[11px] text-fg-muted",
                    "What is the rest of the week for? Add the things you want time for \u{2014} protected blocks on the timeline count toward them."
                }
            }
            div {
                class: "flex flex-wrap items-center gap-2 border-t border-faint pt-2",
                input {
                    class: "h-7 min-w-0 flex-1 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                    placeholder: "New category (Reading, Outside\u{2026})",
                    value: "{new_name}",
                    oninput: move |e| new_name.set(e.value()),
                    onkeydown: move |e| if e.key() == Key::Enter { add() },
                }
                input {
                    class: "h-7 w-14 rounded border border-faint bg-input px-2 text-right font-mono text-xs text-fg outline-none",
                    title: "Hours a week",
                    value: "{new_hours}",
                    oninput: move |e| new_hours.set(e.value()),
                    onkeydown: move |e| if e.key() == Key::Enter { add() },
                }
                span { class: "text-[11px] text-fg-muted", "h / week" }
                div {
                    class: "flex items-center gap-1",
                    for (_, c) in DEFAULT_PALETTE.iter() {
                        button {
                            key: "{c}",
                            class: if chosen.eq_ignore_ascii_case(c) {
                                "h-4 w-4 rounded-full ring-2 ring-primary-300"
                            } else {
                                "h-4 w-4 rounded-full ring-1 ring-black/20 hover:ring-primary-400"
                            },
                            style: "background: {c};",
                            title: "{c}",
                            onclick: {
                                let c = c.to_string();
                                move |_| new_colour.set(Some(c.clone()))
                            },
                        }
                    }
                }
                button {
                    class: "text-[11px] rounded px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover shrink-0",
                    onclick: move |_| add(),
                    "Add"
                }
            }
            if !in_use_names.is_empty() || !unused_names.is_empty() {
                div {
                    class: "flex flex-wrap items-center gap-1",
                    if !in_use_names.is_empty() {
                        span { class: "text-[10.5px] text-fg-muted", title: "On your blocks, without a goal yet", "In use:" }
                    }
                    for n in in_use_names.iter().cloned() {
                        button {
                            key: "in-use-{n}",
                            class: "inline-flex h-[18px] items-center gap-1 rounded-full border border-faint px-1.5 text-[10.5px] text-fg hover:text-fg",
                            title: "Make {n} a goal",
                            onclick: {
                                let name = n.clone();
                                move |_| {
                                    let colour = DEFAULT_PALETTE
                                        .iter()
                                        .find(|(p, _)| p.eq_ignore_ascii_case(&name))
                                        .map(|(_, c)| c.to_string());
                                    new_name.set(name.clone());
                                    new_colour.set(colour);
                                }
                            },
                            span {
                                class: "h-[7px] w-[7px] rounded-full",
                                style: "background: {category_colour(&[], &n)};",
                            }
                            "{n}"
                        }
                    }
                    if !unused_names.is_empty() {
                        span { class: "text-[10.5px] text-fg-muted", "Suggestions:" }
                    }
                    for n in unused_names {
                        button {
                            key: "{n}",
                            class: "inline-flex h-[18px] items-center gap-1 rounded-full border border-faint px-1.5 text-[10.5px] text-fg-muted hover:text-fg",
                            onclick: {
                                let name = n.to_string();
                                move |_| {
                                    let colour = DEFAULT_PALETTE.iter().find(|(p, _)| *p == name).map(|(_, c)| c.to_string());
                                    new_name.set(name.clone());
                                    new_colour.set(colour);
                                }
                            },
                            span {
                                class: "h-[7px] w-[7px] rounded-full",
                                style: "background: {category_colour(&[], n)};",
                            }
                            "{n}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn GoalRow(goal: TimeGoal) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut hours = use_signal(|| hours_field(goal.weekly_target_minutes));
    let mut name = use_signal(|| goal.category.clone());
    let mut colour_open = use_signal(|| false);
    let colour = category_colour(&[goal.clone()], &goal.category);
    let id = goal.id.clone();

    let mut update = move |id: &str, f: &dyn Fn(&mut TimeGoal)| {
        let saved = {
            let mut state = planner.write();
            state.time_goals.iter_mut().find(|g| g.id == id).map(|g| {
                f(g);
                g.clone()
            })
        };
        if let Some(g) = saved {
            persist_goal(&g);
        }
    };

    rsx! {
        div {
            class: "relative flex items-center gap-2",
            button {
                class: "h-4 w-4 shrink-0 rounded-full ring-1 ring-black/20 hover:ring-primary-400",
                style: "background: {colour};",
                title: "Change colour",
                onclick: move |_| colour_open.toggle(),
            }
            input {
                class: "h-6 min-w-0 flex-1 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                title: "Rename \u{2014} its blocks move with it",
                value: "{name}",
                oninput: move |e| name.set(e.value()),
                onchange: {
                    let id = id.clone();
                    move |e: FormEvent| {
                        let to = e.value().trim().to_string();
                        let renamed = {
                            let mut state = planner.write();
                            let Some(from) = state.time_goals.iter().find(|g| g.id == id).map(|g| g.category.clone()) else {
                                return;
                            };
                            if from == to {
                                return;
                            }
                            categories::rename_goal(&mut state.time_goals, &from, &to).map(|g| (from, g))
                        };
                        match renamed {
                            Ok((from, Some(g))) => {
                                persist_goal(&g);
                                apply_recategorise(&mut planner, &from, &g.category);
                                name.set(g.category.clone());
                            }
                            Ok((_, None)) => {}
                            Err(_) => {
                                // Empty, or another goal's name: put the old one back.
                                let old = planner.peek().time_goals.iter().find(|g| g.id == id).map(|g| g.category.clone());
                                if let Some(old) = old {
                                    name.set(old);
                                }
                            }
                        }
                    }
                },
            }
            input {
                class: "h-6 w-14 rounded border border-faint bg-input px-2 text-right font-mono text-xs text-fg outline-none",
                title: "Hours a week",
                value: "{hours}",
                oninput: move |e| hours.set(e.value()),
                onchange: {
                    let id = id.clone();
                    move |e: FormEvent| {
                        if let Some(m) = parse_weekly_target(&e.value()) {
                            update(&id, &|g| g.weekly_target_minutes = m);
                        }
                    }
                },
            }
            span { class: "w-14 text-[11px] text-fg-muted", "h / week" }
            button {
                class: "rounded p-0.5 text-fg-muted hover:bg-input hover:text-red-300",
                title: "Remove this category (its blocks stay on the timeline)",
                onclick: {
                    let id = id.clone();
                    move |_| {
                        if let Err(e) = store::delete_time_goal(&id) {
                            tracing::error!("planner: failed to delete time goal {}: {}", id, e);
                        }
                        planner.write().time_goals.retain(|g| g.id != id);
                    }
                },
                Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
            }
            if *colour_open.read() {
                div {
                    class: "fixed inset-0 z-40",
                    onclick: move |_| colour_open.set(false),
                }
                div {
                    class: "absolute left-0 top-full z-50 mt-1 flex gap-1 rounded-md border border-subtle bg-card p-1.5 shadow-lg",
                    for (_, c) in DEFAULT_PALETTE.iter() {
                        button {
                            key: "{c}",
                            class: "h-4 w-4 rounded-full ring-1 ring-black/20 hover:ring-primary-400",
                            style: "background: {c};",
                            onclick: {
                                let c = c.to_string();
                                let id = id.clone();
                                move |_| {
                                    let c = c.clone();
                                    update(&id, &move |g| g.colour = Some(c.clone()));
                                    colour_open.set(false);
                                }
                            },
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
    use crate::todo::model::BlockSource;

    fn at(d: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        crate::components::planner_timeline::local_minutes_to_utc(d, h * 60 + m)
    }

    fn yt(cat: &str, d: NaiveDate, h1: u32, h2: u32) -> TimeBlock {
        TimeBlock {
            id: format!("{cat}-{h1}"),
            todo_id: None,
            title: cat.into(),
            start: at(d, h1, 0),
            end: at(d, h2, 0),
            source: BlockSource::YourTime { category: cat.into() },
        }
    }

    #[test]
    fn palette_text_colours_match_the_design() {
        let dark = |name: &str| text_on(&category_colour(&[], name)) == "#141414";
        assert!(dark("Reading"));
        assert!(dark("Outside"));
        assert!(dark("Experimenting"));
        assert!(!dark("Gaming"));
        assert!(!dark("Shows & movies"));
    }

    #[test]
    fn goal_colour_beats_palette_and_unknown_falls_back() {
        let g = TimeGoal { colour: Some("#2C7A71".into()), ..TimeGoal::new("Reading", 60) };
        assert_eq!(category_colour(&[g], "reading"), "#2C7A71");
        assert_eq!(category_colour(&[], "Knitting"), FALLBACK_COLOUR);
        let used = TimeGoal { colour: Some("#E2BCC3".into()), ..TimeGoal::new("Reading", 60) };
        assert_eq!(next_palette_colour(&[used]), "#8DBB7A");
    }

    #[test]
    fn readout_measures_against_the_daily_share() {
        let d: NaiveDate = "2026-10-07".parse().unwrap();
        let goals = vec![TimeGoal::new("Outside", 7 * 60), TimeGoal::new("Gaming", 7 * 60)];
        assert_eq!(daily_share(&goals), 120);
        let blocks = vec![yt("Outside", d, 12, 13), yt("Gaming", d, 20, 21), yt("Gaming", d, 21, 21)];
        let now = at(d, 14, 0);
        let r = your_time_today(&goals, &blocks, d, now);
        assert_eq!(r.protected_minutes, 120);
        assert_eq!(r.goal_minutes, 120);
        assert_eq!(r.goal_note(), "of 2h \u{b7} goal met");
        assert_eq!(r.chips.len(), 2);
        assert!(r.chips[0].done, "the walk is over");
        assert!(!r.chips[1].done);
        let segs = r.segments();
        assert_eq!(segs[0].1, 50.0);

        let short = your_time_today(&goals, &blocks[..1], d, now);
        assert_eq!(short.goal_note(), "of 2h \u{b7} 1h short");
        assert_eq!(your_time_today(&[], &[], d, now).goal_note(), "no goals set");
    }

    #[test]
    fn weekly_target_field_parses_hours_and_units() {
        assert_eq!(parse_weekly_target("4"), Some(240));
        assert_eq!(parse_weekly_target("2.5"), Some(150));
        assert_eq!(parse_weekly_target("1h 30m"), Some(90));
        assert_eq!(parse_weekly_target("45m"), Some(45));
        assert_eq!(parse_weekly_target(""), None);
        assert_eq!(parse_weekly_target("lots"), None);
        assert_eq!(hours_field(240), "4");
        assert_eq!(hours_field(150), "2.5");
    }
}
