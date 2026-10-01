//! Pure view logic: which todos belong in which list, in what order.
//!
//! Kept free of SQL and Dioxus so the rules that define "Today" are testable
//! directly. The store loads everything into memory (the list is bounded by
//! human effort), so filtering here rather than in SQL keeps one definition of
//! each view for both the UI and the AI's `HOBBES_TODO_LIST`.

use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::model::{CalendarEvent, Priority, TimeOfDay, Todo, TodoBucket};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoView {
    /// Captured but not yet triaged.
    #[default]
    Inbox,
    /// Scheduled for today or earlier — the day's actual workload.
    Today,
    /// Scheduled for a future day.
    Upcoming,
    /// Triaged, undated, do whenever.
    Anytime,
    /// Deliberately deferred.
    Someday,
    /// Finished and abandoned work, newest first.
    Logbook,
}

impl TodoView {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoView::Inbox => "inbox",
            TodoView::Today => "today",
            TodoView::Upcoming => "upcoming",
            TodoView::Anytime => "anytime",
            TodoView::Someday => "someday",
            TodoView::Logbook => "logbook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "inbox" => TodoView::Inbox,
            "today" => TodoView::Today,
            "upcoming" => TodoView::Upcoming,
            "anytime" => TodoView::Anytime,
            "someday" => TodoView::Someday,
            "logbook" => TodoView::Logbook,
            _ => return None,
        })
    }
}

/// Whether a single todo belongs in a view on a given day.
///
/// Today deliberately includes todos scheduled *before* today: work you planned
/// and didn't finish is still work you owe, and silently hiding it is how a
/// planner loses the user's trust.
pub fn matches_view(todo: &Todo, view: TodoView, today: NaiveDate) -> bool {
    let open = !todo.status.is_closed();
    let undated = todo.scheduled_for.is_none();

    match view {
        TodoView::Inbox => open && undated && todo.bucket == TodoBucket::Inbox,
        TodoView::Today => open && todo.scheduled_for.is_some_and(|d| d <= today),
        TodoView::Upcoming => open && todo.scheduled_for.is_some_and(|d| d > today),
        TodoView::Anytime => open && undated && todo.bucket == TodoBucket::Anytime,
        TodoView::Someday => open && undated && todo.bucket == TodoBucket::Someday,
        TodoView::Logbook => todo.status.is_closed(),
    }
}

/// The todos in a view, ordered for display.
pub fn in_view(todos: &[Todo], view: TodoView, today: NaiveDate) -> Vec<&Todo> {
    let mut out: Vec<&Todo> = todos
        .iter()
        .filter(|t| matches_view(t, view, today))
        .collect();

    if view == TodoView::Logbook {
        // Newest first — the logbook is a record, not a queue.
        out.sort_by(|a, b| {
            b.completed_at
                .unwrap_or(b.updated_at)
                .cmp(&a.completed_at.unwrap_or(a.updated_at))
        });
    } else {
        out.sort_by(|a, b| {
            a.sort_order
                .partial_cmp(&b.sort_order)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
    }
    out
}

/// Todos finished (or abandoned) on the local day `today`, oldest first.
///
/// A deliberately separate query from `matches_view(Today)`: Today means
/// "open work owed" — the AI's list tool and the rollover both depend on
/// that — while this feeds the UI's struck-through "done today" rows so
/// checking a todo off doesn't delete it from sight.
pub fn completed_today(todos: &[Todo], today: NaiveDate) -> Vec<&Todo> {
    let mut out: Vec<&Todo> = todos
        .iter()
        .filter(|t| {
            t.status.is_closed()
                && t.completed_at
                    .is_some_and(|c| c.with_timezone(&chrono::Local).date_naive() == today)
        })
        .collect();
    out.sort_by_key(|t| t.completed_at);
    out
}

/// The one date formatter (U3.6): every user-facing date — scheduled chips,
/// deadline chips, logbook completion lines, the detail card — goes through
/// here so "the same day" never reads two different ways. "Today" /
/// "Tomorrow", otherwise "Fri 15 Aug", with the year appended only when it
/// differs from today's.
pub fn friendly_date(d: NaiveDate, today: NaiveDate) -> String {
    if d == today {
        "Today".to_string()
    } else if Some(d) == today.succ_opt() {
        "Tomorrow".to_string()
    } else if d.year() == today.year() {
        d.format("%a %-d %b").to_string()
    } else {
        d.format("%a %-d %b %Y").to_string()
    }
}

/// All-day calendar events covering the local day `day`, for enabled
/// subscriptions only. All-day events are cached but never materialized as
/// `TimeBlock`s (a 24h block would wreck the timeline ruler), so every surface
/// that shows them — the timeline banner, the `planner_today` context, the
/// HOBBES_CALENDAR_LIST tool — reads the cache through this one filter.
/// Multi-day spans show on every local day they cover; the end is exclusive,
/// so an event ending at local midnight does not bleed into the next day.
pub fn all_day_events_on(
    events: &[CalendarEvent],
    enabled_subscriptions: &std::collections::HashSet<String>,
    day: NaiveDate,
) -> Vec<CalendarEvent> {
    let mut out: Vec<CalendarEvent> = events
        .iter()
        .filter(|e| e.all_day && enabled_subscriptions.contains(&e.subscription_id))
        .filter(|e| {
            let start_day = e.start.with_timezone(&chrono::Local).date_naive();
            // Exclusive end; degenerate spans (end <= start) still cover
            // their start day.
            let last_day = (e.end - chrono::Duration::seconds(1))
                .with_timezone(&chrono::Local)
                .date_naive()
                .max(start_day);
            start_day <= day && day <= last_day
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title).then(a.uid.cmp(&b.uid)));
    out
}

/// Upcoming grouped by scheduled date, dates ascending, within each date in
/// `in_view`'s order (sort_order, then created_at). Shared by the UI's day
/// headers and (flattened) by `HOBBES_TODO_LIST`'s "upcoming" view so the two
/// surfaces can't drift.
pub fn upcoming_grouped(todos: &[Todo], today: NaiveDate) -> Vec<(NaiveDate, Vec<&Todo>)> {
    // in_view already applies the comparator; a stable grouping keeps it.
    let mut by_date: std::collections::BTreeMap<NaiveDate, Vec<&Todo>> =
        std::collections::BTreeMap::new();
    for t in in_view(todos, TodoView::Upcoming, today) {
        let Some(d) = t.scheduled_for else { continue };
        by_date.entry(d).or_default().push(t);
    }
    by_date.into_iter().collect()
}

/// Today split into the main list and Things' "This Evening" group.
///
/// The UI now renders through [`arrange`], whose Manual lens must match this
/// split exactly (`arrange_manual_is_in_view_order` pins it); kept as that
/// reference definition.
#[allow(dead_code)]
pub struct TodaySections<'a> {
    pub daytime: Vec<&'a Todo>,
    pub evening: Vec<&'a Todo>,
}

#[allow(dead_code)]
pub fn today_sections(todos: &[Todo], today: NaiveDate) -> TodaySections<'_> {
    let all = in_view(todos, TodoView::Today, today);
    let (evening, daytime) = all
        .into_iter()
        .partition(|t| t.time_of_day == Some(TimeOfDay::Evening));
    TodaySections { daytime, evening }
}

