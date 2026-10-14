use super::super::planner_view::mutate_todo;
use crate::todo::model::{TimeOfDay, Todo, TodoBucket};
use crate::todo::store;
use crate::todo::PlannerState;
use chrono::NaiveDate;
use dioxus::prelude::*;

/// How loudly a deadline should read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DueTone {
    Neutral,
    /// Due today, tomorrow, or the day after.
    Soon,
    Overdue,
}

pub(super) fn due_tone(due: NaiveDate, today: NaiveDate) -> DueTone {
    if due < today {
        DueTone::Overdue
    } else if (due - today).num_days() <= 2 {
        DueTone::Soon
    } else {
        DueTone::Neutral
    }
}

pub(super) fn due_tone_class(tone: DueTone) -> &'static str {
    match tone {
        DueTone::Neutral => "border-faint text-fg-muted",
        DueTone::Soon => "border-yellow-700 text-yellow-300",
        DueTone::Overdue => "border-red-700 text-red-400",
    }
}

/// The date pill's text. `None` when the todo has no scheduled day — the pill
/// is the *date's* control, so an undated todo simply doesn't wear one.
///
/// "Evening" wins only on today: on any other day the day itself is the more
/// useful word, and the evening grouping is visible from its section header.
pub(super) fn date_pill_label(
    scheduled_for: Option<NaiveDate>,
    evening: bool,
    today: NaiveDate,
) -> Option<String> {
    let d = scheduled_for?;
    if evening && d == today {
        return Some("Evening".to_string());
    }
    if d == today {
        return Some("Today".to_string());
    }
    if Some(d) == today.succ_opt() {
        return Some("Tomorrow".to_string());
    }
    Some(d.format("%a %-d").to_string())
}

/// One "when" the schedule popover (and the detail card's date field) can set.
///
/// Every one of these is a three-step mutation — patch the todo, prune the
/// blocks that no longer sit on its day, delete those block rows — which used
/// to be hand-rolled at each call site. [`apply_schedule_choice`] is the pure
/// patch; [`set_schedule`] is the whole sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleChoice {
    Today,
    ThisEvening,
    Tomorrow,
    On(NaiveDate),
    /// Drop the date without touching the bucket — the detail card's "Clear".
    ClearDate,
    /// Undated and triaged.
    Anytime,
    Someday,
}

pub(crate) fn apply_schedule_choice(t: &mut Todo, choice: ScheduleChoice, today: NaiveDate) {
    match choice {
        ScheduleChoice::Today => {
            t.scheduled_for = Some(today);
            t.time_of_day = None;
        }
        // Evening is a time-of-day, not a reschedule: a dated todo keeps its
        // day (yanking an Upcoming todo onto today was the U1.4 bug); only an
        // undated one gets scheduled today.
        ScheduleChoice::ThisEvening => {
            t.time_of_day = Some(TimeOfDay::Evening);
            if t.scheduled_for.is_none() {
                t.scheduled_for = Some(today);
            }
        }
        ScheduleChoice::Tomorrow => {
            t.scheduled_for = today.succ_opt();
            t.time_of_day = None;
        }
        // A picked day leaves the time-of-day grouping alone (the detail
        // card's date field has always behaved this way).
        ScheduleChoice::On(d) => t.scheduled_for = Some(d),
        // An undated todo has no day for a time-of-day group to belong to.
        ScheduleChoice::ClearDate => {
            t.scheduled_for = None;
            t.time_of_day = None;
        }
        ScheduleChoice::Anytime => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Anytime;
        }
        ScheduleChoice::Someday => {
            t.scheduled_for = None;
            t.time_of_day = None;
            t.bucket = TodoBucket::Someday;
        }
    }
}

/// Which popover entry wears the tick.
pub(super) fn current_schedule_choice(todo: &Todo, today: NaiveDate) -> ScheduleChoice {
    if todo.bucket == TodoBucket::Someday {
        return ScheduleChoice::Someday;
    }
    match todo.scheduled_for {
        Some(d) if d == today && todo.time_of_day == Some(TimeOfDay::Evening) => {
            ScheduleChoice::ThisEvening
        }
        Some(d) if d == today => ScheduleChoice::Today,
        Some(d) if Some(d) == today.succ_opt() => ScheduleChoice::Tomorrow,
        Some(d) => ScheduleChoice::On(d),
        None => ScheduleChoice::Anytime,
    }
}

