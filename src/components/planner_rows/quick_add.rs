use super::super::planner_timeline::{fmt_hhmm, local_minutes_to_utc};
use super::super::planner_view::{persist_block, today, PlannerSelection};
use super::helpers::{priority_tone_class, todos_for_selection};
use super::list::ProjectPicker;
use super::{MENU_ITEM, MENU_LABEL, MENU_RULE, META, PILL, POPOVER};
use crate::settings::Settings;
use crate::todo::model::{
    self, BlockSource, HumanKind, Lane, TimeBlock, TimeOfDay, Todo, TodoBucket, TodoStatus,
};
use crate::todo::project_match::{
    self, MatchTier, ProjectIndex, ProjectResolution, Ranked, PENDING_PROJECT_ID,
};
use crate::todo::quick_add::{
    active_project_query, parse_capture, set_project_token, QuickAddParse,
};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;
use chrono::{NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::collections::HashMap;

/// Where a quick-add surface's default project came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InheritedFrom {
    /// The Planner was showing this project when ⌘K opened.
    View,
    /// The palette's previous ⇧↵ capture went here.
    LastCapture,
}

/// A surface default project — not text, so it's shown as a dashed pill.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inherited {
    pub(crate) project_id: String,
    pub(crate) why: InheritedFrom,
}

/// Everything a quick-add surface knows beyond its text. The text stays the
/// source of truth: pins only disambiguate a picked token, and a pending
/// create only exists while its `+slug` is still typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QuickAddDraft {
    pub(crate) text: String,
    /// slug → project id, written by list picks (see `ProjectIndex::with_pins`).
    pub(crate) pins: HashMap<String, String>,
    /// Surface default (the view's project, or the last ⇧↵ capture's).
    pub(crate) inherited: Option<Inherited>,
    /// "Create project …" chosen from the list: created at commit, never before.
    pub(crate) create_project: Option<String>,
    /// "Keep it yours?" was chosen on a lane suggestion: the person's
    /// explicit pick (never applied by the parser). `kind_pick` is the kind
    /// the suggestion named, accepted with it.
    pub(crate) keep_yours: bool,
    pub(crate) kind_pick: Option<HumanKind>,
}

impl QuickAddDraft {
    pub(crate) fn with_inherited(inherited: Option<Inherited>) -> Self {
        Self {
            inherited,
            ..Default::default()
        }
    }

    fn index(&self, state: &PlannerState) -> ProjectIndex {
        ProjectIndex::from_state(state)
            .with_pins(&self.pins)
            .with_pending(self.create_project.as_deref())
    }

    /// The full capture grammar, with `@worker` resolving against the live
    /// fleet's session names.
    pub(crate) fn parse(&self, state: &PlannerState, day: NaiveDate) -> QuickAddParse {
        let workers = super::super::planner_view::agent_lane::capture_workers(
            &crate::fleet::shared().snapshot(),
        );
        parse_capture(&self.text, day, &self.index(state), &workers)
    }

    /// "Hand to Hobbes": the suggestion's agent half, as a typed token (the
    /// text stays the source of truth).
    pub(crate) fn hand_to_hobbes(&mut self) {
        let t = self.text.trim_end();
        self.text = if t.is_empty() { "@hobbes ".to_string() } else { format!("{t} @hobbes ") };
        self.keep_yours = false;
        self.kind_pick = None;
    }
}

