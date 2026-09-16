//! Planner centre column: the Things-style todo list surfaces.
//!
//! Split out of `planner_view.rs` (P-split for concurrent editing) — keeps
//! `CentreColumn`, `QuickAdd`, `TodoList`, and `TodoRow`, plus the pure
//! helpers only they use. Root types (`PlannerSelection`, persistence
//! helpers, `today()`) and timeline geometry live in the sibling modules
//! `planner_view` and `planner_timeline`.
//!
//! The row is a three-column grid — WHO (status/actor), WHAT (title, project,
//! carrier session), WHEN (estimate, timebox, due, schedule) — so a scan down
//! the list answers the three questions in fixed places instead of reading a
//! ragged chip stream.

use chrono::{DateTime, Local, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::fleet::{FleetOrigin, FleetSession, FleetStatus};
use crate::settings::Settings;
use crate::todo::model::{
    self, BlockSource, FocusActor, FocusSession, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoStatus,
};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

use super::planner_timeline::{fmt_hhmm, local_minutes_to_utc};
use super::planner_view::{
    mutate_todo, persist_block, persist_focus_sessions, persist_todo, today, PlannerSelection,
    SelectedBlockContext,
};

// ── Pure helpers (unit-tested at the bottom) ────────────────────────────────

fn cycle_estimate(current: Option<u32>) -> Option<u32> {
    const LADDER: [u32; 4] = [15, 30, 60, 120];
    match current {
        None => Some(15),
        Some(m) => match LADDER.iter().position(|&step| step == m) {
            Some(pos) => LADDER.get(pos + 1).copied(),
            // min_by_key breaks distance ties on Reverse(step): the larger
            // step wins, which is the ties-round-up rule.
            None => LADDER
                .iter()
                .min_by_key(|&&step| (m.abs_diff(step), std::cmp::Reverse(step)))
                .copied(),
        },
    }
}

/// The draft seeded into a fresh chat when a todo is activated. Speaks in the
/// user's voice (it is their message to edit), carries the id so the AI can
/// drive HOBBES_TODO_UPDATE, and ends mid-sentence so the caret invites the
/// details.
fn activation_seed(todo: &Todo) -> String {
    let mut meta: Vec<String> = Vec::new();
    if let Some(m) = todo.estimate_minutes {
        meta.push(model::format_minutes(m));
    }
    if let Some(d) = todo.deadline {
        meta.push(format!("due {}", d.format("%-d %b")));
    }
    for tag in &todo.tags {
        meta.push(format!("#{}", tag));
    }
    let meta = if meta.is_empty() {
        String::new()
    } else {
        format!(" ({})", meta.join(", "))
    };
    format!(
        "Let's work on my to-do \"{}\"{} — todo id {}.\n\nDetails: ",
        todo.title, meta, todo.id
    )
}

/// Who is on this todo right now — the WHO column's whole vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WhoState {
    /// Nobody: an open circle that completes on click.
    Open,
    /// You, in a person focus session.
    YouFocusing,
    /// A Hobbes chat tab, carrying its profile initial and swatch.
    Hobbes { initial: String, color: String },
    /// A coding-agent (non-Hobbes) fleet session.
    FleetAgent,
    /// The carrier session is blocked on an approval or a question.
    NeedsYou,
    /// Closed work.
    Done,
}

/// Resolve the WHO state from the three inputs the row can see.
///
/// Precedence, highest first: **done** (closed work is closed, whatever a
/// stale link says) → **needs you** (a blocked carrier outranks the work it
/// is blocked in) → **you focusing** (you pressed Start; that is the most
/// immediate truth about the row) → **Hobbes tab** → **fleet agent** →
/// **open**.
///
/// `profile` is the linked Hobbes tab's `(initial, bg-class)` swatch; when it
/// cannot be resolved the avatar falls back to the default profile colour.
fn who_state(
    todo: &Todo,
    focus: Option<&FocusSession>,
    fleet: Option<&FleetSession>,
    profile: Option<(String, String)>,
) -> WhoState {
    if todo.status.is_closed() {
        return WhoState::Done;
    }
    if fleet.is_some_and(|f| f.status.needs_attention()) {
        return WhoState::NeedsYou;
    }

    let live_focus = focus.filter(|f| f.is_open() && todo.status == TodoStatus::InProgress);
    if live_focus.is_some_and(|f| matches!(f.actor, FocusActor::Person)) {
        return WhoState::YouFocusing;
    }

    let fleet_working = fleet.is_some_and(|f| {
        matches!(
            f.status,
            FleetStatus::Working | FleetStatus::WorkingBackground
        )
    });
    let agent_focus = live_focus.is_some_and(|f| matches!(f.actor, FocusActor::Agent { .. }));
    let hobbes_carrier = fleet.is_some_and(|f| f.origin == FleetOrigin::Hobbes);

    if agent_focus || (fleet_working && hobbes_carrier) {
        let (initial, color) = profile
            .unwrap_or_else(|| ("H".to_string(), DEFAULT_PROFILE_COLOR.to_string()));
        return WhoState::Hobbes { initial, color };
    }
    if fleet_working {
        return WhoState::FleetAgent;
    }
    WhoState::Open
}

/// How loudly a deadline should read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DueTone {
    Neutral,
    /// Due today, tomorrow, or the day after.
    Soon,
    Overdue,
}

fn due_tone(due: NaiveDate, today: NaiveDate) -> DueTone {
    if due < today {
        DueTone::Overdue
    } else if (due - today).num_days() <= 2 {
        DueTone::Soon
    } else {
        DueTone::Neutral
    }
}

fn due_tone_class(tone: DueTone) -> &'static str {
    match tone {
        DueTone::Neutral => "border-faint text-fg-muted",
        DueTone::Soon => "border-yellow-700 text-yellow-300",
        DueTone::Overdue => "border-red-700 text-red-400",
    }
}

/// The date pill's text. `None` when the todo has no scheduled day — the pill
/// is the *date's* control, so an undated todo simply doesn't wear one.
///
/// "Evening" wins only on today: on any other day the day itself is the more
/// useful word, and the evening grouping is visible from its section header.
fn date_pill_label(scheduled_for: Option<NaiveDate>, evening: bool, today: NaiveDate) -> Option<String> {
    let d = scheduled_for?;
    if evening && d == today {
        return Some("Evening".to_string());
    }
    if d == today {
        return Some("Today".to_string());
    }
    if Some(d) == today.succ_opt() {
        return Some("Tomorrow".to_string());
    }
    Some(d.format("%a %-d").to_string())
}

/// One "when" the schedule popover (and the detail card's date field) can set.
///
/// Every one of these is a three-step mutation — patch the todo, prune the
/// blocks that no longer sit on its day, delete those block rows — which used
/// to be hand-rolled at each call site. [`apply_schedule_choice`] is the pure
/// patch; [`set_schedule`] is the whole sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleChoice {
    Today,
    ThisEvening,
    Tomorrow,
    On(NaiveDate),
    /// Drop the date without touching the bucket — the detail card's "Clear".
    ClearDate,
    /// Undated and triaged.
    Anytime,
    Someday,
}

pub(crate) fn apply_schedule_choice(t: &mut Todo, choice: ScheduleChoice, today: NaiveDate) {
    match choice {
        ScheduleChoice::Today => {
            t.scheduled_for = Some(today);
            t.time_of_day = None;
        }
        // Evening is a time-of-day, not a reschedule: a dated todo keeps its
        // day (yanking an Upcoming todo onto today was the U1.4 bug); only an
        // undated one gets scheduled today.
        ScheduleChoice::ThisEvening => {
            t.time_of_day = Some(TimeOfDay::Evening);
            if t.scheduled_for.is_none() {
                t.scheduled_for = Some(today);
            }
        }
        ScheduleChoice::Tomorrow => {
            t.scheduled_for = today.succ_opt();
            t.time_of_day = None;
        }
        // A picked day leaves the time-of-day grouping alone (the detail
        // card's date field has always behaved this way).
        ScheduleChoice::On(d) => t.scheduled_for = Some(d),
        // An undated todo has no day for a time-of-day group to belong to.
        ScheduleChoice::ClearDate => {
            t.scheduled_for = None;
            t.time_of_day = None;
        }
        ScheduleChoice::Anytime => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Anytime;
        }
        ScheduleChoice::Someday => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Someday;
        }
    }
}