/// Open todos past their deadline, soonest-missed first.
pub fn overdue(todos: &[Todo], today: NaiveDate) -> Vec<&Todo> {
    let mut out: Vec<&Todo> = todos.iter().filter(|t| t.is_overdue(today)).collect();
    out.sort_by_key(|t| t.deadline);
    out
}

/// Pull unfinished work forward from previous days onto `today`.
///
/// Returns the number of todos moved. This is the Sunsama rollover: yesterday's
/// leftovers reappear on today's plate rather than quietly rotting in the past.
/// Deadlines are untouched — only the intended workday moves.
pub fn rollover_unfinished(todos: &mut [Todo], today: NaiveDate, now: DateTime<Utc>) -> usize {
    let mut moved = 0;
    for todo in todos.iter_mut() {
        if !todo.status.is_closed() && todo.scheduled_for.is_some_and(|d| d < today) {
            todo.scheduled_for = Some(today);
            todo.updated_at = now;
            moved += 1;
        }
    }
    moved
}

// ── Display order: sort lens, bands and groups ─────────────────────────────
//
// Everything below is a *lens*: it rearranges what the planner shows and
// never writes `sort_order`. `in_view`, `today_sections` and
// `upcoming_grouped` above stay the canonical order the AI's list tool and
// the `planner_today` context read — nothing here feeds them.

/// What a list is sorted by on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// `sort_order` — the user's intent, and the AI's canonical order.
    #[default]
    Manual,
    /// Timeline slot on the todo's scheduled day, then time-of-day, then
    /// unslotted.
    Time,
    Deadline,
    Title,
    Estimate,
    /// Eisenhower quadrant, Q1 first.
    Priority,
}

impl SortKey {
    pub const ALL: [SortKey; 6] = [
        SortKey::Manual,
        SortKey::Time,
        SortKey::Deadline,
        SortKey::Title,
        SortKey::Estimate,
        SortKey::Priority,
    ];

    /// The name on the control pill and in its menu.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Manual => "Manual",
            SortKey::Time => "Time",
            SortKey::Deadline => "Due date",
            SortKey::Title => "A–Z",
            SortKey::Estimate => "Length",
            SortKey::Priority => "Priority",
        }
    }

    /// Plain-language direction, e.g. "Soonest first". `None` for Manual,
    /// which has no direction.
    pub fn dir_label(self, dir: SortDir) -> Option<&'static str> {
        let (asc, desc) = match self {
            SortKey::Manual => return None,
            SortKey::Time => ("Earliest first", "Latest first"),
            SortKey::Deadline => ("Soonest first", "Latest first"),
            SortKey::Title => ("A → Z", "Z → A"),
            SortKey::Estimate => ("Shortest first", "Longest first"),
            SortKey::Priority => ("Q1 → Q4", "Q4 → Q1"),
        };
        Some(if dir == SortDir::Asc { asc } else { desc })
    }

    /// Keys whose values fall into header bands when nothing else claims
    /// the headers (group = None).
    pub fn is_banded(self) -> bool {
        matches!(self, SortKey::Time | SortKey::Deadline | SortKey::Priority)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

impl SortDir {
    pub fn flipped(self) -> Self {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    None,
    Project,
    /// By first tag — a row appears exactly once.
    Tag,
}

impl GroupBy {
    pub fn label(self) -> &'static str {
        match self {
            GroupBy::None => "None",
            GroupBy::Project => "Project",
            GroupBy::Tag => "Tag",
        }
    }
}

/// A sort key a [`ListOrder`] can carry. `Default` must be the list's own
/// manual order — the one with no direction and no indicator.
pub trait OrderKey: Copy + Eq + Default + 'static {
    fn label(self) -> &'static str;
    fn dir_label(self, dir: SortDir) -> Option<&'static str>;
}

/// A grouping a [`ListOrder`] can carry. `Default` must be "no grouping".
pub trait OrderGroup: Copy + Eq + Default + 'static {
    fn label(self) -> &'static str;
}

impl OrderKey for SortKey {
    fn label(self) -> &'static str {
        SortKey::label(self)
    }
    fn dir_label(self, dir: SortDir) -> Option<&'static str> {
        SortKey::dir_label(self, dir)
    }
}

impl OrderGroup for GroupBy {
    fn label(self) -> &'static str {
        GroupBy::label(self)
    }
}

/// How one list is ordered on screen. A lens: never written to todos.
///
/// Generic so other lists (the tab sidebar's `tab_list::TabListOrder`) share
/// the same lens shape and the same `ListOrderControl`; the defaults keep
/// every planner use a plain `ListOrder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListOrder<K = SortKey, G = GroupBy> {
    #[serde(default)]
    pub key: K,
    #[serde(default)]
    pub dir: SortDir,
    #[serde(default)]
    pub group: G,
}

