//! Manage projects: rename, aliases, status, areas, and the person-driven
//! cleanup of duplicates.
//!
//! "Possible duplicates" groups come from `project_identity::duplicate_groups`
//! (exact or similar names). Nothing merges on its own: in each group the
//! person checks the members to merge and picks the one to keep ("Merge N
//! into …", then a confirm stating what moves), or says "Not duplicates"
//! (remembered on the projects, so the pair stops being suggested). Any 2+
//! projects checked in All projects can be merged the same way, for
//! duplicates the detector missed — or the person can ask Hobbes, whose
//! `HOBBES_PROJECT_MERGE` always asks for approval. A merge moves every
//! to-do, retags every chat (hydrated ones in `SessionState`, the rest
//! through `session_store`, off the UI thread), rewrites trust rules and the
//! agent-time scope, and keeps the merged names as aliases.
//!
//! P-017: nothing here touches the store synchronously beyond a single-row
//! upsert. Counting chats, a merge's disk half, and any multi-row write run
//! in `spawn_blocking` from a spawned task, with an in-progress state shown.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::chat_input::ChatCommand;
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
const CHECKBOX: &str = "w-3.5 h-3.5 shrink-0 accent-white/50 cursor-pointer disabled:cursor-default";
const SELECT: &str = "h-6 min-w-0 max-w-[10rem] rounded border border-faint bg-input px-1.5 text-xs text-fg outline-none";
const ICON_BTN: &str = "rounded p-0.5 text-fg-muted hover:bg-input hover:text-fg disabled:opacity-30 disabled:hover:bg-transparent";
const TEXT_BTN: &str = "text-[11px] text-fg-muted hover:text-fg";

/// The chat "Ask Hobbes to tidy projects" opens with.
pub(crate) const TIDY_PROMPT: &str = "Help me tidy my projects. Look them up with HOBBES_PROJECT_LIST, suggest which to merge and which to keep separate, and ask me before merging anything.";

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

/// The confirm dialog's (and the merge tool's approval prompt's) statement
/// of what a merge does. `chats` is `None` while the count is still running.
pub(crate) fn merge_message(keep: &str, merged: &[String], todos: usize, chats: Option<usize>) -> String {
    let chats = match chats {
        Some(n) => plural(n, "chat", "chats"),
        None => "its chats (counting chats\u{2026})".to_string(),
    };
    format!(
        "Merge {} into \u{201c}{keep}\u{201d}? {} and {} move to \u{201c}{keep}\u{201d}, and {} become{} its alias{}. This can't be undone.",
        quoted_list(merged),
        plural(todos, "to-do", "to-dos"),
        chats,
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
    area_id: Option<String>,
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
        area_id: p.area_id.clone(),
        area: p
            .area_id
            .as_ref()
            .and_then(|aid| state.areas.iter().find(|a| &a.id == aid))
            .map(|a| a.title.clone()),
        state: ProjectState::of(p.status),
        open_todos: stats.open_todos,
    }
}

/// One area as the Areas section shows it.
#[derive(Debug, Clone, PartialEq)]
struct AreaRow {
    id: String,
    title: String,
    projects: usize,
}

/// A duplicate group's identity across renders: its member ids.
fn group_key(members: &[Row]) -> String {
    members.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join("|")
}

/// The keeper a group starts with: the member with the most open to-dos
/// (the first on a tie).
fn default_keeper(members: &[Row]) -> Option<String> {
    members
        .iter()
        .enumerate()
        .max_by(|(ia, a), (ib, b)| a.open_todos.cmp(&b.open_todos).then(ib.cmp(ia)))
        .map(|(_, m)| m.id.clone())
}

/// What "Merge N into <keeper>" folds in: the checked members other than
/// the keeper (`unchecked` holds the ones the person unticked).
fn group_merge_ids(members: &[Row], keep: &str, unchecked: &HashSet<String>) -> Vec<String> {
    members
        .iter()
        .filter(|m| m.id != keep && !unchecked.contains(&m.id))
        .map(|m| m.id.clone())
        .collect()
}

/// The checked rows of All projects, in table order.
fn selected_rows(rows: &[Row], selected: &HashSet<String>) -> Vec<Row> {
    rows.iter().filter(|r| selected.contains(&r.id)).cloned().collect()
}

/// A merge waiting on the confirm dialog.
#[derive(Debug, Clone, PartialEq)]
struct PendingMerge {
    /// Matches the count task that fills `chats`, so a stale count (the
    /// dialog was closed and reopened for another merge) is dropped.
    token: u64,
    keep_id: String,
    keep_title: String,
    merge_ids: Vec<String>,
    merged_titles: Vec<String>,
    todos: usize,
    /// `None` until the off-thread count lands.
    chats: Option<usize>,
}

impl PendingMerge {
    fn message(&self) -> String {
        merge_message(&self.keep_title, &self.merged_titles, self.todos, self.chats)
    }
}

// ── Persistence ─────────────────────────────────────────────────────────────

