//! Planner centre column: the Things-style todo list surfaces.
//!
//! Split out of `planner_view.rs` (P-split for concurrent editing) — keeps
//! `CentreColumn`, `QuickAdd`, `TodoList`, and `TodoRow`, plus the pure
//! helpers only they use. Root types (`PlannerSelection`, persistence
//! helpers, `today()`) and timeline geometry live in the sibling modules
//! `planner_view` and `planner_timeline`.

use chrono::{Local, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::settings::Settings;
use crate::todo::model::{
    self, BlockSource, FocusActor, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoStatus,
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

// ── Centre column ───────────────────────────────────────────────────────────

// ── Centre column ───────────────────────────────────────────────────────────

#[component]
pub(crate) fn CentreColumn(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
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
    let subtitle = (sel == PlannerSelection::View(TodoView::Today))
        .then(|| Local::now().format("%A, %-d %B").to_string());
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

    rsx! {
        div {
            class: "px-6 pb-3",
            input {
                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
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
                    class: "mt-1.5 flex flex-wrap items-center gap-1.5",
                    if let Some(m) = preview.estimate_minutes {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg", "{model::format_minutes(m)}" }
                    }
                    if let Some(start) = preview.block_start {
                        span { class: "rounded bg-btn-primary px-1.5 py-0.5 text-[10px] text-fg", "on timeline @{fmt_hhmm(start)}" }
                    }
                    if let Some(tod) = preview.time_of_day {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg",
                            match tod {
                                TimeOfDay::Morning => "morning",
                                TimeOfDay::Afternoon => "afternoon",
                                TimeOfDay::Evening => "this evening",
                            }
                        }
                    }
                    if let Some(d) = preview.scheduled {
                        span {
                            class: "flex items-center gap-1 rounded bg-input px-1.5 py-0.5 text-[10px] text-fg",
                            Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                            "→ {views::friendly_date(d, day)}"
                        }
                    }
                    if let Some(d) = preview.deadline {
                        span { class: "rounded bg-red-500/20 px-1.5 py-0.5 text-[10px] text-fg", "due {views::friendly_date(d, day)}" }
                    }
                    for tag in preview.tags.iter() {
                        span { class: "rounded bg-input px-1.5 py-0.5 text-[10px] text-fg-muted", "#{tag}" }
                    }
                    if preview.title.is_empty() {
                        span { class: "text-[10px] text-fg-muted", "…needs a title" }
                    }
                }
            }
        }
    }
}

#[component]
fn TodoList(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
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

    rsx! {
        div {
            class: "flex flex-col px-4",
            if is_empty {
                p { class: "px-2 py-8 text-sm text-fg-muted", "{empty_state_line(&sel)}" }
            }
            for todo in main_rows {
                // Cloned because the generated key borrows `todo.id` after the
                // prop takes ownership.
                TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
            }
            for (date, rows) in upcoming_groups {
                div {
                    key: "day-{date}",
                    h2 {
                        class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                        "{views::friendly_date(date, day)}"
                    }
                    for todo in rows {
                        TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
                    }
                }
            }
            if !evening_rows.is_empty() {
                h2 {
                    class: "px-2 pt-5 pb-1 text-sm font-medium text-fg-muted",
                    "This Evening"
                }
                for todo in evening_rows {
                    TodoRow { key: "{todo.id}", todo: todo.clone(), selection }
                }
            }
        }
    }
}

