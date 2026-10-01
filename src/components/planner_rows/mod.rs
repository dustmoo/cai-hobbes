//! Planner centre column: the Things-style todo list surfaces.
//!
//! Split out of `planner_view.rs` (P-split for concurrent editing) — keeps
//! `CentreColumn`, `QuickAdd`, `TodoList`, and `TodoRow`, plus the pure
//! helpers only they use. Root types (`PlannerSelection`, persistence
//! helpers, `today()`) and timeline geometry live in the sibling modules
//! `planner_view` and `planner_timeline`.
//!
//! The row is a three-column grid — WHO (status/actor), WHAT (title, project,
//! carrier session), WHEN (estimate, timebox, due, schedule) — so a scan down
//! the list answers the three questions in fixed places instead of reading a
//! ragged chip stream.

mod centre;
mod helpers;
mod list;
mod quick_add;
mod reorder;
mod row;
mod schedule;
#[cfg(test)]
mod test_support;

pub(crate) use centre::{CentreColumn, ListOrderControl};
pub(crate) use list::ProjectPicker;
pub(crate) use quick_add::{
    apply_project_pick, birth_project_title, commit_quick_add, landing_label, resolve_birth,
    BirthProject, Inherited, InheritedFrom, QuickAddDraft, QuickAddField,
};
pub(crate) use schedule::{set_schedule, ScheduleChoice};

// ── Shared class vocabulary ────────────────────────────────────────────────

/// Mirrors `default_profile_color()` in `settings.rs` (private there).
const DEFAULT_PROFILE_COLOR: &str = "bg-blue-600";

const PILL: &str = "h-[22px] inline-flex items-center rounded-full border border-faint px-2 text-[11px] text-fg-muted gap-1.5 shrink-0";
const META: &str = "font-mono text-[11px] tabular-nums text-fg-muted shrink-0";
const BTN_PRIMARY: &str =
    "text-[11px] rounded px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover shrink-0";
const BTN_SECONDARY: &str =
    "text-[11px] rounded px-2 py-0.5 border border-faint text-fg hover:bg-input shrink-0";
const POPOVER: &str = "absolute right-0 top-full mt-1 w-56 bg-card border border-subtle rounded-lg shadow-xl z-50 py-1";
const MENU_ITEM: &str =
    "w-full flex items-center gap-2 px-2.5 py-1.5 text-[11px] text-fg text-left hover:bg-input";
pub(crate) const SECTION_HEADER: &str =
    "text-[11px] uppercase tracking-[0.08em] text-fg-muted px-2.5 pt-3.5 pb-1";

const MENU_LABEL: &str = "px-2.5 pt-1.5 pb-1 text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const MENU_RULE: &str = "h-px bg-[var(--color-border-muted)] my-1";
/// The faint "this is what the list is sorted by" ring on a row's chip.
const KEY_RING: &str = " ring-1 ring-primary-300/50";
