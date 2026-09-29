use super::super::planner_view::persist_todo;
use crate::todo::views::{ListOrder, SortDir, SortKey};
use crate::todo::PlannerState;
use chrono::Utc;
use dioxus::prelude::*;

/// Where a menu or keyboard move sends a row within its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reorder {
    Top,
    Bottom,
    Up,
    Down,
}

/// The destination index for a [`Reorder`], or `None` when it's a no-op.
pub(super) fn reorder_target(from: usize, len: usize, r: Reorder) -> Option<usize> {
    if from >= len {
        return None;
    }
    let to = match r {
        Reorder::Top => 0,
        Reorder::Bottom => len - 1,
        Reorder::Up => from.checked_sub(1)?,
        Reorder::Down => (from + 1 < len).then_some(from + 1)?,
    };
    (to != from).then_some(to)
}

/// `ids` with the row at `from` moved to `to`.
fn moved(ids: &[String], from: usize, to: usize) -> Vec<String> {
    let mut out = ids.to_vec();
    if from < out.len() {
        let id = out.remove(from);
        out.insert(to.min(out.len()), id);
    }
    out
}

/// A row's height (`h-9`): a drag's insertion index is the grab index plus
/// whole rows travelled, so nothing is measured mid-drag.
const ROW_HEIGHT_PX: f64 = 36.0;

pub(super) fn drag_over_index(from: usize, len: usize, dy: f64) -> usize {
    let steps = (dy / ROW_HEIGHT_PX).round() as i64;
    (from as i64 + steps).clamp(0, len.saturating_sub(1) as i64) as usize
}

/// The on-screen open order across every group, with one group's segment
/// replaced by its post-move order — what an adopt makes the manual order.
fn flatten_with(
    groups: &[(String, Vec<String>)],
    group_id: &str,
    replacement: &[String],
) -> Vec<String> {
    groups
        .iter()
        .flat_map(|(id, ids)| {
            if id == group_id {
                replacement.to_vec()
            } else {
                ids.clone()
            }
        })
        .collect()
}

/// An armed or live row drag. Rows arm it on mousedown with the id and the
/// pointer's y; `TodoList` fills in the group once the pointer has moved
/// past the threshold.
#[derive(Clone, PartialEq, Debug, Default)]
pub(crate) struct ListDrag {
    pub todo_id: String,
    /// RowGroup id — drops are confined to the grab's group.
    pub group_id: String,
    /// The group's open rows, in on-screen order.
    pub ids: Vec<String>,
    pub grab_y: f64,
    pub from_index: usize,
    /// Current insertion index within the group's open rows.
    pub over_index: usize,
    /// Past the 4px threshold: the overlay is up.
    pub active: bool,
}

/// List-scoped drag state, provided by TodoList (same pattern as
/// SelectedBlockContext). The drag itself is a non-reactive `CopyValue`, so
/// mousemove never re-renders; only crossing the threshold does.
#[derive(Clone, Copy)]
pub(crate) struct ListDragContext {
    pub(super) drag: CopyValue<Option<ListDrag>>,
    /// ⋯ menu "Move to top/bottom" and ⌥↑/⌥↓.
    pub(super) reorder: Callback<(String, Reorder)>,
}

/// Undo for an adopt, rendered as a toast at the CentreColumn root.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct AdoptUndo {
    pub previous_order: ListOrder,
    pub previous_slots: Vec<(String, f64)>,
    /// Identifies this toast to its own 6s timer, so an older timer can't
    /// close a newer toast.
    generation: u64,
}

static UNDO_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Queue a FLIP play for after the next render (see `assets/list_flip.js`).
pub(super) fn bump_flip(mut flip: Signal<(u64, u32)>, duration_ms: u32) {
    flip.with_mut(|f| {
        f.0 = f.0.wrapping_add(1);
        f.1 = duration_ms;
    });
}

pub(super) fn flip_snap() {
    let _ = document::eval("window.hobbesFlip?.snap()");
}