/// Which popover entry wears the tick.
fn current_schedule_choice(todo: &Todo, today: NaiveDate) -> ScheduleChoice {
    if todo.bucket == TodoBucket::Someday {
        return ScheduleChoice::Someday;
    }
    match todo.scheduled_for {
        Some(d) if d == today && todo.time_of_day == Some(TimeOfDay::Evening) => {
            ScheduleChoice::ThisEvening
        }
        Some(d) if d == today => ScheduleChoice::Today,
        Some(d) if Some(d) == today.succ_opt() => ScheduleChoice::Tomorrow,
        Some(d) => ScheduleChoice::On(d),
        None => ScheduleChoice::Anytime,
    }
}

/// The whole schedule mutation: patch, prune the now-orphaned blocks, delete
/// their rows. The single owner of the sequence — the row popover, the detail
/// card and anything else that moves a todo's day all go through here (the
/// AI's update and plan-day handlers run the same rule server-side).
pub(crate) fn set_schedule(
    mut planner: Signal<PlannerState>,
    id: &str,
    choice: ScheduleChoice,
    today: NaiveDate,
) {
    mutate_todo(planner, id, |t| apply_schedule_choice(t, choice, today));
    let keep = planner.peek().todo(id).and_then(|t| t.scheduled_for);
    let pruned = planner.write().prune_blocks_for_todo(id, keep);
    for b in &pruned {
        if let Err(e) = store::delete_block(&b.id) {
            tracing::error!("planner: failed to delete rescheduled block {}: {}", b.id, e);
        }
    }
}

/// The left-edge stripe matching a profile swatch. Tailwind only sees class
/// literals, so the derived names are enumerated here for the scanner:
///
/// border-l-blue-600 border-l-purple-600 border-l-green-600 border-l-red-600
/// border-l-orange-600 border-l-pink-600 border-l-teal-600
///
/// ...and the matching pill borders (already emitted for the chat bar's
/// profile pill, repeated so this module stands on its own):
///
/// border-blue-600 border-purple-600 border-green-600 border-red-600
/// border-orange-600 border-pink-600 border-teal-600
fn profile_stripe_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-l-{}", rest),
        _ => "border-l-blue-600".to_string(),
    }
}

fn profile_pill_border_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-{}", rest),
        _ => "border-faint".to_string(),
    }
}

/// Minutes the carrier has been working, for the session pill's tail.
fn working_minutes(
    fleet: Option<&FleetSession>,
    focus: Option<&FocusSession>,
    now: DateTime<Utc>,
) -> Option<u32> {
    let started = focus
        .filter(|f| f.is_open())
        .map(|f| f.started_at)
        .or_else(|| fleet.and_then(|f| f.working_since))?;
    Some((now - started).num_minutes().max(0) as u32)
}

fn view_title(view: TodoView) -> &'static str {
    match view {
        TodoView::Inbox => "Inbox",
        TodoView::Today => "Today",
        TodoView::Upcoming => "Upcoming",
        TodoView::Anytime => "Anytime",
        TodoView::Someday => "Someday",
        TodoView::Logbook => "Logbook",
    }
}

fn empty_state_line(selection: &PlannerSelection) -> &'static str {
    match selection {
        PlannerSelection::View(TodoView::Inbox) => "Nothing to triage.",
        PlannerSelection::View(TodoView::Today) => "Nothing planned for today.",
        PlannerSelection::View(TodoView::Upcoming) => "Nothing scheduled ahead.",
        PlannerSelection::View(TodoView::Anytime) => "Nothing queued.",
        PlannerSelection::View(TodoView::Someday) => "Nothing on the back burner.",
        PlannerSelection::View(TodoView::Logbook) => "Nothing finished yet.",
        PlannerSelection::Project(_) => "No todos in this project yet.",
    }
}

/// Todos for the current selection, ordered for display.
fn todos_for_selection(state: &PlannerState, selection: &PlannerSelection, day: NaiveDate) -> Vec<Todo> {
    match selection {
        PlannerSelection::View(view) => views::in_view(&state.todos, *view, day)
            .into_iter()
            .cloned()
            .collect(),
        PlannerSelection::Project(id) => {
            let mut out: Vec<Todo> = state
                .todos
                .iter()
                .filter(|t| !t.status.is_closed() && t.project_id.as_deref() == Some(id))
                .cloned()
                .collect();
            out.sort_by(|a, b| {
                a.sort_order
                    .partial_cmp(&b.sort_order)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });
            out
        }
    }
}

// ── Shared class vocabulary ────────────────────────────────────────────────

/// Mirrors `default_profile_color()` in `settings.rs` (private there).
const DEFAULT_PROFILE_COLOR: &str = "bg-blue-600";

const PILL: &str = "h-[22px] inline-flex items-center rounded-full border border-faint px-2 text-[11px] text-fg-muted gap-1.5 shrink-0";
const META: &str = "font-mono text-[11px] tabular-nums text-fg-muted shrink-0";
const BTN_PRIMARY: &str =
    "text-[11px] rounded px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover shrink-0";
const BTN_SECONDARY: &str =
    "text-[11px] rounded px-2 py-0.5 border border-faint text-fg hover:bg-input shrink-0";
const POPOVER: &str = "absolute right-0 top-full mt-1 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 py-1";
const MENU_ITEM: &str =
    "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-fg text-left hover:bg-input";
const SECTION_HEADER: &str =
    "text-[11px] uppercase tracking-[0.08em] text-fg-muted px-2.5 pt-3.5 pb-1";

/// Which of a row's popovers is open. Exactly one at a time, so a menu never
/// stacks on top of another.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowMenu {
    Project,
    Date,
    Hand,
    More,
}

// ── Centre column ───────────────────────────────────────────────────────────

#[component]
pub(crate) fn CentreColumn(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let sel = selection.read().clone();

    let title = match &sel {
        PlannerSelection::View(v) => view_title(*v).to_string(),
        PlannerSelection::Project(id) => planner
            .read()
            .projects
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.title.clone())
            .unwrap_or_else(|| "Project".to_string()),
    };
    let is_today_view = sel == PlannerSelection::View(TodoView::Today);
    let subtitle = is_today_view.then(|| Local::now().format("%A, %-d %B").to_string());

    // The day's numbers, in the same words the rail's capacity bar uses — the
    // centre column should not make you look sideways to know if today fits.
    let capacity_line = is_today_view.then(|| {
        let (default_capacity, meetings_count) = {
            let s = settings.read();
            (
                s.planner_daily_capacity_minutes,
                s.planner_calendar_counts_against_capacity,
            )
        };
        let state = planner.read();
        let day = today();
        model::measure_capacity_full(
            &state.todos,
            &state.blocks,
            &state.focus_sessions,
            day,
            state.capacity_for(day, default_capacity),
            meetings_count,
        )
        .summary()
    });

    // Quick-adding into the logbook would resurrect nothing — hide it there.
    let show_quick_add = sel != PlannerSelection::View(TodoView::Logbook);

    rsx! {
        div {
            class: "flex-1 min-w-0 flex flex-col overflow-y-auto",
            div {
                class: "px-6 pt-6 pb-2",
                div {
                    class: "flex items-baseline gap-3",
                    h1 { class: "text-xl font-semibold", "{title}" }
                    if let Some(date_line) = subtitle {
                        span { class: "text-sm text-fg-muted", "{date_line}" }
                    }
                    div { class: "flex-1" }
                    if let Some(line) = capacity_line {
                        span { class: "{META} text-right", "{line}" }
                    }
                }
            }
            if show_quick_add {
                QuickAdd { selection }
            }
            TodoList { selection }
            // Breathing room so hover menus on the last row aren't clipped.
            div { class: "h-16 shrink-0" }
        }
    }
}

