use chrono::{DateTime, Local, NaiveDate, Utc};
use crate::fleet::{FleetOrigin, FleetSession, FleetStatus};
use crate::todo::model::{self, FocusActor, FocusSession, Priority, TimeBlock, Todo, TodoStatus};
use crate::todo::views::{self, ListOrder, TodoView};
use super::super::planner_view::{PlannerSelection};
use std::collections::HashMap;
use crate::todo::PlannerState;
use super::{DEFAULT_PROFILE_COLOR};

pub(super) fn cycle_estimate(current: Option<u32>) -> Option<u32> {
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
pub(super) fn activation_seed(todo: &Todo) -> String {
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
pub(super) fn who_state(
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
pub(super) fn profile_stripe_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-l-{}", rest),
        _ => "border-l-blue-600".to_string(),
    }
}

pub(super) fn profile_pill_border_class(bg_class: &str) -> String {
    match bg_class.trim().strip_prefix("bg-") {
        Some(rest) if !rest.is_empty() => format!("border-{}", rest),
        _ => "border-faint".to_string(),
    }
}

/// Minutes the carrier has been working, for the session pill's tail.
pub(super) fn working_minutes(
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

pub(super) fn view_title(view: TodoView) -> &'static str {
    match view {
        TodoView::Inbox => "Inbox",
        TodoView::Today => "Today",
        TodoView::Upcoming => "Upcoming",
        TodoView::Anytime => "Anytime",
        TodoView::Someday => "Someday",
        TodoView::Logbook => "Logbook",
    }
}

pub(super) fn empty_state_line(selection: &PlannerSelection) -> &'static str {
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
pub(super) fn todos_for_selection(state: &PlannerState, selection: &PlannerSelection, day: NaiveDate) -> Vec<Todo> {
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

/// A todo's earliest timeline block on its scheduled day. The one query
/// behind both the row's "@ 10:15" chip and the Time sort, so the chip and
/// the sort can't disagree.
pub(super) fn first_block_on_day<'a>(blocks: &'a [TimeBlock], todo_id: &str, day: NaiveDate) -> Option<&'a TimeBlock> {
    blocks
        .iter()
        .filter(|b| {
            b.todo_id.as_deref() == Some(todo_id) && b.start.with_timezone(&Local).date_naive() == day
        })
        .min_by_key(|b| b.start)
}

/// todo id → local minutes since midnight of its first block on its
/// scheduled day. Built once per list render for the Time sort.
pub(super) fn block_slots(todos: &[Todo], blocks: &[TimeBlock]) -> HashMap<String, u32> {
    use chrono::Timelike;
    todos
        .iter()
        .filter(|t| !t.status.is_closed())
        .filter_map(|t| {
            let day = t.scheduled_for?;
            let start = first_block_on_day(blocks, &t.id, day)?.start.with_timezone(&Local);
            Some((t.id.clone(), start.hour() * 60 + start.minute()))
        })
        .collect()
}

/// The quadrant badge's tone, from the colours rows already use: red for
/// Q1 (the overdue red), primary for Q2, yellow for Q3 (the due-soon
/// yellow), muted for Q4.
pub(super) fn priority_tone_class(p: Priority) -> &'static str {
    match p {
        Priority::UrgentImportant => "border-red-700 text-red-400",
        Priority::Important => "border-primary-600 text-primary-300",
        Priority::Urgent => "border-yellow-700 text-yellow-300",
        Priority::Neither => "border-faint text-fg-muted",
    }
}

/// The toolbar's left-hand hint. `None` in the default state, which shows
/// no indicator at all.
pub(super) fn order_hint(order: ListOrder) -> Option<String> {
    if order.is_manual() {
        return None;
    }
    Some(match order.key.dir_label(order.dir) {
        None => format!(
            "Grouped by {} — drag within a group to reorder",
            order.group.label().to_lowercase()
        ),
        Some(dir) => format!(
            "{} · {} — drag a row to make this your order",
            order.key.label(),
            dir
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::fleet::AttentionKind;
    use crate::fleet::{FleetOrigin, FleetSession, FleetStatus};
    use crate::todo::model::{FocusActor, FocusSession, Priority, Todo, TodoStatus};
    use crate::todo::views::{GroupBy, ListOrder, SortDir, SortKey};
    use super::super::{DEFAULT_PROFILE_COLOR, DueTone, due_tone_class};

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
            dispatched_from: None,
            continuation_sent: false,
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

    #[test]
    fn order_hint_is_silent_in_the_default_state() {
        assert_eq!(order_hint(ListOrder::default()), None);
        let sorted = ListOrder { key: SortKey::Deadline, dir: SortDir::Asc, group: GroupBy::None };
        assert_eq!(
            order_hint(sorted).as_deref(),
            Some("Due date · Soonest first — drag a row to make this your order")
        );
        let grouped = ListOrder { group: GroupBy::Project, ..ListOrder::default() };
        assert_eq!(
            order_hint(grouped).as_deref(),
            Some("Grouped by project — drag within a group to reorder")
        );
    }

    #[test]
    fn priority_tones_reuse_existing_row_colours() {
        assert_eq!(priority_tone_class(Priority::UrgentImportant), due_tone_class(DueTone::Overdue));
        assert_eq!(priority_tone_class(Priority::Urgent), due_tone_class(DueTone::Soon));
        assert_eq!(priority_tone_class(Priority::Neither), due_tone_class(DueTone::Neutral));
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