impl<K: OrderKey, G: OrderGroup> ListOrder<K, G> {
    /// The default state: no indicator, drag handles live.
    pub fn is_manual(&self) -> bool {
        self.key == K::default() && self.group == G::default()
    }

    /// Drop anything this list doesn't offer (a stored Time sort on a list
    /// that no longer has it, say) back to its default.
    pub fn sanitized(self, keys: &[K], groups: &[G]) -> Self {
        let key = if keys.contains(&self.key) { self.key } else { K::default() };
        let group = if groups.contains(&self.group) { self.group } else { G::default() };
        // Manual has no direction; keep the stored one from leaking into
        // equality checks.
        let dir = if key == K::default() { SortDir::Asc } else { self.dir };
        ListOrder { key, dir, group }
    }
}

/// Which lists offer which keys and groups (`None` = a project list).
/// Logbook offers nothing: it's a fixed newest-first record.
pub fn order_options(view: Option<TodoView>) -> (Vec<SortKey>, Vec<GroupBy>) {
    use SortKey::*;
    let all_groups = vec![GroupBy::None, GroupBy::Project, GroupBy::Tag];
    match view {
        Some(TodoView::Today) => (SortKey::ALL.to_vec(), all_groups),
        // The day headers are Upcoming's grouping; sorts apply within a day.
        Some(TodoView::Upcoming) => (SortKey::ALL.to_vec(), Vec::new()),
        // Nothing here is scheduled, so there's no Time to sort by.
        Some(TodoView::Inbox | TodoView::Anytime | TodoView::Someday) => (
            vec![Manual, Deadline, Title, Estimate, Priority],
            all_groups,
        ),
        Some(TodoView::Logbook) => (Vec::new(), Vec::new()),
        None => (SortKey::ALL.to_vec(), vec![GroupBy::None, GroupBy::Tag]),
    }
}

/// One header's worth of rows. `label: None` is the headerless lead section.
#[derive(Debug, Clone)]
pub struct RowGroup<'a> {
    /// Stable key for Dioxus (`"band-overdue"`, `"project-{id}"`, `"evening"`).
    pub id: String,
    pub label: Option<String>,
    /// Red header tone (the Overdue band).
    pub warn: bool,
    /// A sort band (hairline-ruled header) rather than a group header.
    pub band: bool,
    pub rows: Vec<&'a Todo>,
    /// Σ estimate of the group's open rows — shown on group headers.
    pub estimate_minutes: u32,
}

/// Everything the comparators need that isn't on `Todo`.
pub struct ArrangeInputs<'a> {
    pub today: NaiveDate,
    /// todo id → earliest block start on its scheduled day, local minutes
    /// since midnight (the same query TodoRow's "@ 10:15" chip uses).
    pub slots: &'a HashMap<String, u32>,
    /// project id → (rail sort_order, title).
    pub projects: &'a HashMap<String, (f64, String)>,
    /// Keep Today's "This evening" split when nothing else claims headers.
    pub split_evening: bool,
    /// Whether banded keys may emit band headers. Off where the list already
    /// has its own headers (Upcoming's days) — headers never nest.
    pub bands: bool,
}

/// The canonical manual comparator — `in_view`'s, reused as every sort's
/// final tie-break so a sort is fully determined by the user's intent.
fn cmp_manual(a: &Todo, b: &Todo) -> Ordering {
    a.sort_order
        .partial_cmp(&b.sort_order)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.created_at.cmp(&b.created_at))
}

/// Present values in `dir`, absent values last regardless of direction.
fn cmp_opt<T: Ord>(a: Option<T>, b: Option<T>, dir: SortDir) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => match dir {
            SortDir::Asc => x.cmp(&y),
            SortDir::Desc => y.cmp(&x),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Lowercase and strip common Latin accents, so "Émail" sorts with "email".
fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'š' => 's',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' => 'z',
        c => c,
    }
}

/// A title chunk for natural ordering. Variant order matters: numbers sort
/// before words, as in Finder.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NatChunk {
    /// (digit count without leading zeros, digits) — compares numerically
    /// without overflow on absurdly long numbers.
    Num(usize, String),
    Text(String),
}

/// Case- and accent-insensitive, numeric-aware: "Step 2" < "Step 10".
pub(crate) fn natural_key(title: &str) -> Vec<NatChunk> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_num = false;
    let flush = |buf: &mut String, in_num: bool, out: &mut Vec<NatChunk>| {
        if buf.is_empty() {
            return;
        }
        let chunk = std::mem::take(buf);
        out.push(if in_num {
            let trimmed = chunk.trim_start_matches('0').to_string();
            NatChunk::Num(trimmed.len(), trimmed)
        } else {
            NatChunk::Text(chunk)
        });
    };
    for c in title.chars().flat_map(char::to_lowercase).map(fold_char) {
        let digit = c.is_ascii_digit();
        if digit != in_num {
            flush(&mut buf, in_num, &mut out);
            in_num = digit;
        }
        buf.push(c);
    }
    flush(&mut buf, in_num, &mut out);
    out
}

/// The Time key: (band rank, unslotted?, minute). Evening time-of-day claims
/// the Evening band even with an earlier slot, matching the evening split.
fn time_value(t: &Todo, inputs: &ArrangeInputs) -> Option<(u8, u8, u32)> {
    let slot = inputs.slots.get(&t.id).copied();
    let band = if t.time_of_day == Some(TimeOfDay::Evening) {
        2
    } else if let Some(m) = slot {
        match m {
            0..=719 => 0,
            720..=1019 => 1,
            _ => 2,
        }
    } else {
        match t.time_of_day {
            Some(TimeOfDay::Morning) => 0,
            Some(TimeOfDay::Afternoon) => 1,
            _ => return None,
        }
    };
    Some((band, slot.is_none() as u8, slot.unwrap_or(0)))
}

fn priority_rank(p: Priority) -> u8 {
    p.quadrant()
}

