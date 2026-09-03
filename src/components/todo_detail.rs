//! The todo detail card (U2.1): the one surface where everything the model
//! holds on a todo is visible and editable — notes, checklist, dates, tags,
//! off-ladder estimates, and the read-only provenance the AI stamps.
//!
//! Every mutation flows through the shared sync primitives so this card obeys
//! the same contracts as the row menus and the AI handlers: date changes run
//! `prune_blocks_for_todo`, estimate changes run `resize_blocks_to_estimate`,
//! plain field edits go through `mutate_todo`.

use chrono::{Local, NaiveDate};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::planner_view::{mutate_todo, persist_block};
use crate::todo::model::{self, ChecklistItem, TimeOfDay, TodoOrigin, TodoStatus};
use crate::todo::store;
use crate::todo::views;
use crate::todo::PlannerState;

/// Which todo's detail card is open. Provided at the PlannerView root,
/// written by TodoRow's info button, read by [`TodoDetailCard`].
#[derive(Clone, Copy)]
pub struct TodoDetailContext(pub Signal<Option<String>>);

/// What committing the estimate field means: `None` = not a duration (leave
/// the todo alone), `Some(None)` = clear the estimate, `Some(Some(m))` = set.
pub(crate) fn parse_estimate_input(raw: &str) -> Option<Option<u32>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    crate::todo::quick_add::parse_duration_minutes(trimmed).map(Some)
}

/// Compact estimate text that round-trips through the duration parser
/// ("1h30m", "45m") — `format_minutes`' "1h 35m" contains a space the parser
/// rejects, so it can't seed the input.
pub(crate) fn estimate_input_value(minutes: Option<u32>) -> String {
    match minutes {
        None => String::new(),
        Some(m) => match (m / 60, m % 60) {
            (0, m) => format!("{}m", m),
            (h, 0) => format!("{}h", h),
            (h, m) => format!("{}h{}m", h, m),
        },
    }
}