/// Lane fields a committed to-do takes from its parse plus the person's
/// suggestion pick: `(lane, kind, outcome)`. A marker always wins; a kept
/// suggestion supplies only the kind it named.
pub(crate) fn capture_lane(
    parsed: &QuickAddParse,
    draft: &QuickAddDraft,
) -> (Lane, Option<HumanKind>, Option<String>) {
    match parsed.lane {
        Some(Lane::Agent) => (Lane::Agent, None, parsed.outcome.clone()),
        _ => {
            let kind = parsed.kind.or(if draft.keep_yours { draft.kind_pick } else { None });
            (Lane::Human, kind, None)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BirthProject {
    None,
    Existing(String),
    /// Create a project with this title at commit.
    Create(String),
}

/// Where a quick-added todo is born: the placement half of the commit,
/// pure so the precedence is testable without Signals.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Birth {
    pub(crate) project: BirthProject,
    pub(crate) bucket: TodoBucket,
    pub(crate) scheduled_for: Option<NaiveDate>,
}

/// Project: a resolved `+token` beats a surface's inherited project, which
/// beats the view's own project. Bucket: the view's default, then `*day` or
/// `@clock` triage it, then filing into a project triages an Inbox birth.
pub(crate) fn resolve_birth(
    parsed: &QuickAddParse,
    draft: &QuickAddDraft,
    selection: &PlannerSelection,
    day: NaiveDate,
) -> Birth {
    let mut bucket = TodoBucket::Inbox;
    let mut scheduled_for = None;
    let mut view_project = None;
    // New todos land in the context being looked at, so quick-add never
    // makes work vanish into a different list. Scheduled births are triaged
    // (Anytime): clearing the date later must not dump them into Inbox.
    match selection {
        PlannerSelection::View(TodoView::Today) => {
            scheduled_for = Some(day);
            bucket = TodoBucket::Anytime;
        }
        PlannerSelection::View(TodoView::Upcoming) => {
            scheduled_for = day.succ_opt();
            bucket = TodoBucket::Anytime;
        }
        PlannerSelection::View(TodoView::Anytime) => bucket = TodoBucket::Anytime,
        PlannerSelection::View(TodoView::Someday) => bucket = TodoBucket::Someday,
        PlannerSelection::Project(id) => {
            view_project = Some(id.clone());
            bucket = TodoBucket::Anytime;
        }
        _ => {} // Inbox (and Logbook, which hides quick-add)
    }

    // A *day token overrides the view-context date — the user said when.
    if let Some(d) = parsed.scheduled {
        scheduled_for = Some(d);
        bucket = TodoBucket::Anytime;
    }
    // An @clock token means "this happens at that time": it implies a day
    // (today unless something already picked one).
    if parsed.block_start.is_some() {
        scheduled_for.get_or_insert(day);
        if bucket == TodoBucket::Inbox {
            bucket = TodoBucket::Anytime;
        }
    }

    let project = match &parsed.project {
        Some(p) if p.id == PENDING_PROJECT_ID => BirthProject::Create(
            draft
                .create_project
                .clone()
                .unwrap_or_else(|| p.title.clone()),
        ),
        Some(p) => BirthProject::Existing(p.id.clone()),
        None => match (&draft.inherited, view_project) {
            (Some(i), _) => BirthProject::Existing(i.project_id.clone()),
            (None, Some(id)) => BirthProject::Existing(id),
            (None, None) => BirthProject::None,
        },
    };
    // Choosing a project is a triage decision: the todo is born in Anytime.
    if project != BirthProject::None && bucket == TodoBucket::Inbox {
        bucket = TodoBucket::Anytime;
    }
    Birth {
        project,
        bucket,
        scheduled_for,
    }
}

/// Where a todo ended up, in the words the Planner's sidebar uses, led by
/// its project: "Hobbes RC · Friday at 2:00 PM".
pub(crate) fn landing_label(
    project: Option<&str>,
    scheduled_for: Option<NaiveDate>,
    bucket: TodoBucket,
    block_start: Option<u32>,
    day: NaiveDate,
) -> String {
    let place = match scheduled_for {
        Some(d) if d == day => "Today".to_string(),
        Some(d) => views::friendly_date(d, day),
        None => match bucket {
            TodoBucket::Someday => "Someday".to_string(),
            TodoBucket::Anytime => "Anytime".to_string(),
            TodoBucket::Inbox => "Inbox".to_string(),
        },
    };
    let place = match block_start {
        Some(m) => format!("{place} at {}", fmt_hhmm(m)),
        None => place,
    };
    match project {
        // A project's Anytime is just "the project".
        Some(p) if scheduled_for.is_none() && bucket == TodoBucket::Anytime => p.to_string(),
        Some(p) => format!("{p} · {place}"),
        None => place,
    }
}

/// The project title a birth lands in, for labels.
pub(crate) fn birth_project_title(state: &PlannerState, project: &BirthProject) -> Option<String> {
    match project {
        BirthProject::None => None,
        BirthProject::Create(t) => Some(t.clone()),
        BirthProject::Existing(id) => state
            .projects
            .iter()
            .find(|p| &p.id == id)
            .map(|p| p.title.clone()),
    }
}

/// `"garden-shed"` → `"Garden shed"`: the title a "Create project" row offers.
fn title_from_query(q: &str) -> String {
    let words = q.replace('-', " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Splits `title` into runs marked by whether they match `query`'s
/// characters in order (the list's highlight).
fn highlight_runs(title: &str, query: &str) -> Vec<(String, bool)> {
    let mut want = query
        .chars()
        .filter(|c| *c != '-')
        .flat_map(char::to_lowercase)
        .peekable();
    let mut runs: Vec<(String, bool)> = Vec::new();
    for c in title.chars() {
        let hit = want
            .peek()
            .is_some_and(|w| c.to_lowercase().any(|l| l == *w));
        if hit {
            want.next();
        }
        match runs.last_mut() {
            Some((s, h)) if *h == hit => s.push(c),
            _ => runs.push((c.to_string(), hit)),
        }
    }
    runs
}

/// Picking from a project picker is a text edit: it writes (or removes) the
/// `+slug` token and pins it, so the pick survives colliding slugs.
pub(crate) fn apply_project_pick(
    mut draft: Signal<QuickAddDraft>,
    state: &PlannerState,
    pick: Option<String>,
) {
    let mut d = draft.write();
    d.create_project = None;
    match pick.and_then(|id| state.projects.iter().find(|p| p.id == id)) {
        Some(p) => {
            let s = project_match::slug(&p.title);
            d.text = set_project_token(&d.text, Some(&s));
            d.pins.insert(s, p.id.clone());
        }
        None => {
            d.text = set_project_token(&d.text, None);
            d.inherited = None;
        }
    }
    if !d.text.is_empty() && !d.text.ends_with(' ') {
        d.text.push(' ');
    }
}

fn create_project(mut planner: Signal<PlannerState>, title: &str) -> String {
    let now = Utc::now();
    let sort_order = planner
        .peek()
        .projects
        .iter()
        .map(|p| p.sort_order)
        .fold(0.0f64, f64::max)
        + model::SORT_STEP;
    let project = model::Project {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.trim().to_string(),
        notes: String::new(),
        area_id: None,
        status: TodoStatus::default(),
        deadline: None,
        sort_order,
        path: None,
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = store::save_project(&project) {
        tracing::error!("planner: failed to save new project: {}", e);
    }
    let id = project.id.clone();
    planner.write().projects.push(project);
    id
}

#[component]
pub(super) fn QuickAdd(selection: Signal<PlannerSelection>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut draft = use_signal(QuickAddDraft::default);
    // "Added … → Hobbes RC", only when the todo landed outside this list.
    // The counter lets a newer confirmation outlive an older one's timer.
    let mut confirmation = use_signal(|| None::<(u64, String, String)>);

    let submit = move |_keep_open: bool| {
        let sel = selection.peek().clone();
        let d = draft.peek().clone();
        let day = today();
        let block_start = d.parse(&planner.peek(), day).block_start;
        let Some(todo) = commit_quick_add(planner, settings, &d, &sel) else {
            return; // no title yet — the preview already says so
        };
        // The input stays mounted, so focus is retained across submits.
        draft.set(QuickAddDraft::default());
        let (visible, landing) = {
            let state = planner.peek();
            let visible = todos_for_selection(&state, &sel, day)
                .iter()
                .any(|t| t.id == todo.id);
            let project = todo
                .project_id
                .as_ref()
                .and_then(|id| state.projects.iter().find(|p| &p.id == id))
                .map(|p| p.title.clone());
            (
                visible,
                landing_label(
                    project.as_deref(),
                    todo.scheduled_for,
                    todo.bucket,
                    block_start,
                    day,
                ),
            )
        };
        if visible {
            return;
        }
        let generation = confirmation.peek().as_ref().map_or(0, |c| c.0) + 1;
        confirmation.set(Some((generation, todo.title, landing)));
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            if confirmation
                .peek()
                .as_ref()
                .is_some_and(|c| c.0 == generation)
            {
                confirmation.set(None);
            }
        });
    };

    rsx! {
        div {
            class: "px-6 pb-3",
            // The composer idiom: one framed well that lights on focus, with
            // the input itself invisible inside it.
            div {
                class: "bg-input border border-subtle rounded-xl px-3.5 pt-2.5 pb-2 focus-within:ring-2 focus-within:ring-primary-500",
                QuickAddField {
                    draft,
                    selection: selection.read().clone(),
                    on_submit: submit,
                    on_escape: move |_| {
                        let _ = document::eval("document.activeElement?.blur()");
                    },
                    input_class: "w-full bg-transparent border-0 p-0 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:ring-0",
                    placeholder: "Add a to-do — @me · @hobbes · ~30m · !chore · +project",
                }
            }
            if let Some((_, title, landing)) = confirmation.read().clone() {
                div {
                    class: "mt-1.5 px-1 flex items-center gap-1.5 text-[11px] text-fg-muted",
                    Icon { width: 11, height: 11, icon: fi_icons::FiCheck }
                    span { class: "truncate", "Added “{title}” → {landing}" }
                }
            }
        }
    }
}

/// Turns a quick-add draft into a saved todo (plus a linked timeline block
/// for an `@clock` token, and the project a "Create project …" pick asked
/// for). `selection` is the context the todo is born into; tokens override
/// it (see [`resolve_birth`]). Returns the saved todo, or None — saving
/// nothing — when the text has no title. Shared by the Planner's inline
/// quick-add and the Cmd+K quick-capture palette.
pub(crate) fn commit_quick_add(
    mut planner: Signal<PlannerState>,
    settings: Signal<Settings>,
    draft: &QuickAddDraft,
    selection: &PlannerSelection,
) -> Option<Todo> {
    let day = today();
    let parsed = draft.parse(&planner.peek(), day);
    if parsed.title.is_empty() {
        return None; // tokens without a title aren't a todo yet
    }
    let birth = resolve_birth(&parsed, draft, selection, day);

    let mut todo = Todo::new(parsed.title.clone(), planner.peek().next_sort_order());
    todo.project_id = match birth.project {
        BirthProject::None => None,
        BirthProject::Existing(id) => Some(id),
        // Created only now, at commit: an abandoned draft creates nothing.
        BirthProject::Create(title) => Some(create_project(planner, &title)),
    };
    todo.bucket = birth.bucket;
    todo.scheduled_for = birth.scheduled_for;

    // Tokens beat defaults; defaults beat nothing.
    let default_estimate = settings.peek().planner_default_estimate_minutes;
    todo.estimate_minutes = parsed
        .estimate_minutes
        .or((default_estimate > 0).then_some(default_estimate));
    todo.tags = parsed.tags.clone();
    todo.deadline = parsed.deadline;
    todo.time_of_day = parsed.time_of_day;
    todo.priority = parsed.priority;

    // Lanes (SYMBIOTIC_PLANNER.md §4): a marker decides; a suggestion only
    // counts when the person picked it.
    let (lane, kind, outcome) = capture_lane(&parsed, draft);
    if lane == Lane::Agent {
        todo.set_lane(Lane::Agent);
        todo.outcome = outcome;
        // `@worker` naming a live fleet session hands it to that worker:
        // link it, and let the fleet sync derive its state.
        if let Some(w) = parsed.worker.as_deref().filter(|w| *w != "Hobbes") {
            let live = crate::fleet::shared().snapshot();
            if let Some(row) = live
                .sessions
                .values()
                .find(|r| r.ended_at.is_none() && r.display_name() == w)
            {
                todo.linked_fleet_session = Some(row.id.clone());
                todo.agent_state = None;
            }
        }
    } else {
        todo.kind = kind;
    }

    // An @clock token also implies a time-of-day group when none was given
    // explicitly, so "@7pm" lands in This Evening without a second token.
    if todo.time_of_day.is_none() {
        todo.time_of_day = parsed.block_start.map(|m| match m / 60 {
            0..=11 => TimeOfDay::Morning,
            12..=16 => TimeOfDay::Afternoon,
            _ => TimeOfDay::Evening,
        });
    }

    if let Err(e) = store::save_todo(&todo) {
        tracing::error!("planner: failed to save new todo: {}", e);
    }

    // resolve_birth already gave an @clock todo its day.
    let block = match (todo.scheduled_for, parsed.block_start) {
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
        state.upsert_todo(todo.clone());
        if let Some(b) = block {
            state.blocks.push(b);
        }
    }
    Some(todo)
}

/// One row of the `+project` autocomplete list.
#[derive(Clone, Debug, PartialEq)]
enum ProjectRow {
    Project(Ranked),
    Create(String),
}

/// Input + `+project` autocomplete + chip row, shared by the Planner's
/// inline add and the ⌘K palette. They differ only in classes and in what
/// Enter / Esc do.
#[component]
pub(crate) fn QuickAddField(
    draft: Signal<QuickAddDraft>,
    /// The context the todo is born into (the palette passes Inbox).
    selection: PlannerSelection,
    /// Enter (false) / ⇧Enter (true) with the list closed.
    on_submit: EventHandler<bool>,
    /// Esc with the list closed. The palette closes; the inline add blurs.
    on_escape: EventHandler<()>,
    input_class: String,
    placeholder: String,
    #[props(default)] autofocus: bool,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut hi = use_signal(|| 0usize);
    // Esc closes the list for this query only; typing on reopens it.
    let mut dismissed_for = use_signal(|| None::<String>);
    let mut input_el = use_signal(|| None::<std::rc::Rc<MountedData>>);

    let text = draft.read().text.clone();
    let query = active_project_query(&text).map(str::to_string);
    let rows: Vec<ProjectRow> = match &query {
        Some(q) if dismissed_for.read().as_deref() != Some(q.as_str()) => {
            let state = planner.read();
            let ix = ProjectIndex::from_state(&state).with_pins(&draft.read().pins);
            // An exact token needs no list: "… +hobbes-rc⏎" submits at once.
            let exact = matches!(
                ix.resolve(q),
                ProjectResolution::Resolved {
                    tier: MatchTier::Exact,
                    ..
                }
            );
            if exact {
                Vec::new()
            } else {
                let mut rows: Vec<ProjectRow> = ix
                    .rank(q)
                    .into_iter()
                    .take(8)
                    .map(ProjectRow::Project)
                    .collect();
                let title = title_from_query(q);
                if !title.is_empty() {
                    rows.push(ProjectRow::Create(title));
                }
                rows
            }
        }
        _ => Vec::new(),
    };
    let list_open = !rows.is_empty();
    let hi_now = (*hi.read()).min(rows.len().saturating_sub(1));

    let accept = move |row: ProjectRow| {
        {
            let mut d = draft.write();
            match row {
                ProjectRow::Project(r) => {
                    d.text = set_project_token(&d.text, Some(&r.slug));
                    d.pins.insert(r.slug, r.id);
                }
                ProjectRow::Create(title) => {
                    d.text = set_project_token(&d.text, Some(&title));
                    d.create_project = Some(title);
                }
            }
            // The trailing space closes the list; the next ↵ submits.
            d.text.push(' ');
        }
        hi.set(0);
    };

    let query_for_list = query.clone().unwrap_or_default();

    rsx! {
        div {
            class: "relative",
            input {
                class: "{input_class}",
                r#type: "text",
                placeholder: "{placeholder}",
                value: "{text}",
                autofocus,
                onmounted: move |evt| {
                    let mounted = evt.data();
                    input_el.set(Some(mounted.clone()));
                    if autofocus {
                        spawn(async move {
                            let _ = mounted.set_focus(true).await;
                        });
                    }
                },
                oninput: move |evt| {
                    draft.write().text = evt.value();
                    hi.set(0);
                },
                onkeydown: {
                    let rows = rows.clone();
                    let query = query.clone();
                    let mut accept = accept;
                    move |evt: KeyboardEvent| {
                        let key = evt.key();
                        let ctrl = evt.modifiers().ctrl();
                        // Every key the list claims is prevent_default()ed, so
                        // the global hotkey listener (which skips
                        // defaultPrevented) never sees ⌃N/⌃P or friends.
                        if list_open {
                            let step: Option<i64> = match &key {
                                Key::ArrowDown => Some(1),
                                Key::ArrowUp => Some(-1),
                                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("n") => Some(1),
                                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("p") => Some(-1),
                                _ => None,
                            };
                            if let Some(s) = step {
                                evt.prevent_default();
                                let n = rows.len() as i64;
                                hi.set((hi_now as i64 + s).rem_euclid(n) as usize);
                                return;
                            }
                            match key {
                                // ⇧↵ accepts too — it never submits past an open list.
                                Key::Enter | Key::Tab => {
                                    evt.prevent_default();
                                    if let Some(row) = rows.get(hi_now) {
                                        accept(row.clone());
                                    }
                                    return;
                                }
                                Key::Escape => {
                                    // Stop here so the palette card doesn't close.
                                    evt.prevent_default();
                                    evt.stop_propagation();
                                    dismissed_for.set(query.clone());
                                    return;
                                }
                                _ => {}
                            }
                        }
                        match key {
                            Key::Enter => {
                                evt.prevent_default();
                                on_submit.call(evt.modifiers().contains(Modifiers::SHIFT));
                            }
                            Key::Escape => {
                                evt.prevent_default();
                                evt.stop_propagation();
                                on_escape.call(());
                            }
                            // The tokenized-input gesture: ⌫ in an empty input
                            // clears the inherited project, never typed text.
                            Key::Backspace => {
                                let clear = {
                                    let d = draft.peek();
                                    d.text.is_empty() && d.inherited.is_some()
                                };
                                if clear {
                                    evt.prevent_default();
                                    draft.write().inherited = None;
                                }
                            }
                            _ => {}
                        }
                    }
                },
            }
            if list_open {
                div {
                    class: "{POPOVER} left-0 right-auto w-72",
                    div { class: "{MENU_LABEL}", "Project" }
                    for (i, row) in rows.iter().cloned().enumerate() {
                        button {
                            key: "{i}",
                            class: if i == hi_now { "{MENU_ITEM} bg-input" } else { "{MENU_ITEM}" },
                            // Keep focus (and the caret) in the input.
                            onmousedown: move |evt| evt.prevent_default(),
                            onmouseenter: move |_| hi.set(i),
                            onclick: {
                                let row = row.clone();
                                let mut accept = accept;
                                move |evt: MouseEvent| {
                                    evt.stop_propagation();
                                    accept(row.clone());
                                    if let Some(el) = input_el.peek().clone() {
                                        spawn(async move {
                                            let _ = el.set_focus(true).await;
                                        });
                                    }
                                }
                            },
                            match row.clone() {
                                ProjectRow::Project(r) => rsx! {
                                    Icon { width: 11, height: 11, icon: fi_icons::FiFolder }
                                    span {
                                        class: "flex-1 truncate",
                                        for (j, (run, hit)) in highlight_runs(&r.title, &query_for_list).into_iter().enumerate() {
                                            span { key: "{j}", class: if hit { "text-primary-300" } else { "" }, "{run}" }
                                        }
                                    }
                                    if let Some(area) = r.area.clone() {
                                        span { class: "{META}", "{area}" }
                                    }
                                },
                                ProjectRow::Create(title) => rsx! {
                                    Icon { width: 11, height: 11, icon: fi_icons::FiPlus }
                                    span { class: "flex-1 truncate", "Create project “{title}”" }
                                },
                            }
                        }
                    }
                    div {
                        class: "{MENU_RULE}",
                    }
                    div { class: "px-2.5 pb-1 text-[11px] text-fg-muted", "↑↓ choose · ↵ pick · esc close" }
                }
            }
        }
        QuickAddPreview { draft, selection, input_el }
    }
}

/// The project pill's state, first in the chip row.
#[derive(Clone, Debug, PartialEq)]
enum ProjectPill {
    /// A resolved `+token`. `not_view` names the view's project it overrides.
    Token {
        title: String,
        not_view: Option<String>,
    },
    Creating {
        title: String,
    },
    Inherited {
        title: String,
        why: InheritedFrom,
    },
}

/// The token chips under a quick-add input: what Enter will actually do.
#[component]
fn QuickAddPreview(
    draft: Signal<QuickAddDraft>,
    selection: PlannerSelection,
    input_el: Signal<Option<std::rc::Rc<MountedData>>>,
) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut picker_open = use_signal(|| false);
    // Parsed inline on every keystroke (P-012: draft is a Signal, so this
    // tracks). The chips show what Enter will actually do — the premium tell
    // of Things/Todoist quick entry is that tokens are confirmed before commit.
    let day = today();
    let (preview, birth, pill) = {
        let state = planner.read();
        let d = draft.read();
        let preview = d.parse(&state, day);
        let birth = resolve_birth(&preview, &d, &selection, day);
        let title_of = |id: &str| {
            state
                .projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.title.clone())
        };
        let pill = match &birth.project {
            BirthProject::Create(title) => Some(ProjectPill::Creating {
                title: title.clone(),
            }),
            BirthProject::Existing(id) if preview.project.is_some() => {
                let not_view = match &selection {
                    PlannerSelection::Project(v) if v != id => title_of(v),
                    _ => None,
                };
                title_of(id).map(|title| ProjectPill::Token { title, not_view })
            }
            // The inline add in a project view shows no pill: the heading
            // already says which project.
            BirthProject::Existing(id) => d.inherited.as_ref().and_then(|i| {
                title_of(id).map(|title| ProjectPill::Inherited { title, why: i.why })
            }),
            BirthProject::None => None,
        };
        (preview, birth, pill)
    };
    let current_project = match &birth.project {
        BirthProject::Existing(id) => Some(id.clone()),
        _ => None,
    };
    let miss_label = match &preview.project_miss {
        Some(ProjectResolution::NoMatch { query }) => {
            Some(format!("+{query} · no such project, stays in title"))
        }
        Some(ProjectResolution::Ambiguous { query, candidates }) => Some(format!(
            "+{query} · {candidates} projects match, stays in title"
        )),
        _ => None,
    };
    let timeline_hint = preview
        .block_start
        .map(|m| format!("Enter to add · goes on the timeline at {}", fmt_hhmm(m)));
    let priority_display = settings.read().planner_priority.clone();
    // Lane chips: what the capture grammar detected, and the person's pick
    // on a suggestion. Suggestions are offered, never applied.
    let (keep_yours, kind_pick) = {
        let d = draft.read();
        (d.keep_yours, d.kind_pick)
    };
    let lane_chip: Option<(bool, String)> = match preview.lane {
        Some(Lane::Agent) => Some((true, format!("Agent \u{b7} {}", preview.worker.clone().unwrap_or_else(|| "Hobbes".into())))),
        Some(Lane::Human) => Some((false, match preview.kind {
            Some(k) => format!("Yours \u{b7} {}", capitalize(k.label())),
            None => "Yours".to_string(),
        })),
        None if keep_yours => Some((false, match kind_pick {
            Some(k) => format!("Yours \u{b7} {}", capitalize(k.label())),
            None => "Yours".to_string(),
        })),
        None => None,
    };
    let suggestion = preview
        .lane_suggestion
        .clone()
        .filter(|_| !keep_yours && !preview.title.is_empty());

    let refocus = move || {
        if let Some(el) = input_el.peek().clone() {
            spawn(async move {
                let _ = el.set_focus(true).await;
            });
        }
    };

    rsx! {
        if preview.has_tokens() || pill.is_some() {
            div {
                class: "mt-2 flex flex-wrap items-center gap-1.5",
                if pill.is_some() || miss_label.is_some() {
                    div {
                        class: "relative flex items-center gap-1.5 shrink-0",
                        match pill.clone() {
                            Some(ProjectPill::Inherited { title, why }) => rsx! {
                                span {
                                    class: "{PILL} border-dashed border-primary-600 text-fg pr-1",
                                    button {
                                        class: "inline-flex items-center gap-1.5 max-w-48 hover:text-fg",
                                        title: "Change project",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            picker_open.toggle();
                                        },
                                        Icon { width: 10, height: 10, icon: fi_icons::FiFolder }
                                        span { class: "truncate", "{title}" }
                                        span { class: "text-fg-muted",
                                            match why {
                                                InheritedFrom::View => "· from view",
                                                InheritedFrom::LastCapture => "· from last",
                                            }
                                        }
                                    }
                                    button {
                                        class: "px-1 text-fg-muted hover:text-fg",
                                        title: "Don't file into this project (⌫ in an empty input)",
                                        onclick: move |evt| {
                                            evt.stop_propagation();
                                            draft.write().inherited = None;
                                        },
                                        "×"
                                    }
                                }
                            },
                            Some(ProjectPill::Token { title, not_view }) => rsx! {
                                button {
                                    class: "{PILL} border-primary-600 text-fg max-w-64 hover:brightness-125",
                                    title: "Change project",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        picker_open.toggle();
                                    },
                                    Icon { width: 10, height: 10, icon: fi_icons::FiFolder }
                                    span { class: "truncate", "{title}" }
                                    if let Some(v) = not_view {
                                        span { class: "text-fg-muted truncate", "· not {v}" }
                                    }
                                }
                            },
                            Some(ProjectPill::Creating { title }) => rsx! {
                                button {
                                    class: "{PILL} border-dashed border-primary-600 text-fg max-w-64",
                                    title: "Created when you add this to-do",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        picker_open.toggle();
                                    },
                                    Icon { width: 10, height: 10, icon: fi_icons::FiPlus }
                                    span { class: "truncate", "New project “{title}”" }
                                }
                            },
                            None => rsx! {},
                        }
                        if let Some(label) = miss_label.clone() {
                            button {
                                class: "{PILL} border-yellow-700 text-yellow-300 max-w-72",
                                title: "Not filed — pick a project",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    picker_open.toggle();
                                },
                                span { class: "truncate", "{label}" }
                            }
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
                                class: "{POPOVER} left-0 right-auto",
                                onkeydown: {
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Escape {
                                            evt.prevent_default();
                                            evt.stop_propagation();
                                            picker_open.set(false);
                                            refocus();
                                        }
                                    }
                                },
                                ProjectPicker {
                                    current: current_project.clone(),
                                    heading: "File under…",
                                    on_pick: {
                                        move |pick: Option<String>| {
                                            apply_project_pick(draft, &planner.peek(), pick);
                                            picker_open.set(false);
                                            refocus();
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
                if let Some((agent, label)) = lane_chip.clone() {
                    span {
                        class: if agent { "{PILL} border-[#EA8A2E]/70 text-fg" } else { "{PILL} text-fg" },
                        if agent {
                            Icon { width: 10, height: 10, icon: fi_icons::FiTerminal }
                        }
                        span { class: "font-semibold", "Lane" }
                        span { class: "truncate max-w-48", "{label}" }
                        if keep_yours && preview.lane.is_none() {
                            button {
                                class: "px-0.5 text-fg-muted hover:text-fg",
                                title: "Undo — leave it untriaged",
                                onclick: move |evt| {
                                    evt.stop_propagation();
                                    let mut d = draft.write();
                                    d.keep_yours = false;
                                    d.kind_pick = None;
                                },
                                "×"
                            }
                        }
                    }
                }
                if let Some(o) = preview.outcome.clone() {
                    span {
                        class: "{PILL} border-primary-400 text-fg max-w-80",
                        Icon { width: 10, height: 10, icon: fi_icons::FiTarget }
                        span { class: "font-semibold", "Outcome" }
                        span { class: "truncate", "{o}" }
                    }
                }
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
                if let Some(p) = preview.priority {
                    span {
                        class: "{PILL} {priority_tone_class(p)}",
                        "{priority_display.badge(p)} · {priority_display.name(p)}"
                    }
                }
                for tag in preview.tags.iter() {
                    span { class: "{PILL}", "#{tag}" }
                }
                if preview.title.is_empty() {
                    if preview.has_tokens() {
                        span { class: "text-[11px] text-fg-muted", "…needs a title" }
                    }
                } else if let Some(hint) = timeline_hint {
                    span { class: "text-[11px] text-fg-muted ml-auto", "{hint}" }
                }
            }
        }
        // Could go either way: no marker, but the text reads like an
        // outcome (or like something only you can do). Offer both; Enter
        // keeps it yours, untriaged.
        if let Some(sg) = suggestion {
            div {
                class: "mt-2 flex flex-wrap items-center gap-1.5",
                span {
                    class: "{PILL} text-fg",
                    span { class: "font-semibold", "Suggested" }
                    match sg.lane {
                        Lane::Agent => rsx! { "Agent \u{b7} Hobbes" },
                        Lane::Human => rsx! {
                            {match sg.kind {
                                Some(k) => format!("Yours \u{b7} {}", capitalize(k.label())),
                                None => "Yours".to_string(),
                            }}
                        },
                    }
                }
                span { class: "text-[11px] text-fg-muted", "{sg.reason}" }
                div { class: "flex-1" }
                button {
                    class: if sg.lane == Lane::Human { "{BTN_PRIMARY_SM}" } else { "{BTN_SECONDARY_SM}" },
                    onmousedown: move |evt| evt.prevent_default(),
                    onclick: {
                        let kind = if sg.lane == Lane::Human { sg.kind } else { None };
                        move |evt: MouseEvent| {
                            evt.stop_propagation();
                            let mut d = draft.write();
                            d.keep_yours = true;
                            d.kind_pick = kind;
                        }
                    },
                    "Keep it yours?"
                }
                button {
                    class: if sg.lane == Lane::Agent { "{BTN_PRIMARY_SM}" } else { "{BTN_SECONDARY_SM}" },
                    onmousedown: move |evt| evt.prevent_default(),
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        draft.write().hand_to_hobbes();
                        refocus();
                    },
                    "Hand to Hobbes"
                }
                span { class: "text-[11px] text-fg-muted", "\u{21b5} keeps it yours" }
            }
        }
    }
}

const BTN_PRIMARY_SM: &str =
    "text-[11px] rounded-md px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover shrink-0";
const BTN_SECONDARY_SM: &str =
    "text-[11px] rounded-md px-2 py-0.5 border border-faint bg-input text-fg hover:bg-card shrink-0";

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::planner_view::PlannerSelection;
    use super::super::test_support::d;
    use super::*;
    use crate::todo::model::{self, TodoBucket, TodoStatus};
    use crate::todo::views::TodoView;
    use crate::todo::PlannerState;
    use chrono::Utc;

    // ── Quick-add birth ─────────────────────────────────────────────────────

    fn planner_with_projects() -> PlannerState {
        let p = |id: &str, title: &str, sort_order: f64| model::Project {
            id: id.into(),
            title: title.into(),
            notes: String::new(),
            area_id: None,
            status: TodoStatus::default(),
            deadline: None,
            sort_order,
            path: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut state = PlannerState::default();
        state.projects = vec![p("rc", "Hobbes RC", 1.0), p("reno", "Home renovation", 2.0)];
        state
    }

    fn birth(draft: &QuickAddDraft, sel: &PlannerSelection) -> Birth {
        let day = d("2026-09-24");
        let parsed = draft.parse(&planner_with_projects(), day);
        resolve_birth(&parsed, draft, sel, day)
    }

    fn text(t: &str) -> QuickAddDraft {
        QuickAddDraft {
            text: t.into(),
            ..Default::default()
        }
    }

    fn inherited(id: &str) -> Option<Inherited> {
        Some(Inherited {
            project_id: id.into(),
            why: InheritedFrom::View,
        })
    }

    #[test]
    fn token_overrides_view_project() {
        let b = birth(
            &text("tiles +reno"),
            &PlannerSelection::Project("rc".into()),
        );
        assert_eq!(b.project, BirthProject::Existing("reno".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);

        let mut with_inherited = text("tiles +reno");
        with_inherited.inherited = inherited("rc");
        let b = birth(&with_inherited, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("reno".into()));
    }

    #[test]
    fn inherited_used_when_no_token() {
        let mut draft = text("ship notes");
        draft.inherited = inherited("rc");
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
        assert_eq!(
            b.bucket,
            TodoBucket::Anytime,
            "filed into a project = triaged"
        );
    }

    #[test]
    fn cleared_inherited_means_no_project() {
        let b = birth(
            &text("ship notes"),
            &PlannerSelection::View(TodoView::Inbox),
        );
        assert_eq!(b.project, BirthProject::None);
        assert_eq!(b.bucket, TodoBucket::Inbox);
    }

    #[test]
    fn view_project_is_the_inline_default() {
        let b = birth(&text("ship notes"), &PlannerSelection::Project("rc".into()));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
    }

    #[test]
    fn project_in_inbox_view_is_born_triaged() {
        let b = birth(&text("ship +rc"), &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Existing("rc".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);
        assert_eq!(b.scheduled_for, None);
    }

    #[test]
    fn project_with_someday_view_stays_someday() {
        let b = birth(
            &text("ship +rc"),
            &PlannerSelection::View(TodoView::Someday),
        );
        assert_eq!(b.bucket, TodoBucket::Someday);
        let today = birth(&text("ship +rc"), &PlannerSelection::View(TodoView::Today));
        assert_eq!(today.scheduled_for, Some(d("2026-09-24")));
    }

    #[test]
    fn create_project_only_when_token_unchanged() {
        let mut draft = text("sand it +garden-shed ");
        draft.create_project = Some("Garden shed".into());
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::Create("Garden shed".into()));
        assert_eq!(b.bucket, TodoBucket::Anytime);

        // Token edited after the pick: the pending create is dropped.
        draft.text = "sand it +garden".into();
        let b = birth(&draft, &PlannerSelection::View(TodoView::Inbox));
        assert_eq!(b.project, BirthProject::None);
        assert_eq!(b.bucket, TodoBucket::Inbox);
    }

    #[test]
    fn capture_lane_honours_markers_and_only_picked_suggestions() {
        let day = d("2026-09-24");
        let state = PlannerState::default();
        // @hobbes → agent lane, the rest is the outcome.
        let draft = text("@hobbes ship the release with tests passing");
        let parsed = draft.parse(&state, day);
        let (lane, kind, outcome) = capture_lane(&parsed, &draft);
        assert_eq!(lane, Lane::Agent);
        assert_eq!(kind, None);
        assert_eq!(outcome.as_deref(), Some("ship the release with tests passing"));

        // !chore → yours, a chore.
        let draft = text("!chore pay Comcast bill ~5m");
        let (lane, kind, _) = capture_lane(&draft.parse(&state, day), &draft);
        assert_eq!((lane, kind), (Lane::Human, Some(HumanKind::Chore)));

        // No marker: a suggestion exists but is never applied by itself.
        let mut draft = text("call mom ~20m");
        let parsed = draft.parse(&state, day);
        let sg = parsed.lane_suggestion.clone().expect("call → commitment suggestion");
        assert_eq!(sg.kind, Some(HumanKind::Commitment));
        assert_eq!(capture_lane(&parsed, &draft), (Lane::Human, None, None));
        // "Keep it yours?" accepts the suggested kind.
        draft.keep_yours = true;
        draft.kind_pick = sg.kind;
        assert_eq!(capture_lane(&parsed, &draft).1, Some(HumanKind::Commitment));

        // "Hand to Hobbes" types the marker.
        let mut draft = text("research KV-cache offload");
        draft.hand_to_hobbes();
        assert_eq!(draft.text, "research KV-cache offload @hobbes ");
        let (lane, _, outcome) = capture_lane(&draft.parse(&state, day), &draft);
        assert_eq!(lane, Lane::Agent);
        assert_eq!(outcome.as_deref(), Some("research KV-cache offload"));
    }

    #[test]
    fn create_row_titles_and_list_highlights() {
        assert_eq!(title_from_query("garden-shed"), "Garden shed");
        assert_eq!(title_from_query("x"), "X");
        let runs = highlight_runs("Home renovation", "reno");
        assert_eq!(
            runs,
            vec![
                ("Home ".to_string(), false),
                ("reno".to_string(), true),
                ("vation".to_string(), false)
            ]
        );
    }
}