fn persist_project(planner: &Signal<PlannerState>, id: &str) {
    if let Some(p) = planner.peek().projects.iter().find(|p| p.id == id) {
        if let Err(e) = store::save_project(p) {
            tracing::error!("planner: failed to save project {}: {}", id, e);
        }
    }
}

fn persist_area(planner: &Signal<PlannerState>, id: &str) {
    if let Some(a) = planner.peek().areas.iter().find(|a| a.id == id) {
        if let Err(e) = store::save_area(a) {
            tracing::error!("planner: failed to save area {}: {}", id, e);
        }
    }
}

/// Snapshot these planner rows with seqs allocated now (UI thread), for
/// [`write_rows_async`].
fn snapshot_rows(state: &PlannerState, projects: &[String], areas: &[String], todos: &[String]) -> store::PlannerRows {
    let seq = crate::session_store::next_seq;
    store::PlannerRows {
        todos: todos.iter().filter_map(|id| state.todo(id)).map(|t| (t.clone(), seq())).collect(),
        projects: state
            .projects
            .iter()
            .filter(|p| projects.contains(&p.id))
            .map(|p| (p.clone(), seq()))
            .collect(),
        areas: state.areas.iter().filter(|a| areas.contains(&a.id)).map(|a| (a.clone(), seq())).collect(),
        ..Default::default()
    }
}

/// Write a batch of planner rows on a blocking thread (P-017). Call from a
/// handler; the write lands shortly after.
fn write_rows_async(rows: store::PlannerRows) {
    if rows.is_empty() {
        return;
    }
    spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            if crate::session_store::is_available() {
                store::save_planner_rows(&rows)
            } else {
                Ok(())
            }
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("planner: failed to save rows: {e}"),
            Err(e) => tracing::error!("planner: row save task failed: {e}"),
        }
    });
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

/// The disk half of a merge, snapshotted on the UI thread by
/// [`merge_projects_persisted`] and written by [`finish_merge`] on a
/// blocking thread (P-017): the planner rows (seqs allocated at snapshot
/// time), the stored chat rows' retag, and the retag journal.
pub(crate) struct MergeJob {
    rows: store::PlannerRows,
    from_ids: Vec<String>,
    to_id: String,
    /// Hydrated session ids — retagged in `SessionState`, skipped on disk.
    skip: HashSet<String>,
    hydrated_retagged: Vec<(String, bool)>,
}

impl MergeJob {
    /// Chats already retagged in memory.
    pub(crate) fn hydrated_chats(&self) -> usize {
        self.hydrated_retagged.len()
    }

    /// Blocking. Returns how many stored (non-hydrated) chats moved.
    fn run_blocking(self) -> usize {
        if crate::session_store::is_available() {
            if let Err(e) = store::save_planner_rows(&self.rows) {
                tracing::error!("planner: failed to persist the merge: {}", e);
            }
        }
        crate::services::project_tagger::retag_stored_sessions_blocking(
            &self.from_ids,
            &self.to_id,
            &self.skip,
            &self.hydrated_retagged,
        )
    }
}

/// Write a merge's disk half on a blocking thread and return how many chats
/// moved in all (hydrated + stored). Await it from a spawned task (a Dioxus
/// `spawn`, or the async tool dispatcher) — never call store code from a
/// handler body. Afterwards, any session that hydrated while the rows were
/// being rewritten (its in-memory copy loaded before the retag) is moved in
/// memory too, so its next save can't write the old tag back.
pub(crate) async fn finish_merge(job: MergeJob, session_state: Signal<SessionState>) -> usize {
    let hydrated = job.hydrated_chats();
    let (from_ids, to_id) = (job.from_ids.clone(), job.to_id.clone());
    let stored = match tokio::task::spawn_blocking(move || job.run_blocking()).await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("project merge: persistence task failed: {e}");
            0
        }
    };
    // Already counted (and journaled) among the stored rows.
    let _ = crate::services::project_tagger::retag_hydrated_sessions(session_state, &from_ids, &to_id);
    hydrated + stored
}

/// Merge: the in-memory half runs here, on the caller's (UI) thread —
/// planner state, hydrated chats in `SessionState` (saved through P-009),
/// and trust rules scoped to a merged project (settings saved async). The
/// disk half comes back as a [`MergeJob`] for [`finish_merge`].
pub(crate) fn merge_projects_persisted(
    mut planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    mut settings: Signal<Settings>,
    settings_manager: &SettingsManager,
    keep_id: &str,
    merge_ids: &[String],
) -> Result<(MergeOutcome, MergeJob), String> {
    let outcome = planner.write().merge_projects(keep_id, merge_ids, Utc::now())?;
    let rows = {
        let state = planner.peek();
        let projects: Vec<String> =
            std::iter::once(outcome.keep_id.clone()).chain(outcome.touched_projects.iter().cloned()).collect();
        let mut rows = snapshot_rows(&state, &projects, &[], &outcome.moved_todos);
        rows.grant = outcome
            .grant_changed
            .then(|| (state.agent_time_grant.clone(), crate::session_store::next_seq()));
        rows.deleted_projects = outcome.merged_ids.clone();
        rows
    };

    let (hydrated_retagged, skip) =
        crate::services::project_tagger::retag_hydrated_sessions(session_state, &outcome.merged_ids, keep_id);
    let job = MergeJob {
        rows,
        from_ids: outcome.merged_ids.clone(),
        to_id: keep_id.to_string(),
        skip,
        hydrated_retagged,
    };

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
        settings_manager.save_async(settings.peek().clone(), None);
    }

    tracing::info!(
        keep_id,
        merged = outcome.merged_ids.len(),
        todos = outcome.moved_todos.len(),
        "merged projects"
    );
    Ok((outcome, job))
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
    let rows = snapshot_rows(&planner.peek(), ids, &[], &[]);
    write_rows_async(rows);
}

