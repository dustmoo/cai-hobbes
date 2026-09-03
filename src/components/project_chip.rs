//! The chat bar's project chip: quiet session metadata, not a button
//! competing with the icon cluster. Tagged → the project title in a faint
//! pill; untagged → a dim "+ project" invitation. Click opens a picker
//! mirroring the connector-selector popover idiom; any manual choice
//! (including "No project") sets `project_tag_user_set`, which the
//! auto-tagger permanently respects.

use dioxus::prelude::*;

use crate::session::SessionState;
use crate::session_events::{log_event, SessionEvent};
use crate::settings::Settings;

#[component]
pub fn ProjectChip() -> Element {
    let mut session_state = use_context::<Signal<SessionState>>();
    let planner = use_context::<Signal<crate::todo::PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut open = use_signal(|| false);

    // Projects are a planner concept; no planner, no chip.
    if !settings.read().planner_enabled {
        return rsx! {};
    }

    let session_id = session_state.read().active_session_id.clone();
    let current: Option<String> = session_state
        .read()
        .sessions
        .get(&session_id)
        .and_then(|s| s.project_id.clone());

    let (label, projects): (Option<String>, Vec<(String, String)>) = {
        let p = planner.read();
        let label = current.as_deref().map(|id| {
            crate::services::project_tagger::project_title(&p.projects, id)
                .unwrap_or("(missing project)")
                .to_string()
        });
        let mut rows: Vec<(String, String)> = p
            .projects
            .iter()
            .filter(|pr| {
                matches!(
                    pr.status,
                    crate::todo::model::TodoStatus::Open
                        | crate::todo::model::TodoStatus::InProgress
                )
            })
            .map(|pr| (pr.id.clone(), pr.title.clone()))
            .collect();
        rows.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        (label, rows)
    };

    // Nothing to pick from and nothing tagged: stay out of the toolbar
    // entirely rather than inviting into an empty picker.
    if projects.is_empty() && label.is_none() {
        return rsx! {};
    }

    let mut set_tag = move |project_id: Option<String>| {
        let sid = session_state.peek().active_session_id.clone();
        {
            let mut state = session_state.write();
            if let Some(s) = state.sessions.get_mut(&sid) {
                s.project_id = project_id.clone();
                s.project_tag_user_set = true;
            }
        }
        log_event(
            &sid,
            SessionEvent::ProjectTagged {
                project_id,
                user_set: true,
            },
        );
        crate::session::SessionState::save_signal(&session_state, None);
        open.set(false);
    };

    rsx! {
        div {
            class: "relative",
            if let Some(title) = &label {
                button {
                    class: "max-w-44 flex items-center gap-1.5 px-2.5 py-1 rounded-full border border-faint text-xs text-fg-muted cursor-pointer hover:text-fg hover:border-primary-500 hover:bg-card transition-colors select-none",
                    title: "Project: {title} — click to change",
                    onclick: move |_| {
                        let v = !*open.peek();
                        open.set(v);
                    },
                    span { class: "truncate", "{title}" }
                    // The caret is the "this opens something" signal — a
                    // label without it reads as static metadata.
                    span { class: "text-[9px] opacity-60 shrink-0", "▾" }
                }
            } else {
                button {
                    class: "flex items-center gap-1 px-2.5 py-1 rounded-full border border-dashed border-faint text-xs text-fg-muted/70 cursor-pointer hover:text-fg hover:border-subtle hover:bg-card transition-colors select-none",
                    title: "Tag this chat to a project",
                    onclick: move |_| {
                        let v = !*open.peek();
                        open.set(v);
                    },
                    span { "+ project" }
                }
            }

            if *open.read() {
                div {
                    class: "absolute bottom-10 left-0 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 overflow-hidden py-1 max-h-72 overflow-y-auto",
                    p {
                        class: "px-4 pt-2 pb-1 text-[11px] uppercase tracking-wider text-fg-muted",
                        "Tag this chat"
                    }
                    button {
                        class: "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors",
                        onclick: move |_| set_tag(None),
                        "No project"
                    }
                    for (pid, title) in projects {
                        {
                            let is_current = current.as_deref() == Some(pid.as_str());
                            let key = pid.clone();
                            let row_class = if is_current {
                                "w-full text-left px-4 py-2 text-sm text-fg bg-primary-900/40 transition-colors flex items-center justify-between"
                            } else {
                                "w-full text-left px-4 py-2 text-sm text-fg-muted hover:bg-primary-900/50 hover:text-fg transition-colors flex items-center justify-between"
                            };
                            rsx! {
                                button {
                                    key: "{key}",
                                    class: "{row_class}",
                                    onclick: move |_| set_tag(Some(pid.clone())),
                                    span { class: "truncate", "{title}" }
                                    if is_current {
                                        span { class: "text-primary-400 text-xs shrink-0", "✓" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