/// The whole schedule mutation: patch, prune the now-orphaned blocks, delete
/// their rows. The single owner of the sequence — the row popover, the detail
/// card and anything else that moves a todo's day all go through here (the
/// AI's update and plan-day handlers run the same rule server-side).
pub(crate) fn set_schedule(
    mut planner: Signal<PlannerState>,
    id: &str,
    choice: ScheduleChoice,
    today: NaiveDate,
) {
    mutate_todo(planner, id, |t| apply_schedule_choice(t, choice, today));
    let keep = planner.peek().todo(id).and_then(|t| t.scheduled_for);
    let pruned = planner.write().prune_blocks_for_todo(id, keep);
    for b in &pruned {
        if let Err(e) = store::delete_block(&b.id) {
            tracing::error!(
                "planner: failed to delete rescheduled block {}: {}",
                b.id,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::d;
    use super::*;
    use crate::todo::model::{TimeOfDay, Todo, TodoBucket};

    // ── due_tone ───────────────────────────────────────────────────────────

    #[test]
    fn due_tone_reads_the_next_two_days_as_soon() {
        let today = d("2026-09-16");
        assert_eq!(due_tone(d("2026-09-15"), today), DueTone::Overdue);
        assert_eq!(due_tone(today, today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-17"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-18"), today), DueTone::Soon);
        assert_eq!(due_tone(d("2026-09-19"), today), DueTone::Neutral);
    }

    // ── date_pill_label ────────────────────────────────────────────────────

    #[test]
    fn date_pill_label_names_the_day() {
        let today = d("2026-09-16"); // a Wednesday
        assert_eq!(date_pill_label(None, false, today), None);
        assert_eq!(
            date_pill_label(Some(today), false, today).as_deref(),
            Some("Today")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-17")), false, today).as_deref(),
            Some("Tomorrow")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), false, today).as_deref(),
            Some("Fri 18")
        );
        // Evening only rebrands today's pill; a future evening still says the day.
        assert_eq!(
            date_pill_label(Some(today), true, today).as_deref(),
            Some("Evening")
        );
        assert_eq!(
            date_pill_label(Some(d("2026-09-18")), true, today).as_deref(),
            Some("Fri 18")
        );
    }

    // ── the shared schedule helper ─────────────────────────────────────────

    #[test]
    fn apply_schedule_choice_pins_the_row_menu_behaviour() {
        let today = d("2026-09-16");
        let tomorrow = d("2026-09-17");

        let mut t = Todo::new("x", 0.0);
        t.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut t, ScheduleChoice::Today, today);
        assert_eq!(t.scheduled_for, Some(today));
        assert_eq!(t.time_of_day, None, "Today means the daytime list");

        apply_schedule_choice(&mut t, ScheduleChoice::Tomorrow, today);
        assert_eq!(t.scheduled_for, Some(tomorrow));
        assert_eq!(t.time_of_day, None);

        // Evening is a time-of-day, not a reschedule: a dated todo keeps its day.
        apply_schedule_choice(&mut t, ScheduleChoice::ThisEvening, today);
        assert_eq!(t.scheduled_for, Some(tomorrow), "the day is not yanked");
        assert_eq!(t.time_of_day, Some(TimeOfDay::Evening));

        // ...but an undated one lands on today.
        let mut u = Todo::new("y", 0.0);
        apply_schedule_choice(&mut u, ScheduleChoice::ThisEvening, today);
        assert_eq!(u.scheduled_for, Some(today));
        assert_eq!(u.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn apply_schedule_choice_buckets_and_clears() {
        let today = d("2026-09-16");

        // Someday clears the date and the grouping — and, with no date left,
        // `set_schedule` prunes every block (keep = None).
        let mut t = Todo::new("x", 0.0);
        t.scheduled_for = Some(today);
        t.time_of_day = Some(TimeOfDay::Evening);
        t.bucket = TodoBucket::Anytime;
        apply_schedule_choice(&mut t, ScheduleChoice::Someday, today);
        assert_eq!(t.scheduled_for, None);
        assert_eq!(t.time_of_day, None);
        assert_eq!(t.bucket, TodoBucket::Someday);

        // Anytime is the inverse: undated, but triaged rather than deferred.
        apply_schedule_choice(&mut t, ScheduleChoice::Anytime, today);
        assert_eq!(t.bucket, TodoBucket::Anytime);

        // ClearDate (the detail card's "Clear") leaves the bucket alone.
        let mut s = Todo::new("y", 0.0);
        s.bucket = TodoBucket::Someday;
        s.scheduled_for = Some(today);
        s.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut s, ScheduleChoice::ClearDate, today);
        assert_eq!(s.scheduled_for, None);
        assert_eq!(s.time_of_day, None);
        assert_eq!(s.bucket, TodoBucket::Someday);

        // A picked day sets the date without touching the grouping.
        let mut p = Todo::new("z", 0.0);
        p.time_of_day = Some(TimeOfDay::Evening);
        apply_schedule_choice(&mut p, ScheduleChoice::On(d("2026-10-01")), today);
        assert_eq!(p.scheduled_for, Some(d("2026-10-01")));
        assert_eq!(p.time_of_day, Some(TimeOfDay::Evening));
    }

    #[test]
    fn current_schedule_choice_ticks_the_todays_state() {
        let today = d("2026-09-16");
        let mut t = Todo::new("x", 0.0);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Anytime);

        t.bucket = TodoBucket::Someday;
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Someday);

        t.bucket = TodoBucket::Anytime;
        t.scheduled_for = Some(today);
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Today);

        t.time_of_day = Some(TimeOfDay::Evening);
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::ThisEvening
        );

        t.time_of_day = None;
        t.scheduled_for = Some(d("2026-09-17"));
        assert_eq!(current_schedule_choice(&t, today), ScheduleChoice::Tomorrow);

        t.scheduled_for = Some(d("2026-10-01"));
        assert_eq!(
            current_schedule_choice(&t, today),
            ScheduleChoice::On(d("2026-10-01"))
        );
    }
}
