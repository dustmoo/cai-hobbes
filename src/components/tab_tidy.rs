#![allow(non_snake_case)]
//! The "Tidy tabs" sheet: review the suggestions from `crate::tab_tidy` and
//! apply them. A pure view over the snapshot it is handed — it closes and
//! tags nothing itself; `main.rs` applies the `TidyApply` it emits through
//! the same close-tab and project-tag paths the sidebar and chat chip use.
//!
//! Selection state is kept as *exceptions* to the suggestions (deselected
//! ids, project overrides), keyed by session id, so the sheet stays right if
//! its snapshot is refreshed while it is open.

use crate::tab_tidy::{
    suggest, TidyProject, TidySuggestions, TidyTab, TidyTodo, UnsortedTab,
};
use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::collections::{HashMap, HashSet};

/// What the user confirmed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TidyApply {
    /// Session ids of tabs to close.
    pub close: Vec<String>,
    /// (session id, project id) tags to set.
    pub tag: Vec<(String, String)>,
}

#[derive(Props, PartialEq, Clone)]
pub struct TidyTabsSheetProps {
    pub tabs: Vec<TidyTab>,
    pub todos: Vec<TidyTodo>,
    /// Projects the user can file tabs into (open ones, in a stable order).
    pub projects: Vec<TidyProject>,
    pub on_apply: EventHandler<TidyApply>,
    pub on_cancel: EventHandler<()>,
}

fn short_date(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&Local).format("%b %d").to_string()
}

const SECTION: &str = "flex items-center gap-2.5 text-[13px] font-medium text-fg";
const WHY: &str = "font-mono text-[10.5px] text-fg-muted whitespace-nowrap";
const ITEM: &str = "flex items-center gap-2.5 h-[30px] px-2.5 rounded-md text-[12.5px] text-fg even:bg-card/40";
const CB: &str = "w-3.5 h-3.5 shrink-0 accent-primary-500";
const BTN: &str = "h-[30px] px-3.5 rounded-lg border border-subtle bg-input text-[13px] font-medium text-fg hover:bg-card focus:outline-none";
const BTN_PRI: &str = "h-[30px] px-3.5 rounded-lg border border-primary-600 bg-primary-600 text-[13px] font-medium text-white hover:bg-primary-500 focus:outline-none disabled:opacity-40 disabled:cursor-not-allowed";