/// Same tag rule as the quick-add `#` token: alnum plus `-`/`_`.
pub(crate) fn valid_tag(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[component]
pub fn TodoDetailCard() -> Element {
    let open = use_context::<TodoDetailContext>().0;
    let id = open.read().clone();
    match id {
        // Keyed so the commit-on-blur buffers reset when a different todo opens.
        Some(id) => rsx! {
            DetailCardInner { key: "{id}", todo_id: id.clone() }
        },
        None => rsx! {},
    }
}

#[component]
fn DetailCardInner(todo_id: String) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let mut open = use_context::<TodoDetailContext>().0;

    // Buffers for commit-on-Enter/blur fields, seeded once at mount (the
    // component is keyed by todo id, so a different todo reseeds them).
    let seed = planner.peek().todo(&todo_id).cloned();
    let mut title_buf =
        use_signal(|| seed.as_ref().map(|t| t.title.clone()).unwrap_or_default());
    let mut notes_buf =
        use_signal(|| seed.as_ref().map(|t| t.notes.clone()).unwrap_or_default());
    let mut estimate_buf =
        use_signal(|| estimate_input_value(seed.as_ref().and_then(|t| t.estimate_minutes)));
    let mut checklist_draft = use_signal(String::new);
    let mut tag_draft = use_signal(String::new);

    // If the todo disappears underneath (deleted from a row, by the AI, or
    // from another surface) the card must not keep editing a ghost.
    {
        let todo_id = todo_id.clone();
        use_effect(move || {
            if planner.read().todo(&todo_id).is_none() {
                open.set(None);
            }
        });
    }

    // Everything below binds live from state each render, so AI edits made
    // while the card is open show immediately.
    let Some(todo) = planner.read().todo(&todo_id).cloned() else {
        return rsx! {};
    };
    let id = todo_id.clone();

    let commit_title = {
        let id = id.clone();
        move || {
            let new_title = title_buf.peek().trim().to_string();
            if new_title.is_empty() {
                // An empty title isn't a todo — restore rather than commit.
                let cur = planner.peek().todo(&id).map(|t| t.title.clone()).unwrap_or_default();
                title_buf.set(cur);
                return;
            }
            mutate_todo(planner, &id, |t| t.title = new_title);
        }
    };

    let commit_notes = {
        let id = id.clone();
        move || {
            let notes = notes_buf.peek().clone();
            mutate_todo(planner, &id, |t| t.notes = notes);
        }
    };

    // Date changes mirror the row's schedule menu exactly: mutate, then prune
    // the blocks left on days that no longer match (deleting their store rows).
    let set_scheduled = {
        let id = id.clone();
        move |date: Option<NaiveDate>| {
            mutate_todo(planner, &id, |t| {
                t.scheduled_for = date;
                if date.is_none() {
                    // Same rule as the row's "Clear date": an undated todo has
                    // no day for a time-of-day group to belong to.
                    t.time_of_day = None;
                }
            });
            let keep = planner.peek().todo(&id).and_then(|t| t.scheduled_for);
            let pruned = planner.write().prune_blocks_for_todo(&id, keep);
            for b in &pruned {
                if let Err(e) = store::delete_block(&b.id) {
                    tracing::error!("planner: failed to delete rescheduled block {}: {}", b.id, e);
                }
            }
        }
    };
    let mut set_scheduled_clear = set_scheduled.clone();
    let mut set_scheduled_input = set_scheduled;

    let commit_estimate = {
        let id = id.clone();
        move || {
            let raw = estimate_buf.peek().clone();
            match parse_estimate_input(&raw) {
                // Not a duration — restore the stored value rather than guess.
                None => {
                    let cur = planner.peek().todo(&id).and_then(|t| t.estimate_minutes);
                    estimate_buf.set(estimate_input_value(cur));
                }
                // Empty clears the estimate; the block (if any) is left alone.
                Some(None) => mutate_todo(planner, &id, |t| t.estimate_minutes = None),
                Some(Some(m)) => {
                    mutate_todo(planner, &id, |t| t.estimate_minutes = Some(m));
                    // The estimate IS the timebox's length (same as the chip).
                    for b in planner.write().resize_blocks_to_estimate(&id) {
                        persist_block(&b);
                    }
                    estimate_buf.set(estimate_input_value(Some(m)));
                }
            }
        }
    };

    let add_checklist_item = {
        let id = id.clone();
        move || {
            let title = checklist_draft.peek().trim().to_string();
            if title.is_empty() {
                return;
            }
            mutate_todo(planner, &id, |t| {
                // Same id scheme as the AI handlers' checklist items.
                t.checklist.push(ChecklistItem {
                    id: uuid::Uuid::new_v4().to_string(),
                    title,
                    done: false,
                });
            });
            checklist_draft.set(String::new());
        }
    };

    let add_tag = {
        let id = id.clone();
        move || {
            let tag = tag_draft.peek().trim().trim_start_matches('#').to_string();
            if !valid_tag(&tag) {
                return;
            }
            mutate_todo(planner, &id, |t| {
                if !t.tags.contains(&tag) {
                    t.tags.push(tag);
                }
            });
            tag_draft.set(String::new());
        }
    };

    let status_label = match todo.status {
        TodoStatus::Open => "Open",
        TodoStatus::InProgress => "In progress",
        TodoStatus::Completed => "Completed",
        TodoStatus::Cancelled => "Cancelled",
    };
    // One date formatter across the planner (U3.6).
    let today = Local::now().date_naive();
    let completed_label = todo.status.is_closed().then(|| {
        todo.completed_at
            .map(|d| views::friendly_date(d.with_timezone(&Local).date_naive(), today))
            .unwrap_or_default()
    });
    let scheduled_value = todo.scheduled_for.map(|d| d.to_string()).unwrap_or_default();
    let deadline_value = todo.deadline.map(|d| d.to_string()).unwrap_or_default();
    let origin_label = match &todo.origin {
        TodoOrigin::User => "User".to_string(),
        TodoOrigin::Ai { session_id } => {
            format!("AI · session {}", session_id.chars().take(8).collect::<String>())
        }
    };
    let created_label =
        views::friendly_date(todo.created_at.with_timezone(&Local).date_naive(), today);

    let label_class = "pb-1 text-xs font-medium text-fg-muted";
    let input_class = "w-full rounded border border-subtle bg-input px-2 py-1.5 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint";
    let clear_class = "shrink-0 rounded px-2 py-1 text-xs text-fg-muted hover:bg-input hover:text-fg";
    let tod_options: [(&str, TimeOfDay); 3] = [
        ("Morning", TimeOfDay::Morning),
        ("Afternoon", TimeOfDay::Afternoon),
        ("Evening", TimeOfDay::Evening),
    ];

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/70",
            onclick: move |_| open.set(None),
            div {
                class: "flex max-h-[80vh] w-[480px] flex-col gap-4 overflow-y-auto rounded-lg border border-subtle bg-section p-5",
                tabindex: "0",
                autofocus: true,
                onmounted: move |evt| {
                    let mounted = evt.data();
                    spawn(async move {
                        let _ = mounted.set_focus(true).await;
                    });
                },
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape {
                        open.set(None);
                    }
                },
                onclick: move |e| e.stop_propagation(),

                // Title + status
                div {
                    input {
                        class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm font-medium text-fg focus:outline-none focus:border-faint",
                        r#type: "text",
                        value: "{title_buf}",
                        oninput: move |evt| title_buf.set(evt.value()),
                        onkeydown: {
                            let mut commit = commit_title.clone();
                            move |evt: KeyboardEvent| {
                                if evt.key() == Key::Enter {
                                    evt.prevent_default();
                                    commit();
                                }
                            }
                        },
                        onblur: {
                            let mut commit = commit_title.clone();
                            move |_| commit()
                        },
                    }
                    div {
                        class: "flex items-center gap-2 pt-1.5 text-xs text-fg-muted",
                        span { "{status_label}" }
                        if let Some(c) = completed_label {
                            if !c.is_empty() {
                                span { "· {c}" }
                            }
                        }
                    }
                }

                // Notes
                div {
                    p { class: "{label_class}", "Notes" }
                    textarea {
                        class: "h-24 w-full resize-y rounded border border-subtle bg-input px-2 py-1.5 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
                        placeholder: "Anything worth remembering…",
                        value: "{notes_buf}",
                        oninput: move |evt| notes_buf.set(evt.value()),
                        onblur: {
                            let commit = commit_notes.clone();
                            move |_| commit()
                        },
                    }
                }

                // Checklist
                div {
                    p { class: "{label_class}", "Checklist" }
                    div {
                        class: "flex flex-col gap-1",
                        for item in todo.checklist.iter() {
                            {
                                let item_id = item.id.clone();
                                let remove_id = item.id.clone();
                                let toggle_todo = id.clone();
                                let remove_todo = id.clone();
                                rsx! {
                                    div {
                                        key: "{item.id}",
                                        class: "group flex items-center gap-2 rounded px-1 py-0.5 hover:bg-card",
                                        input {
                                            class: "shrink-0 accent-current",
                                            r#type: "checkbox",
                                            checked: item.done,
                                            onchange: move |_| {
                                                let item_id = item_id.clone();
                                                mutate_todo(planner, &toggle_todo, move |t| {
                                                    if let Some(step) = t.checklist.iter_mut().find(|s| s.id == item_id) {
                                                        step.done = !step.done;
                                                    }
                                                });
                                            },
                                        }
                                        span {
                                            class: if item.done {
                                                "flex-1 min-w-0 truncate text-sm text-fg-muted line-through"
                                            } else {
                                                "flex-1 min-w-0 truncate text-sm text-fg"
                                            },
                                            "{item.title}"
                                        }
                                        button {
                                            class: "shrink-0 rounded px-1 text-fg-muted opacity-0 group-hover:opacity-100 hover:text-red-400",
                                            title: "Remove step",
                                            onclick: move |_| {
                                                let remove_id = remove_id.clone();
                                                mutate_todo(planner, &remove_todo, move |t| {
                                                    t.checklist.retain(|s| s.id != remove_id);
                                                });
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                        input {
                            class: "{input_class}",
                            r#type: "text",
                            placeholder: "Add a step — press Enter",
                            value: "{checklist_draft}",
                            oninput: move |evt| checklist_draft.set(evt.value()),
                            onkeydown: {
                                let mut add = add_checklist_item.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        add();
                                    }
                                }
                            },
                        }
                    }
                }

                // Dates
                div {
                    class: "grid grid-cols-2 gap-3",
                    div {
                        p { class: "{label_class}", "Scheduled" }
                        div {
                            class: "flex items-center gap-1",
                            input {
                                class: "{input_class}",
                                r#type: "date",
                                value: "{scheduled_value}",
                                oninput: move |evt| {
                                    let raw = evt.value();
                                    if raw.is_empty() {
                                        set_scheduled_input(None);
                                    } else if let Ok(d) = raw.parse::<NaiveDate>() {
                                        set_scheduled_input(Some(d));
                                    }
                                },
                            }
                            if todo.scheduled_for.is_some() {
                                button {
                                    class: "{clear_class}",
                                    onclick: move |_| set_scheduled_clear(None),
                                    "Clear"
                                }
                            }
                        }
                    }
                    div {
                        p { class: "{label_class}", "Deadline" }
                        div {
                            class: "flex items-center gap-1",
                            input {
                                class: "{input_class}",
                                r#type: "date",
                                value: "{deadline_value}",
                                oninput: {
                                    let id = id.clone();
                                    move |evt: FormEvent| {
                                        let raw = evt.value();
                                        if raw.is_empty() {
                                            mutate_todo(planner, &id, |t| t.deadline = None);
                                        } else if let Ok(d) = raw.parse::<NaiveDate>() {
                                            mutate_todo(planner, &id, |t| t.deadline = Some(d));
                                        }
                                    }
                                },
                            }
                            if todo.deadline.is_some() {
                                button {
                                    class: "{clear_class}",
                                    onclick: {
                                        let id = id.clone();
                                        move |_| mutate_todo(planner, &id, |t| t.deadline = None)
                                    },
                                    "Clear"
                                }
                            }
                        }
                    }
                }

                // Time of day + estimate
                div {
                    class: "grid grid-cols-2 gap-3",
                    div {
                        p { class: "{label_class}", "Time of day" }
                        div {
                            class: "flex gap-1",
                            for (label, value) in tod_options {
                                {
                                    let active = todo.time_of_day == Some(value);
                                    let id = id.clone();
                                    rsx! {
                                        button {
                                            key: "{label}",
                                            class: if active {
                                                "rounded border border-subtle bg-btn-primary px-2 py-1 text-xs text-fg"
                                            } else {
                                                "rounded border border-faint px-2 py-1 text-xs text-fg-muted hover:bg-input hover:text-fg"
                                            },
                                            // Clicking the active segment clears it (three-way toggle).
                                            onclick: move |_| {
                                                mutate_todo(planner, &id, move |t| {
                                                    t.time_of_day = if t.time_of_day == Some(value) {
                                                        None
                                                    } else {
                                                        Some(value)
                                                    };
                                                });
                                            },
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div {
                        p { class: "{label_class}", "Estimate" }
                        input {
                            class: "{input_class}",
                            r#type: "text",
                            placeholder: "45, 1h30m — empty clears",
                            value: "{estimate_buf}",
                            oninput: move |evt| estimate_buf.set(evt.value()),
                            onkeydown: {
                                let mut commit = commit_estimate.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        commit();
                                    }
                                }
                            },
                            onblur: {
                                let mut commit = commit_estimate.clone();
                                move |_| commit()
                            },
                        }
                    }
                }

                // Tags
                div {
                    p { class: "{label_class}", "Tags" }
                    div {
                        class: "flex flex-wrap items-center gap-1.5",
                        for tag in todo.tags.iter() {
                            {
                                let tag_name = tag.clone();
                                let id = id.clone();
                                rsx! {
                                    span {
                                        key: "{tag}",
                                        class: "flex items-center gap-1 rounded-full bg-input px-2 py-0.5 text-xs text-fg-muted",
                                        "#{tag}"
                                        button {
                                            class: "text-fg-muted hover:text-red-400",
                                            title: "Remove tag",
                                            onclick: move |_| {
                                                let tag_name = tag_name.clone();
                                                mutate_todo(planner, &id, move |t| {
                                                    t.tags.retain(|x| *x != tag_name);
                                                });
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                        input {
                            class: "w-28 rounded border border-subtle bg-input px-2 py-0.5 text-xs text-fg placeholder:text-fg-muted focus:outline-none focus:border-faint",
                            r#type: "text",
                            placeholder: "add tag ⏎",
                            value: "{tag_draft}",
                            oninput: move |evt| tag_draft.set(evt.value()),
                            onkeydown: {
                                let mut add = add_tag.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter {
                                        evt.prevent_default();
                                        add();
                                    }
                                }
                            },
                        }
                    }
                }

                // Latest progress from the linked fleet session (read-only —
                // reported by briefs, never driving status).
                if let Some(progress) = &todo.latest_progress {
                    div {
                        class: "rounded border border-faint bg-input/30 px-3 py-2",
                        p { class: "text-[11px] uppercase tracking-wider text-fg-muted mb-1", "Latest progress" }
                        p { class: "text-xs text-fg leading-relaxed", "{progress}" }
                    }
                }

                // Read-only footer
                div {
                    class: "flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-faint pt-3 text-xs text-fg-muted",
                    if todo.actual_minutes > 0 {
                        span { "Actual: {model::format_minutes(todo.actual_minutes)}" }
                    }
                    span { "{origin_label}" }
                    span { "Created {created_label}" }
                    div { class: "flex-1" }
                    button {
                        class: "rounded p-1 text-fg-muted hover:bg-input hover:text-fg",
                        title: "Close (Esc)",
                        onclick: move |_| open.set(None),
                        Icon { width: 14, height: 14, icon: fi_icons::FiX }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_input_accepts_minutes_and_unit_forms() {
        assert_eq!(parse_estimate_input("45"), Some(Some(45)));
        assert_eq!(parse_estimate_input(" 1h30m "), Some(Some(90)));
        assert_eq!(parse_estimate_input("2h"), Some(Some(120)));
        // Empty clears; junk leaves the todo alone.
        assert_eq!(parse_estimate_input(""), Some(None));
        assert_eq!(parse_estimate_input("   "), Some(None));
        assert_eq!(parse_estimate_input("soon"), None);
        assert_eq!(parse_estimate_input("1h30"), None);
        assert_eq!(parse_estimate_input("0"), None);
    }

    #[test]
    fn estimate_input_value_round_trips_through_the_parser() {
        for m in [1, 15, 45, 60, 90, 95, 120, 150] {
            let text = estimate_input_value(Some(m));
            assert_eq!(
                parse_estimate_input(&text),
                Some(Some(m)),
                "'{}' must parse back to {}m",
                text,
                m
            );
        }
        assert_eq!(estimate_input_value(None), "");
        assert_eq!(estimate_input_value(Some(90)), "1h30m");
        assert_eq!(estimate_input_value(Some(120)), "2h");
        assert_eq!(estimate_input_value(Some(45)), "45m");
    }

    #[test]
    fn tags_follow_the_quick_add_rule() {
        assert!(valid_tag("writing"));
        assert!(valid_tag("q3-report"));
        assert!(valid_tag("deep_work"));
        assert!(!valid_tag(""));
        assert!(!valid_tag("two words"));
        assert!(!valid_tag("a#b"));
    }
}