/// Commit a move of `todo_id` to index `to` within its group.
///
/// In Manual this writes one row (or a renormalised group) through
/// `move_between`. In a sorted view it first adopts the on-screen order as
/// the manual order — permuting existing slots — then switches the list to
/// Manual (keeping its grouping) and offers Undo. Dragging is the choice.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_move(
    mut planner: Signal<PlannerState>,
    order: Signal<ListOrder>,
    mut undo: Signal<Option<AdoptUndo>>,
    set_order: Callback<ListOrder>,
    groups: &[(String, Vec<String>)],
    todo_id: &str,
    group_id: &str,
    to: usize,
) {
    let Some(ids) = groups.iter().find(|g| g.0 == group_id).map(|g| &g.1) else {
        return;
    };
    let Some(from) = ids.iter().position(|id| id == todo_id) else {
        return;
    };
    if from == to {
        return;
    }
    let new_ids = moved(ids, from, to);
    let now = Utc::now();
    let current = *order.peek();

    if current.key == SortKey::Manual {
        let before = to.checked_sub(1).map(|i| new_ids[i].as_str());
        let after = new_ids.get(to + 1).map(String::as_str);
        let changed = {
            planner
                .write()
                .move_between(todo_id, before, after, &new_ids, now)
        };
        for id in &changed {
            persist_todo(&planner, id);
        }
        return;
    }

    let flat = flatten_with(groups, group_id, &new_ids);
    let previous_slots = { planner.write().adopt_order(&flat, now) };
    for (id, _) in &previous_slots {
        persist_todo(&planner, id);
    }
    set_order.call(ListOrder {
        key: SortKey::Manual,
        dir: SortDir::Asc,
        group: current.group,
    });
    let generation = UNDO_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    undo.set(Some(AdoptUndo {
        previous_order: current,
        previous_slots,
        generation,
    }));
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        if undo
            .peek()
            .as_ref()
            .is_some_and(|u| u.generation == generation)
        {
            undo.set(None);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── list order helpers ─────────────────────────────────────────────────

    fn ids(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_targets_stay_inside_the_group() {
        assert_eq!(reorder_target(2, 4, Reorder::Top), Some(0));
        assert_eq!(reorder_target(2, 4, Reorder::Bottom), Some(3));
        assert_eq!(reorder_target(2, 4, Reorder::Up), Some(1));
        assert_eq!(reorder_target(2, 4, Reorder::Down), Some(3));
        // Already there → no-op, never a wasted write.
        assert_eq!(reorder_target(0, 4, Reorder::Top), None);
        assert_eq!(reorder_target(0, 4, Reorder::Up), None);
        assert_eq!(reorder_target(3, 4, Reorder::Down), None);
        assert_eq!(reorder_target(3, 4, Reorder::Bottom), None);
        assert_eq!(reorder_target(5, 4, Reorder::Top), None);
    }

    #[test]
    fn moved_lifts_and_reinserts() {
        let list = ids(&["a", "b", "c", "d"]);
        assert_eq!(moved(&list, 0, 2), ids(&["b", "c", "a", "d"]));
        assert_eq!(moved(&list, 3, 0), ids(&["d", "a", "b", "c"]));
        assert_eq!(moved(&list, 1, 1), list);
    }

    #[test]
    fn drag_over_index_counts_whole_rows_and_clamps() {
        assert_eq!(drag_over_index(1, 4, 0.0), 1);
        assert_eq!(drag_over_index(1, 4, 17.0), 1, "under half a row stays put");
        assert_eq!(drag_over_index(1, 4, 19.0), 2);
        assert_eq!(drag_over_index(1, 4, 36.0 * 10.0), 3);
        assert_eq!(drag_over_index(1, 4, -36.0 * 10.0), 0);
        assert_eq!(drag_over_index(0, 0, 50.0), 0);
    }

    #[test]
    fn flatten_with_replaces_only_the_dragged_group() {
        let groups = vec![
            ("band-overdue".to_string(), ids(&["a", "b"])),
            ("band-later".to_string(), ids(&["c", "d"])),
        ];
        assert_eq!(
            flatten_with(&groups, "band-later", &ids(&["d", "c"])),
            ids(&["a", "b", "d", "c"])
        );
    }
}