fn cmp_by(a: &Todo, b: &Todo, order: ListOrder, inputs: &ArrangeInputs) -> Ordering {
    let dir = order.dir;
    let primary = match order.key {
        SortKey::Manual => Ordering::Equal,
        SortKey::Time => cmp_opt(time_value(a, inputs), time_value(b, inputs), dir),
        SortKey::Deadline => cmp_opt(a.deadline, b.deadline, dir),
        SortKey::Title => cmp_opt(
            Some(natural_key(&a.title)),
            Some(natural_key(&b.title)),
            dir,
        ),
        SortKey::Estimate => cmp_opt(a.estimate_minutes, b.estimate_minutes, dir),
        SortKey::Priority => cmp_opt(
            a.priority.map(priority_rank),
            b.priority.map(priority_rank),
            dir,
        ),
    };
    primary.then_with(|| cmp_manual(a, b))
}

/// A band a row falls into under a banded key.
struct Band {
    rank: u8,
    slug: &'static str,
    label: String,
    warn: bool,
    /// The "no value" band: always last, whatever the direction.
    empty: bool,
}

fn band_of(t: &Todo, key: SortKey, inputs: &ArrangeInputs) -> Option<Band> {
    let band = |rank, slug, label: &str, warn, empty| Band {
        rank,
        slug,
        label: label.to_string(),
        warn,
        empty,
    };
    Some(match key {
        SortKey::Time => match time_value(t, inputs).map(|v| v.0) {
            Some(0) => band(0, "morning", "Morning", false, false),
            Some(1) => band(1, "afternoon", "Afternoon", false, false),
            Some(_) => band(2, "evening", "Evening", false, false),
            None => {
                let label = if inputs.split_evening { "Anytime today" } else { "No time" };
                band(9, "anytime", label, false, true)
            }
        },
        SortKey::Deadline => match t.deadline {
            Some(d) if d < inputs.today => band(0, "overdue", "Overdue", true, false),
            Some(d) if d == inputs.today => band(1, "due-today", "Due today", false, false),
            Some(d) if Some(d) == inputs.today.succ_opt() => {
                band(2, "due-tomorrow", "Due tomorrow", false, false)
            }
            Some(d) if (d - inputs.today).num_days() <= 6 => {
                band(3, "this-week", "This week", false, false)
            }
            Some(_) => band(4, "later", "Later", false, false),
            None => band(9, "no-deadline", "No deadline", false, true),
        },
        SortKey::Priority => match t.priority {
            Some(p) => Band {
                rank: priority_rank(p),
                slug: p.as_str(),
                label: p.label().to_string(),
                warn: false,
                empty: false,
            },
            None => band(9, "no-priority", "No priority", false, true),
        },
        _ => return None,
    })
}

fn open_estimate(rows: &[&Todo]) -> u32 {
    rows.iter()
        .filter(|t| !t.status.is_closed())
        .filter_map(|t| t.estimate_minutes)
        .fold(0u32, |acc, m| acc.saturating_add(m))
}

fn group(id: impl Into<String>, label: Option<String>, band: bool, warn: bool, rows: Vec<&Todo>) -> RowGroup<'_> {
    let estimate_minutes = open_estimate(&rows);
    RowGroup {
        id: id.into(),
        label,
        warn,
        band,
        rows,
        estimate_minutes,
    }
}

/// Open rows (already in `in_view` order) plus the day's closed rows →
/// the groups to render. Pure; unit-tested.
///
/// One level of headers: a group axis replaces bands, and bands replace the
/// evening split. Closed rows never sort by the key — they keep completion
/// order at the end of their section, so checking a row off doesn't make the
/// list jump.
pub fn arrange<'a>(
    open: Vec<&'a Todo>,
    done: Vec<&'a Todo>,
    order: ListOrder,
    inputs: &ArrangeInputs,
) -> Vec<RowGroup<'a>> {
    let mut open = open;
    open.sort_by(|a, b| cmp_by(a, b, order, inputs));

    match order.group {
        GroupBy::Project | GroupBy::Tag => arrange_grouped(open, done, order.group, inputs),
        GroupBy::None if inputs.bands && order.key.is_banded() => {
            arrange_banded(open, done, order, inputs)
        }
        GroupBy::None if inputs.split_evening => {
            let is_evening = |t: &&Todo| t.time_of_day == Some(TimeOfDay::Evening);
            let (eve_open, day_open): (Vec<&Todo>, Vec<&Todo>) = open.into_iter().partition(is_evening);
            let (eve_done, day_done): (Vec<&Todo>, Vec<&Todo>) = done.into_iter().partition(is_evening);
            let mut out = Vec::new();
            let lead: Vec<&Todo> = day_open.into_iter().chain(day_done).collect();
            if !lead.is_empty() {
                out.push(group("main", None, false, false, lead));
            }
            let evening: Vec<&Todo> = eve_open.into_iter().chain(eve_done).collect();
            if !evening.is_empty() {
                out.push(group("evening", Some("This evening".to_string()), false, false, evening));
            }
            out
        }
        GroupBy::None => {
            let rows: Vec<&Todo> = open.into_iter().chain(done).collect();
            if rows.is_empty() {
                Vec::new()
            } else {
                vec![group("main", None, false, false, rows)]
            }
        }
    }
}

fn arrange_banded<'a>(
    open: Vec<&'a Todo>,
    done: Vec<&'a Todo>,
    order: ListOrder,
    inputs: &ArrangeInputs,
) -> Vec<RowGroup<'a>> {
    let mut bands: Vec<(Band, Vec<&'a Todo>)> = Vec::new();
    for t in open {
        let Some(b) = band_of(t, order.key, inputs) else { continue };
        match bands.iter_mut().find(|x| x.0.slug == b.slug) {
            Some(x) => x.1.push(t),
            None => bands.push((b, vec![t])),
        }
    }
    // The empty band stays last; the rest follow the direction.
    bands.sort_by(|(a, _), (b, _)| {
        a.empty.cmp(&b.empty).then_with(|| match order.dir {
            SortDir::Asc => a.rank.cmp(&b.rank),
            SortDir::Desc => b.rank.cmp(&a.rank),
        })
    });
    let mut out: Vec<RowGroup<'a>> = bands
        .into_iter()
        .map(|(b, rows)| group(format!("band-{}", b.slug), Some(b.label), true, b.warn, rows))
        .collect();
    if !done.is_empty() {
        out.push(group("band-done", Some("Done today".to_string()), true, false, done));
    }
    out
}

