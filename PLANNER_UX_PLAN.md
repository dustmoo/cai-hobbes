# Planner UX Hardening Plan

Remediation plan for the UX audit of 2026-08-13 (post-P3). The planner's data
model and sync rules are sound; this plan addresses the seams where the *user
surface* is incoherent, hides data the model already holds, or advertises
affordances it doesn't honour.

Companion to `PLANNER_DESIGN.md` (the feature spec). Ground rules that bind
every item here:

- **One definition per rule.** View membership stays in `todo/views.rs`,
  capacity math in `todo/model.rs`, sync primitives on `PlannerState` — the UI
  and the AI handlers consume the same functions, so a fix lands on both
  surfaces or it isn't done.
- **Every metadata change keeps the sync contract**: schedule ↔ timebox,
  estimate ↔ block size, title live everywhere, delete cascades.
- Tailwind: no computed class names; geometry stays in inline `style:`.

---

## U1 — Coherence (small fixes, high value; ship as one batch)

### U1.1 Completed work stays visible today
**Problem**: every non-Logbook view filters to open todos, so checking one off
deletes the row on the spot — no strikethrough moment, no in-place undo.
**Fix**: the Today list additionally shows todos whose `completed_at` falls on
the local day (regardless of current `scheduled_for`), sorted after open items
within their section, rendered muted + struck through; the checkbox reopens
in place. Implement in `views.rs` as a distinct `completed_today()` query used
by `TodoList` (and by U1.2) — do NOT widen `matches_view(Today)`, which the AI's
list tool and rollover both depend on for "open work owed".
**Accept**: complete a todo in Today → row stays, struck; uncheck → restored;
next day it appears only in Logbook.

### U1.2 Capacity shows planned vs done
**Problem**: `measure_capacity` counts open todos only, so finishing work makes
"planned" shrink; a fully executed day reads "0m planned of 6h".
**Fix**: `Capacity` gains `done_minutes` (estimates of todos completed today
that were scheduled today; fall back to `actual_minutes` when it's larger).
Planned = open + done. `CapacityBar` renders a done segment (distinct fill)
before the open segment; `Capacity::summary()` becomes
"3h30m planned · 1h done · 2h30m free". Flows automatically into
`planner_today` and `HOBBES_PLAN_DAY` responses via the shared summary.
**Accept**: completing a 1h scheduled todo moves 1h from the open segment to
the done segment; the total doesn't shrink.

### U1.3 No invisible timeboxes
**Problem**: first-fit "add to timeline" on a full day places a block past the
workday end; `block_geometry` culls fully-out-of-window blocks, so the block
exists but renders nowhere and the todo leaves the pool.
**Fix**: keep creation honest (no silent clamping). Render an **After hours**
strip pinned under the ruler listing off-window blocks for the day
("18:00–19:00 Draft the proposal"), clickable to select/delete like ruler
blocks. Same strip handles before-window blocks ("Early").
**Accept**: overfilling the day yields a visible entry; nothing the store holds
is unrepresented on the Today rail.

### U1.4 Context-aware hover menu + Evening fix
**Problem**: the row's hover cluster is identical in every view — "Today" shown
in Today (no-op), "Someday" in Someday (no-op) — and **Evening rewrites
`scheduled_for` to today**, silently yanking an Upcoming todo off its date.
**Fix**: `TodoRow` receives the current selection (it already gets
`show_scheduled`; generalise to a small context struct) and drops actions that
are no-ops for that view. Evening becomes: set `time_of_day` only, preserving
an existing scheduled date; it schedules today only when the todo is undated.
**Accept**: Evening on a Tuesday-scheduled todo keeps Tuesday; hover cluster
never offers the state the row is already in.

### U1.5 Delete asks once
**Problem**: todo delete is instant and adjacent to "Someday" in the cluster.
**Fix**: honour the existing `settings.confirm_on_delete`: first click flips
the trash button into a red "Sure?" state that must be clicked again within a
few seconds (armed state on a signal — no timer needed beyond a dismiss on
mouse-leave). No new modal.
**Accept**: with confirm on, one stray click never deletes.

### U1.6 Focus implies Today
**Problem**: starting focus (row control or chat activation) on an unscheduled
or future todo runs the timer while the task appears nowhere in today's plan —
contradicting "activation means working on it now".
**Fix**: `PlannerState::start_focus` also runs `schedule_todo_on(today)` when
the todo isn't already scheduled today (which prunes off-day blocks per the
existing rule). Both surfaces inherit it; the AI's update response re-summary
already reflects the date.
**Accept**: focusing a Someday todo lands it in Today with the timer running.