#[component]
fn QuickAdd(selection: Signal<PlannerSelection>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut draft = use_signal(String::new);

    let mut submit = move |_| {
        let day = today();
        let parsed = crate::todo::quick_add::parse_quick_add(&draft.peek(), day);
        if parsed.title.is_empty() {
            return; // tokens without a title aren't a todo yet
        }

        let mut todo = Todo::new(parsed.title, planner.peek().next_sort_order());
        // New todos land in the context being looked at, so quick-add never
        // makes work vanish into a different list.
        match selection.peek().clone() {
            // Scheduled todos are born triaged (Anytime, like block-created
            // ones): clearing the date later must not dump them into Inbox.
            PlannerSelection::View(TodoView::Today) => {
                todo.scheduled_for = Some(day);
                todo.bucket = TodoBucket::Anytime;
            }
            PlannerSelection::View(TodoView::Upcoming) => {
                todo.scheduled_for = day.succ_opt();
                todo.bucket = TodoBucket::Anytime;
            }
            PlannerSelection::View(TodoView::Anytime) => todo.bucket = TodoBucket::Anytime,
            PlannerSelection::View(TodoView::Someday) => todo.bucket = TodoBucket::Someday,
            PlannerSelection::Project(id) => {
                todo.project_id = Some(id);
                todo.bucket = TodoBucket::Anytime;
            }
            _ => {} // Inbox (and Logbook, which hides quick-add)
        }

        // A *day token overrides the view-context date — the user said when.
        // Born triaged (Anytime) like every other scheduled birth, so clearing
        // the date later never dumps it into Inbox.
        if let Some(d) = parsed.scheduled {
            todo.scheduled_for = Some(d);
            todo.bucket = TodoBucket::Anytime;
        }

        // Tokens beat defaults; defaults beat nothing.
        let default_estimate = settings.peek().planner_default_estimate_minutes;
        todo.estimate_minutes = parsed
            .estimate_minutes
            .or((default_estimate > 0).then_some(default_estimate));
        todo.tags = parsed.tags;
        todo.deadline = parsed.deadline;
        todo.time_of_day = parsed.time_of_day;

        // An @clock token also implies a time-of-day group when none was given
        // explicitly, so "@7pm" lands in This Evening without a second token.
        if todo.time_of_day.is_none() {
            todo.time_of_day = parsed.block_start.map(|m| match m / 60 {
                0..=11 => TimeOfDay::Morning,
                12..=16 => TimeOfDay::Afternoon,
                _ => TimeOfDay::Evening,
            });
        }

        // An @clock token means "this happens at that time": it implies a
        // scheduled day (today unless the view already picked one) and puts a
        // linked block on that day's timeline.
        let block_date = parsed.block_start.map(|_| {
            let d = todo.scheduled_for.unwrap_or(day);
            todo.scheduled_for = Some(d);
            if todo.bucket == TodoBucket::Inbox {
                todo.bucket = TodoBucket::Anytime;
            }
            d
        });

        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save new todo: {}", e);
        }

        let block = match (block_date, parsed.block_start) {
            (Some(d), Some(start)) => {
                let duration = todo
                    .estimate_minutes
                    .unwrap_or(settings.peek().planner_default_block_minutes)
                    .max(15);
                let block = TimeBlock {
                    id: uuid::Uuid::new_v4().to_string(),
                    todo_id: Some(todo.id.clone()),
                    title: todo.title.clone(),
                    start: local_minutes_to_utc(d, start),
                    end: local_minutes_to_utc(d, start + duration),
                    source: BlockSource::Manual,
                };
                persist_block(&block);
                Some(block)
            }
            _ => None,
        };

        {
            let mut state = planner.write();
            state.upsert_todo(todo);
            if let Some(b) = block {
                state.blocks.push(b);
            }
        }
        draft.set(String::new());
        // The input stays mounted, so focus is retained across submits.
    };

    // Parsed inline on every keystroke (P-012: draft is a Signal, so this
    // tracks). The chips show what Enter will actually do — the premium tell
    // of Things/Todoist quick entry is that tokens are confirmed before commit.
    let day = today();
    let preview = crate::todo::quick_add::parse_quick_add(&draft.read(), day);
    let timeline_hint = preview
        .block_start
        .map(|m| format!("Enter to add · goes on the timeline at {}", fmt_hhmm(m)));

    rsx! {
        div {
            class: "px-6 pb-3",
            // The composer idiom: one framed well that lights on focus, with
            // the input itself invisible inside it.
            div {
                class: "bg-input border border-subtle rounded-xl px-3.5 pt-2.5 pb-2 focus-within:ring-2 focus-within:ring-primary-500",
                input {
                    class: "w-full bg-transparent border-0 p-0 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:ring-0",
                    r#type: "text",
                    placeholder: "Add a to-do — ~30m · #tag · @2pm · *mon · !fri — press Enter",
                    value: "{draft}",
                    oninput: move |evt| draft.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter {
                            evt.prevent_default();
                            submit(());
                        }
                    },
                }
                if preview.has_tokens() {
                    div {
                        class: "mt-2 flex flex-wrap items-center gap-1.5",
                        if let Some(m) = preview.estimate_minutes {
                            span { class: "{PILL}", "{model::format_minutes(m)}" }
                        }
                        if let Some(start) = preview.block_start {
                            span {
                                class: "{PILL} border-primary-600 text-fg",
                                "on timeline @{fmt_hhmm(start)}"
                            }
                        }
                        if let Some(tod) = preview.time_of_day {
                            span { class: "{PILL}",
                                match tod {
                                    TimeOfDay::Morning => "morning",
                                    TimeOfDay::Afternoon => "afternoon",
                                    TimeOfDay::Evening => "this evening",
                                }
                            }
                        }
                        if let Some(d) = preview.scheduled {
                            span {
                                class: "{PILL}",
                                Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                                "→ {views::friendly_date(d, day)}"
                            }
                        }
                        if let Some(d) = preview.deadline {
                            span {
                                class: "{PILL} border-red-700 text-red-400",
                                "due {views::friendly_date(d, day)}"
                            }
                        }
                        for tag in preview.tags.iter() {
                            span { class: "{PILL}", "#{tag}" }
                        }
                        if preview.title.is_empty() {
                            span { class: "text-[11px] text-fg-muted", "…needs a title" }
                        } else if let Some(hint) = timeline_hint {
                            span { class: "text-[11px] text-fg-muted ml-auto", "{hint}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn TodoList(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let sel = selection.read().clone();
    let day = today();
    let state = planner.read();

    let is_today = sel == PlannerSelection::View(TodoView::Today);
    let is_upcoming = sel == PlannerSelection::View(TodoView::Upcoming);

    // Upcoming renders under day headers — the same grouped query the AI's
    // list tool flattens, so the two orderings can't drift.
    let upcoming_groups: Vec<(NaiveDate, Vec<Todo>)> = if is_upcoming {
        views::upcoming_grouped(&state.todos, day)
            .into_iter()
            .map(|(d, ts)| (d, ts.into_iter().cloned().collect()))
            .collect()
    } else {
        Vec::new()
    };

    // Today gets its "This Evening" split; every other selection is one flat
    // list. Completing a todo must keep its row visible for the rest of the
    // day (struck through, reopenable in place), so Today appends the day's
    // completions after the open rows in each section — a separate query from
    // the view membership, which the AI's list tool needs to stay "open work
    // owed".
    let (main_rows, evening_rows): (Vec<Todo>, Vec<Todo>) = if is_today {
        let sections = views::today_sections(&state.todos, day);
        let (done_evening, done_daytime): (Vec<&Todo>, Vec<&Todo>) =
            views::completed_today(&state.todos, day)
                .into_iter()
                .partition(|t| t.time_of_day == Some(TimeOfDay::Evening));
        (
            sections
                .daytime
                .into_iter()
                .chain(done_daytime)
                .cloned()
                .collect(),
            sections
                .evening
                .into_iter()
                .chain(done_evening)
                .cloned()
                .collect(),
        )
    } else if is_upcoming {
        (Vec::new(), Vec::new())
    } else {
        (todos_for_selection(&state, &sel, day), Vec::new())
    };
    let is_empty = main_rows.is_empty() && evening_rows.is_empty() && upcoming_groups.is_empty();

    // One read of the fleet mirror for the whole list: rows receive their own
    // carrier session as a prop rather than each reaching for shared state.
    let fleet = fleet_state.read();
    let carrier = |t: &Todo| -> Option<FleetSession> {
        t.linked_fleet_session
            .as_ref()
            .and_then(|id| fleet.sessions.get(id).cloned())
    };

    rsx! {
        div {
            class: "flex flex-col px-4",
            if is_empty {
                p { class: "px-2.5 py-8 text-sm text-fg-muted", "{empty_state_line(&sel)}" }
            }
            for todo in main_rows {
                // Cloned because the generated key borrows `todo.id` after the
                // prop takes ownership.
                TodoRow {
                    key: "{todo.id}",
                    fleet: carrier(&todo),
                    todo: todo.clone(),
                    selection,
                }
            }
            for (date, rows) in upcoming_groups {
                div {
                    key: "day-{date}",
                    h2 {
                        class: "{SECTION_HEADER}",
                        "{views::friendly_date(date, day)}"
                    }
                    for todo in rows {
                        TodoRow {
                            key: "{todo.id}",
                            fleet: carrier(&todo),
                            todo: todo.clone(),
                            selection,
                        }
                    }
                }
            }
            if !evening_rows.is_empty() {
                h2 { class: "{SECTION_HEADER}", "This evening" }
                for todo in evening_rows {
                    TodoRow {
                        key: "{todo.id}",
                        fleet: carrier(&todo),
                        todo: todo.clone(),
                        selection,
                    }
                }
            }
        }
    }
}

/// The project popover's body and the ⋯ menu's "Move to project…" both need
/// the same list, so it is one component.
#[component]
fn ProjectPicker(todo_id: String, on_pick: EventHandler<()>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let projects: Vec<(String, String)> = {
        let state = planner.read();
        let mut ps: Vec<(String, String)> = state
            .projects
            .iter()
            .filter(|p| !p.status.is_closed())
            .map(|p| (p.id.clone(), p.title.clone()))
            .collect();
        ps.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        ps
    };
    let current = planner
        .read()
        .todo(&todo_id)
        .and_then(|t| t.project_id.clone());

    rsx! {
        div {
            class: "px-2.5 pt-1.5 pb-1 text-[11px] uppercase tracking-[0.08em] text-fg-muted",
            "Move to project…"
        }
        if projects.is_empty() {
            div { class: "px-2.5 py-1.5 text-[11px] text-fg-muted", "No projects yet." }
        }
        button {
            class: "{MENU_ITEM}",
            onclick: {
                let todo_id = todo_id.clone();
                move |evt: MouseEvent| {
                    evt.stop_propagation();
                    mutate_todo(planner, &todo_id, |t| t.project_id = None);
                    on_pick.call(());
                }
            },
            span { class: "w-3 shrink-0", if current.is_none() { "✓" } }
            span { class: "truncate text-fg-muted", "No project" }
        }
        for (pid, ptitle) in projects {
            button {
                key: "{pid}",
                class: "{MENU_ITEM}",
                onclick: {
                    let todo_id = todo_id.clone();
                    let pid = pid.clone();
                    move |evt: MouseEvent| {
                        evt.stop_propagation();
                        let pid = pid.clone();
                        mutate_todo(planner, &todo_id, |t| t.project_id = Some(pid));
                        on_pick.call(());
                    }
                },
                span { class: "w-3 shrink-0", if current.as_deref() == Some(pid.as_str()) { "✓" } }
                span { class: "truncate", "{ptitle}" }
            }
        }
    }
}

#[component]
fn TodoRow(
    todo: Todo,
    /// The carrier fleet session, resolved once per list render (never locked
    /// from inside a row).
    fleet: Option<FleetSession>,
    mut selection: Signal<PlannerSelection>,
) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let session_state = use_context::<Signal<crate::session::SessionState>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    // The same dependency bundle the model's built-in tools get, so the row's
    // "Dispatch to a background agent" runs HOBBES_DISPATCH's own code path.
    let builtin_ctx = crate::components::builtin_tools::builtin_ctx_from_tree();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);
    // U1.5: first click arms, second click deletes; leaving the row disarms.
    let mut delete_armed = use_signal(|| false);
    let mut menu = use_signal(|| Option::<RowMenu>::None);
    let mut pick_day = use_signal(String::new);
    let mut detail_open = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    let mut selected_block = use_context::<SelectedBlockContext>().0;

    let sel = selection.read().clone();
    let id = todo.id.clone();
    let day = today();
    let now = Utc::now();
    let is_done = todo.status.is_closed();
    let in_focus = todo.status == TodoStatus::InProgress;
    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);

    // ── WHO ────────────────────────────────────────────────────────────────
    let focus_session = planner.read().open_focus_session_for(&id).cloned();
    // The carrier's identity, when it is one of our own chat tabs.
    let carrier_session_id = todo.linked_fleet_session.clone().or_else(|| {
        focus_session.as_ref().and_then(|f| match &f.actor {
            FocusActor::Agent { session_id } => session_id.clone(),
            FocusActor::Person => None,
        })
    });
    let is_chat_carrier = carrier_session_id
        .as_ref()
        .is_some_and(|cid| session_state.read().sessions.contains_key(cid));
    let profile_badge: Option<(String, String)> = carrier_session_id.as_ref().and_then(|cid| {
        let s = session_state.read();
        let session = s.sessions.get(cid)?;
        let settings_read = settings.read();
        let profile = session
            .composio_profile
            .as_ref()
            .and_then(|pid| settings_read.composio_profiles.iter().find(|p| &p.id == pid))
            .or_else(|| settings_read.get_active_profile())?;
        Some((
            profile
                .name
                .chars()
                .next()
                .unwrap_or('H')
                .to_uppercase()
                .to_string(),
            profile.color.clone(),
        ))
    });
    let profile_name: Option<String> = {
        let settings_read = settings.read();
        settings_read.get_active_profile().map(|p| p.name.clone())
    };

    let who = who_state(
        &todo,
        focus_session.as_ref(),
        fleet.as_ref(),
        profile_badge.clone(),
    );

    // The stripe is the row's left edge; `pl-[7px]` keeps the 10px gutter the
    // un-striped rows have (7 + a 3px border).
    let stripe = match &who {
        WhoState::YouFocusing => " border-l-[3px] border-l-primary-500 pl-[7px]".to_string(),
        WhoState::Hobbes { color, .. } => {
            format!(" border-l-[3px] {} pl-[7px]", profile_stripe_class(color))
        }
        WhoState::FleetAgent => " border-l-[3px] border-l-accent-500 pl-[7px]".to_string(),
        WhoState::NeedsYou => " border-l-[3px] border-l-yellow-700 pl-[7px]".to_string(),
        _ => String::new(),
    };
    let row_class = format!(
        "group relative grid grid-cols-[20px_minmax(0,1fr)_auto] items-center gap-2.5 h-9 px-2.5 rounded-md hover:bg-section{}",
        stripe
    );

    // ── WHAT ───────────────────────────────────────────────────────────────
    let project_title = todo.project_id.as_ref().and_then(|pid| {
        planner
            .read()
            .projects
            .iter()
            .find(|p| &p.id == pid)
            .map(|p| p.title.clone())
    });
    let carrier_label = carrier_session_id.as_ref().and_then(|cid| {
        fleet
            .as_ref()
            .map(|f| f.display_name().to_string())
            .or_else(|| {
                session_state
                    .read()
                    .sessions
                    .get(cid)
                    .map(|s| s.name.clone())
            })
    });
    let carrier_minutes = working_minutes(fleet.as_ref(), focus_session.as_ref(), now);
    let carrier_pill_class = match &who {
        WhoState::NeedsYou => format!("{} border-yellow-700 text-yellow-300", PILL),
        WhoState::FleetAgent => format!("{} border-accent-500 text-accent-300", PILL),
        WhoState::Hobbes { color, .. } => {
            format!("{} {} text-fg", PILL, profile_pill_border_class(color))
        }
        _ => PILL.to_string(),
    };

    // ── WHEN ───────────────────────────────────────────────────────────────
    // The row mirrors the timeline: if this todo is timeboxed on its
    // scheduled day, its start time appears as a chip (with the block id so
    // clicking the chip can highlight it on arrival). Scheduling metadata
    // must be legible from every visual state, not just the ruler.
    let block_info: Option<(String, String)> = todo.scheduled_for.and_then(|d| {
        let state = planner.read();
        state
            .blocks
            .iter()
            .filter(|b| {
                b.todo_id.as_deref() == Some(todo.id.as_str())
                    && b.start.with_timezone(&Local).date_naive() == d
            })
            .min_by_key(|b| b.start)
            .map(|b| {
                (
                    b.start.with_timezone(&Local).format("%H:%M").to_string(),
                    b.id.clone(),
                )
            })
    });
    let date_label = date_pill_label(
        todo.scheduled_for,
        todo.time_of_day == Some(TimeOfDay::Evening),
        day,
    );
    let current_choice = current_schedule_choice(&todo, day);
    // U2.2: closed work surfaces estimate accuracy — "52m of 1h", or just the
    // actual when the work was never estimated.
    let actuals_line = (is_done && todo.actual_minutes > 0).then(|| match todo.estimate_minutes {
        Some(est) => format!(
            "{} of {}",
            model::format_minutes(todo.actual_minutes),
            model::format_minutes(est)
        ),
        None => model::format_minutes(todo.actual_minutes),
    });
    let completed_line = in_logbook.then(|| {
        todo.completed_at
            .map(|d| views::friendly_date(d.with_timezone(&Local).date_naive(), day))
            .unwrap_or_default()
    });

    // ── Handlers ───────────────────────────────────────────────────────────
    let commit_title = {
        let id = id.clone();
        move || {
            let new_title = edit_buffer.peek().trim().to_string();
            editing.set(false);
            if new_title.is_empty() {
                return;
            }
            mutate_todo(planner, &id, |t| t.title = new_title);
        }
    };

    let toggle_done = {
        let id = id.clone();
        move |_| {
            let now = Utc::now();
            let currently_done = planner
                .peek()
                .todo(&id)
                .map(|t| t.status.is_closed())
                .unwrap_or(false);
            // State-level transitions: completing (or reopening an
            // in-progress todo) must also close its focus-session row.
            {
                let mut state = planner.write();
                if currently_done {
                    state.reopen_todo(&id, now);
                } else {
                    state.complete_todo(&id, now);
                }
            }
            persist_todo(&planner, &id);
            persist_focus_sessions(&mut planner);
        }
    };

    let delete = {
        let id = id.clone();
        move |_| {
            // Honour confirm-on-delete without a modal: the first click only
            // arms the button, the second (while still on the row) deletes.
            if settings.peek().confirm_on_delete && !*delete_armed.peek() {
                delete_armed.set(true);
                return;
            }
            // store::delete_todo cascades to the todo's block rows;
            // remove_todo mirrors that in memory.
            if let Err(e) = store::delete_todo(&id) {
                tracing::error!("planner: failed to delete todo {}: {}", id, e);
            }
            planner.write().remove_todo(&id);
            // Deleting mid-focus closed its session row; keep the history.
            persist_focus_sessions(&mut planner);
            menu.set(None);
        }
    };

    let cycle = {
        let id = id.clone();
        move |_| {
            mutate_todo(planner, &id, |t| {
                t.estimate_minutes = cycle_estimate(t.estimate_minutes)
            });
            // The estimate IS the timebox's length: a re-estimate resizes the
            // block on the ruler from its anchored start.
            for b in planner.write().resize_blocks_to_estimate(&id) {
                persist_block(&b);
            }
        }
    };

    let start_focus = {
        let id = id.clone();
        move || {
            // Focus implies today: start_focus may reschedule the todo and
            // prune off-day blocks, whose rows we must delete.
            let (changed, pruned) = planner
                .write()
                .start_focus(&id, Utc::now(), FocusActor::Person);
            for cid in &changed {
                persist_todo(&planner, cid);
            }
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("planner: failed to delete stale block {}: {}", b.id, e);
                }
            }
            persist_focus_sessions(&mut planner);
        }
    };

    let pick_schedule = {
        let id = id.clone();
        move |choice: ScheduleChoice| {
            set_schedule(planner, &id, choice, today());
            menu.set(None);
        }
    };

    let menu_now = *menu.read();
    // An open popover must survive the pointer leaving the hover zone.
    let cluster_class = if menu_now.is_some() {
        "flex items-center gap-1.5"
    } else {
        "hidden group-hover:flex items-center gap-1.5"
    };

    rsx! {
        div {
            class: "{row_class}",
            // A wandering pointer disarms the delete confirmation.
            onmouseleave: move |_| {
                if *delete_armed.peek() {
                    delete_armed.set(false);
                }
            },

            // ── Column 1 — WHO ────────────────────────────────────────────
            div {
                class: "w-5 flex items-center justify-center",
                match &who {
                    WhoState::Hobbes { initial, color } => rsx! {
                        button {
                            class: "w-4 h-4 rounded-full {color} flex items-center justify-center text-[9px] font-bold text-fg",
                            title: "Hobbes is on this — click to complete",
                            onclick: toggle_done,
                            "{initial}"
                        }
                    },
                    WhoState::FleetAgent => rsx! {
                        button {
                            class: "text-accent-400 animate-pulse hover:text-fg transition-colors",
                            title: "A coding agent is on this — click to complete",
                            onclick: toggle_done,
                            Icon {
                                width: 16,
                                height: 16,
                                icon: crate::components::pixel_icons::HobbesTiger,
                            }
                        }
                    },
                    WhoState::NeedsYou => rsx! {
                        button {
                            class: "text-yellow-400 hover:text-fg transition-colors",
                            title: "Its session needs you",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiAlertCircle }
                        }
                    },
                    WhoState::YouFocusing => rsx! {
                        button {
                            class: "text-primary-300 animate-pulse hover:text-fg transition-colors",
                            title: "In focus — click to complete",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiPlayCircle }
                        }
                    },
                    WhoState::Done => rsx! {
                        button {
                            class: "text-[#86efac] hover:text-fg transition-colors",
                            title: "Reopen",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiCheckCircle }
                        }
                    },
                    WhoState::Open => rsx! {
                        button {
                            class: "text-fg-muted hover:text-fg transition-colors",
                            title: "Complete",
                            onclick: toggle_done,
                            Icon { width: 16, height: 16, icon: fi_icons::FiCircle }
                        }
                    },
                }
            }

            // ── Column 2 — WHAT ───────────────────────────────────────────
            div {
                class: "min-w-0 flex items-center gap-2",
                if *editing.read() {
                    input {
                        class: "flex-1 min-w-0 rounded border border-subtle bg-input px-2 py-0.5 text-sm text-fg focus:outline-none",
                        r#type: "text",
                        autofocus: true,
                        value: "{edit_buffer}",
                        oninput: move |evt| edit_buffer.set(evt.value()),
                        onkeydown: {
                            let mut commit = commit_title.clone();
                            move |evt: KeyboardEvent| {
                                if evt.key() == Key::Enter {
                                    evt.prevent_default();
                                    commit();
                                } else if evt.key() == Key::Escape {
                                    editing.set(false);
                                }
                            }
                        },
                        onblur: {
                            let mut commit = commit_title.clone();
                            move |_| commit()
                        },
                    }
                } else {
                    span {
                        class: if is_done {
                            "min-w-0 truncate text-sm text-fg-muted line-through cursor-text"
                        } else {
                            "min-w-0 truncate text-sm text-fg cursor-text"
                        },
                        onclick: {
                            let title = todo.title.clone();
                            move |_| {
                                edit_buffer.set(title.clone());
                                editing.set(true);
                            }
                        },
                        "{todo.title}"
                    }
                }

                // Project pill — the door to this todo's tags and its project.
                // A todo with neither wears it only on hover, so untagged rows
                // stay quiet but never lose the affordance.
                div {
                    class: "relative shrink-0",
                    button {
                        class: if project_title.is_some() || !todo.tags.is_empty() {
                            "{PILL} hover:text-fg hover:border-subtle max-w-40"
                        } else {
                            "{PILL} max-w-40 opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                        },
                        title: "Project and tags",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            let next = (menu_now != Some(RowMenu::Project)).then_some(RowMenu::Project);
                            menu.set(next);
                        },
                        span {
                            class: "truncate",
                            if let Some(p) = project_title.clone() { "{p}" } else { "Project…" }
                        }
                    }
                    if menu_now == Some(RowMenu::Project) {
                        div {
                            class: "fixed inset-0 z-40",
                            onclick: move |_| menu.set(None),
                        }
                        div {
                            class: "{POPOVER} left-0 right-auto",
                            if !todo.tags.is_empty() {
                                div {
                                    class: "flex flex-wrap gap-1.5 px-2.5 pt-1.5 pb-2",
                                    for tag in todo.tags.iter() {
                                        span { key: "{tag}", class: "{PILL}", "#{tag}" }
                                    }
                                }
                                div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                            }
                            ProjectPicker {
                                todo_id: id.clone(),
                                on_pick: move |_| menu.set(None),
                            }
                        }
                    }
                }

                // Carrier session pill — the assignment's door.
                if let Some(label) = carrier_label.clone() {
                    button {
                        class: "{carrier_pill_class} max-w-44 hover:brightness-110",
                        title: if is_chat_carrier { "Carried by this chat — click to open" } else { "Carried by this agent session — click to open the Fleet" },
                        onclick: {
                            let carrier_session_id = carrier_session_id.clone();
                            move |evt: MouseEvent| {
                                evt.stop_propagation();
                                let cmd = if is_chat_carrier {
                                    carrier_session_id.clone().map(
                                        crate::components::chat_input::ChatCommand::SwitchToSession,
                                    )
                                } else {
                                    Some(crate::components::chat_input::ChatCommand::ToggleFleet)
                                };
                                chat_command.set(cmd);
                            }
                        },
                        if !is_chat_carrier {
                            // The tiny-chip stencil: a fleet carrier reads as
                            // an agent at a glance, without a second pill.
                            Icon {
                                width: 10,
                                height: 10,
                                icon: crate::components::pixel_icons::HobbesTigerStencil,
                            }
                        }
                        span { class: "truncate", "{label}" }
                        if let Some(m) = carrier_minutes {
                            span { class: "font-mono tabular-nums opacity-80", "· working {model::format_minutes(m)}" }
                        }
                    }
                }
            }

            // ── Column 3 — WHEN (+ the hover cluster) ─────────────────────
            div {
                class: "flex items-center justify-end gap-1.5 shrink-0",

                // Hover cluster.
                div {
                    class: "{cluster_class}",
                    if !in_logbook && !is_done {
                        button {
                            class: "{BTN_PRIMARY}",
                            title: if in_focus { "Stop focus — elapsed time is kept" } else { "Start focus (pauses any other)" },
                            onclick: {
                                let id = id.clone();
                                let start_focus = start_focus.clone();
                                move |_| {
                                    if in_focus {
                                        if planner.write().stop_focus(Utc::now()).is_some() {
                                            persist_todo(&planner, &id);
                                            persist_focus_sessions(&mut planner);
                                        }
                                    } else {
                                        { let mut f = start_focus.clone(); f(); }
                                    }
                                }
                            },
                            if in_focus { "Stop" } else { "Start" }
                        }
                        // Hand to Hobbes — a chat tab, or a background agent.
                        div {
                            class: "relative",
                            button {
                                class: "{BTN_SECONDARY}",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    let next = (menu_now != Some(RowMenu::Hand)).then_some(RowMenu::Hand);
                                    menu.set(next);
                                },
                                "Hand to Hobbes ▾"
                            }
                            if menu_now == Some(RowMenu::Hand) {
                                div {
                                    class: "fixed inset-0 z-40",
                                    onclick: move |_| menu.set(None),
                                }
                                div {
                                    class: "{POPOVER} w-72",
                                    button {
                                        class: "{MENU_ITEM} justify-between",
                                        onclick: {
                                            let todo = todo.clone();
                                            let id = id.clone();
                                            let start_focus = start_focus.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                menu.set(None);
                                                { let mut f = start_focus.clone(); f(); }
                                                let _ = &id;
                                                chat_command.set(Some(
                                                    crate::components::chat_input::ChatCommand::StartTodoInChat(
                                                        activation_seed(&todo),
                                                    ),
                                                ));
                                            }
                                        },
                                        span { "Work on it in a new chat" }
                                        if let Some(name) = profile_name.clone() {
                                            span { class: "{META}", "{name}" }
                                        }
                                    }
                                    button {
                                        class: "{MENU_ITEM}",
                                        onclick: {
                                            let id = id.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                menu.set(None);
                                                match crate::components::builtin_tools::dispatch_todo_to_background(
                                                    builtin_ctx, &id, None,
                                                ) {
                                                    Ok(msg) => tracing::info!("planner: {msg}"),
                                                    Err(e) => {
                                                        tracing::error!("planner: dispatch failed: {e}");
                                                        // Surface it where the
                                                        // user is looking: the
                                                        // todo's progress line.
                                                        mutate_todo(planner, &id, |t| {
                                                            t.latest_progress = Some(format!("Dispatch failed: {e}"))
                                                        });
                                                    }
                                                }
                                            }
                                        },
                                        span { "Dispatch to a background agent" }
                                    }
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    p {
                                        class: "px-2.5 py-1 text-[11px] leading-snug text-fg-muted",
                                        "Both open with the project, estimate and notes in the message. You can edit before sending."
                                    }
                                }
                            }
                        }
                    }
                    // ⋯ — details, project, delete.
                    div {
                        class: "relative",
                        button {
                            class: "rounded px-1.5 py-0.5 text-[11px] text-fg-muted hover:bg-input hover:text-fg",
                            title: "More",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                let next = (menu_now != Some(RowMenu::More)).then_some(RowMenu::More);
                                menu.set(next);
                            },
                            "⋯"
                        }
                        if menu_now == Some(RowMenu::More) {
                            div {
                                class: "fixed inset-0 z-40",
                                onclick: move |_| menu.set(None),
                            }
                            div {
                                class: "{POPOVER}",
                                button {
                                    class: "{MENU_ITEM}",
                                    onclick: {
                                        let id = id.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            menu.set(None);
                                            detail_open.set(Some(id.clone()));
                                        }
                                    },
                                    Icon { width: 12, height: 12, icon: fi_icons::FiInfo }
                                    span { "Details" }
                                }
                                if !in_logbook {
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    ProjectPicker {
                                        todo_id: id.clone(),
                                        on_pick: move |_| menu.set(None),
                                    }
                                }
                                div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                button {
                                    class: if *delete_armed.read() {
                                        "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-left text-red-400 font-medium bg-red-900/30 hover:bg-input"
                                    } else {
                                        "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-left text-red-400 hover:bg-input"
                                    },
                                    onclick: delete,
                                    Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                    if *delete_armed.read() { "Sure? Click again" } else { "Delete" }
                                }
                            }
                        }
                    }
                    div { class: "w-px h-4 bg-[var(--color-border-muted)] mx-0.5" }
                }

                // WHEN meta.
                if is_done {
                    if let Some(actuals) = actuals_line {
                        span { class: "{META}", "{actuals}" }
                    }
                    if let Some(date_line) = completed_line {
                        span { class: "{META}", "{date_line}" }
                    }
                } else {
                    if who == WhoState::NeedsYou {
                        button {
                            class: "{BTN_PRIMARY}",
                            title: "Open the Fleet to answer it",
                            onclick: move |_| {
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::ToggleFleet,
                                ));
                            },
                            "Open"
                        }
                    }
                    button {
                        class: if todo.estimate_minutes.is_some() {
                            "{META} hover:text-fg"
                        } else {
                            "{META} opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                        },
                        title: "Estimate — click to cycle 15m / 30m / 1h / 2h / none",
                        onclick: cycle,
                        if let Some(m) = todo.estimate_minutes {
                            "{model::format_minutes(m)}"
                        } else {
                            "est"
                        }
                    }
                    if let Some((t, block_id)) = block_info {
                        // A live affordance, not a dead label: the chip jumps
                        // to Today with its block selected, so "@ 09:00" is
                        // always one click from the timebox it names.
                        button {
                            class: "{META} hover:text-fg",
                            title: "On the timeline — click to show it in Today",
                            onclick: move |_| {
                                selected_block.set(Some(block_id.clone()));
                                selection.set(PlannerSelection::View(TodoView::Today));
                            },
                            "@ {t}"
                        }
                    }
                    if let Some(deadline) = todo.deadline {
                        span {
                            class: "{PILL} {due_tone_class(due_tone(deadline, day))}",
                            "due {views::friendly_date(deadline, day)}"
                        }
                    }
                    // The date pill: always present once a day is set, in every
                    // view (it used to hide outside Projects, which made the
                    // schedule invisible exactly where it mattered).
                    {
                        let scheduled = date_label.is_some();
                        let label = date_label.clone().unwrap_or_else(|| "When".to_string());
                        rsx! {
                        div {
                            class: "relative shrink-0",
                            button {
                                class: if scheduled {
                                    "{PILL} hover:text-fg hover:border-subtle"
                                } else {
                                    "{PILL} opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                                },
                                title: "When — click to reschedule",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    let next = (menu_now != Some(RowMenu::Date)).then_some(RowMenu::Date);
                                    menu.set(next);
                                },
                                "{label} ▾"
                            }
                            if menu_now == Some(RowMenu::Date) {
                                div {
                                    class: "fixed inset-0 z-40",
                                    onclick: move |_| menu.set(None),
                                }
                                div {
                                    class: "{POPOVER}",
                                    for (choice, label) in [
                                        (ScheduleChoice::Today, "Today"),
                                        (ScheduleChoice::ThisEvening, "This evening"),
                                        (ScheduleChoice::Tomorrow, "Tomorrow"),
                                    ] {
                                        button {
                                            key: "{label}",
                                            class: "{MENU_ITEM}",
                                            onclick: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    pick(choice);
                                                }
                                            },
                                            span { class: "w-3 shrink-0", if current_choice == choice { "✓" } }
                                            span { "{label}" }
                                        }
                                    }
                                    div {
                                        class: "flex items-center gap-2 px-2.5 py-1.5",
                                        span { class: "w-3 shrink-0" }
                                        span { class: "text-[11px] text-fg-muted shrink-0", "Pick a day…" }
                                        input {
                                            class: "flex-1 min-w-0 rounded border border-subtle bg-input px-1.5 py-0.5 text-[11px] text-fg focus:outline-none",
                                            r#type: "date",
                                            value: "{pick_day}",
                                            onclick: move |evt| evt.stop_propagation(),
                                            oninput: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: FormEvent| {
                                                    let raw = evt.value();
                                                    pick_day.set(raw.clone());
                                                    if let Ok(d) = raw.parse::<NaiveDate>() {
                                                        pick(ScheduleChoice::On(d));
                                                    }
                                                }
                                            },
                                        }
                                    }
                                    div { class: "h-px bg-[var(--color-border-muted)] my-1" }
                                    for (choice, label) in [
                                        (ScheduleChoice::Anytime, "Anytime"),
                                        (ScheduleChoice::Someday, "Someday"),
                                    ] {
                                        button {
                                            key: "{label}",
                                            class: "{MENU_ITEM}",
                                            onclick: {
                                                let mut pick = pick_schedule.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    pick(choice);
                                                }
                                            },
                                            span { class: "w-3 shrink-0", if current_choice == choice { "✓" } }
                                            span { "{label}" }
                                        }
                                    }
                                }
                            }
                        }
                        } // rsx!
                    }
                }
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::AttentionKind;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn fleet_session(origin: FleetOrigin, status: FleetStatus) -> FleetSession {
        let mut s = FleetSession {
            id: "fs_1".into(),
            cwd: "/tmp/repo".into(),
            name: "repo".into(),
            status,
            last_event_at: Utc::now(),
            working_since: None,
            day_minutes: Default::default(),
            day_seconds: Default::default(),
            auto_passthrough: false,
            ended_at: None,
            pending_gate: None,
            transcript_path: None,
            brief: None,
            brief_dirty_at: None,
            origin,
            session_title: None,
            active_prompt: None,
            stopped_prompts: Vec::new(),
            attention_at: None,
            dispatched_todo: None,
        };
        s.working_since = matches!(
            s.status,
            FleetStatus::Working | FleetStatus::WorkingBackground
        )
        .then(Utc::now);
        s
    }

    fn focus(todo_id: &str, actor: FocusActor) -> FocusSession {
        FocusSession::open(todo_id, Utc::now(), actor)
    }

    // ── who_state ──────────────────────────────────────────────────────────

    #[test]
    fn who_state_open_when_nobody_is_on_it() {
        let t = Todo::new("Write the brief", 0.0);
        assert_eq!(who_state(&t, None, None, None), WhoState::Open);
    }

    #[test]
    fn who_state_you_focusing_needs_an_open_person_session() {
        let mut t = Todo::new("Write the brief", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(&t.id, FocusActor::Person);
        assert_eq!(who_state(&t, Some(&f), None, None), WhoState::YouFocusing);

        // A closed session is history, not a live state.
        let mut closed = f.clone();
        closed.ended_at = Some(Utc::now());
        assert_eq!(who_state(&t, Some(&closed), None, None), WhoState::Open);
    }

    #[test]
    fn who_state_hobbes_from_an_agent_focus_session_or_a_hobbes_carrier() {
        let mut t = Todo::new("Draft the memo", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(
            &t.id,
            FocusActor::Agent {
                session_id: Some("sess_1".into()),
            },
        );
        let badge = Some(("W".to_string(), "bg-purple-600".to_string()));
        assert_eq!(
            who_state(&t, Some(&f), None, badge.clone()),
            WhoState::Hobbes {
                initial: "W".into(),
                color: "bg-purple-600".into()
            }
        );

        // ...and without a focus session, a working Hobbes-origin carrier.
        let mut open = Todo::new("Draft the memo", 0.0);
        open.linked_fleet_session = Some("fs_1".into());
        let fs = fleet_session(FleetOrigin::Hobbes, FleetStatus::WorkingBackground);
        assert_eq!(
            who_state(&open, None, Some(&fs), badge),
            WhoState::Hobbes {
                initial: "W".into(),
                color: "bg-purple-600".into()
            }
        );
    }

    #[test]
    fn who_state_hobbes_falls_back_to_the_default_swatch() {
        let mut t = Todo::new("Draft the memo", 0.0);
        t.status = TodoStatus::InProgress;
        let f = focus(&t.id, FocusActor::Agent { session_id: None });
        assert_eq!(
            who_state(&t, Some(&f), None, None),
            WhoState::Hobbes {
                initial: "H".into(),
                color: DEFAULT_PROFILE_COLOR.to_string()
            }
        );
    }

    #[test]
    fn who_state_fleet_agent_for_a_working_external_carrier() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.linked_fleet_session = Some("fs_1".into());
        let fs = fleet_session(FleetOrigin::External, FleetStatus::Working);
        assert_eq!(who_state(&t, None, Some(&fs), None), WhoState::FleetAgent);

        // An idle carrier is not "on it".
        let idle = fleet_session(FleetOrigin::External, FleetStatus::Idle);
        assert_eq!(who_state(&t, None, Some(&idle), None), WhoState::Open);
    }

    #[test]
    fn who_state_needs_you_beats_working() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.status = TodoStatus::InProgress;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Agent { session_id: None });
        let fs = fleet_session(
            FleetOrigin::External,
            FleetStatus::NeedsAttention(AttentionKind::Gate),
        );
        assert_eq!(who_state(&t, Some(&f), Some(&fs), None), WhoState::NeedsYou);
    }

    #[test]
    fn who_state_done_beats_everything() {
        let mut t = Todo::new("Ship the migration", 0.0);
        t.status = TodoStatus::Completed;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Person);
        let fs = fleet_session(
            FleetOrigin::External,
            FleetStatus::NeedsAttention(AttentionKind::Gate),
        );
        assert_eq!(who_state(&t, Some(&f), Some(&fs), None), WhoState::Done);

        // Cancelled is closed too.
        t.status = TodoStatus::Cancelled;
        assert_eq!(who_state(&t, None, None, None), WhoState::Done);
    }

    #[test]
    fn who_state_you_beat_a_stale_working_link() {
        // You pressed Start; a linked session still reporting Working must not
        // steal the row's identity from you.
        let mut t = Todo::new("Write the brief", 0.0);
        t.status = TodoStatus::InProgress;
        t.linked_fleet_session = Some("fs_1".into());
        let f = focus(&t.id, FocusActor::Person);
        let fs = fleet_session(FleetOrigin::External, FleetStatus::Working);
        assert_eq!(
            who_state(&t, Some(&f), Some(&fs), None),
            WhoState::YouFocusing
        );
    }

    // ── due_tone ───────────────────────────────────────────────────────────

    #[test]
    fn due_tone_reads_the_next_two_days_as_soon() {
        let today = d("2026-09-16");
        assert_eq!(due_tone(d("2026-09-15"), today), DueTone::Overdue);
        assert_eq!(due_tone(today, today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-17"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-18"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-19"), today), DueTone::Neutral);
    }

    // ── date_pill_label ────────────────────────────────────────────────────

    #[test]
    fn date_pill_label_names_the_day() {
        let today = d("2026-09-16"); // a Wednesday
        assert_eq!(date_pill_label(None, false, today), None);
        assert_eq!(
            date_pill_label(Some(today), false, today).as_deref(),
            Some("Today")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-17")), false, today).as_deref(),
            Some("Tomorrow")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), false, today).as_deref(),
            Some("Fri 18")
        );
        // Evening only rebrands today's pill; a future evening still says the day.
        assert_eq!(
            date_pill_label(Some(today), true, today).as_deref(),
            Some("Evening")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), true, today).as_deref(),
            Some("Fri 18")
        );
    }

    // ── the shared schedule helper ─────────────────────────────────────────

    #[test]
    fn apply_schedule_choice_pins_the_row_menu_behaviour() {
        let today = d("2026-09-16");
        let tomorrow = d("2026-09-17");

        let mut t = Todo::new("x", 0.0);
        t.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut t, ScheduleChoice::Today, today);
        assert_eq!(t.scheduled_for, Some(today));
        assert_eq!(t.time_of_day, None, "Today means the daytime list");

        apply_schedule_choice(&mut t, ScheduleChoice::Tomorrow, today);
        assert_eq!(t.scheduled_for, Some(tomorrow));
        assert_eq!(t.time_of_day, None);

        // Evening is a time-of-day, not a reschedule: a dated todo keeps its day.
        apply_schedule_choice(&mut t, ScheduleChoice::ThisEvening, today);
        assert_eq!(t.scheduled_for, Some(tomorrow), "the day is not yanked");
        assert_eq!(t.time_of_day, Some(TimeOfDay::Evening));

        // ...but an undated one lands on today.
        let mut u = Todo::new("y", 0.0);
        apply_schedule_choice(&mut u, ScheduleChoice::ThisEvening, today);
        assert_eq!(u.scheduled_for, Some(today));
        assert_eq!(u.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn apply_schedule_choice_buckets_and_clears() {
        let today = d("2026-09-16");

        // Someday clears the date and the grouping — and, with no date left,
        // `set_schedule` prunes every block (keep = None).
        let mut t = Todo::new("x", 0.0);
        t.scheduled_for = Some(today);
        t.time_of_day = Some(TimeOfDay::Evening);
        t.bucket = TodoBucket::Anytime;
        apply_schedule_choice(&mut t, ScheduleChoice::Someday, today);
        assert_eq!(t.scheduled_for, None);
        assert_eq!(t.time_of_day, None);
        assert_eq!(t.bucket, TodoBucket::Someday);

        // Anytime is the inverse: undated, but triaged rather than deferred.
        apply_schedule_choice(&mut t, ScheduleChoice::Anytime, today);
        assert_eq!(t.bucket, TodoBucket::Anytime);

        // ClearDate (the detail card's "Clear") leaves the bucket alone.
        let mut s = Todo::new("y", 0.0);
        s.bucket = TodoBucket::Someday;
        s.scheduled_for = Some(today);
        s.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut s, ScheduleChoice::ClearDate, today);
        assert_eq!(s.scheduled_for, None);
        assert_eq!(s.time_of_day, None);
        assert_eq!(s.bucket, TodoBucket::Someday);

        // A picked day sets the date without touching the grouping.
        let mut p = Todo::new("z", 0.0);
        p.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut p, ScheduleChoice::On(d("2026-10-01")), today);
        assert_eq!(p.scheduled_for, Some(d("2026-10-01")));
        assert_eq!(p.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn current_schedule_choice_ticks_the_todays_state() {
        let today = d("2026-09-16");
        let mut t = Todo::new("x", 0.0);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Anytime);

        t.bucket = TodoBucket::Someday;
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Someday);

        t.bucket = TodoBucket::Anytime;
        t.scheduled_for = Some(today);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Today);

        t.time_of_day = Some(TimeOfDay::Evening);
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::ThisEvening
        );

        t.time_of_day = None;
        t.scheduled_for = Some(d("2026-09-17"));
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Tomorrow);

        t.scheduled_for = Some(d("2026-10-01"));
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::On(d("2026-10-01"))
        );
    }

    // ── swatch derivation ──────────────────────────────────────────────────

    #[test]
    fn profile_classes_derive_from_the_swatch() {
        assert_eq!(profile_stripe_class("bg-purple-600"), "border-l-purple-600");
        assert_eq!(profile_pill_border_class("bg-teal-600"), "border-teal-600");
        // Anything that isn't a bg utility falls back rather than emitting a
        // class Tailwind never generated.
        assert_eq!(profile_stripe_class("nonsense"), "border-l-blue-600");
        assert_eq!(profile_pill_border_class("nonsense"), "border-faint");
    }

    // ── pre-existing helpers ───────────────────────────────────────────────

    #[test]
    fn activation_seed_carries_the_task_and_invites_details() {
        let mut t = Todo::new("Finalize Case Study", 0.0);
        t.id = "td_1a2b".into();
        t.estimate_minutes = Some(60);
        t.deadline = Some("2026-08-13".parse().unwrap());
        t.tags = vec!["writing".into()];

        let seed = activation_seed(&t);
        assert_eq!(
            seed,
            "Let's work on my to-do \"Finalize Case Study\" (1h, due 13 Aug, #writing) — todo id td_1a2b.\n\nDetails: "
        );

        // Bare todos skip the empty parenthetical.
        let bare = Todo::new("Email", 0.0);
        let seed = activation_seed(&bare);
        assert!(seed.starts_with("Let's work on my to-do \"Email\" — todo id"));
        assert!(seed.ends_with("Details: "), "caret parks after the prompt");
    }

    #[test]
    fn cycle_estimate_walks_the_bucket_ladder() {
        assert_eq!(cycle_estimate(None), Some(15));
        assert_eq!(cycle_estimate(Some(15)), Some(30));
        assert_eq!(cycle_estimate(Some(30)), Some(60));
        assert_eq!(cycle_estimate(Some(60)), Some(120));
        // Only an exact 2h cycles to none.
        assert_eq!(cycle_estimate(Some(120)), None);
    }

    #[test]
    fn cycle_estimate_snaps_off_ladder_values_to_the_nearest_step() {
        // 45 is equidistant from 30 and 60 — ties round up.
        assert_eq!(cycle_estimate(Some(45)), Some(60));
        // 90 is equidistant from 60 and 120 — same rule.
        assert_eq!(cycle_estimate(Some(90)), Some(120));
        // Plain nearest when there's no tie.
        assert_eq!(cycle_estimate(Some(20)), Some(15));
        assert_eq!(cycle_estimate(Some(50)), Some(60));
        // Past the top of the ladder snaps back to 2h, never to none.
        assert_eq!(cycle_estimate(Some(300)), Some(120));
        // ...and the next click continues the ladder normally.
        assert_eq!(cycle_estimate(cycle_estimate(Some(45))), Some(120));
    }
}