fn arrange_grouped<'a>(
    open: Vec<&'a Todo>,
    done: Vec<&'a Todo>,
    axis: GroupBy,
    inputs: &ArrangeInputs,
) -> Vec<RowGroup<'a>> {
    // Group identity: (sort position, id suffix, label). `None` = the
    // "No project" / "Untagged" catch-all, always last.
    type GroupKey = Option<(f64, String, String)>;
    let key_of = |t: &Todo| -> GroupKey {
        match axis {
            GroupBy::Project => {
                let pid = t.project_id.as_ref()?;
                let (order, title) = inputs.projects.get(pid)?;
                Some((*order, pid.clone(), title.clone()))
            }
            GroupBy::Tag => {
                let tag = t.tags.first()?;
                Some((0.0, tag.clone(), format!("#{tag}")))
            }
            GroupBy::None => None,
        }
    };

    let mut groups: Vec<(GroupKey, Vec<&'a Todo>)> = Vec::new();
    // Open rows first (in key order), then closed rows in completion order,
    // so each group ends with its done rows.
    for t in open.into_iter().chain(done) {
        let k = key_of(t);
        let id = k.as_ref().map(|k| k.1.clone());
        match groups.iter_mut().find(|g| g.0.as_ref().map(|k| &k.1) == id.as_ref()) {
            Some(g) => g.1.push(t),
            None => groups.push((k, vec![t])),
        }
    }
    groups.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => match axis {
            GroupBy::Tag => natural_key(&x.1).cmp(&natural_key(&y.1)),
            _ => x.0.partial_cmp(&y.0).unwrap_or(Ordering::Equal).then_with(|| x.2.cmp(&y.2)),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });

    let prefix = if axis == GroupBy::Tag { "tag" } else { "project" };
    let none_label = if axis == GroupBy::Tag { "Untagged" } else { "No project" };
    groups
        .into_iter()
        .map(|(k, rows)| match k {
            Some((_, id, label)) => group(format!("{prefix}-{id}"), Some(label), false, false, rows),
            None => group(format!("{prefix}-none"), Some(none_label.to_string()), false, false, rows),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{TodoBucket, TodoStatus};

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn todo(title: &str, bucket: TodoBucket, scheduled: Option<&str>) -> Todo {
        let mut t = Todo::new(title, 0.0);
        t.bucket = bucket;
        t.scheduled_for = scheduled.map(date);
        t
    }

    fn titles(todos: Vec<&Todo>) -> Vec<String> {
        todos.into_iter().map(|t| t.title.clone()).collect()
    }

    fn fixture() -> Vec<Todo> {
        let mut done = todo("finished", TodoBucket::Anytime, Some("2026-08-12"));
        done.mark_completed(Utc::now());

        vec![
            todo("untriaged", TodoBucket::Inbox, None),
            todo("today", TodoBucket::Anytime, Some("2026-08-12")),
            todo("yesterday", TodoBucket::Anytime, Some("2026-08-11")),
            todo("tomorrow", TodoBucket::Anytime, Some("2026-08-13")),
            todo("whenever", TodoBucket::Anytime, None),
            todo("maybe", TodoBucket::Someday, None),
            done,
        ]
    }

    #[test]
    fn today_includes_unfinished_work_from_earlier_days() {
        let todos = fixture();
        let t = titles(in_view(&todos, TodoView::Today, date("2026-08-12")));
        assert!(t.contains(&"today".to_string()));
        assert!(
            t.contains(&"yesterday".to_string()),
            "work scheduled before today is still owed and must not be hidden"
        );
        assert!(!t.contains(&"tomorrow".to_string()));
        assert!(!t.contains(&"finished".to_string()));
    }

    #[test]
    fn views_are_mutually_exclusive_for_open_todos() {
        let todos = fixture();
        let today = date("2026-08-12");
        let views = [
            TodoView::Inbox,
            TodoView::Today,
            TodoView::Upcoming,
            TodoView::Anytime,
            TodoView::Someday,
        ];

        for todo in todos.iter().filter(|t| !t.status.is_closed()) {
            let hits = views.iter().filter(|v| matches_view(todo, **v, today)).count();
            assert_eq!(hits, 1, "'{}' should appear in exactly one list", todo.title);
        }
    }

    #[test]
    fn scheduling_a_todo_removes_it_from_the_undated_lists() {
        let today = date("2026-08-12");
        let mut t = todo("whenever", TodoBucket::Anytime, None);
        assert!(matches_view(&t, TodoView::Anytime, today));

        t.scheduled_for = Some(today);
        assert!(!matches_view(&t, TodoView::Anytime, today));
        assert!(matches_view(&t, TodoView::Today, today));
    }

    #[test]
    fn logbook_holds_both_completed_and_cancelled_newest_first() {
        let mut older = todo("older", TodoBucket::Anytime, None);
        older.mark_completed("2026-08-10T09:00:00Z".parse().unwrap());
        let mut newer = todo("newer", TodoBucket::Anytime, None);
        newer.mark_completed("2026-08-12T09:00:00Z".parse().unwrap());
        let mut dropped = todo("dropped", TodoBucket::Anytime, None);
        dropped.status = TodoStatus::Cancelled;
        dropped.completed_at = Some("2026-08-11T09:00:00Z".parse().unwrap());

        let todos = vec![older, newer, dropped];
        assert_eq!(
            titles(in_view(&todos, TodoView::Logbook, date("2026-08-12"))),
            vec!["newer", "dropped", "older"]
        );
    }

    #[test]
    fn today_splits_out_the_evening_group() {
        let today = date("2026-08-12");
        let mut evening = todo("read the spec", TodoBucket::Anytime, Some("2026-08-12"));
        evening.time_of_day = Some(TimeOfDay::Evening);
        let todos = vec![todo("work", TodoBucket::Anytime, Some("2026-08-12")), evening];

        let sections = today_sections(&todos, today);
        assert_eq!(titles(sections.daytime), vec!["work"]);
        assert_eq!(titles(sections.evening), vec!["read the spec"]);
    }

    /// A UTC instant on the *local* day `d`, the way completion stamps are
    /// compared. Keeps these tests independent of the machine's timezone.
    fn local_instant(d: NaiveDate, hour: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn completed_today_holds_the_day_s_closures_oldest_first() {
        let today = date("2026-08-12");
        let yesterday = date("2026-08-11");

        let mut evening = todo("evening", TodoBucket::Anytime, Some("2026-08-12"));
        evening.mark_completed(local_instant(today, 18));
        let mut morning = todo("morning", TodoBucket::Anytime, None);
        morning.mark_completed(local_instant(today, 9));
        // Cancelled still counts: it was decided today, and the row should
        // stay visible for the undo moment either way.
        let mut dropped = todo("dropped", TodoBucket::Anytime, Some("2026-08-12"));
        dropped.status = TodoStatus::Cancelled;
        dropped.completed_at = Some(local_instant(today, 12));
        let mut stale = todo("stale", TodoBucket::Anytime, Some("2026-08-12"));
        stale.mark_completed(local_instant(yesterday, 15));
        let open = todo("open", TodoBucket::Anytime, Some("2026-08-12"));

        let todos = vec![evening, morning, dropped, stale, open];
        assert_eq!(
            titles(completed_today(&todos, today)),
            vec!["morning", "dropped", "evening"],
            "yesterday's completions and open work don't belong here"
        );

        // The open-work views are untouched: completed rows never leak into
        // the AI's "today" list.
        assert_eq!(titles(in_view(&todos, TodoView::Today, today)), vec!["open"]);
    }

    #[test]
    fn rollover_moves_only_unfinished_past_work() {
        let today = date("2026-08-12");
        let now = Utc::now();
        let mut todos = fixture();

        let moved = rollover_unfinished(&mut todos, today, now);
        assert_eq!(moved, 1, "only 'yesterday' should move");

        let by_title = |name: &str| {
            todos
                .iter()
                .find(|t| t.title == name)
                .unwrap()
                .scheduled_for
        };
        assert_eq!(by_title("yesterday"), Some(today));
        assert_eq!(by_title("tomorrow"), Some(date("2026-08-13")));
        assert_eq!(by_title("whenever"), None);
        // A completed todo keeps its original date — the logbook is history.
        assert_eq!(by_title("finished"), Some(date("2026-08-12")));

        // Running it twice is a no-op.
        assert_eq!(rollover_unfinished(&mut todos, today, now), 0);
    }

    #[test]
    fn rollover_leaves_deadlines_alone() {
        let today = date("2026-08-12");
        let mut t = todo("ship it", TodoBucket::Anytime, Some("2026-08-10"));
        t.deadline = Some(date("2026-08-11"));
        let mut todos = vec![t];

        rollover_unfinished(&mut todos, today, Utc::now());
        assert_eq!(todos[0].scheduled_for, Some(today));
        assert_eq!(
            todos[0].deadline,
            Some(date("2026-08-11")),
            "rolling the plan forward must not silently extend the deadline"
        );
        assert!(todos[0].is_overdue(today));
    }

    #[test]
    fn overdue_lists_worst_first() {
        let today = date("2026-08-12");
        let mut a = todo("a", TodoBucket::Anytime, None);
        a.deadline = Some(date("2026-08-09"));
        let mut b = todo("b", TodoBucket::Anytime, None);
        b.deadline = Some(date("2026-08-11"));
        let mut fine = todo("fine", TodoBucket::Anytime, None);
        fine.deadline = Some(date("2026-08-20"));

        let todos = vec![b, fine, a];
        assert_eq!(titles(overdue(&todos, today)), vec!["a", "b"]);
    }

    #[test]
    fn friendly_date_reads_naturally_and_shows_foreign_years() {
        let today = date("2026-08-13");
        assert_eq!(friendly_date(today, today), "Today");
        assert_eq!(friendly_date(date("2026-08-14"), today), "Tomorrow");
        assert_eq!(friendly_date(date("2026-08-21"), today), "Fri 21 Aug");
        // Same-year past dates stay short; a different year must say so.
        assert_eq!(friendly_date(date("2026-01-05"), today), "Mon 5 Jan");
        assert_eq!(friendly_date(date("2027-01-04"), today), "Mon 4 Jan 2027");
        assert_eq!(friendly_date(date("2025-12-31"), today), "Wed 31 Dec 2025");
    }

    #[test]
    fn upcoming_grouped_orders_dates_then_rows() {
        let today = date("2026-08-12");
        let mut far_first = todo("far first", TodoBucket::Anytime, Some("2026-08-20"));
        far_first.sort_order = 1.0;
        let mut far_second = todo("far second", TodoBucket::Anytime, Some("2026-08-20"));
        far_second.sort_order = 2.0;
        let near = todo("near", TodoBucket::Anytime, Some("2026-08-13"));
        // Listed out of order on purpose: grouping must sort, not preserve.
        let todos = vec![far_second.clone(), near.clone(), far_first.clone()];

        let grouped = upcoming_grouped(&todos, today);
        assert_eq!(
            grouped
                .iter()
                .map(|(d, ts)| (*d, titles(ts.clone())))
                .collect::<Vec<_>>(),
            vec![
                (date("2026-08-13"), vec!["near".to_string()]),
                (
                    date("2026-08-20"),
                    vec!["far first".to_string(), "far second".to_string()]
                ),
            ]
        );

        // Today and past days never leak in — Upcoming is strictly future.
        let only_today = [todo("now", TodoBucket::Anytime, Some("2026-08-12"))];
        assert!(upcoming_grouped(&only_today, today).is_empty());
    }

    // ── arrange ────────────────────────────────────────────────────────────

    struct Fx {
        slots: HashMap<String, u32>,
        projects: HashMap<String, (f64, String)>,
    }

    impl Fx {
        fn new() -> Self {
            Fx { slots: HashMap::new(), projects: HashMap::new() }
        }
        fn inputs(&self, today: NaiveDate, split_evening: bool) -> ArrangeInputs<'_> {
            ArrangeInputs {
                today,
                slots: &self.slots,
                projects: &self.projects,
                split_evening,
                bands: true,
            }
        }
    }

    fn ordered(title: &str, sort_order: f64) -> Todo {
        let mut t = todo(title, TodoBucket::Anytime, Some("2026-08-12"));
        t.sort_order = sort_order;
        t
    }

    fn order(key: SortKey, dir: SortDir, group: GroupBy) -> ListOrder {
        ListOrder { key, dir, group }
    }

    fn shape(groups: &[RowGroup]) -> Vec<(String, Vec<String>)> {
        groups
            .iter()
            .map(|g| (g.id.clone(), g.rows.iter().map(|t| t.title.clone()).collect()))
            .collect()
    }

    fn flat(groups: &[RowGroup]) -> Vec<String> {
        groups.iter().flat_map(|g| g.rows.iter().map(|t| t.title.clone())).collect()
    }

    #[test]
    fn arrange_manual_is_in_view_order() {
        let today = date("2026-08-12");
        let mut eve = ordered("eve", 1.0);
        eve.time_of_day = Some(TimeOfDay::Evening);
        let mut done_eve = ordered("done eve", 5.0);
        done_eve.time_of_day = Some(TimeOfDay::Evening);
        done_eve.mark_completed(local_instant(today, 18));
        let mut done_day = ordered("done day", 6.0);
        done_day.mark_completed(local_instant(today, 9));
        let todos = vec![ordered("b", 3.0), eve, ordered("a", 2.0), done_eve, done_day];

        let fx = Fx::new();
        let sections = today_sections(&todos, today);
        let groups = arrange(
            in_view(&todos, TodoView::Today, today),
            completed_today(&todos, today),
            ListOrder::default(),
            &fx.inputs(today, true),
        );
        assert_eq!(
            shape(&groups),
            vec![
                ("main".to_string(), vec!["a".into(), "b".into(), "done day".into()]),
                ("evening".to_string(), vec!["eve".into(), "done eve".into()]),
            ]
        );
        assert_eq!(titles(sections.daytime), vec!["a", "b"]);
        assert_eq!(groups[1].label.as_deref(), Some("This evening"));
        assert!(<ListOrder>::default().is_manual());
    }

    #[test]
    fn empty_values_sort_last_in_both_directions() {
        let today = date("2026-08-12");
        let mut fx = Fx::new();
        let mut early = ordered("early", 3.0);
        let mut late = ordered("late", 2.0);
        let none = ordered("none", 1.0);
        early.deadline = Some(date("2026-08-13"));
        late.deadline = Some(date("2026-08-30"));
        early.estimate_minutes = Some(15);
        late.estimate_minutes = Some(90);
        early.priority = Some(Priority::UrgentImportant);
        late.priority = Some(Priority::Neither);
        fx.slots.insert(early.id.clone(), 9 * 60);
        fx.slots.insert(late.id.clone(), 15 * 60);
        let todos = vec![none, late, early];
        let open: Vec<&Todo> = todos.iter().collect();
        let mut inputs = fx.inputs(today, false);
        inputs.bands = false;

        for key in [SortKey::Time, SortKey::Deadline, SortKey::Estimate, SortKey::Priority] {
            let asc = arrange(open.clone(), vec![], order(key, SortDir::Asc, GroupBy::None), &inputs);
            assert_eq!(flat(&asc), vec!["early", "late", "none"], "{key:?} asc");
            let desc = arrange(open.clone(), vec![], order(key, SortDir::Desc, GroupBy::None), &inputs);
            assert_eq!(flat(&desc), vec!["late", "early", "none"], "{key:?} desc");
        }
    }

    #[test]
    fn ties_fall_back_to_manual_order() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let mut a = ordered("second", 2.0);
        let mut b = ordered("first", 1.0);
        a.deadline = Some(today);
        b.deadline = Some(today);
        // Handed over out of manual order on purpose.
        let todos = [a, b];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Deadline, SortDir::Desc, GroupBy::None),
            &fx.inputs(today, false),
        );
        assert_eq!(flat(&groups), vec!["first", "second"]);
    }

    #[test]
    fn title_sort_is_natural_and_accent_insensitive() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let todos = [
            ordered("Step 10", 1.0),
            ordered("step 2", 2.0),
            ordered("Émail Bob", 3.0),
            ordered("apple", 4.0),
        ];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Title, SortDir::Asc, GroupBy::None),
            &fx.inputs(today, false),
        );
        assert_eq!(flat(&groups), vec!["apple", "Émail Bob", "step 2", "Step 10"]);
    }

    #[test]
    fn time_sort_absorbs_the_evening_split() {
        let today = date("2026-08-12");
        let mut fx = Fx::new();
        let mut eve = ordered("read", 1.0);
        eve.time_of_day = Some(TimeOfDay::Evening);
        let morning = ordered("standup", 2.0);
        let loose = ordered("whenever", 3.0);
        fx.slots.insert(morning.id.clone(), 9 * 60);
        let todos = [eve, morning, loose];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Time, SortDir::Asc, GroupBy::None),
            &fx.inputs(today, true),
        );
        assert_eq!(
            shape(&groups),
            vec![
                ("band-morning".to_string(), vec!["standup".into()]),
                ("band-evening".to_string(), vec!["read".into()]),
                ("band-anytime".to_string(), vec!["whenever".into()]),
            ]
        );
        assert!(groups.iter().all(|g| g.id != "evening"), "no separate This evening header");
        assert_eq!(groups[2].label.as_deref(), Some("Anytime today"));
    }

    #[test]
    fn desc_reverses_bands_but_keeps_the_empty_band_last() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let mut overdue = ordered("overdue", 1.0);
        overdue.deadline = Some(date("2026-08-01"));
        let mut later = ordered("later", 2.0);
        later.deadline = Some(date("2026-09-30"));
        let none = ordered("none", 3.0);
        let todos = [overdue, later, none];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Deadline, SortDir::Desc, GroupBy::None),
            &fx.inputs(today, true),
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, vec!["band-later", "band-overdue", "band-no-deadline"]);
        assert!(groups[1].warn, "Overdue reads red");
        assert!(groups.iter().all(|g| g.band));
    }

    #[test]
    fn priority_bands_follow_the_quadrants() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let mut q2 = ordered("plan", 1.0);
        q2.priority = Some(Priority::Important);
        let mut q1 = ordered("fire", 2.0);
        q1.priority = Some(Priority::UrgentImportant);
        let unset = ordered("untriaged", 3.0);
        let todos = [q2, q1, unset];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Priority, SortDir::Asc, GroupBy::None),
            &fx.inputs(today, true),
        );
        assert_eq!(
            shape(&groups),
            vec![
                ("band-urgent_important".to_string(), vec!["fire".into()]),
                ("band-important".to_string(), vec!["plan".into()]),
                ("band-no-priority".to_string(), vec!["untriaged".into()]),
            ]
        );
    }

    #[test]
    fn one_level_of_headers() {
        let today = date("2026-08-12");
        let mut fx = Fx::new();
        fx.projects.insert("p_b".into(), (2.0, "Beta".into()));
        fx.projects.insert("p_a".into(), (1.0, "Alpha".into()));
        let mut a = ordered("a", 1.0);
        a.project_id = Some("p_b".into());
        a.deadline = Some(date("2026-08-20"));
        a.estimate_minutes = Some(30);
        let mut b = ordered("b", 2.0);
        b.project_id = Some("p_b".into());
        b.deadline = Some(date("2026-08-01"));
        b.estimate_minutes = Some(45);
        let mut c = ordered("c", 3.0);
        c.project_id = Some("p_a".into());
        c.time_of_day = Some(TimeOfDay::Evening);
        let d = ordered("d", 4.0);
        let todos = [a, b, c, d];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Deadline, SortDir::Asc, GroupBy::Project),
            &fx.inputs(today, true),
        );
        assert_eq!(
            shape(&groups),
            vec![
                ("project-p_a".to_string(), vec!["c".into()]),
                ("project-p_b".to_string(), vec!["b".into(), "a".into()]),
                ("project-none".to_string(), vec!["d".into()]),
            ]
        );
        assert!(groups.iter().all(|g| !g.band), "project headers only, no bands");
        assert_eq!(groups[1].label.as_deref(), Some("Beta"));
        assert_eq!(groups[1].estimate_minutes, 75);
    }

    #[test]
    fn tag_grouping_places_each_row_once() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let mut a = ordered("a", 1.0);
        a.tags = vec!["work".into(), "admin".into()];
        let mut b = ordered("b", 2.0);
        b.tags = vec!["admin".into()];
        let c = ordered("c", 3.0);
        let todos = [a, b, c];
        let groups = arrange(
            todos.iter().collect(),
            vec![],
            order(SortKey::Manual, SortDir::Asc, GroupBy::Tag),
            &fx.inputs(today, true),
        );
        assert_eq!(
            shape(&groups),
            vec![
                ("tag-admin".to_string(), vec!["b".into()]),
                ("tag-work".to_string(), vec!["a".into()]),
                ("tag-none".to_string(), vec!["c".into()]),
            ]
        );
        assert_eq!(flat(&groups).len(), 3);
    }

    #[test]
    fn done_rows_never_sort_by_key() {
        let today = date("2026-08-12");
        let fx = Fx::new();
        let open = ordered("open", 1.0);
        let mut z = ordered("zulu", 2.0);
        z.mark_completed(local_instant(today, 9));
        let mut a = ordered("alpha", 3.0);
        a.mark_completed(local_instant(today, 10));
        let todos = [open, z, a];
        let done: Vec<&Todo> = completed_today(&todos, today);

        // Plain sort: done rows trail in completion order.
        let groups = arrange(
            vec![&todos[0]],
            done.clone(),
            order(SortKey::Title, SortDir::Asc, GroupBy::None),
            &fx.inputs(today, true),
        );
        assert_eq!(flat(&groups), vec!["open", "zulu", "alpha"]);

        // Banded sort: they collect in a trailing "Done today" band.
        let groups = arrange(
            vec![&todos[0]],
            done,
            order(SortKey::Deadline, SortDir::Asc, GroupBy::None),
            &fx.inputs(today, true),
        );
        let last = groups.last().unwrap();
        assert_eq!(last.id, "band-done");
        assert_eq!(last.rows.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["zulu", "alpha"]);
    }

    #[test]
    fn list_order_round_trips_and_defaults() {
        let o = order(SortKey::Priority, SortDir::Desc, GroupBy::Tag);
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<ListOrder>(&json).unwrap(), o);
        assert_eq!(serde_json::from_str::<ListOrder>("{}").unwrap(), ListOrder::default());
        assert_eq!(<ListOrder>::default().key, SortKey::Manual);

        // A key the list doesn't offer falls back to Manual.
        let (keys, groups) = order_options(Some(TodoView::Inbox));
        let stored = order(SortKey::Time, SortDir::Desc, GroupBy::Project);
        assert_eq!(
            stored.sanitized(&keys, &groups),
            order(SortKey::Manual, SortDir::Asc, GroupBy::Project)
        );
        assert!(order_options(Some(TodoView::Logbook)).0.is_empty());
        assert!(order_options(Some(TodoView::Upcoming)).1.is_empty());
    }

    #[test]
    fn view_names_round_trip() {
        for view in [
            TodoView::Inbox,
            TodoView::Today,
            TodoView::Upcoming,
            TodoView::Anytime,
            TodoView::Someday,
            TodoView::Logbook,
        ] {
            assert_eq!(TodoView::parse(view.as_str()), Some(view));
        }
        assert_eq!(TodoView::parse("  TODAY "), Some(TodoView::Today));
        assert_eq!(TodoView::parse("nonsense"), None);
    }
}