---

## U2 — Visible data (the detail editor; one focused feature)

### U2.1 Row detail popover
**Problem**: notes and checklists exist in the model and are AI-writable but
unrenderable; deadlines, tags, and off-ladder estimates have no post-creation
editor. AI-enriched todos are lossy on the user side.
**Fix**: an info affordance on row hover (and click on the row background,
keeping title-click = inline rename) opens a detail card — repo modal
conventions (`fixed inset-0 bg-black/70` overlay, `bg-section` card, Escape
closes, `stop_propagation`). Contents:
- title (edit), notes (plain textarea now; markdown later)
- checklist: add / toggle / remove
- scheduled date + deadline: native `input type="date"` (first date inputs in
  the app — verify WebView rendering on both platforms)
- estimate: free-entry minutes (accepts `45`, `1h30m` via the quick-add
  duration parser)
- tags: chip editor (add on Enter, × to remove)
- read-only: actual minutes, origin (AI session provenance), created/completed
Every mutation goes through `mutate_todo` + the sync primitives
(`schedule_todo_on` / `prune_blocks_for_todo` / `resize_blocks_to_estimate`).
**Accept**: everything `HOBBES_TODO_UPDATE` can patch, the user can see and
edit; estimate edits resize the timebox; date edits move it.

### U2.2 Actuals surfaced
**Fix**: Logbook rows show "52m of 1h" (actual vs estimate) where actual > 0;
`Todo::summary()` appends actuals for closed todos so the AI sees estimate
accuracy too. Groundwork for the P5 shutdown ritual.

### U2.3 Unestimated todos reach the timeline
**Fix**: the "Not on the timeline" pool includes unestimated todos; add-to-
timeline uses `planner_default_block_minutes` and (per the estimate↔block sync)
stamps that as the estimate. Pool rows without an estimate show an "est?" tint.

---

## U3 — Consistency polish (batch of small items)

- **U3.1 Estimate chip**: off-ladder values cycle to the *nearest* ladder step
  instead of `None`; `None` only follows `2h`. (Precision entry lives in U2.1.)
- **U3.2 Timebox chip is a button**: click jumps to Today view with the block
  briefly highlighted (reuse the selection ring).
- **U3.3 Upcoming grouped by date**: sort by `(scheduled_for, sort_order)` and
  insert day headers ("Tomorrow", "Fri 15 Aug"). Special-case in `in_view` or a
  dedicated `upcoming_grouped()` in `views.rs` shared with the AI list tool.
- **U3.4 Scheduled-date quick-add token**: `*fri` / `*tomorrow` mirrors `!` for
  the *scheduled* day (the `!`=deadline asymmetry misleads in Upcoming). Update
  parser, chip preview, placeholder, and the settings syntax card.
- **U3.5 Logbook rows act**: reduced hover cluster (delete only) so closed work
  can be pruned without reopening it.
- **U3.6 One date formatter**: shared helper — "Today" / "Tomorrow" /
  "%a %-d %b", year appended only when not current. Used by scheduled chips,
  deadline chips, completed lines, and the detail card.

---

## U4 — Deferred (recorded, not scheduled)

- Tag filtering in the left rail (tags become useful search keys once U2.1 can
  edit them).
- Project/area creation UI (rail "+" affordances); today only the AI can build
  the tree. Pairs with a project detail editor.
- Keyboard navigation in the list (arrows, Enter-to-complete, E-to-edit).
- Row drag-reorder + drag-to-timeline (`sort_between` machinery is ready).
- Multi-day timeline view; drag-across-days then routes through
  `schedule_todo_on`.

---

## Sequencing & verification

| Batch | Size | Risk | Verification focus |
|-------|------|------|--------------------|
| U1 | ~1 day | low | views/capacity unit tests updated alongside; `HOBBES_TODO_LIST` output unchanged for "today" (U1.1 must not leak closed todos into the AI's open-work list) |
| U2 | ~1–2 days | medium | detail-card mutations exercise every sync primitive; date-input rendering on macOS + Windows WebView |
| U3 | ~½ day | low | parser tests for `*` token; grouped-Upcoming shared with AI list |

Each batch: `cargo test --release` green, clippy clean on touched files,
`npm run build:css` after class changes, manual pass in `dx serve`.
