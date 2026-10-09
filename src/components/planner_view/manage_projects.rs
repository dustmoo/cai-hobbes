//! Manage projects: rename, aliases, status, and the person-driven cleanup
//! of duplicates.
//!
//! "Possible duplicates" groups come from `project_identity::duplicate_groups`
//! (exact or similar names). Nothing merges on its own: each group offers
//! "Merge into…" (pick the keeper, then a confirm stating what moves) and
//! "Not duplicates" (remembered on the projects, so the pair stops being
//! suggested). A merge moves every to-do, retags every chat (hydrated ones in
//! `SessionState`, the rest through `session_store`), rewrites trust rules
//! and the agent-time scope, and keeps the merged names as aliases.

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::confirm_delete_modal::ConfirmDeleteModal;
use crate::components::planner_rows::{BTN_PRIMARY, BTN_SECONDARY, MENU_ITEM, MENU_LABEL, META, PILL, POPOVER};
use crate::session::SessionState;
use crate::settings::{Settings, SettingsManager};
use crate::todo::model::{Project, TodoStatus};
use crate::todo::project_identity::{self, ExactLookup};
use crate::todo::project_merge::{project_stats, MergeOutcome};
use crate::todo::{store, PlannerState};

const PANEL: &str = "flex min-w-0 flex-col gap-3 rounded-lg border border-faint bg-section p-4";
const EYEBROW: &str = "text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const SUB: &str = "text-xs text-fg-muted";

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The three states a person sets on a project. Archived is stored as
/// `TodoStatus::Cancelled`; in-progress reads as active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectState {
    Active,
    Completed,
    Archived,
}

impl ProjectState {
    pub(crate) fn of(s: TodoStatus) -> Self {
        match s {
            TodoStatus::Open | TodoStatus::InProgress => ProjectState::Active,
            TodoStatus::Completed => ProjectState::Completed,
            TodoStatus::Cancelled => ProjectState::Archived,
        }
    }

    fn status(self) -> TodoStatus {
        match self {
            ProjectState::Active => TodoStatus::Open,
            ProjectState::Completed => TodoStatus::Completed,
            ProjectState::Archived => TodoStatus::Cancelled,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ProjectState::Active => "Active",
            ProjectState::Completed => "Completed",
            ProjectState::Archived => "Archived",
        }
    }
}

/// `"today"`, `"yesterday"`, `"3d ago"`, `"Sep 2"`, `"Sep 2, 2025"` — in
/// local days.
pub(crate) fn fmt_last_activity(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let day = at.with_timezone(&Local).date_naive();
    let today = now.with_timezone(&Local).date_naive();
    let days = (today - day).num_days();
    match days {
        i64::MIN..=0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => format!("{days}d ago"),
        _ if day.format("%Y").to_string() == today.format("%Y").to_string() => {
            day.format("%b %-d").to_string()
        }
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn quoted_list(names: &[String]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("\u{201c}{n}\u{201d}")).collect();
    match q.len() {
        0 => String::new(),
        1 => q[0].clone(),
        _ => format!("{} and {}", q[..q.len() - 1].join(", "), q[q.len() - 1]),
    }
}

/// The confirm dialog's statement of what a merge does.
pub(crate) fn merge_message(keep: &str, merged: &[String], todos: usize, chats: usize) -> String {
    format!(
        "Merge {} into \u{201c}{keep}\u{201d}? {} and {} move to \u{201c}{keep}\u{201d}, and {} become{} its alias{}. This can't be undone.",
        quoted_list(merged),
        plural(todos, "to-do", "to-dos"),
        plural(chats, "chat", "chats"),
        if merged.len() == 1 { "the merged name" } else { "the merged names" },
        if merged.len() == 1 { "s" } else { "" },
        if merged.len() == 1 { "" } else { "es" },
    )
}

/// One project as this view shows it.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    title: String,
    aliases: Vec<String>,
    area: Option<String>,
    state: ProjectState,
    open_todos: usize,
    /// `"aka a, b"`, empty without aliases.
    aka: String,
    /// `"3 open to-dos · yesterday"`.
    meta: String,
}

