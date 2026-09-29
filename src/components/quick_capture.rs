//! Quick-capture palette: `hotkeys.quick_capture` (Cmd+K by default) opens a
//! one-line input over whatever is on screen and drops the line straight into
//! the Planner, so capturing never means leaving the chat.
//!
//! It speaks the same token grammar as the Planner's inline quick-add
//! (`todo::quick_add`) through the same `QuickAddField`, and commits through
//! the same `commit_quick_add`, so a captured todo is indistinguishable from
//! one typed in the Planner. With no view context to borrow, capture lands in
//! Inbox unless a token (or the project the Planner is showing) says where.

use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::focus_context::FocusContext;
use crate::components::planner_rows::{
    apply_project_pick, birth_project_title, commit_quick_add, landing_label, resolve_birth,
    BirthProject, Inherited, InheritedFrom, ProjectPicker, QuickAddDraft, QuickAddField,
};
use crate::components::planner_view::{today, PlannerFocusHint, PlannerSelection};
use crate::hotkey::display_combo;
use crate::settings::Settings;
use crate::todo::views::TodoView;
use crate::todo::PlannerState;

/// Mounted once at the app root. Renders nothing while closed; the card is
/// remounted on every open so the draft always starts empty.
#[component]
pub fn QuickCapture(open: Signal<bool>) -> Element {
    let mut focus_context = use_context::<Signal<FocusContext>>();
    // Whoever owned keyboard focus before we opened gets it back on close.
    let mut previous_focus = use_signal(|| None::<FocusContext>);

    use_effect(move || {
        if *open.read() {
            let current = *focus_context.peek();
            if current != FocusContext::QuickCapture {
                previous_focus.set(Some(current));
                focus_context.set(FocusContext::QuickCapture);
            }
        } else if let Some(prev) = previous_focus.take() {
            focus_context.set(prev);
        }
    });

    if !*open.read() {
        return rsx! {};
    }
    rsx! {
        QuickCaptureCard { open }
    }
}

#[component]
fn QuickCaptureCard(open: Signal<bool>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let focus_hint = use_context::<PlannerFocusHint>().0;
    // Sampled once, at open: with the Planner showing a project, capture
    // files there by default (a dashed pill, cleared with × or ⌫). peek, so
    // Planner re-renders never re-render the palette.
    let mut draft = use_signal(|| {
        QuickAddDraft::with_inherited(match focus_hint.peek().clone() {
            Some(PlannerSelection::Project(id)) => Some(Inherited {
                project_id: id,
                why: InheritedFrom::View,
            }),
            _ => None,
        })
    });
    // Confirmation of the last Shift+Enter capture, shown while the palette
    // stays open for the next one.
    let mut last_captured = use_signal(|| None::<(String, String)>);

    let submit = move |keep_open: bool| {
        let day = today();
        let d = draft.peek().clone();
        let block_start = d.parse(&planner.peek(), day).block_start;
        let Some(todo) = commit_quick_add(planner, settings, &d, &capture_selection()) else {
            return; // no title yet — the preview already says so
        };
        if keep_open {
            let project = todo.project_id.as_ref().and_then(|id| {
                planner.peek().projects.iter().find(|p| &p.id == id).map(|p| p.title.clone())
            });
            let landing =
                landing_label(project.as_deref(), todo.scheduled_for, todo.bucket, block_start, day);
            last_captured.set(Some((todo.title, landing)));
            // "Add another" carries the project forward, as an inherited pill.
            draft.set(QuickAddDraft::with_inherited(todo.project_id.map(|project_id| Inherited {
                project_id,
                why: InheritedFrom::LastCapture,
            })));
        } else {
            open.set(false);
        }
    };

    // Where Enter would file the current draft — the header and footer.
    let (dest_title, dest_id, landing) = {
        let day = today();
        let state = planner.read();
        let d = draft.read();
        let parsed = d.parse(&state, day);
        let birth = resolve_birth(&parsed, &d, &capture_selection(), day);
        let project = birth_project_title(&state, &birth.project);
        let landing = (!d.text.trim().is_empty() || project.is_some()).then(|| {
            landing_label(project.as_deref(), birth.scheduled_for, birth.bucket, parsed.block_start, day)
        });
        let current = match &birth.project {
            BirthProject::Existing(id) => Some(id.clone()),
            _ => None,
        };
        (project, current, landing)
    };

    let combo = display_combo(&settings.read().hotkeys.quick_capture);

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-start justify-center bg-black/50 backdrop-blur-sm pt-[18vh] px-4",
            onclick: move |_| open.set(false),
            div {
                class: "w-[560px] max-w-full rounded-xl border border-subtle bg-section shadow-2xl",
                onclick: |e| e.stop_propagation(),
                onkeydown: move |evt: KeyboardEvent| {
                    // Esc anywhere in the card dismisses; stop it here so no
                    // other overlay underneath also reacts. The input stops
                    // its own Esc first while the project list is open.
                    if evt.key() == Key::Escape {
                        evt.prevent_default();
                        evt.stop_propagation();
                        open.set(false);
                    }
                },

                div {
                    class: "flex items-center gap-2 px-4 pt-3 text-[11px] uppercase tracking-wide text-fg-muted",
                    CaptureDestination { draft, title: dest_title, current: dest_id }
                    div { class: "flex-1" }
                    if !combo.is_empty() {
                        span { class: "font-mono normal-case tracking-normal", "{combo}" }
                    }
                }

                div {
                    class: "px-4 pt-2 pb-3",
                    QuickAddField {
                        draft,
                        selection: capture_selection(),
                        on_submit: submit,
                        on_escape: move |_| open.set(false),
                        input_class: "w-full bg-transparent border-0 p-0 text-base text-fg placeholder:text-fg-muted focus:outline-none focus:ring-0",
                        placeholder: "What needs doing? ~30m · #tag · +project · @2pm · *mon · !fri",
                        autofocus: true,
                    }
                    if let Some((title, landing)) = last_captured.read().clone() {
                        div {
                            class: "mt-2 flex items-center gap-1.5 text-[11px] text-fg-muted",
                            Icon { width: 11, height: 11, icon: fi_icons::FiCheck }
                            span { class: "truncate", "Added “{title}” → {landing}" }
                        }
                    }
                }

                div {
                    class: "flex items-center gap-3 border-t border-faint px-4 py-2 text-[11px] text-fg-muted",
                    match landing {
                        Some(l) => rsx! { span { class: "truncate", "Lands in {l}" } },
                        None => rsx! { span { "Lands in Inbox unless +project, *day or @time says otherwise" } },
                    }
                    div { class: "flex-1" }
                    span { class: "shrink-0", "↵ add" }
                    span { class: "shrink-0", "⇧↵ add another" }
                    span { class: "shrink-0", "esc close" }
                }
            }
        }
    }
}