pub fn TidyTabsSheet(props: TidyTabsSheetProps) -> Element {
    let mut deselected = use_signal(HashSet::<String>::new);
    // `Some(None)` = the user chose "keep unsorted" for that tab.
    let mut overrides = use_signal(HashMap::<String, Option<String>>::new);
    // Unsorted tabs the person ticked to close (opt-in: nothing suggests
    // closing them). Filing is the project chip, independent of closing.
    let mut unsorted_close = use_signal(HashSet::<String>::new);
    let mut menu_for = use_signal(|| None::<String>);
    // Where the open project menu sits: the click point, in viewport px.
    // The menu is `position: fixed` so the scrolling list can't clip it.
    let mut menu_at = use_signal(|| (0.0f64, 0.0f64));

    let sug: TidySuggestions = suggest(&props.tabs, &props.todos, &props.projects, Utc::now());
    let tab_of = |id: &str| props.tabs.iter().find(|t| t.session_id == id);
    let label_of = |id: &str| {
        tab_of(id)
            .map(|t| t.topic.clone().unwrap_or_else(|| t.title.clone()))
            .unwrap_or_default()
    };
    let emoji_of = |id: &str| tab_of(id).and_then(|t| t.emoji.clone()).unwrap_or_default();
    let project_title = |pid: &str| {
        props
            .projects
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.title.clone())
            .unwrap_or_default()
    };
    let effective_project = |u: &UnsortedTab| -> Option<String> {
        match overrides.read().get(&u.session_id) {
            Some(choice) => choice.clone(),
            None => u.suggested_project.clone(),
        }
    };
    let closes_unsorted = |u: &UnsortedTab| unsorted_close.read().contains(&u.session_id);

    let finished_ids: Vec<String> = sug.finished.iter().map(|f| f.session_id.clone()).collect();
    let idle_ids: Vec<String> = sug.idle.iter().map(|i| i.session_id.clone()).collect();
    let all_finished_on = finished_ids.iter().all(|id| !deselected.read().contains(id));
    let all_idle_on = idle_ids.iter().all(|id| !deselected.read().contains(id));
    let unsorted_ids: Vec<String> = sug.unsorted.iter().map(|u| u.session_id.clone()).collect();
    let all_unsorted_closing = !unsorted_ids.is_empty() && sug.unsorted.iter().all(&closes_unsorted);

    // Filed: every unsorted tab whose chip names a project (a suggestion,
    // or the person's pick). A tab both filed and closed is filed first, so
    // it lands in History under that project.
    let tag: Vec<(String, String)> = sug
        .unsorted
        .iter()
        .filter_map(|u| effective_project(u).map(|p| (u.session_id.clone(), p)))
        .collect();
    let group_count = tag.len();
    let close: Vec<String> = finished_ids
        .iter()
        .chain(idle_ids.iter())
        .filter(|id| !deselected.read().contains(*id))
        .cloned()
        .chain(sug.unsorted.iter().filter(|u| closes_unsorted(u)).map(|u| u.session_id.clone()))
        .collect();
    let close_count = close.len();
    let suggestion_count = sug.finished.len() + sug.idle.len() + sug.unsorted.len();

    let apply = {
        let close = close.clone();
        let tag = tag.clone();
        let on_apply = props.on_apply;
        move |_| on_apply.call(TidyApply { close: close.clone(), tag: tag.clone() })
    };
    let primary_label = format!("Close {close_count} · file {group_count}");

    let mut toggle_close = move |ids: Vec<String>, on: bool| {
        let mut d = deselected.write();
        for id in ids {
            if on {
                d.remove(&id);
            } else {
                d.insert(id);
            }
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/50",
            tabindex: "0",
            onmounted: move |evt| {
                let mounted = evt.data();
                spawn(async move {
                    let _ = mounted.set_focus(true).await;
                });
            },
            onclick: move |_| props.on_cancel.call(()),
            onkeydown: move |evt: KeyboardEvent| {
                if evt.key() == Key::Escape {
                    props.on_cancel.call(());
                }
            },
            div {
                class: "flex flex-col gap-3 w-[560px] max-w-[92vw] max-h-[86vh] bg-section border border-faint rounded-xl shadow-xl px-[18px] py-4",
                role: "dialog",
                "aria-label": "Tidy tabs",
                onclick: move |evt| {
                    evt.stop_propagation();
                    menu_for.set(None);
                },

                div {
                    class: "flex items-baseline gap-2.5",
                    span { class: "text-lg font-semibold text-fg", "Tidy tabs" }
                    span {
                        class: "text-[12.5px] text-fg-muted",
                        "{props.tabs.len()} open · {suggestion_count} suggestions"
                    }
                }

                div {
                    class: "flex-1 min-h-0 overflow-y-auto flex flex-col gap-3 pr-1",
                    // A fixed menu would float away from its row: close it.
                    onscroll: move |_| {
                        if menu_for.peek().is_some() {
                            menu_for.set(None);
                        }
                    },
                    if suggestion_count == 0 {
                        p { class: "text-sm text-fg-muted py-6 text-center", "Nothing to tidy. Every open tab is in use or already filed." }
                    }

                    if !sug.finished.is_empty() {
                        div {
                            class: "{SECTION}",
                            input {
                                r#type: "checkbox",
                                class: "{CB}",
                                "aria-label": "Close finished tabs",
                                checked: all_finished_on,
                                onchange: {
                                    let ids = finished_ids.clone();
                                    move |evt: FormEvent| toggle_close(ids.clone(), evt.checked())
                                },
                            }
                            span { "Close finished" }
                            span { class: "{WHY}", "linked to-do is done" }
                        }
                        div {
                            class: "flex flex-col",
                            for f in sug.finished.iter() {
                                {
                                    let id = f.session_id.clone();
                                    let checked = !deselected.read().contains(&id);
                                    let why = format!("to-do done {}", short_date(f.done_at));
                                    let label = label_of(&id);
                                    let emoji = emoji_of(&id);
                                    rsx! {
                                        div {
                                            key: "{id}",
                                            class: "{ITEM}",
                                            input {
                                                r#type: "checkbox",
                                                class: "{CB}",
                                                checked,
                                                onchange: {
                                                    let id = id.clone();
                                                    move |evt: FormEvent| toggle_close(vec![id.clone()], evt.checked())
                                                },
                                            }
                                            span { class: "w-4 text-center", "{emoji}" }
                                            span { class: "flex-1 min-w-0 truncate", "{label}" }
                                            span { class: "{WHY}", "{why}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if !sug.idle.is_empty() {
                        div {
                            class: "{SECTION}",
                            input {
                                r#type: "checkbox",
                                class: "{CB}",
                                "aria-label": "Close idle tabs",
                                checked: all_idle_on,
                                onchange: {
                                    let ids = idle_ids.clone();
                                    move |evt: FormEvent| toggle_close(ids.clone(), evt.checked())
                                },
                            }
                            span { "Close idle" }
                            span { class: "{WHY}", "untouched {crate::tab_tidy::IDLE_DAYS}+ days, no open to-do" }
                        }
                        div {
                            class: "flex flex-col",
                            for i in sug.idle.iter() {
                                {
                                    let id = i.session_id.clone();
                                    let checked = !deselected.read().contains(&id);
                                    let why = format!("idle since {}", short_date(i.idle_since));
                                    let label = label_of(&id);
                                    let emoji = emoji_of(&id);
                                    rsx! {
                                        div {
                                            key: "{id}",
                                            class: "{ITEM}",
                                            input {
                                                r#type: "checkbox",
                                                class: "{CB}",
                                                checked,
                                                onchange: {
                                                    let id = id.clone();
                                                    move |evt: FormEvent| toggle_close(vec![id.clone()], evt.checked())
                                                },
                                            }
                                            span { class: "w-4 text-center", "{emoji}" }
                                            span { class: "flex-1 min-w-0 truncate", "{label}" }
                                            span { class: "{WHY}", "{why}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if !sug.unsorted.is_empty() {
                        div {
                            class: "{SECTION}",
                            input {
                                r#type: "checkbox",
                                class: "{CB}",
                                "aria-label": "Close all unsorted tabs",
                                checked: all_unsorted_closing,
                                onchange: {
                                    let ids = unsorted_ids.clone();
                                    move |evt: FormEvent| {
                                        let mut c = unsorted_close.write();
                                        for id in ids.iter() {
                                            if evt.checked() {
                                                c.insert(id.clone());
                                            } else {
                                                c.remove(id);
                                            }
                                        }
                                    }
                                },
                            }
                            span { "Unsorted tabs" }
                            span { class: "{WHY}", "tick to close · the chip files it into a project" }
                        }
                        div {
                            class: "flex flex-col",
                            for u in sug.unsorted.iter() {
                                {
                                    let id = u.session_id.clone();
                                    let eff = effective_project(u);
                                    let checked = closes_unsorted(u);
                                    let label = label_of(&id);
                                    let emoji = emoji_of(&id);
                                    let chip = match &eff {
                                        Some(pid) => project_title(pid),
                                        None => "No project · keep unsorted".to_string(),
                                    };
                                    let menu_open = menu_for.read().as_deref() == Some(id.as_str());
                                    rsx! {
                                        div {
                                            key: "{id}",
                                            class: "{ITEM} relative",
                                            input {
                                                r#type: "checkbox",
                                                class: "{CB}",
                                                "aria-label": "Close {label}",
                                                checked,
                                                onchange: {
                                                    let id = id.clone();
                                                    move |evt: FormEvent| {
                                                        let mut c = unsorted_close.write();
                                                        if evt.checked() {
                                                            c.insert(id.clone());
                                                        } else {
                                                            c.remove(&id);
                                                        }
                                                    }
                                                },
                                            }
                                            span { class: "w-4 text-center", "{emoji}" }
                                            span { class: "flex-1 min-w-0 truncate", "{label}" }
                                            button {
                                                class: "inline-flex items-center gap-1 h-5 max-w-[220px] px-2 rounded-full border border-primary-700 bg-primary-900/60 text-[11px] text-primary-200 whitespace-nowrap hover:border-primary-500",
                                                title: "Change project",
                                                "aria-label": "Change project for {label}",
                                                onclick: {
                                                    let id = id.clone();
                                                    move |evt: MouseEvent| {
                                                        evt.stop_propagation();
                                                        let open_now = menu_for.peek().as_deref() == Some(id.as_str());
                                                        let p = evt.client_coordinates();
                                                        menu_at.set((p.x, p.y));
                                                        menu_for.set(if open_now { None } else { Some(id.clone()) });
                                                    }
                                                },
                                                span { class: "truncate", "{chip}" }
                                                Icon { icon: fi_icons::FiChevronDown, width: 10, height: 10, class: "shrink-0 opacity-70" }
                                            }
                                            if menu_open {
                                                div {
                                                    class: "fixed w-56 max-h-48 overflow-y-auto bg-card border border-subtle rounded-lg shadow-xl z-50 py-1",
                                                    style: "{menu_style(*menu_at.read())}",
                                                    onclick: |evt| evt.stop_propagation(),
                                                    button {
                                                        class: "w-full text-left px-3 py-1.5 text-xs text-fg-muted hover:bg-primary-900/50 hover:text-fg",
                                                        onclick: {
                                                            let id = id.clone();
                                                            move |_| {
                                                                overrides.write().insert(id.clone(), None);
                                                                menu_for.set(None);
                                                            }
                                                        },
                                                        "No project · keep unsorted"
                                                    }
                                                    for p in props.projects.iter() {
                                                        {
                                                            let pid = p.id.clone();
                                                            let pick = p.id.clone();
                                                            let title = p.title.clone();
                                                            let id = id.clone();
                                                            rsx! {
                                                                button {
                                                                    key: "{pid}",
                                                                    class: "w-full text-left px-3 py-1.5 text-xs text-fg-muted hover:bg-primary-900/50 hover:text-fg truncate",
                                                                    onclick: move |_| {
                                                                        overrides.write().insert(id.clone(), Some(pick.clone()));
                                                                        menu_for.set(None);
                                                                    },
                                                                    "{title}"
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
                        }
                    }
                }

                div {
                    class: "flex items-center gap-2.5 pt-3 border-t border-faint",
                    span { class: "flex-1 text-xs text-fg-muted", "Closed tabs stay in History. Nothing is deleted." }
                    button {
                        class: "{BTN}",
                        onclick: move |_| props.on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        class: "{BTN_PRI}",
                        disabled: close_count == 0 && group_count == 0,
                        onclick: apply,
                        "{primary_label}"
                    }
                }
            }
        }
    }
}

/// Inline geometry for the fixed project menu: just below the click,
/// right-aligned to it, clamped inside the viewport (the 192px list cap and
/// 224px width) so a row near the bottom opens its menu upward.
fn menu_style((x, y): (f64, f64)) -> String {
    format!(
        "left: clamp(8px, {:.0}px, calc(100vw - 232px)); top: min({:.0}px, calc(100vh - 200px));",
        x - 200.0,
        y + 10.0
    )
}