fn row_for(state: &PlannerState, p: &Project, now: DateTime<Utc>) -> Row {
    let stats = project_stats(state, p);
    let last_activity = fmt_last_activity(stats.last_activity, now);
    Row {
        aka: if p.aliases.is_empty() { String::new() } else { format!("aka {}", p.aliases.join(", ")) },
        meta: format!("{} \u{b7} {}", plural(stats.open_todos, "open to-do", "open to-dos"), last_activity),
        id: p.id.clone(),
        title: p.title.clone(),
        aliases: p.aliases.clone(),
        area: p
            .area_id
            .as_ref()
            .and_then(|aid| state.areas.iter().find(|a| &a.id == aid))
            .map(|a| a.title.clone()),
        state: ProjectState::of(p.status),
        open_todos: stats.open_todos,
    }
}

/// A merge waiting on the confirm dialog.
#[derive(Debug, Clone, PartialEq)]
struct PendingMerge {
    keep_id: String,
    merge_ids: Vec<String>,
    message: String,
}

// ── Persistence ─────────────────────────────────────────────────────────────

fn persist_project(planner: &Signal<PlannerState>, id: &str) {
    if let Some(p) = planner.peek().projects.iter().find(|p| p.id == id) {
        if let Err(e) = store::save_project(p) {
            tracing::error!("planner: failed to save project {}: {}", id, e);
        }
    }
}

/// Patch one project in state, stamp `updated_at`, and persist it.
fn mutate_project(mut planner: Signal<PlannerState>, id: &str, f: impl FnOnce(&mut Project)) {
    {
        let mut state = planner.write();
        if let Some(p) = state.projects.iter_mut().find(|p| p.id == id) {
            f(p);
            p.updated_at = Utc::now();
        }
    }
    persist_project(&planner, id);
}

/// Merge and persist: planner rows, the agent-time grant, deleted project
/// rows, chat sessions (hydrated + stored), and trust rules scoped to a
/// merged project.
pub(crate) fn merge_projects_persisted(
    mut planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    mut settings: Signal<Settings>,
    settings_manager: Signal<SettingsManager>,
    keep_id: &str,
    merge_ids: &[String],
) -> Result<MergeOutcome, String> {
    let outcome = planner.write().merge_projects(keep_id, merge_ids, Utc::now())?;
    {
        let state = planner.peek();
        for id in &outcome.moved_todos {
            if let Some(t) = state.todo(id) {
                if let Err(e) = store::save_todo(t) {
                    tracing::error!("planner: failed to save todo {}: {}", id, e);
                }
            }
        }
        for id in std::iter::once(&outcome.keep_id).chain(outcome.touched_projects.iter()) {
            if let Some(p) = state.projects.iter().find(|p| &p.id == id) {
                if let Err(e) = store::save_project(p) {
                    tracing::error!("planner: failed to save project {}: {}", id, e);
                }
            }
        }
        if outcome.grant_changed {
            if let Err(e) = store::save_agent_time_grant(&state.agent_time_grant) {
                tracing::error!("planner: failed to save agent-time grant: {}", e);
            }
        }
    }
    for id in &outcome.merged_ids {
        if let Err(e) = store::delete_project(id) {
            tracing::error!("planner: failed to delete merged project {}: {}", id, e);
        }
    }

    crate::services::project_tagger::retag_merged_sessions(session_state, &outcome.merged_ids, keep_id);

    // Trust rules scoped to a merged project now scope the keeper.
    let rules_changed = {
        let mut s = settings.write();
        let rules = &mut s.permission_settings.trust_rules;
        let mut changed = false;
        for r in rules.iter_mut() {
            if r.project_id.as_ref().is_some_and(|p| outcome.merged_ids.contains(p)) {
                r.project_id = Some(keep_id.to_string());
                changed = true;
            }
        }
        if changed {
            // Same dedupe key as `Settings::add_trust_rule`.
            let mut kept: Vec<crate::context::permissions::TrustRule> = Vec::new();
            for r in rules.drain(..) {
                let dup = kept.iter().any(|k| {
                    k.server == r.server
                        && k.tool == r.tool
                        && k.command_prefix == r.command_prefix
                        && k.project_id == r.project_id
                });
                if !dup {
                    kept.push(r);
                }
            }
            *rules = kept;
        }
        changed
    };
    if rules_changed {
        settings_manager.peek().save_async(settings.peek().clone(), None);
    }

    tracing::info!(
        keep_id,
        merged = outcome.merged_ids.len(),
        todos = outcome.moved_todos.len(),
        "merged projects"
    );
    Ok(outcome)
}