/// With no view to borrow, the palette births into Inbox; tokens, an
/// inherited project, or a pick move it from there.
fn capture_selection() -> PlannerSelection {
    PlannerSelection::View(TodoView::Inbox)
}

/// The header: "Capture to Inbox ▾" / "Capture to Hobbes RC ▾" — the same
/// picker as the project pill, editing the same text.
#[component]
fn CaptureDestination(
    draft: Signal<QuickAddDraft>,
    title: Option<String>,
    current: Option<String>,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut picker_open = use_signal(|| false);
    rsx! {
        div {
            class: "relative",
            button {
                class: "flex items-center gap-2 uppercase tracking-wide hover:text-fg",
                title: "Choose a project",
                onclick: move |evt| {
                    evt.stop_propagation();
                    picker_open.toggle();
                },
                if let Some(t) = title.clone() {
                    Icon { width: 12, height: 12, icon: fi_icons::FiFolder }
                    span { class: "normal-case tracking-normal text-fg max-w-80 truncate", "Capture to {t}" }
                } else {
                    Icon { width: 12, height: 12, icon: fi_icons::FiInbox }
                    span { "Capture to Inbox" }
                }
                span { "▾" }
            }
            if *picker_open.read() {
                div {
                    class: "fixed inset-0 z-40",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        picker_open.set(false);
                    },
                }
                div {
                    class: "absolute left-0 top-full mt-1 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 py-1 normal-case tracking-normal",
                    onkeydown: move |evt: KeyboardEvent| {
                        if evt.key() == Key::Escape {
                            evt.prevent_default();
                            evt.stop_propagation();
                            picker_open.set(false);
                        }
                    },
                    ProjectPicker {
                        current,
                        heading: "Capture to…",
                        on_pick: move |pick: Option<String>| {
                            apply_project_pick(draft, &planner.peek(), pick);
                            picker_open.set(false);
                        },
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::planner_timeline::fmt_hhmm;
    use crate::todo::model::TodoBucket;
    use crate::todo::views;
    use chrono::NaiveDate;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 24).unwrap()
    }

    #[test]
    fn untriaged_capture_lands_in_inbox() {
        assert_eq!(landing_label(None, None, TodoBucket::Inbox, None, day()), "Inbox");
    }

    #[test]
    fn scheduled_today_with_a_block_names_the_time() {
        assert_eq!(
            landing_label(None, Some(day()), TodoBucket::Anytime, Some(14 * 60), day()),
            format!("Today at {}", fmt_hhmm(14 * 60))
        );
    }

    #[test]
    fn scheduled_later_uses_the_planner_date_wording() {
        let later = day().succ_opt().unwrap();
        assert_eq!(
            landing_label(None, Some(later), TodoBucket::Anytime, None, day()),
            views::friendly_date(later, day())
        );
    }

    #[test]
    fn landing_label_prefixes_project() {
        let later = day().succ_opt().unwrap();
        assert_eq!(
            landing_label(Some("Hobbes RC"), Some(later), TodoBucket::Anytime, Some(14 * 60), day()),
            format!("Hobbes RC · {} at {}", views::friendly_date(later, day()), fmt_hhmm(14 * 60))
        );
        // A project's Anytime is just the project.
        assert_eq!(
            landing_label(Some("Hobbes RC"), None, TodoBucket::Anytime, None, day()),
            "Hobbes RC"
        );
        assert_eq!(
            landing_label(Some("Hobbes RC"), None, TodoBucket::Someday, None, day()),
            "Hobbes RC · Someday"
        );
    }
}