/// File `ids` under `area_id` (`None` = No area) and persist what changed.
fn assign_area_persisted(mut planner: Signal<PlannerState>, ids: &[String], area_id: Option<&str>) -> Result<usize, String> {
    let changed = planner.write().assign_area(ids, area_id, Utc::now())?;
    match changed.as_slice() {
        [] => {}
        [one] => persist_project(&planner, one),
        _ => {
            let rows = snapshot_rows(&planner.peek(), &changed, &[], &[]);
            write_rows_async(rows);
        }
    }
    Ok(changed.len())
}

/// Delete an area; its projects (and to-dos filed under it) move to No area.
fn delete_area_persisted(mut planner: Signal<PlannerState>, id: &str) -> Result<usize, String> {
    let out = planner.write().delete_area(id, Utc::now())?;
    let mut rows = snapshot_rows(&planner.peek(), &out.projects, &[], &out.todos);
    rows.deleted_areas.push(id.to_string());
    write_rows_async(rows);
    Ok(out.projects.len())
}

/// Select value for "No area" in the area pickers.
const NO_AREA: &str = "__none__";

// ── View ────────────────────────────────────────────────────────────────────

#[component]
pub(crate) fn ManageProjects() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let chat_command = try_use_context::<Signal<Option<ChatCommand>>>();

    let mut show_closed = use_signal(|| false);
    // (project id, draft) while a title or an alias is being typed.
    let mut editing_title = use_signal(|| None::<(String, String)>);
    let mut alias_draft = use_signal(|| None::<(String, String)>);
    // Duplicate groups: the keeper picked per group (by group key) and the
    // members the person unticked (by project id).
    let mut group_keep = use_signal(HashMap::<String, String>::new);
    let mut group_unchecked = use_signal(HashSet::<String>::new);
    // All projects: checked rows, and whether the "keep which?" menu is open.
    let mut selected = use_signal(HashSet::<String>::new);
    let mut keep_menu = use_signal(|| false);
    let mut pending_merge = use_signal(|| None::<PendingMerge>);
    let mut merge_token = use_signal(|| 0u64);
    let mut confirm_visible = use_signal(|| false);
    let mut notice = use_signal(|| None::<String>);
    // Areas.
    let mut editing_area = use_signal(|| None::<(String, String)>);
    let mut new_area = use_signal(|| None::<String>);
    let mut pending_area_delete = use_signal(|| None::<(String, String)>);
    let mut area_confirm_visible = use_signal(|| false);

    let now = Utc::now();
    let (open_rows, closed_rows, groups, areas) = {
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
        let areas: Vec<AreaRow> = state
            .areas_sorted()
            .into_iter()
            .map(|a| AreaRow { id: a.id.clone(), title: a.title.clone(), projects: state.area_project_count(&a.id) })
            .collect();
        (open, closed, groups, areas)
    };
    let closed_count = closed_rows.len();
    let showing_closed = *show_closed.read();
    let mut rows = open_rows.clone();
    if showing_closed {
        rows.extend(closed_rows);
    }

    // Opening the confirm: the dialog shows at once with "counting chats…";
    // the chat count runs off the UI thread and fills in when it lands.
    let ask_merge = move |keep_id: String, members: Vec<Row>| {
        let merge_ids: Vec<String> = members.iter().filter(|r| r.id != keep_id).map(|r| r.id.clone()).collect();
        if merge_ids.is_empty() {
            return;
        }
        let merged_titles: Vec<String> = members.iter().filter(|r| r.id != keep_id).map(|r| r.title.clone()).collect();
        let keep_title = members.iter().find(|r| r.id == keep_id).map(|r| r.title.clone()).unwrap_or_default();
        let todos = planner
            .peek()
            .todos
            .iter()
            .filter(|t| t.project_id.as_ref().is_some_and(|p| merge_ids.contains(p)))
            .count();
        let token = *merge_token.peek() + 1;
        merge_token.set(token);
        pending_merge.set(Some(PendingMerge {
            token,
            keep_id,
            keep_title,
            merge_ids: merge_ids.clone(),
            merged_titles,
            todos,
            chats: None,
        }));
        keep_menu.set(false);
        confirm_visible.set(true);
        spawn(async move {
            let n = crate::services::project_tagger::count_sessions_tagged(session_state, merge_ids).await;
            let mut pending = pending_merge.write();
            if let Some(p) = pending.as_mut().filter(|p| p.token == token) {
                p.chats = Some(n);
            }
        });
    };

    let confirm_message = pending_merge.read().as_ref().map(|p| p.message()).unwrap_or_default();
    let area_confirm_message = pending_area_delete
        .read()
        .as_ref()
        .map(|(_, title)| {
            format!("Delete the area \u{201c}{title}\u{201d}? Projects in it move to No area; nothing else is deleted.")
        })
        .unwrap_or_default();
    let open_label = format!("{} open", open_rows.len());
    let notice_now = notice.read().clone();
    let editing_now = editing_title.read().clone();
    let alias_now = alias_draft.read().clone();
    let keep_now = group_keep.read().clone();
    let unchecked_now = group_unchecked.read().clone();
    let selected_now = selected.read().clone();
    let picked = selected_rows(&rows, &selected_now);
    let keep_menu_open = *keep_menu.read();
    let editing_area_now = editing_area.read().clone();
    let new_area_now = new_area.read().clone();
    let area_count = areas.len();
    let area_choices: Vec<(String, String)> = areas.iter().map(|a| (a.id.clone(), a.title.clone())).collect();

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
                    if let Some(mut cmd) = chat_command {
                        button {
                            class: "{BTN_SECONDARY}",
                            title: "Open a chat where Hobbes looks over your projects and suggests what to merge. It asks before merging anything.",
                            onclick: move |_| cmd.set(Some(ChatCommand::StartTodoInChat(TIDY_PROMPT.to_string()))),
                            "Ask Hobbes to tidy projects"
                        }
                    }
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
                                "These projects have the same or similar names. Tick the ones to merge and pick the one to keep: their to-dos and chats move into it and their names become its aliases. Nothing merges until you confirm."
                            }
                        }
                        for (exact, members) in groups.into_iter() {
                            {
                                let gkey = group_key(&members);
                                let keep_id = keep_now
                                    .get(&gkey)
                                    .filter(|k| members.iter().any(|m| &m.id == *k))
                                    .cloned()
                                    .or_else(|| default_keeper(&members))
                                    .unwrap_or_default();
                                let keep_title = members.iter().find(|m| m.id == keep_id).map(|m| m.title.clone()).unwrap_or_default();
                                let merge_ids = group_merge_ids(&members, &keep_id, &unchecked_now);
                                let n = merge_ids.len();
                                rsx! {
                                    div {
                                        key: "{gkey}",
                                        class: "flex flex-col gap-1.5 rounded-md border border-faint bg-card p-3",
                                        div {
                                            class: "flex flex-wrap items-center gap-2",
                                            span { class: "{PILL}", if exact { "Same name" } else { "Similar names" } }
                                            div { class: "flex-1" }
                                            button {
                                                class: "{BTN_PRIMARY} disabled:opacity-40 disabled:cursor-default",
                                                disabled: n == 0,
                                                title: if n == 0 { "Tick at least one other project to merge" } else { "" },
                                                onclick: {
                                                    let keep_id = keep_id.clone();
                                                    let picked: Vec<Row> = members
                                                        .iter()
                                                        .filter(|m| m.id == keep_id || merge_ids.contains(&m.id))
                                                        .cloned()
                                                        .collect();
                                                    let mut ask_merge = ask_merge;
                                                    move |_| ask_merge(keep_id.clone(), picked.clone())
                                                },
                                                "Merge {n} into \u{201c}{keep_title}\u{201d}"
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
                                            {
                                                let is_keep = m.id == keep_id;
                                                let checked = is_keep || !unchecked_now.contains(&m.id);
                                                rsx! {
                                                    div {
                                                        key: "{m.id}",
                                                        class: "flex min-w-0 items-center gap-2 text-sm text-fg",
                                                        input {
                                                            r#type: "checkbox",
                                                            class: "{CHECKBOX}",
                                                            title: if is_keep { "The project you keep" } else { "Include in the merge" },
                                                            checked,
                                                            disabled: is_keep,
                                                            onchange: {
                                                                let id = m.id.clone();
                                                                move |_| {
                                                                    let mut set = group_unchecked.write();
                                                                    if !set.remove(&id) {
                                                                        set.insert(id.clone());
                                                                    }
                                                                }
                                                            },
                                                        }
                                                        Icon { width: 13, height: 13, icon: fi_icons::FiFolder }
                                                        span { class: "min-w-0 truncate", "{m.title}" }
                                                        if !m.aka.is_empty() {
                                                            span { class: "min-w-0 truncate text-xs text-fg-muted", "{m.aka}" }
                                                        }
                                                        div { class: "flex-1" }
                                                        span { class: "{META}", "{m.meta}" }
                                                        label {
                                                            class: "flex shrink-0 cursor-pointer items-center gap-1 text-[11px] text-fg-muted",
                                                            input {
                                                                r#type: "radio",
                                                                name: "keep-{gkey}",
                                                                class: "{CHECKBOX}",
                                                                checked: is_keep,
                                                                onchange: {
                                                                    let id = m.id.clone();
                                                                    let gkey = gkey.clone();
                                                                    move |_| {
                                                                        group_keep.write().insert(gkey.clone(), id.clone());
                                                                        // The keeper is always part of the merge.
                                                                        group_unchecked.write().remove(&id);
                                                                    }
                                                                },
                                                            }
                                                            "keep"
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

                // ── Areas ────────────────────────────────────────────────
                div {
                    class: "{PANEL}",
                    div {
                        class: "flex items-center gap-2",
                        div {
                            class: "flex min-w-0 flex-1 flex-col gap-1",
                            span { class: "{EYEBROW}", "Areas" }
                            span { class: "{SUB}", "Areas group projects in the Planner sidebar." }
                        }
                        if new_area_now.is_none() {
                            button {
                                class: "{BTN_SECONDARY}",
                                onclick: move |_| new_area.set(Some(String::new())),
                                "+ New area"
                            }
                        }
                    }
                    if let Some(draft) = new_area_now {
                        input {
                            class: "w-56 rounded border border-faint bg-input px-2 py-0.5 text-sm text-fg",
                            r#type: "text",
                            placeholder: "Area name\u{2026}",
                            value: "{draft}",
                            autofocus: true,
                            oninput: move |evt: FormEvent| new_area.set(Some(evt.value())),
                            onkeydown: move |evt: KeyboardEvent| match evt.key() {
                                Key::Enter => {
                                    evt.prevent_default();
                                    let draft = new_area.peek().clone().unwrap_or_default();
                                    new_area.set(None);
                                    if draft.trim().is_empty() {
                                        return;
                                    }
                                    let created = planner.write().create_area(&draft, Utc::now());
                                    match created {
                                        Ok((id, true)) => persist_area(&planner, &id),
                                        Ok((_, false)) => notice.set(Some(format!(
                                            "An area named \u{201c}{}\u{201d} already exists.",
                                            draft.trim()
                                        ))),
                                        Err(e) => notice.set(Some(format!("Couldn't add the area: {e}"))),
                                    }
                                }
                                Key::Escape => {
                                    evt.prevent_default();
                                    evt.stop_propagation();
                                    new_area.set(None);
                                }
                                _ => {}
                            },
                        }
                    }
                    if areas.is_empty() {
                        span { class: "{SUB}", "No areas yet." }
                    }
                    div { class: "flex flex-col gap-1",
                    for (ai, a) in areas.into_iter().enumerate() {
                        div {
                            key: "{a.id}",
                            class: "flex min-w-0 items-center gap-2 text-sm text-fg",
                            Icon { width: 13, height: 13, icon: fi_icons::FiLayers }
                            match editing_area_now.clone() {
                                Some((eid, draft)) if eid == a.id => rsx! {
                                    input {
                                        class: "min-w-0 flex-1 rounded border border-faint bg-input px-2 py-0.5 text-sm text-fg",
                                        r#type: "text",
                                        value: "{draft}",
                                        autofocus: true,
                                        oninput: {
                                            let id = a.id.clone();
                                            move |evt: FormEvent| editing_area.set(Some((id.clone(), evt.value())))
                                        },
                                        onkeydown: {
                                            let id = a.id.clone();
                                            move |evt: KeyboardEvent| match evt.key() {
                                                Key::Enter => {
                                                    evt.prevent_default();
                                                    let draft = editing_area.peek().as_ref().map(|(_, d)| d.trim().to_string());
                                                    editing_area.set(None);
                                                    let Some(t) = draft.filter(|t| !t.is_empty()) else { return };
                                                    let renamed = planner.write().rename_area(&id, &t, Utc::now());
                                                    match renamed {
                                                        Ok(()) => persist_area(&planner, &id),
                                                        Err(e) => notice.set(Some(format!("Couldn't rename the area: {e}"))),
                                                    }
                                                }
                                                Key::Escape => {
                                                    evt.prevent_default();
                                                    evt.stop_propagation();
                                                    editing_area.set(None);
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
                                            let id = a.id.clone();
                                            let title = a.title.clone();
                                            move |_| editing_area.set(Some((id.clone(), title.clone())))
                                        },
                                        "{a.title}"
                                    }
                                    div { class: "flex-1" }
                                },
                            }
                            span { class: "{META}", "{plural(a.projects, \"project\", \"projects\")}" }
                            button {
                                class: "{ICON_BTN}",
                                title: "Move up",
                                disabled: ai == 0,
                                onclick: {
                                    let id = a.id.clone();
                                    move |_| {
                                        let mut p = planner;
                                        let changed = p.write().move_area(&id, true, Utc::now());
                                        let rows = snapshot_rows(&planner.peek(), &[], &changed, &[]);
                                        write_rows_async(rows);
                                    }
                                },
                                Icon { width: 12, height: 12, icon: fi_icons::FiChevronUp }
                            }
                            button {
                                class: "{ICON_BTN}",
                                title: "Move down",
                                disabled: ai + 1 >= area_count,
                                onclick: {
                                    let id = a.id.clone();
                                    move |_| {
                                        let mut p = planner;
                                        let changed = p.write().move_area(&id, false, Utc::now());
                                        let rows = snapshot_rows(&planner.peek(), &[], &changed, &[]);
                                        write_rows_async(rows);
                                    }
                                },
                                Icon { width: 12, height: 12, icon: fi_icons::FiChevronDown }
                            }
                            button {
                                class: "{TEXT_BTN}",
                                onclick: {
                                    let id = a.id.clone();
                                    let title = a.title.clone();
                                    move |_| {
                                        pending_area_delete.set(Some((id.clone(), title.clone())));
                                        area_confirm_visible.set(true);
                                    }
                                },
                                "Delete"
                            }
                        }
                    }
                    }
                }

                // ── All projects ─────────────────────────────────────────
                div {
                    class: "{PANEL}",
                    span { class: "{EYEBROW}", "All projects" }
                    if picked.len() >= 2 {
                        div {
                            class: "flex flex-wrap items-center gap-2 rounded-md border border-faint bg-card px-3 py-1.5 text-xs text-fg",
                            span { "{picked.len()} selected" }
                            div {
                                class: "relative",
                                button {
                                    class: "{BTN_PRIMARY}",
                                    onclick: move |_| {
                                        let open = *keep_menu.peek();
                                        keep_menu.set(!open);
                                    },
                                    "Merge selected\u{2026}"
                                }
                                if keep_menu_open {
                                    div {
                                        class: "fixed inset-0 z-40",
                                        onclick: move |_| keep_menu.set(false),
                                    }
                                    div {
                                        class: "{POPOVER} left-0 right-auto",
                                        div { class: "{MENU_LABEL}", "Keep which project?" }
                                        for m in picked.iter().cloned() {
                                            button {
                                                key: "{m.id}",
                                                class: "{MENU_ITEM}",
                                                onclick: {
                                                    let members = picked.clone();
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
                            span { class: "text-fg-muted", "pick which to keep" }
                            if area_count > 0 {
                                select {
                                    class: "{SELECT}",
                                    "aria-label": "Move the selected projects to an area",
                                    onchange: {
                                        let ids: Vec<String> = picked.iter().map(|r| r.id.clone()).collect();
                                        move |e: FormEvent| {
                                            let v = e.value();
                                            if v.is_empty() {
                                                return;
                                            }
                                            let area = (v != NO_AREA).then_some(v.as_str());
                                            match assign_area_persisted(planner, &ids, area) {
                                                Ok(n) => notice.set(Some(format!("Moved {}.", plural(n, "project", "projects")))),
                                                Err(e) => notice.set(Some(format!("Couldn't move them: {e}"))),
                                            }
                                        }
                                    },
                                    option { value: "", selected: true, "Move to area\u{2026}" }
                                    option { value: "{NO_AREA}", selected: false, "No area" }
                                    for (aid, atitle) in area_choices.iter().cloned() {
                                        option { key: "{aid}", value: "{aid}", selected: false, "{atitle}" }
                                    }
                                }
                            }
                            div { class: "flex-1" }
                            button {
                                class: "{BTN_SECONDARY}",
                                onclick: move |_| {
                                    selected.write().clear();
                                    keep_menu.set(false);
                                },
                                "Clear"
                            }
                        }
                    }
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
                                input {
                                    r#type: "checkbox",
                                    class: "{CHECKBOX}",
                                    title: "Select (check two or more to merge them or move them to an area)",
                                    checked: selected_now.contains(&r.id),
                                    onchange: {
                                        let id = r.id.clone();
                                        move |_| {
                                            let mut set = selected.write();
                                            if !set.remove(&id) {
                                                set.insert(id.clone());
                                            }
                                        }
                                    },
                                }
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
                                        div { class: "flex-1" }
                                    },
                                }
                                if area_count > 0 || r.area_id.is_some() {
                                    select {
                                        class: "{SELECT}",
                                        "aria-label": "Area",
                                        title: "Area",
                                        onchange: {
                                            let id = r.id.clone();
                                            move |e: FormEvent| {
                                                let v = e.value();
                                                let area = (v != NO_AREA && !v.is_empty()).then_some(v.as_str());
                                                if let Err(e) = assign_area_persisted(planner, std::slice::from_ref(&id), area) {
                                                    notice.set(Some(format!("Couldn't set the area: {e}")));
                                                }
                                            }
                                        },
                                        option { value: "{NO_AREA}", selected: r.area_id.is_none(), "No area" }
                                        for (aid, atitle) in area_choices.iter().cloned() {
                                            option {
                                                key: "{aid}",
                                                value: "{aid}",
                                                selected: r.area_id.as_deref() == Some(aid.as_str()),
                                                "{atitle}"
                                            }
                                        }
                                    }
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
                                class: "flex min-w-0 flex-wrap items-center gap-1.5 pl-12",
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
                                            class: "{TEXT_BTN}",
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
                    let manager = settings_manager.peek().clone();
                    // The in-memory half runs now; the disk half (planner
                    // rows, stored chats, journal) runs off the UI thread.
                    match merge_projects_persisted(planner, session_state, settings, &manager, &p.keep_id, &p.merge_ids) {
                        Ok((out, job)) => {
                            selected.write().retain(|id| !out.merged_ids.contains(id));
                            let head = format!(
                                "Merged {} into \u{201c}{}\u{201d}: {} moved",
                                plural(out.merged_ids.len(), "project", "projects"),
                                p.keep_title,
                                plural(out.moved_todos.len(), "to-do", "to-dos"),
                            );
                            let retagging = match p.chats {
                                Some(n) => format!("retagging {}\u{2026}", plural(n, "chat", "chats")),
                                None => "retagging chats\u{2026}".to_string(),
                            };
                            notice.set(Some(format!("{head}; {retagging}")));
                            spawn(async move {
                                let n = finish_merge(job, session_state).await;
                                notice.set(Some(format!("{head}, {} moved.", plural(n, "chat", "chats"))));
                            });
                        }
                        Err(e) => notice.set(Some(format!("Couldn't merge: {e}"))),
                    }
                },
                on_cancel: move |_| {
                    pending_merge.set(None);
                    confirm_visible.set(false);
                },
            }
            ConfirmDeleteModal {
                is_visible: area_confirm_visible,
                title: "Delete area".to_string(),
                message: area_confirm_message,
                confirm_button_text: "Delete".to_string(),
                show_dont_ask_again: false,
                on_confirm: move |_| {
                    let pending = pending_area_delete.peek().clone();
                    pending_area_delete.set(None);
                    area_confirm_visible.set(false);
                    let Some((id, title)) = pending else { return };
                    match delete_area_persisted(planner, &id) {
                        Ok(n) => notice.set(Some(format!(
                            "Deleted the area \u{201c}{title}\u{201d}; {} moved to No area.",
                            plural(n, "project", "projects")
                        ))),
                        Err(e) => notice.set(Some(format!("Couldn't delete the area: {e}"))),
                    }
                },
                on_cancel: move |_| {
                    pending_area_delete.set(None);
                    area_confirm_visible.set(false);
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
            merge_message("Hobbes", &["cai-hobbes".into()], 3, Some(1)),
            "Merge \u{201c}cai-hobbes\u{201d} into \u{201c}Hobbes\u{201d}? 3 to-dos and 1 chat move to \u{201c}Hobbes\u{201d}, and the merged name becomes its alias. This can't be undone."
        );
        let m = merge_message("Hobbes", &["a".into(), "b".into(), "c".into()], 1, Some(0));
        assert!(m.starts_with("Merge \u{201c}a\u{201d}, \u{201c}b\u{201d} and \u{201c}c\u{201d} into"), "{m}");
        assert!(m.contains("1 to-do and 0 chats"), "{m}");
        assert!(m.contains("the merged names become its aliases"), "{m}");
        // While the count runs off the UI thread.
        let m = merge_message("Hobbes", &["a".into()], 2, None);
        assert!(m.contains("counting chats"), "{m}");
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
        state.create_area("Work", Utc::now()).unwrap();
        state
    }

    fn rows_of(state: &PlannerState) -> Vec<Row> {
        state.projects.iter().map(|p| row_for(state, p, Utc::now())).collect()
    }

    #[test]
    fn manage_view_renders_duplicates_without_a_borrow_panic() {
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            use_context_provider(|| Signal::new(real_planner()));
            use_context_provider(|| Signal::new(SessionState::default()));
            use_context_provider(|| Signal::new(Settings::default()));
            use_context_provider(|| Signal::new(None::<ChatCommand>));
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
    fn group_selection_merges_only_checked_members_into_the_keeper() {
        let state = real_planner();
        let rows = rows_of(&state);
        let group: Vec<Row> = rows.iter().filter(|r| ["h1", "h2", "hb", "hp"].contains(&r.id.as_str())).cloned().collect();
        // Default keeper: most open to-dos (h2 has two).
        assert_eq!(default_keeper(&group).as_deref(), Some("h2"));
        // Everything checked: all the others merge.
        let none = HashSet::new();
        assert_eq!(group_merge_ids(&group, "hb", &none), vec!["h1", "h2", "hp"]);
        // Untick hp: it stays out; the keeper never merges into itself even
        // if it was unticked before being picked.
        let unchecked: HashSet<String> = ["hp".to_string(), "hb".to_string()].into();
        assert_eq!(group_merge_ids(&group, "hb", &unchecked), vec!["h1", "h2"]);
        // Only the keeper left checked: nothing to merge (button disabled).
        let all_but_keep: HashSet<String> = ["h1", "h2", "hp"].iter().map(|s| s.to_string()).collect();
        assert!(group_merge_ids(&group, "hb", &all_but_keep).is_empty());
    }

    #[test]
    fn selected_rows_keep_table_order() {
        let state = real_planner();
        let rows = rows_of(&state);
        let sel: HashSet<String> = ["tx".to_string(), "g1".to_string()].into();
        let picked: Vec<String> = selected_rows(&rows, &sel).into_iter().map(|r| r.id).collect();
        assert_eq!(picked, vec!["g1", "tx"]);
    }

    #[test]
    fn partial_and_selected_merges_leave_the_rest_alone() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let planner = use_signal(real_planner);
            let session_state = use_signal(SessionState::default);
            let settings = use_signal(Settings::default);
            let dir = use_hook(|| std::rc::Rc::new(tempfile::tempdir().unwrap()));
            let manager = SettingsManager::new(dir.path().join("settings.json"));

            // A partial merge of a duplicate group: hp unticked, stays.
            let (out, job) =
                merge_projects_persisted(planner, session_state, settings, &manager, "hb", &["h1".into(), "h2".into()])
                    .expect("merged");
            assert_eq!(out.merged_ids, vec!["h1", "h2"]);
            assert_eq!(job.hydrated_chats(), 0);
            {
                let state = planner.peek();
                assert!(state.projects.iter().any(|p| p.id == "hp"), "unticked member stays");
                assert_eq!(state.todos.iter().filter(|t| t.project_id.as_deref() == Some("hp")).count(), 1);
                // The groups recompute: hb and hp are still similar.
                let groups = project_identity::duplicate_groups(&state.projects);
                assert!(groups.iter().any(|g| g.ids.contains(&"hb".to_string()) && g.ids.contains(&"hp".to_string())), "{groups:?}");
            }

            // Merge-selected from the table: projects the detector never
            // grouped.
            let (out, _job) =
                merge_projects_persisted(planner, session_state, settings, &manager, "tx", &["g1".into()]).expect("merged");
            assert_eq!(out.merged_ids, vec!["g1"]);
            let state = planner.peek();
            assert!(!state.projects.iter().any(|p| p.id == "g1"));
            assert!(state.projects.iter().any(|p| p.id == "g2"));
            assert!(state.projects.iter().find(|p| p.id == "tx").unwrap().aliases.contains(&"Garbage Vibe Kids".to_string()));
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn merge_moves_todos_retags_hydrated_chats_and_trust_rules() {
        // The trust-rule change saves settings through tokio; finish_merge
        // awaits a blocking task.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let planner = use_signal(real_planner);
            let mut session_state = use_signal(SessionState::default);
            let mut settings = use_signal(Settings::default);
            let dir = use_hook(|| std::rc::Rc::new(tempfile::tempdir().unwrap()));
            let manager = SettingsManager::new(dir.path().join("settings.json"));
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

            let (out, job) = merge_projects_persisted(
                planner,
                session_state,
                settings,
                &manager,
                "hb",
                &["h1".into(), "h2".into(), "hp".into()],
            )
            .expect("merged");
            assert_eq!(out.moved_todos.len(), 4);
            // Hydrated chats move in memory before any disk work.
            assert_eq!(session_state.peek().sessions[&chat].project_id.as_deref(), Some("hb"));
            assert_eq!(job.hydrated_chats(), 1);
            {
                let state = planner.peek();
                assert_eq!(state.projects.len(), 4, "hb, g1, g2, tx remain");
                assert!(state.todos.iter().filter(|t| t.title != "todo 4").all(|t| t.project_id.as_deref() == Some("hb")));
                let hb = state.projects.iter().find(|p| p.id == "hb").unwrap();
                assert_eq!(hb.aliases, vec!["cai-hobbes", "Hobbes (cai-hobbes)"]);
            }
            assert_eq!(
                settings.peek().permission_settings.trust_rules[0].project_id.as_deref(),
                Some("hb")
            );
            // The disk half (no store in tests) reports the hydrated chat.
            let moved = tokio::runtime::Handle::current().block_on(finish_merge(job, session_state));
            assert_eq!(moved, 1);
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    #[test]
    fn area_assign_and_delete_paths_update_state() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            let planner = use_signal(real_planner);
            let work = planner.peek().areas[0].id.clone();
            let ids = vec!["h1".to_string(), "tx".to_string()];
            assert_eq!(assign_area_persisted(planner, &ids, Some(&work)).unwrap(), 2);
            assert_eq!(planner.peek().area_project_count(&work), 2);
            assert!(assign_area_persisted(planner, &ids, Some("nope")).is_err());
            assert_eq!(delete_area_persisted(planner, &work).unwrap(), 2);
            let state = planner.peek();
            assert!(state.areas.is_empty());
            assert!(state.projects.iter().all(|p| p.area_id.is_none()));
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