/// "Not duplicates": every member records every other, so no pair in the
/// group is suggested again.
fn mark_distinct(mut planner: Signal<PlannerState>, ids: &[String]) {
    {
        let mut state = planner.write();
        let now = Utc::now();
        for p in state.projects.iter_mut().filter(|p| ids.contains(&p.id)) {
            for other in ids {
                if other != &p.id && !p.distinct_from.contains(other) {
                    p.distinct_from.push(other.clone());
                }
            }
            p.updated_at = now;
        }
    }
    for id in ids {
        persist_project(&planner, id);
    }
}

// ── View ────────────────────────────────────────────────────────────────────

#[component]
pub(crate) fn ManageProjects() -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();

    let mut show_closed = use_signal(|| false);
    // (project id, draft) while a title or an alias is being typed.
    let mut editing_title = use_signal(|| None::<(String, String)>);
    let mut alias_draft = use_signal(|| None::<(String, String)>);
    // The duplicate group whose "Merge into…" menu is open.
    let mut merge_menu = use_signal(|| None::<usize>);
    let mut pending_merge = use_signal(|| None::<PendingMerge>);
    let mut confirm_visible = use_signal(|| false);
    let mut notice = use_signal(|| None::<String>);

    let now = Utc::now();
    let (open_rows, closed_rows, groups) = {
        let state = planner.read();
        let mut projects: Vec<&Project> = state.projects.iter().collect();
        projects.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order).then_with(|| a.title.cmp(&b.title)));
        let open: Vec<Row> = projects
            .iter()
            .filter(|p| !p.status.is_closed())
            .map(|p| row_for(&state, p, now))
            .collect();
        let closed: Vec<Row> = projects
            .iter()
            .filter(|p| p.status.is_closed())
            .map(|p| row_for(&state, p, now))
            .collect();
        let groups: Vec<(bool, Vec<Row>)> = project_identity::duplicate_groups(&state.projects)
            .into_iter()
            .map(|g| {
                let rows = g.ids.iter().filter_map(|id| open.iter().find(|r| &r.id == id).cloned()).collect();
                (g.exact, rows)
            })
            .collect();
        (open, closed, groups)
    };
    let closed_count = closed_rows.len();
    let showing_closed = *show_closed.read();
    let mut rows = open_rows.clone();
    if showing_closed {
        rows.extend(closed_rows);
    }

    // Opening the confirm: count what moves now, so the dialog states it.
    let ask_merge = move |keep_id: String, members: Vec<Row>| {
        let merge_ids: Vec<String> = members.iter().filter(|r| r.id != keep_id).map(|r| r.id.clone()).collect();
        let merged_titles: Vec<String> = members.iter().filter(|r| r.id != keep_id).map(|r| r.title.clone()).collect();
        let keep_title = members.iter().find(|r| r.id == keep_id).map(|r| r.title.clone()).unwrap_or_default();
        let todos = planner
            .peek()
            .todos
            .iter()
            .filter(|t| t.project_id.as_ref().is_some_and(|p| merge_ids.contains(p)))
            .count();
        let chats = crate::services::project_tagger::count_sessions_tagged(&session_state.peek(), &merge_ids);
        let message = merge_message(&keep_title, &merged_titles, todos, chats);
        pending_merge.set(Some(PendingMerge { keep_id, merge_ids, message }));
        merge_menu.set(None);
        confirm_visible.set(true);
    };

    let confirm_message = pending_merge.read().as_ref().map(|p| p.message.clone()).unwrap_or_default();
    let open_label = format!("{} open", open_rows.len());
    let notice_now = notice.read().clone();
    let editing_now = editing_title.read().clone();
    let alias_now = alias_draft.read().clone();
    let menu_now = *merge_menu.read();

    rsx! {
        div {
            class: "flex-1 min-w-0 min-h-0 overflow-y-auto bg-app",
            div {
                class: "flex flex-col gap-3 px-6 pt-6 pb-10",
                div {
                    class: "flex flex-wrap items-baseline gap-2.5",
                    h1 { class: "text-xl font-semibold", "Projects" }
                    span { class: "text-sm text-fg-muted", "{open_label}" }
                    div { class: "flex-1" }
                    if closed_count > 0 {
                        button {
                            class: "{BTN_SECONDARY}",
                            onclick: move |_| {
                                let now = *show_closed.peek();
                                show_closed.set(!now);
                            },
                            if showing_closed { "Hide closed" } else { "Show {closed_count} closed" }
                        }
                    }
                }
                if let Some(n) = notice_now {
                    div {
                        class: "flex items-center gap-2 rounded-md border border-faint bg-card px-3 py-1.5 text-xs text-fg",
                        span { class: "min-w-0 flex-1", "{n}" }
                        button {
                            class: "{BTN_SECONDARY}",
                            onclick: move |_| notice.set(None),
                            "OK"
                        }
                    }
                }

                // ── Possible duplicates ──────────────────────────────────
                if !groups.is_empty() {
                    div {
                        class: "{PANEL}",
                        div {
                            class: "flex flex-col gap-1",
                            span { class: "{EYEBROW}", "Possible duplicates" }
                            span { class: "{SUB}",
                                "These projects have the same or similar names. Merging moves the to-dos and chats into the project you keep; the other names become its aliases. Nothing merges until you choose."
                            }
                        }
                        for (gi, (exact, members)) in groups.into_iter().enumerate() {
                            div {
                                key: "{gi}",
                                class: "flex flex-col gap-1.5 rounded-md border border-faint bg-card p-3",
                                div {
                                    class: "flex items-center gap-2",
                                    span { class: "{PILL}", if exact { "Same name" } else { "Similar names" } }
                                    div { class: "flex-1" }
                                    div {
                                        class: "relative",
                                        button {
                                            class: "{BTN_PRIMARY}",
                                            onclick: move |_| {
                                                let open = *merge_menu.peek() == Some(gi);
                                                merge_menu.set(if open { None } else { Some(gi) });
                                            },
                                            "Merge into\u{2026}"
                                        }
                                        if menu_now == Some(gi) {
                                            div {
                                                class: "fixed inset-0 z-40",
                                                onclick: move |_| merge_menu.set(None),
                                            }
                                            div {
                                                class: "{POPOVER}",
                                                div { class: "{MENU_LABEL}", "Keep which project?" }
                                                for m in members.iter().cloned() {
                                                    button {
                                                        key: "{m.id}",
                                                        class: "{MENU_ITEM}",
                                                        onclick: {
                                                            let members = members.clone();
                                                            let id = m.id.clone();
                                                            let mut ask_merge = ask_merge;
                                                            move |_| ask_merge(id.clone(), members.clone())
                                                        },
                                                        Icon { width: 11, height: 11, icon: fi_icons::FiFolder }
                                                        span { class: "flex-1 truncate", "{m.title}" }
                                                        span { class: "{META}", "{m.open_todos} open" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    button {
                                        class: "{BTN_SECONDARY}",
                                        title: "Stop suggesting these as duplicates",
                                        onclick: {
                                            let ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
                                            move |_| mark_distinct(planner, &ids)
                                        },
                                        "Not duplicates"
                                    }
                                }
                                for m in members.iter().cloned() {
                                    div {
                                        key: "{m.id}",
                                        class: "flex min-w-0 items-center gap-2 text-sm text-fg",
                                        Icon { width: 13, height: 13, icon: fi_icons::FiFolder }
                                        span { class: "min-w-0 truncate", "{m.title}" }
                                        if !m.aka.is_empty() {
                                            span { class: "min-w-0 truncate text-xs text-fg-muted", "{m.aka}" }
                                        }
                                        div { class: "flex-1" }
                                        span { class: "{META}", "{m.meta}" }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── All projects ─────────────────────────────────────────
                div {
                    class: "{PANEL}",
                    span { class: "{EYEBROW}", "All projects" }
                    if rows.is_empty() {
                        span { class: "{SUB}", "No projects yet. Add one from quick add with +name." }
                    }
                    div { class: "flex flex-col gap-2",
                    for r in rows.into_iter() {
                        div {
                            key: "{r.id}",
                            class: "flex min-w-0 flex-col gap-1 border-t border-faint pt-2 first:border-t-0 first:pt-0",
                            div {
                                class: "flex min-w-0 items-center gap-2",
                                Icon { width: 14, height: 14, icon: fi_icons::FiFolder }
                                match editing_now.clone() {
                                    Some((eid, draft)) if eid == r.id => rsx! {
                                        input {
                                            class: "min-w-0 flex-1 rounded border border-faint bg-input px-2 py-0.5 text-sm text-fg",
                                            r#type: "text",
                                            value: "{draft}",
                                            autofocus: true,
                                            oninput: {
                                                let id = r.id.clone();
                                                move |evt: FormEvent| editing_title.set(Some((id.clone(), evt.value())))
                                            },
                                            onkeydown: {
                                                let id = r.id.clone();
                                                move |evt: KeyboardEvent| match evt.key() {
                                                    Key::Enter => {
                                                        evt.prevent_default();
                                                        let draft = editing_title.peek().as_ref().map(|(_, d)| d.trim().to_string());
                                                        editing_title.set(None);
                                                        if let Some(t) = draft.filter(|t| !t.is_empty()) {
                                                            mutate_project(planner, &id, |p| p.title = t);
                                                        }
                                                    }
                                                    Key::Escape => {
                                                        evt.prevent_default();
                                                        evt.stop_propagation();
                                                        editing_title.set(None);
                                                    }
                                                    _ => {}
                                                }
                                            },
                                        }
                                    },
                                    _ => rsx! {
                                        button {
                                            class: "min-w-0 truncate text-left text-sm text-fg hover:underline",
                                            title: "Rename",
                                            onclick: {
                                                let id = r.id.clone();
                                                let title = r.title.clone();
                                                move |_| editing_title.set(Some((id.clone(), title.clone())))
                                            },
                                            "{r.title}"
                                        }
                                        if let Some(area) = r.area.clone() {
                                            span { class: "{META}", "{area}" }
                                        }
                                        div { class: "flex-1" }
                                    },
                                }
                                span { class: "{META}", "{r.meta}" }
                                div {
                                    class: "inline-flex shrink-0 overflow-hidden rounded-lg border border-faint",
                                    for choice in [ProjectState::Active, ProjectState::Completed, ProjectState::Archived] {
                                        button {
                                            key: "{choice:?}",
                                            class: if r.state == choice {
                                                "px-2 py-0.5 text-[11px] font-medium bg-btn-primary text-fg"
                                            } else {
                                                "px-2 py-0.5 text-[11px] text-fg-muted hover:bg-input hover:text-fg"
                                            },
                                            onclick: {
                                                let id = r.id.clone();
                                                let current = r.state;
                                                move |_| {
                                                    if current != choice {
                                                        mutate_project(planner, &id, |p| p.status = choice.status());
                                                    }
                                                }
                                            },
                                            "{choice.label()}"
                                        }
                                    }
                                }
                            }
                            div {
                                class: "flex min-w-0 flex-wrap items-center gap-1.5 pl-6",
                                for a in r.aliases.iter().cloned() {
                                    span {
                                        key: "{a}",
                                        class: "{PILL}",
                                        "{a}"
                                        button {
                                            class: "text-fg-muted hover:text-fg",
                                            title: "Remove alias",
                                            onclick: {
                                                let id = r.id.clone();
                                                let a = a.clone();
                                                move |_| mutate_project(planner, &id, |p| p.aliases.retain(|x| x != &a))
                                            },
                                            "\u{d7}"
                                        }
                                    }
                                }
                                match alias_now.clone() {
                                    Some((aid, draft)) if aid == r.id => rsx! {
                                        input {
                                            class: "w-40 rounded border border-faint bg-input px-2 py-0.5 text-[11px] text-fg",
                                            r#type: "text",
                                            placeholder: "Other name\u{2026}",
                                            value: "{draft}",
                                            autofocus: true,
                                            oninput: {
                                                let id = r.id.clone();
                                                move |evt: FormEvent| alias_draft.set(Some((id.clone(), evt.value())))
                                            },
                                            onkeydown: {
                                                let id = r.id.clone();
                                                move |evt: KeyboardEvent| match evt.key() {
                                                    Key::Enter => {
                                                        evt.prevent_default();
                                                        let draft = alias_draft.peek().as_ref().map(|(_, d)| d.trim().to_string()).unwrap_or_default();
                                                        alias_draft.set(None);
                                                        if draft.is_empty() {
                                                            return;
                                                        }
                                                        // An alias may not name a different project.
                                                        let clash = {
                                                            let state = planner.peek();
                                                            let others: Vec<Project> = state.projects.iter().filter(|p| p.id != id).cloned().collect();
                                                            match project_identity::lookup_exact(&others, &draft) {
                                                                ExactLookup::One(oid) => others.iter().find(|p| p.id == oid).map(|p| p.title.clone()),
                                                                ExactLookup::Many(ids) => others.iter().find(|p| p.id == ids[0]).map(|p| p.title.clone()),
                                                                ExactLookup::None => None,
                                                            }
                                                        };
                                                        match clash {
                                                            Some(other) => notice.set(Some(format!(
                                                                "\u{201c}{draft}\u{201d} already names \u{201c}{other}\u{201d}. If they're the same project, merge them instead."
                                                            ))),
                                                            None => mutate_project(planner, &id, |p| {
                                                                let title = p.title.clone();
                                                                project_identity::add_alias(&title, &mut p.aliases, &draft);
                                                            }),
                                                        }
                                                    }
                                                    Key::Escape => {
                                                        evt.prevent_default();
                                                        evt.stop_propagation();
                                                        alias_draft.set(None);
                                                    }
                                                    _ => {}
                                                }
                                            },
                                        }
                                    },
                                    _ => rsx! {
                                        button {
                                            class: "text-[11px] text-fg-muted hover:text-fg",
                                            onclick: {
                                                let id = r.id.clone();
                                                move |_| alias_draft.set(Some((id.clone(), String::new())))
                                            },
                                            "+ alias"
                                        }
                                    },
                                }
                            }
                        }
                    }
                    }
                }
            }
            ConfirmDeleteModal {
                is_visible: confirm_visible,
                title: "Merge projects".to_string(),
                message: confirm_message,
                confirm_button_text: "Merge".to_string(),
                show_dont_ask_again: false,
                on_confirm: move |_| {
                    let pending = pending_merge.peek().clone();
                    pending_merge.set(None);
                    confirm_visible.set(false);
                    let Some(p) = pending else { return };
                    match merge_projects_persisted(planner, session_state, settings, settings_manager, &p.keep_id, &p.merge_ids) {
                        Ok(out) => {
                            let keep = planner
                                .peek()
                                .projects
                                .iter()
                                .find(|x| x.id == out.keep_id)
                                .map(|x| x.title.clone())
                                .unwrap_or_default();
                            notice.set(Some(format!(
                                "Merged {} into \u{201c}{keep}\u{201d}: {} moved.",
                                plural(out.merged_ids.len(), "project", "projects"),
                                plural(out.moved_todos.len(), "to-do", "to-dos"),
                            )));
                        }
                        Err(e) => notice.set(Some(format!("Couldn't merge: {e}"))),
                    }
                },
                on_cancel: move |_| {
                    pending_merge.set(None);
                    confirm_visible.set(false);
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn merge_message_states_what_moves() {
        assert_eq!(
            merge_message("Hobbes", &["cai-hobbes".into()], 3, 1),
            "Merge \u{201c}cai-hobbes\u{201d} into \u{201c}Hobbes\u{201d}? 3 to-dos and 1 chat move to \u{201c}Hobbes\u{201d}, and the merged name becomes its alias. This can't be undone."
        );
        let m = merge_message("Hobbes", &["a".into(), "b".into(), "c".into()], 1, 0);
        assert!(m.starts_with("Merge \u{201c}a\u{201d}, \u{201c}b\u{201d} and \u{201c}c\u{201d} into"), "{m}");
        assert!(m.contains("1 to-do and 0 chats"), "{m}");
        assert!(m.contains("the merged names become its aliases"), "{m}");
    }

    #[test]
    fn last_activity_reads_in_local_days() {
        let now = Local.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap().with_timezone(&Utc);
        let ago = |d: i64| now - chrono::Duration::days(d);
        assert_eq!(fmt_last_activity(now, now), "today");
        assert_eq!(fmt_last_activity(ago(1), now), "yesterday");
        assert_eq!(fmt_last_activity(ago(3), now), "3d ago");
        assert_eq!(fmt_last_activity(ago(30), now), "Sep 9");
        assert_eq!(fmt_last_activity(ago(400), now), "Sep 4, 2025");
    }

    fn project(id: &str, title: &str, sort_order: f64) -> Project {
        Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::Open,
            deadline: None,
            sort_order,
            path: None,
            aliases: Vec::new(),
            distinct_from: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn real_planner() -> PlannerState {
        let names = [
            ("h1", "cai-hobbes"),
            ("h2", "cai-hobbes"),
            ("hb", "Hobbes"),
            ("hp", "Hobbes (cai-hobbes)"),
            ("g1", "Garbage Vibe Kids"),
            ("g2", "Garbage Vibe Kids"),
            ("tx", "Taxes 2026"),
        ];
        let mut state = PlannerState::default();
        state.projects = names.iter().enumerate().map(|(i, (id, t))| project(id, t, i as f64)).collect();
        for (i, pid) in ["h1", "h2", "h2", "hp", "g2"].iter().enumerate() {
            let mut t = crate::todo::model::Todo::new(format!("todo {i}"), i as f64);
            t.project_id = Some(pid.to_string());
            state.todos.push(t);
        }
        state.projects[6].status = TodoStatus::Completed;
        state
    }

    #[test]
    fn manage_view_renders_duplicates_without_a_borrow_panic() {
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            use_context_provider(|| Signal::new(real_planner()));
            use_context_provider(|| Signal::new(SessionState::default()));
            use_context_provider(|| Signal::new(Settings::default()));
            use_context_provider(|| {
                Signal::new(SettingsManager::new(std::env::temp_dir().join("hobbes-manage-projects-test.json")))
            });
            rsx! { ManageProjects {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
        // What the duplicates group renders from.
        let groups = project_identity::duplicate_groups(&real_planner().projects);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|g| g.exact), "{groups:?}");
    }

    #[test]
    fn merge_moves_todos_retags_hydrated_chats_and_trust_rules() {
        // The trust-rule change saves settings through tokio.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let planner = use_signal(real_planner);
            let mut session_state = use_signal(SessionState::default);
            let mut settings = use_signal(Settings::default);
            let dir = use_hook(|| std::rc::Rc::new(tempfile::tempdir().unwrap()));
            let settings_manager = use_signal(|| SettingsManager::new(dir.path().join("settings.json")));
            let chat = {
                let mut s = session_state.write();
                let id = s.create_session_raw(None);
                s.sessions.get_mut(&id).unwrap().project_id = Some("h2".into());
                id
            };
            settings.write().permission_settings.trust_rules.push(crate::context::permissions::TrustRule {
                id: "r1".into(),
                server: "hobbes-terminal".into(),
                tool: None,
                command_prefix: Some("git status".into()),
                project_id: Some("h1".into()),
                created_at: Utc::now(),
            });

            let out = merge_projects_persisted(
                planner,
                session_state,
                settings,
                settings_manager,
                "hb",
                &["h1".into(), "h2".into(), "hp".into()],
            )
            .expect("merged");
            assert_eq!(out.moved_todos.len(), 4);
            let state = planner.peek();
            assert_eq!(state.projects.len(), 4, "hb, g1, g2, tx remain");
            assert!(state.todos.iter().filter(|t| t.title != "todo 4").all(|t| t.project_id.as_deref() == Some("hb")));
            let hb = state.projects.iter().find(|p| p.id == "hb").unwrap();
            assert_eq!(hb.aliases, vec!["cai-hobbes", "Hobbes (cai-hobbes)"]);
            assert_eq!(session_state.peek().sessions[&chat].project_id.as_deref(), Some("hb"));
            assert_eq!(
                settings.peek().permission_settings.trust_rules[0].project_id.as_deref(),
                Some("hb")
            );
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn project_state_round_trips_through_status() {
        for s in [ProjectState::Active, ProjectState::Completed, ProjectState::Archived] {
            assert_eq!(ProjectState::of(s.status()), s);
        }
        assert_eq!(ProjectState::of(TodoStatus::InProgress), ProjectState::Active);
        assert!(ProjectState::Archived.status().is_closed());
    }
}