#[component]
fn TodoRow(todo: Todo, mut selection: Signal<PlannerSelection>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let session_state = use_context::<Signal<crate::session::SessionState>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut editing = use_signal(|| false);
    let mut edit_buffer = use_signal(String::new);
    // U1.5: first click arms, second click deletes; leaving the row disarms.
    let mut delete_armed = use_signal(|| false);
    let mut detail_open = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    let mut selected_block = use_context::<SelectedBlockContext>().0;

    let sel = selection.read().clone();
    let id = todo.id.clone();
    let day = today();
    let is_done = todo.status.is_closed();
    let in_focus = todo.status == TodoStatus::InProgress;
    let is_overdue = todo.is_overdue(day);

    let in_logbook = sel == PlannerSelection::View(TodoView::Logbook);
    // A project mixes scheduled with unscheduled work, so its rows answer
    // "when?" inline. Upcoming no longer needs the chip: its day headers
    // (U3.3) already say the date, and repeating it per row is noise.
    let show_scheduled = matches!(sel, PlannerSelection::Project(_));

    // The hover cluster never offers the state the row is already in.
    // "Today" stays visible for an Evening row scheduled today — there it
    // means "back to the daytime list", not a no-op.
    let offer_today = !(sel == PlannerSelection::View(TodoView::Today)
        && todo.scheduled_for == Some(day)
        && todo.time_of_day.is_none());
    let offer_tomorrow = todo.scheduled_for != day.succ_opt();
    let offer_evening =
        !(todo.time_of_day == Some(TimeOfDay::Evening) && todo.scheduled_for.is_some());
    let offer_clear_date = todo.scheduled_for.is_some() || todo.time_of_day.is_some();
    let offer_someday = sel != PlannerSelection::View(TodoView::Someday);

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

    let schedule = |patch: fn(&mut Todo, NaiveDate)| {
        let id = id.clone();
        move |_| {
            mutate_todo(planner, &id, |t| patch(t, today()));
            // The timebox follows the schedule: blocks left on days that no
            // longer match the new date are removed everywhere (same rule as
            // the AI's update and plan-day handlers).
            let keep = planner.peek().todo(&id).and_then(|t| t.scheduled_for);
            let pruned = planner.write().prune_blocks_for_todo(&id, keep);
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("planner: failed to delete rescheduled block {}: {}", b.id, e);
                }
            }
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

    let completed_line = in_logbook.then(|| {
        todo.completed_at
            .map(|d| views::friendly_date(d.with_timezone(&Local).date_naive(), day))
            .unwrap_or_default()
    });
    // U2.2: the logbook surfaces estimate accuracy — "52m of 1h", or just the
    // actual when the work was never estimated.
    let actuals_line = (in_logbook && todo.actual_minutes > 0).then(|| match todo.estimate_minutes {
        Some(est) => format!(
            "{} of {}",
            model::format_minutes(todo.actual_minutes),
            model::format_minutes(est)
        ),
        None => model::format_minutes(todo.actual_minutes),
    });

    rsx! {
        div {
            class: "group flex items-center gap-2 rounded px-2 py-1.5 hover:bg-section",
            // A wandering pointer disarms the delete confirmation.
            onmouseleave: move |_| {
                if *delete_armed.peek() {
                    delete_armed.set(false);
                }
            },
            // Checkbox — a pulsing play glyph while the todo is in focus.
            button {
                class: if in_focus {
                    "shrink-0 text-primary-400 animate-pulse hover:text-fg transition-colors"
                } else {
                    "shrink-0 text-fg-muted hover:text-fg transition-colors"
                },
                title: if is_done { "Reopen" } else { "Complete" },
                onclick: toggle_done,
                if is_done {
                    Icon { width: 16, height: 16, icon: fi_icons::FiCheckCircle }
                } else if in_focus {
                    Icon { width: 16, height: 16, icon: fi_icons::FiPlayCircle }
                } else {
                    Icon { width: 16, height: 16, icon: fi_icons::FiCircle }
                }
            }
            // Title (click to edit inline)
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
                        "flex-1 min-w-0 truncate text-sm text-fg-muted line-through cursor-text"
                    } else {
                        "flex-1 min-w-0 truncate text-sm text-fg cursor-text"
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
            if let Some(date_line) = completed_line {
                span { class: "shrink-0 text-xs text-fg-muted", "{date_line}" }
            }
            if let Some(actuals) = actuals_line {
                span { class: "shrink-0 text-xs text-fg-muted", "{actuals}" }
            }
            // Linked fleet session: the assignment's carrier. Hobbes tabs are
            // doors (click opens the chat); terminal sessions are labels.
            if let Some(link_id) = todo.linked_fleet_session.clone() {
                {
                    let is_chat = session_state.read().sessions.contains_key(&link_id);
                    let label = fleet_state
                        .read()
                        .sessions
                        .get(&link_id)
                        .map(|s| s.name.clone())
                        .or_else(|| {
                            session_state.read().sessions.get(&link_id).map(|s| s.name.clone())
                        })
                        .unwrap_or_else(|| "session".to_string());
                    rsx! {
                        button {
                            class: if is_chat {
                                "shrink-0 flex items-center gap-1 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted hover:text-fg hover:border-primary-500 max-w-32"
                            } else {
                                "shrink-0 flex items-center gap-1 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted max-w-32 cursor-default"
                            },
                            title: if is_chat { "Carried by this chat — click to open" } else { "Carried by this coding-agent session" },
                            onclick: move |evt| {
                                evt.stop_propagation();
                                if is_chat {
                                    chat_command.set(Some(
                                        crate::components::chat_input::ChatCommand::SwitchToSession(link_id.clone()),
                                    ));
                                }
                            },
                            Icon {
                                width: 10,
                                height: 10,
                                icon: crate::components::pixel_icons::HobbesTigerStencil,
                            }
                            span { class: "truncate", "{label}" }
                        }
                    }
                }
            }
            // Chips
            if show_scheduled {
                if let Some(d) = todo.scheduled_for {
                    span {
                        class: "shrink-0 flex items-center gap-1 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted",
                        Icon { width: 10, height: 10, icon: fi_icons::FiCalendar }
                        "{views::friendly_date(d, day)}"
                    }
                }
            }
            if !in_logbook {
                if let Some((t, block_id)) = block_info {
                    // A live affordance, not a dead label: the chip jumps to
                    // Today with its block selected, so "@ 09:00" is always
                    // one click from the timebox it names.
                    button {
                        class: "shrink-0 rounded-full bg-primary-700/50 border border-subtle px-2 py-0.5 text-xs text-fg hover:bg-primary-700",
                        title: "On the timeline — click to show it in Today",
                        onclick: move |_| {
                            selected_block.set(Some(block_id.clone()));
                            selection.set(PlannerSelection::View(TodoView::Today));
                        },
                        "@ {t}"
                    }
                }
            }
            if !in_logbook {
                button {
                    class: if todo.estimate_minutes.is_some() {
                        "shrink-0 rounded-full border border-subtle bg-card px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                    } else {
                        "shrink-0 rounded-full border border-faint px-2 py-0.5 text-xs text-fg-muted opacity-0 group-hover:opacity-100 hover:text-fg transition-opacity"
                    },
                    title: "Estimate — click to cycle 15m / 30m / 1h / 2h / none",
                    onclick: cycle,
                    if let Some(m) = todo.estimate_minutes {
                        "{model::format_minutes(m)}"
                    } else {
                        "est"
                    }
                }
            }
            if let Some(deadline) = todo.deadline {
                span {
                    class: if is_overdue {
                        "shrink-0 rounded-full border border-red-700 bg-red-900/30 px-2 py-0.5 text-xs text-red-400"
                    } else {
                        "shrink-0 rounded-full border border-subtle px-2 py-0.5 text-xs text-fg-muted"
                    },
                    "due {views::friendly_date(deadline, day)}"
                }
            }
            for tag in todo.tags.iter() {
                span {
                    class: "shrink-0 rounded-full bg-input px-2 py-0.5 text-xs text-fg-muted",
                    "#{tag}"
                }
            }
            // Hover cluster. Logbook rows get a reduced one (U3.5): Details
            // and Delete only — closed work can be inspected and pruned, but
            // scheduling or focusing it would mean reopening it first.
            div {
                class: "hidden shrink-0 items-center gap-1 group-hover:flex",
                // Detail card (U2.1): the full editor for this todo.
                button {
                    class: "rounded p-1 text-fg-muted hover:bg-input hover:text-fg",
                    title: "Details",
                    onclick: {
                        let id = id.clone();
                        move |_| detail_open.set(Some(id.clone()))
                    },
                    Icon { width: 14, height: 14, icon: fi_icons::FiInfo }
                }
                if !in_logbook {
                    // Start / stop focus on this todo.
                    button {
                        class: "rounded p-1 text-primary-300 hover:bg-input hover:text-primary-200",
                        title: if in_focus { "Stop focus — elapsed time is kept" } else { "Start focus (pauses any other)" },
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                let now = Utc::now();
                                if in_focus {
                                    if planner.write().stop_focus(now).is_some() {
                                        persist_todo(&planner, &id);
                                        persist_focus_sessions(&mut planner);
                                    }
                                } else {
                                    // Focus implies today: start_focus may
                                    // reschedule the todo and prune off-day
                                    // blocks, whose rows we must delete.
                                    let (changed, pruned) = planner
                                        .write()
                                        .start_focus(&id, now, FocusActor::Person);
                                    for cid in &changed {
                                        persist_todo(&planner, cid);
                                    }
                                    for b in &pruned {
                                        if let Err(e) = store::delete_block(&b.id) {
                                            tracing::error!(
                                                "planner: failed to delete stale block {}: {}",
                                                b.id, e
                                            );
                                        }
                                    }
                                    persist_focus_sessions(&mut planner);
                                }
                            }
                        },
                        if in_focus {
                            Icon { width: 14, height: 14, icon: fi_icons::FiPauseCircle }
                        } else {
                            Icon { width: 14, height: 14, icon: fi_icons::FiPlayCircle }
                        }
                    }
                    // Activate: focus the task AND open a fresh chat seeded
                    // with it — activating means working on it now.
                    button {
                        class: "rounded p-1 text-primary-300 hover:bg-input hover:text-primary-200",
                        title: "Work on this in a new chat",
                        onclick: {
                            let todo = todo.clone();
                            let id = id.clone();
                            move |_| {
                                let (changed, pruned) = planner
                                    .write()
                                    .start_focus(&id, Utc::now(), FocusActor::Person);
                                for cid in &changed {
                                    persist_todo(&planner, cid);
                                }
                                for b in &pruned {
                                    if let Err(e) = store::delete_block(&b.id) {
                                        tracing::error!(
                                            "planner: failed to delete stale block {}: {}",
                                            b.id, e
                                        );
                                    }
                                }
                                persist_focus_sessions(&mut planner);
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::StartTodoInChat(
                                        activation_seed(&todo),
                                    ),
                                ));
                            }
                        },
                        Icon { width: 14, height: 14, icon: fi_icons::FiMessageCircle }
                    }
                    if offer_today {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, day| {
                                t.scheduled_for = Some(day);
                                t.time_of_day = None;
                            }),
                            "Today"
                        }
                    }
                    if offer_tomorrow {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, day| {
                                t.scheduled_for = day.succ_opt();
                                t.time_of_day = None;
                            }),
                            "Tomorrow"
                        }
                    }
                    if offer_evening {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            title: "Move to the This Evening group",
                            // Evening is a time-of-day, not a reschedule: a
                            // dated todo keeps its day (yanking an Upcoming
                            // todo onto today was the U1.4 bug); only an
                            // undated one gets scheduled today.
                            onclick: schedule(|t, day| {
                                t.time_of_day = Some(TimeOfDay::Evening);
                                if t.scheduled_for.is_none() {
                                    t.scheduled_for = Some(day);
                                }
                            }),
                            "Evening"
                        }
                    }
                    if offer_clear_date {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, _| {
                                t.scheduled_for = None;
                                t.time_of_day = None;
                            }),
                            "Clear date"
                        }
                    }
                    if offer_someday {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-fg-muted hover:bg-input hover:text-fg",
                            onclick: schedule(|t, _| {
                                t.scheduled_for = None;
                                t.time_of_day = None;
                                t.bucket = TodoBucket::Someday;
                            }),
                            "Someday"
                        }
                    }
                }
                if *delete_armed.read() {
                        button {
                            class: "rounded border border-red-700 bg-red-900/30 px-1.5 py-0.5 text-xs font-medium text-red-400 hover:bg-input",
                            title: "Click again to delete",
                            onclick: delete.clone(),
                            "Sure?"
                        }
                    } else {
                        button {
                            class: "rounded px-1.5 py-0.5 text-xs text-red-400 hover:bg-input",
                            title: "Delete",
                            onclick: delete,
                            Icon { width: 14, height: 14, icon: fi_icons::FiTrash2 }
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
