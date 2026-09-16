//! Planner day timeline: the Sunsama-style capacity bar and today rail.
//!
//! Split out of `planner_view.rs` (P-split for concurrent editing) — keeps
//! `TodayRail`, `CapacityBar`, and every timeline geometry/drag helper.
//! Timeline geometry uses inline `style:` attributes exclusively — Tailwind
//! purges computed class names, so positioned pixels must never ride in
//! class strings. Root types (`PlannerSelection`, persistence helpers,
//! `today()`) and the todo-list surfaces live in the sibling modules
//! `planner_view` and `planner_rows`.

use chrono::{Local, NaiveDate, TimeZone, Timelike, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::settings::Settings;
use crate::todo::model::{self, BlockSource, TimeBlock, Todo, TodoBucket, TodoStatus};
use crate::todo::store;
use crate::todo::views::{self, TodoView};
use crate::todo::PlannerState;

use super::planner_view::{mutate_todo, persist_block, persist_todo, today, SelectedBlockContext};

/// Vertical scale of the day timeline.
const PX_PER_HOUR: f64 = 48.0;

/// Current-time indicator color: warm orange, complementary to the cool
/// blue/green block palette and distinct from the subscription palette's red.
const NOW_LINE_COLOR: &str = "#fb923c";

/// Pixel offset of the current-time line inside the ruler, or `None` when
/// "now" falls outside the rendered workday window.
fn now_line_offset(now_min: i64, day_start: u32, day_end: u32) -> Option<f64> {
    if now_min < day_start as i64 || now_min > day_end as i64 {
        return None;
    }
    Some((now_min - day_start as i64) as f64 / 60.0 * PX_PER_HOUR)
}

/// What a timeline drag is doing to a block.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockDragMode {
    /// Reposition the whole block, duration preserved.
    Move,
    /// Drag the bottom edge to change the end time.
    ResizeEnd,
}

/// `"09:30"` → minutes since midnight. Rejects malformed and out-of-range input.
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Timeline bounds from settings, falling back to 09:00–17:00 whenever the
/// stored strings are malformed or inverted.
fn workday_bounds(start: &str, end: &str) -> (u32, u32) {
    match (parse_hhmm(start), parse_hhmm(end)) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => (9 * 60, 17 * 60),
    }
}

/// Pixel geometry for a block on the ruler: `(top, height)` clamped into the
/// workday window. `None` when the block lies entirely outside it.
fn block_geometry(
    start_min: i64,
    end_min: i64,
    day_start: u32,
    day_end: u32,
) -> Option<(f64, f64)> {
    let start = start_min.max(day_start as i64);
    let end = end_min.min(day_end as i64);
    if end <= start {
        return None;
    }
    let top = (start - day_start as i64) as f64 / 60.0 * PX_PER_HOUR;
    // The floor keeps a one-line title readable: a 15-minute block is only
    // 12px at 48px/h, which clips its own label.
    let height = ((end - start) as f64 / 60.0 * PX_PER_HOUR).max(20.0);
    Some((top, height))
}

/// Which side of the workday window a fully-off-window block sits on.
///
/// The exact complement of [`block_geometry`]'s culling rule: every block on
/// the day is either drawn on the ruler or classified here for the off-window
/// strip — the store must never hold a block the Today rail doesn't show.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OffWindow {
    /// Ends at or before the workday starts.
    Early,
    /// Starts at or after the workday ends.
    After,
}

fn off_window(start_min: i64, end_min: i64, day_start: u32, day_end: u32) -> Option<OffWindow> {
    if end_min <= day_start as i64 {
        Some(OffWindow::Early)
    } else if start_min >= day_end as i64 {
        Some(OffWindow::After)
    } else {
        None
    }
}

/// [`block_geometry`] plus honesty about the clamp: external (calendar) blocks
/// that spill past a workday edge render clamped with a dashed cue on the
/// clamped side instead of silently looking shorter than the meeting is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ClampedGeometry {
    top: f64,
    height: f64,
    /// The block starts before the workday window opens.
    clamped_top: bool,
    /// The block ends after the workday window closes.
    clamped_bottom: bool,
}

/// Geometry for an external block: identical pixels to [`block_geometry`],
/// with flags for which edge (if either) was clamped to the window. `None` for
/// zero-length spans and blocks entirely outside the window — those are
/// tallied into the off-window count strip, never drawn.
fn clamped_block_geometry(
    start_min: i64,
    end_min: i64,
    day_start: u32,
    day_end: u32,
) -> Option<ClampedGeometry> {
    if end_min <= start_min {
        return None; // zero-length guard, mirrors block_geometry
    }
    let (top, height) = block_geometry(start_min, end_min, day_start, day_end)?;
    Some(ClampedGeometry {
        top,
        height,
        clamped_top: start_min < day_start as i64,
        clamped_bottom: end_min > day_end as i64,
    })
}

/// Label for external events the ruler can't show at all: `"2 earlier · 1
/// later"`. `None` when there is nothing outside the window.
fn fmt_outside_counts(earlier: usize, later: usize) -> Option<String> {
    match (earlier, later) {
        (0, 0) => None,
        (e, 0) => Some(format!("{} earlier", e)),
        (0, l) => Some(format!("{} later", l)),
        (e, l) => Some(format!("{} earlier · {} later", e, l)),
    }
}

/// Resolve a subscription id to its display color. `None` when the block has
/// no subscription or the subscription no longer exists — callers fall back
/// to the muted external style.
fn subscription_color(
    subs: &[crate::settings::CalendarSubscription],
    subscription_id: Option<&str>,
) -> Option<String> {
    let id = subscription_id?;
    subs.iter().find(|s| s.id == id).map(|s| s.color.clone())
}

/// Inline style for an external block: geometry, subscription tint (left color
/// bar + translucent fill — `26` is ~15% alpha appended to the hex), and
/// dashed cues on window-clamped edges. Non-busy (free / focus-time) events
/// get a subtler treatment — fainter fill (`14` ≈ 8% alpha), a dashed left
/// bar, and reduced opacity — so they read as non-blocking. Tentative busy
/// meetings keep the full busy tint (they still block time) but swap the
/// solid left bar for a dashed one — pencilled in, not inked. Inline because
/// Tailwind purges computed class names.
fn external_block_style(
    top: f64,
    height: f64,
    color: Option<&str>,
    clamped_top: bool,
    clamped_bottom: bool,
    busy: bool,
    tentative: bool,
) -> String {
    let mut style = format!("top: {top}px; height: {height}px;");
    if let Some(c) = color {
        if busy {
            let bar = if tentative { "dashed" } else { "solid" };
            style.push_str(&format!(
                " border-left: 3px {bar} {c}; background-color: {c}26;"
            ));
        } else {
            style.push_str(&format!(
                " border-left: 3px dashed {c}; background-color: {c}14;"
            ));
        }
    }
    if !busy {
        style.push_str(" opacity: 0.65;");
    }
    if clamped_top {
        style.push_str(" border-top-width: 2px; border-top-style: dashed;");
    }
    if clamped_bottom {
        style.push_str(" border-bottom-width: 2px; border-bottom-style: dashed;");
    }
    style
}

/// Today's all-day events for enabled subscriptions, for the banner row above
/// the timeline. All-day events are cached but never materialized as blocks
/// (a 24h block would wreck the ruler), so the banner reads the cache
/// directly. Multi-day spans show on every local day they cover; the end is
/// exclusive, so an event ending at local midnight does not bleed into the
/// next day.
fn all_day_events_for(
    events: &[model::CalendarEvent],
    enabled_subscriptions: &std::collections::HashSet<String>,
    day: NaiveDate,
) -> Vec<model::CalendarEvent> {
    // Shared with the planner_today context and HOBBES_CALENDAR_LIST — one
    // definition of "today's all-day events" for every surface.
    crate::todo::views::all_day_events_on(events, enabled_subscriptions, day)
}

/// Round down to the nearest quarter hour, so clicked blocks land on tidy edges.
fn snap_to_quarter(minutes: u32) -> u32 {
    minutes - minutes % 15
}

/// Where a moved block lands: the original span shifted by `delta_min`,
/// snapped to the quarter grid and clamped inside the workday, duration
/// preserved. Returns `(start, end)` in minutes since local midnight.
fn dragged_move(
    orig_start: u32,
    orig_end: u32,
    delta_min: i64,
    day_start: u32,
    day_end: u32,
) -> (u32, u32) {
    let duration = orig_end.saturating_sub(orig_start).max(15);
    let max_start = day_end.saturating_sub(duration).max(day_start);
    let raw = (orig_start as i64 + delta_min).clamp(day_start as i64, max_start as i64) as u32;
    let start = snap_to_quarter(raw).max(day_start);
    (start, start + duration)
}

/// Where a resized block's end lands: at least 15 minutes past the start, at
/// most the end of the workday, snapped to the quarter grid.
fn dragged_resize(orig_start: u32, orig_end: u32, delta_min: i64, day_end: u32) -> (u32, u32) {
    let min_end = orig_start + 15;
    let raw = (orig_end as i64 + delta_min).clamp(min_end as i64, day_end.max(min_end) as i64);
    let end = snap_to_quarter(raw as u32).max(min_end).min(day_end.max(min_end));
    (orig_start, end)
}

/// `570` → `"09:30"` — labels for previewed positions, where no DateTime
/// exists yet.
pub(crate) fn fmt_hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Naive first-fit: the earliest quarter-hour slot at or after `day_start`
/// where `duration` minutes don't collide with any busy interval. May run past
/// the end of the workday — an honest overflow beats silently dropping work.
fn first_free_slot(busy: &[(u32, u32)], day_start: u32, duration: u32) -> u32 {
    let mut intervals: Vec<(u32, u32)> = busy.to_vec();
    intervals.sort_unstable();
    let mut candidate = day_start;
    for (s, e) in intervals {
        if s < candidate + duration && candidate < e {
            candidate = e.div_ceil(15) * 15;
        }
    }
    candidate
}

/// Where automatic placement starts searching for a free slot.
///
/// For today, never before *now*: auto-placing work into the morning that has
/// already passed plans fiction. The current minute snaps FORWARD to the
/// quarter grid so placed blocks keep tidy edges, and the workday start still
/// applies as a floor. Future days search from the workday start as before.
/// Only automatic placement is clamped — manual drags and explicit AI times
/// stay allowed anywhere (user intent wins).
fn placement_search_start(workday_start_min: u32, now_min_local: u32, is_today: bool) -> u32 {
    if !is_today {
        return workday_start_min;
    }
    workday_start_min.max(now_min_local.div_ceil(15) * 15)
}

/// Assign side-by-side columns to overlapping timeline blocks.
///
/// Input: `(id, start_min, end_min)` per block. Output: id → `(column_index,
/// column_count)`. Blocks are grouped into clusters of *transitively*
/// overlapping intervals (touching edges — end == next start — do NOT overlap,
/// consistent with `TimeBlock::overlaps`); within a cluster, a greedy sweep
/// places each block (sorted by start) into the first column whose last block
/// ends at or before its start. The column count is capped at 3: a block that
/// would land in column ≥ 3 shares column 2 instead — a slight visual overlap
/// beats unreadably thin slivers. Single-block clusters keep `(0, 1)`, i.e.
/// full width.
fn assign_overlap_columns(
    blocks: &[(String, u32, u32)],
) -> std::collections::HashMap<String, (usize, usize)> {
    const MAX_COLUMNS: usize = 3;
    let mut out = std::collections::HashMap::new();

    let mut sorted: Vec<&(String, u32, u32)> = blocks.iter().filter(|(_, s, e)| e > s).collect();
    sorted.sort_by_key(|(_, s, e)| (*s, *e));

    let mut i = 0;
    while i < sorted.len() {
        // Grow the cluster while the next block starts before the furthest
        // end seen so far (strict <: touching edges start a new cluster).
        let mut cluster_end = sorted[i].2;
        let mut j = i + 1;
        while j < sorted.len() && sorted[j].1 < cluster_end {
            cluster_end = cluster_end.max(sorted[j].2);
            j += 1;
        }

        // Greedy column assignment within the cluster.
        let mut column_ends: Vec<u32> = Vec::new(); // last end per column
        let mut assigned: Vec<(&str, usize)> = Vec::new();
        for (id, s, e) in &sorted[i..j] {
            let col = match column_ends.iter().position(|end| *end <= *s) {
                Some(col) => {
                    column_ends[col] = *e;
                    col
                }
                None => {
                    column_ends.push(*e);
                    column_ends.len() - 1
                }
            };
            assigned.push((id, col));
        }
        let count = column_ends.len().min(MAX_COLUMNS);
        for (id, col) in assigned {
            out.insert(id.to_string(), (col.min(MAX_COLUMNS - 1), count));
        }
        i = j;
    }
    out
}

/// The horizontal style fragment for a timeline block in its overlap column.
///
/// Column 0 of 1 (no overlap) keeps the classic full-width footprint
/// (44px hour-label gutter on the left, 4px margin on the right). Overlapping
/// blocks split the usable width evenly with a small gutter between columns.
/// Inline style, composed with the vertical geometry — Tailwind purges
/// computed class names.
#[cfg(test)]
fn overlap_column_style(col: usize, count: usize) -> String {
    overlap_column_style_scaled(col, count, 1.0)
}

/// [`overlap_column_style`] over a *fraction* of the usable width.
///
/// `avail` is how much of the block area the person's own work may use:
/// `1.0` normally, [`AGENT_LANE_SPLIT`] on a day where the agent sub-column
/// is drawn, so a block's right edge stops at the lane boundary instead of
/// sliding under it. Inline style — Tailwind purges computed class names.
fn overlap_column_style_scaled(col: usize, count: usize, avail: f64) -> String {
    let avail = avail.clamp(0.05, 1.0);
    if count <= 1 {
        if avail >= 1.0 {
            return " left: 44px; right: 4px;".to_string();
        }
        return format!(" left: 44px; width: calc((100% - 48px) * {avail:.4} - 3px);");
    }
    let frac = avail / count as f64;
    let left_frac = frac * col as f64;
    format!(
        " left: calc(44px + (100% - 48px) * {left_frac:.4}); width: calc((100% - 48px) * {frac:.4} - 3px);"
    )
}

/// Where the agent sub-column starts, as a fraction of the block area. The
/// person keeps the left ~66%; fleet spans get the right third.
const AGENT_LANE_SPLIT: f64 = 0.66;

/// Inline geometry for one agent span, split into `count` equal sub-columns
/// with a 2px gutter within the lane's right-hand region (`col` of `count`,
/// 0-indexed). `(0, 1)` — the common case — keeps the classic full-lane
/// footprint.
fn agent_span_style(top: f64, height: f64, background: bool, col: usize, count: usize) -> String {
    let count = count.max(1);
    let col = col.min(count.saturating_sub(1));
    let sub = 1.0 / count as f64;
    let start_frac = AGENT_LANE_SPLIT + (1.0 - AGENT_LANE_SPLIT) * sub * col as f64;
    let mut style = if count <= 1 {
        format!(
            "top: {top}px; height: {height}px; left: calc(44px + (100% - 48px) * {start_frac:.4}); right: 4px;"
        )
    } else {
        let width_frac = (1.0 - AGENT_LANE_SPLIT) * sub;
        format!(
            "top: {top}px; height: {height}px; left: calc(44px + (100% - 48px) * {start_frac:.4}); width: calc((100% - 48px) * {width_frac:.4} - 2px);"
        )
    };
    if background {
        // A background phase is real work the user isn't driving — present,
        // but never as loud as a turn they are in.
        style.push_str(" opacity: 0.65;");
    }
    style
}

/// One positioned block for the agent lane — either a single session's span
/// or a merged "{n} agents" band standing in for an over-busy cluster.
struct AgentLaneBlock {
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
    background: bool,
    col: usize,
    count: usize,
    key: String,
    title: String,
    label: String,
}

/// How many concurrent sub-columns the agent lane draws before giving up on
/// slivers and collapsing the cluster into one merged band.
const MAX_AGENT_LANE_COLUMNS: usize = 3;

/// Lay out the agent lane: assign side-by-side sub-columns to transitively
/// overlapping external spans (mirrors [`assign_overlap_columns`]'s greedy
/// interval-coloring sweep, capped at [`MAX_AGENT_LANE_COLUMNS`]), except
/// that a cluster whose *true* concurrency exceeds the cap doesn't share a
/// column — it collapses into one merged band spanning the cluster's time
/// range, labelled "{n} agents", so the lane never draws unreadable slivers.
/// `spans` must already be sorted by start (both [`merge_agent_spans`] and
/// the memo that names it produce that order).
fn agent_lane_layout(spans: &[(crate::fleet::replay::DaySpan, String)]) -> Vec<AgentLaneBlock> {
    let mut sorted: Vec<&(crate::fleet::replay::DaySpan, String)> = spans.iter().collect();
    sorted.sort_by_key(|(s, _)| (s.start, s.end));

    let mut out = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut cluster_end = sorted[i].0.end;
        let mut j = i + 1;
        while j < sorted.len() && sorted[j].0.start < cluster_end {
            cluster_end = cluster_end.max(sorted[j].0.end);
            j += 1;
        }
        let cluster = &sorted[i..j];

        // Greedy earliest-fit column assignment — for an interval graph the
        // number of columns it settles on is exactly the cluster's true max
        // concurrency, not just an upper bound.
        let mut column_ends: Vec<chrono::DateTime<Utc>> = Vec::new();
        let mut cols: Vec<usize> = Vec::with_capacity(cluster.len());
        for (span, _) in cluster {
            let col = match column_ends.iter().position(|end| *end <= span.start) {
                Some(col) => {
                    column_ends[col] = span.end;
                    col
                }
                None => {
                    column_ends.push(span.end);
                    column_ends.len() - 1
                }
            };
            cols.push(col);
        }
        let concurrency = column_ends.len();

        if concurrency > MAX_AGENT_LANE_COLUMNS {
            let start = cluster.iter().map(|(s, _)| s.start).min().unwrap();
            let end = cluster.iter().map(|(s, _)| s.end).max().unwrap();
            let mut names: Vec<&str> = cluster.iter().map(|(_, n)| n.as_str()).collect();
            names.sort_unstable();
            out.push(AgentLaneBlock {
                start,
                end,
                background: false,
                col: 0,
                count: 1,
                key: format!("agent-band-{}-{}", start.timestamp(), end.timestamp()),
                title: names.join(", "),
                label: format!("{} agents", cluster.len()),
            });
        } else {
            let count = concurrency.min(MAX_AGENT_LANE_COLUMNS);
            for ((span, name), col) in cluster.iter().zip(cols.into_iter()) {
                let range = format!(
                    "{}\u{2013}{} \u{b7} {}",
                    span.start.with_timezone(&Local).format("%H:%M"),
                    span.end.with_timezone(&Local).format("%H:%M"),
                    name,
                );
                out.push(AgentLaneBlock {
                    start: span.start,
                    end: span.end,
                    background: span.background,
                    col: col.min(MAX_AGENT_LANE_COLUMNS - 1),
                    count,
                    key: format!("agent-{}-{}", span.session_id, span.start.timestamp()),
                    title: range,
                    label: name.clone(),
                });
            }
        }
        i = j;
    }
    out
}

/// A local wall-clock minute on `date`, as UTC. Clamped to the day; DST
/// handling delegates to the planner's single policy (`ics::local_naive_to_utc`
/// — gaps slide an hour forward), so a dragged block lands on the same instant
/// a tool-placed one would. The final fallback is UTC-midnight-of-date, never
/// `Utc::now()` (which would teleport the block to "right now").
pub(crate) fn local_minutes_to_utc(date: NaiveDate, minutes: u32) -> chrono::DateTime<Utc> {
    let minutes = minutes.min(23 * 60 + 59);
    let naive = date
        .and_hms_opt(minutes / 60, minutes % 60, 0)
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).expect("midnight is always valid"));
    crate::todo::ics::local_naive_to_utc(&Local, &naive)
        .unwrap_or_else(|| Utc.from_utc_datetime(&date.and_time(chrono::NaiveTime::MIN)))
}

/// Minutes since local midnight for a stored UTC instant.
fn minutes_in_local_day(dt: &chrono::DateTime<Utc>) -> i64 {
    let local = dt.with_timezone(&Local);
    (local.time().num_seconds_from_midnight() / 60) as i64
}

// ── Day navigation, free gaps, agent lane, shutdown (pure) ──────────────────

/// What to call the day the rail is showing. Named days for the three the
/// user actually lives in; everything else is its weekday (the exact date
/// rides alongside in its own muted span, so the label never has to repeat
/// it).
fn day_label(viewing: NaiveDate, today: NaiveDate) -> String {
    if viewing == today {
        "Today".to_string()
    } else if today.succ_opt() == Some(viewing) {
        "Tomorrow".to_string()
    } else if today.pred_opt() == Some(viewing) {
        "Yesterday".to_string()
    } else {
        viewing.format("%A").to_string()
    }
}

/// The muted date beside the day label: `"Tue 16 Sep"`.
fn day_date_label(viewing: NaiveDate) -> String {
    viewing.format("%a %-d %b").to_string()
}

/// Usable free stretches inside `[day_start, day_end)`: every gap of at least
/// `min_len` minutes not covered by a busy interval.
///
/// Pure and total — busy intervals may overlap, nest, run backwards, or spill
/// past either edge; they are clipped to the window and merged first. Callers
/// that only want the future (today) pass a raised `day_start`.
fn free_gaps(busy: &[(u32, u32)], day_start: u32, day_end: u32, min_len: u32) -> Vec<(u32, u32)> {
    if day_end <= day_start || min_len == 0 {
        return Vec::new();
    }
    let mut spans: Vec<(u32, u32)> = busy
        .iter()
        .filter_map(|(s, e)| {
            let s = (*s).max(day_start);
            let e = (*e).min(day_end);
            (e > s).then_some((s, e))
        })
        .collect();
    spans.sort_unstable();

    let mut out = Vec::new();
    let mut cursor = day_start;
    for (s, e) in spans {
        if s > cursor && s - cursor >= min_len {
            out.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if day_end > cursor && day_end - cursor >= min_len {
        out.push((cursor, day_end));
    }
    out
}

/// Free slots are worth offering from 30 minutes up — anything shorter is
/// changeover, not a slot you'd plan into.
const FREE_GAP_MIN_MINUTES: u32 = 30;

/// How many sessions the agent sub-column will draw before it stops. Past
/// this the lane is noise, not information.
const MAX_AGENT_LANE_SESSIONS: usize = 12;

/// Keep only spans for sessions that resolve to an external Claude Code
/// session (`FleetOrigin::External`). The lane is meant for background
/// agents, not the user's own Hobbes chat tabs, which report through the
/// same event log (`FleetOrigin::Hobbes`) and would otherwise show up as a
/// pile of blocks named after the day. A session that can't be resolved to a
/// row at all (deleted, or a gap in the lookup) is dropped rather than
/// guessed at.
fn filter_external_spans(
    spans: Vec<crate::fleet::replay::DaySpan>,
    origins: &std::collections::HashMap<String, crate::fleet::FleetOrigin>,
) -> Vec<crate::fleet::replay::DaySpan> {
    spans
        .into_iter()
        .filter(|s| origins.get(&s.session_id) == Some(&crate::fleet::FleetOrigin::External))
        .collect()
}

/// Collapse a day's replayed fleet spans into what the lane should draw.
///
/// Spans of the *same* session that touch or overlap merge into one stretch
/// (the replay closes and re-opens a span at every turn boundary — drawing
/// each one paints a barcode, not a working block). A merged stretch counts
/// as background only when every part of it was. Sessions are then capped at
/// `max_sessions`, keeping the busiest; the result is chronological.
fn merge_agent_spans(
    spans: &[crate::fleet::replay::DaySpan],
    max_sessions: usize,
) -> Vec<crate::fleet::replay::DaySpan> {
    use std::collections::HashMap;

    let mut by_session: HashMap<&str, Vec<&crate::fleet::replay::DaySpan>> = HashMap::new();
    for span in spans.iter().filter(|s| s.end > s.start) {
        by_session
            .entry(span.session_id.as_str())
            .or_default()
            .push(span);
    }

    let mut merged_by_session: Vec<(i64, Vec<crate::fleet::replay::DaySpan>)> = by_session
        .into_values()
        .map(|mut group| {
            group.sort_by_key(|s| s.start);
            let mut merged: Vec<crate::fleet::replay::DaySpan> = Vec::new();
            for span in group {
                match merged.last_mut() {
                    Some(prev) if span.start <= prev.end => {
                        prev.end = prev.end.max(span.end);
                        prev.background = prev.background && span.background;
                    }
                    _ => merged.push((*span).clone()),
                }
            }
            let total: i64 = merged.iter().map(|s| (s.end - s.start).num_seconds()).sum();
            (total, merged)
        })
        .collect();

    // Busiest sessions win the lane; ties break on the session id so the cap
    // is deterministic across renders.
    merged_by_session.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            a.1.first()
                .map(|s| s.session_id.as_str())
                .cmp(&b.1.first().map(|s| s.session_id.as_str()))
        })
    });
    merged_by_session.truncate(max_sessions);

    let mut out: Vec<crate::fleet::replay::DaySpan> =
        merged_by_session.into_iter().flat_map(|(_, s)| s).collect();
    out.sort_by_key(|s| s.start);
    out
}

/// Left half of the capacity readout: what's behind you and what's left.
fn capacity_done_label(done: u32, planned: u32) -> String {
    format!(
        "{} done · {} to go",
        model::format_minutes(done),
        model::format_minutes(planned.saturating_sub(done))
    )
}

/// Right half of the capacity readout, and whether it's the alarming kind.
fn capacity_balance_label(cap: &model::Capacity) -> (String, bool) {
    if cap.is_overcommitted() {
        (
            format!("over by {}", model::format_minutes(cap.over_by())),
            true,
        )
    } else {
        (
            format!("{} free", model::format_minutes(cap.remaining())),
            false,
        )
    }
}

/// `150` → `"2.5h"`. The shutdown card's register: round numbers for a day
/// you're closing, not the minute-exact `format_minutes` of one you're
/// still planning.
fn fmt_hours(minutes: u32) -> String {
    let hours = minutes as f64 / 60.0;
    let rounded = (hours * 10.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < f64::EPSILON {
        format!("{}h", rounded.round() as i64)
    } else {
        format!("{rounded:.1}h")
    }
}

/// The headline counts on the End-the-day card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EndOfDaySummary {
    /// Todos closed on the day.
    done: usize,
    /// Todos still open on the day — the carry-over candidates.
    carried: usize,
    /// Person focus minutes on the day.
    you_minutes: u32,
    /// Agent minutes on the day (in-app focus + fleet).
    agent_minutes: u32,
}

/// Count the day for its shutdown card. `done` is work *closed on that day*
/// (a todo completed today but scheduled last week still counts as today's
/// win); `carried` is what's still open on the day's plate.
fn end_of_day_summary(
    todos: &[Todo],
    sessions: &[model::FocusSession],
    day: NaiveDate,
    agent_minutes: u32,
) -> EndOfDaySummary {
    let done = todos
        .iter()
        .filter(|t| {
            t.status.is_closed()
                && t.completed_at
                    .is_some_and(|c| c.with_timezone(&Local).date_naive() == day)
        })
        .count();
    EndOfDaySummary {
        done,
        carried: carry_over_candidates(todos, day).len(),
        you_minutes: model::person_minutes_on(sessions, day),
        agent_minutes,
    }
}

/// The day's open todos, in list order — the carry-over chips, every one of
/// them pre-checked (the default is that unfinished work follows you).
fn carry_over_candidates(todos: &[Todo], day: NaiveDate) -> Vec<(String, String)> {
    todos
        .iter()
        .filter(|t| !t.status.is_closed() && t.scheduled_for == Some(day))
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect()
}

/// The collapsed line a closed day leaves behind: `Closed at 17:42 · "…"`.
fn closed_day_line(shutdown_at: chrono::DateTime<Utc>, reflection: &str) -> String {
    let at = shutdown_at.with_timezone(&Local).format("%H:%M");
    let reflection = reflection.trim();
    if reflection.is_empty() {
        format!("Closed at {at}")
    } else {
        format!("Closed at {at} · \u{201c}{}\u{201d}", crate::fleet::truncate_summary(reflection, 60))
    }
}

// ── Right rail: capacity + timeline (Today only) ────────────────────────────

#[component]
fn CapacityBar(
    planned: u32,
    done: u32,
    capacity: u32,
    agent: u32,
    done_label: String,
    balance_label: String,
    over: bool,
    caveat: Option<String>,
) -> Element {
    // The bar's full width represents whichever is larger, so overcommitment
    // shows as a red tail instead of silently clipping at 100%. `planned`
    // includes `done`: finished work fills from the left with its own tint so
    // completing a todo moves time between segments instead of shrinking the
    // bar.
    let denom = planned.max(capacity).max(1) as f64;
    let within = planned.min(capacity);
    let done_pct = (done.min(within) as f64 / denom) * 100.0;
    let open_pct = (within.saturating_sub(done) as f64 / denom) * 100.0;
    let over_pct = (planned.saturating_sub(capacity) as f64 / denom) * 100.0;
    // The agent lane shares the person bar's scale but never its space:
    // agent minutes consume no capacity, so they render as their own thin
    // strip below rather than a segment of the day.
    let agent_pct = ((agent as f64 / denom) * 100.0).min(100.0);
    let agent_label = format!("{} agent focus today", model::format_minutes(agent));

    rsx! {
        div {
            class: "flex flex-col gap-1.5",
            div {
                class: "h-2 w-full overflow-hidden rounded-full bg-input border border-faint flex",
                if done_pct > 0.0 {
                    div {
                        class: "h-full bg-primary-400",
                        style: "width: {done_pct}%;",
                    }
                }
                div {
                    class: "h-full bg-btn-primary",
                    style: "width: {open_pct}%;",
                }
                if over_pct > 0.0 {
                    div {
                        class: "h-full bg-red-700",
                        style: "width: {over_pct}%;",
                    }
                }
            }
            if agent > 0 {
                div {
                    class: "w-full overflow-hidden rounded-full bg-input border border-faint flex",
                    style: "height: 4px;",
                    title: "{agent_label}",
                    div {
                        class: "h-full",
                        style: "width: {agent_pct}%; background-color: #a78bfa;",
                    }
                }
            }
            // The readout the bar can't draw: what's behind you and what's
            // left on the left, the day's balance on the right — mono and
            // tabular so the digits don't dance as the day moves.
            div {
                class: "flex items-baseline justify-between gap-2",
                span {
                    class: "font-mono text-[11px] tabular-nums text-fg-muted",
                    "{done_label}"
                }
                span {
                    class: if over {
                        "font-mono text-[11px] tabular-nums text-red-300"
                    } else {
                        "font-mono text-[11px] tabular-nums text-fg-muted"
                    },
                    "{balance_label}"
                }
            }
            if let Some(caveat) = caveat {
                p { class: "font-mono text-[11px] tabular-nums text-fg-muted", "{caveat}" }
            }
        }
    }
}

#[component]
pub(crate) fn TodayRail(mut viewing: Signal<NaiveDate>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();
    let mut selected_block = use_context::<SelectedBlockContext>().0;
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();

    // The End-the-day card takes over the foot of the rail when open.
    let mut end_day_open = use_signal(|| false);
    let mut reflection = use_signal(String::new);
    // Carry-over is opt-OUT: unfinished work follows you unless you say
    // otherwise, so the signal holds what was *unchecked*, and a day change
    // needs no resync.
    let mut carry_unchecked = use_signal(std::collections::HashSet::<String>::new);
    let mut rollup_bump = use_signal(|| 0u64);
    let mut rollup_running = use_signal(|| false);
    let mut rollup_error = use_signal(|| Option::<String>::None);

    // Current-time indicator: a once-a-minute tick is enough for a line that
    // moves ~0.8 px/min; subscribing to it re-renders the rail so the line
    // crawls without any other state changing.
    let mut now_tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            now_tick += 1;
        }
    });
    let _subscribe_now = *now_tick.read();

    // The rail is a window onto ONE day, not a hard-wired "today": every read
    // below (blocks, capacity, pool, all-day events, click-to-create, Place)
    // takes `day`, and only the now-line checks it against the real today.
    let real_today = today();
    let day = viewing();
    let is_today = day == real_today;
    let settings_read = settings.read();
    let (day_start, day_end) = workday_bounds(
        &settings_read.planner_workday_start,
        &settings_read.planner_workday_end,
    );
    let default_capacity = settings_read.planner_daily_capacity_minutes;
    let meetings_count = settings_read.planner_calendar_counts_against_capacity;
    let calendar_subs = settings_read.planner_calendar_subscriptions.clone();
    drop(settings_read);

    // All-day events render as a banner, not blocks — they exist only in the
    // calendar cache, never as TimeBlocks. The memo re-reads the cache when
    // the planner state or the subscription set changes (sync passes write the
    // planner signal), keeping per-render cost at a clone, not a SQLite query.
    let all_day_memo = use_memo(move || {
        let enabled: std::collections::HashSet<String> = settings
            .read()
            .planner_calendar_subscriptions
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect();
        let _ = planner.read().blocks.len(); // re-read after a sync pass lands
        let events = store::load_calendar_events(None).unwrap_or_else(|e| {
            tracing::error!("planner: failed to load calendar cache: {}", e);
            Vec::new()
        });
        all_day_events_for(&events, &enabled, viewing())
    });
    let all_day_events: Vec<model::CalendarEvent> = all_day_memo.read().clone();

    let state = planner.read();
    let mut capacity = model::measure_capacity_full(
        &state.todos,
        &state.blocks,
        &state.focus_sessions,
        day,
        state.capacity_for(day, default_capacity),
        meetings_count,
    );
    // Pro surfaces: the agent lane (+ its summary suffix) and the strategic
    // readout. Recording is never gated — the rows exist either way; a Free
    // build simply doesn't display the split.
    let pro = crate::entitlement::pro_active();
    let strategic_line: Option<String> = if pro {
        let person_today = model::person_minutes_on(&state.focus_sessions, day);
        let agent_today = model::agent_minutes_on(&state.focus_sessions, day);
        let (gap_count, gap_minutes) =
            model::strategic_gaps(&state.focus_sessions, (day_start, day_end), day);
        (person_today > 0 || agent_today > 0).then(|| {
            format!(
                "{} person · {} agent · {}x strategic gaps · {}",
                model::format_minutes(person_today),
                model::format_minutes(agent_today),
                gap_count,
                model::format_minutes(gap_minutes),
            )
        })
    } else {
        None
    };
    // Fleet time joins the agent lane: external Claude Code sessions'
    // active minutes today, from the fleet's own tables/state — a distinct
    // source from in-app agent focus sessions, summed only for display.
    // `agent_lane_minutes` zeroes the whole lane for Free builds (the lane
    // is a Pro surface; recording is never gated).
    let fleet_today = if pro {
        crate::fleet::fleet_agent_minutes_on(&fleet_state.read(), day, Utc::now())
    } else {
        0
    };
    capacity.agent_minutes =
        crate::fleet::agent_lane_minutes(pro, capacity.agent_minutes, fleet_today);
    let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
    // Open but not yet on the ruler — the "still to place" pool. Unestimated
    // todos belong here too (U2.3): placing one uses the default block length
    // and stamps it as the estimate, so lacking an estimate must not hide the
    // work from the timeline.
    let unblocked: Vec<Todo> = {
        let blocked_todo_ids: Vec<&str> =
            blocks.iter().filter_map(|b| b.todo_id.as_deref()).collect();
        views::in_view(&state.todos, TodoView::Today, day)
            .into_iter()
            .filter(|t| !blocked_todo_ids.contains(&t.id.as_str()))
            .cloned()
            .collect()
    };
    // Linked blocks render their todo's *current* title (the copy stored on
    // the block goes stale the moment the todo is renamed) and show a done
    // treatment once the todo is closed.
    let linked_info: std::collections::HashMap<String, (String, bool, bool)> = blocks
        .iter()
        .filter_map(|b| {
            let tid = b.todo_id.as_ref()?;
            let t = state.todo(tid)?;
            Some((
                b.id.clone(),
                (
                    t.title.clone(),
                    t.status.is_closed(),
                    t.status == TodoStatus::InProgress,
                ),
            ))
        })
        .collect();
    // End-the-day inputs, gathered while the state borrow is live.
    let carry_candidates = carry_over_candidates(&state.todos, day);
    let eod_core = end_of_day_summary(&state.todos, &state.focus_sessions, day, 0);
    let day_closed: Option<(chrono::DateTime<Utc>, String)> = state
        .day_plans
        .iter()
        .find(|p| p.date == day)
        .and_then(|p| p.shutdown_at.map(|t| (t, p.reflection.clone())));
    drop(state);

    let eod = EndOfDaySummary {
        agent_minutes: capacity.agent_minutes,
        ..eod_core
    };

    // Blocks the ruler culls (fully outside the workday window) still exist
    // and still count — first-fit overflow is deliberately honest — so they
    // get a strip of their own instead of rendering nowhere. External blocks
    // are excluded: the strip carries select/delete affordances that must not
    // exist for read-only mirrored events, so they get a compact count
    // indicator on the ruler instead.
    let off_window_blocks: Vec<(TimeBlock, OffWindow)> = blocks
        .iter()
        .filter(|b| !matches!(b.source, BlockSource::External { .. }))
        .filter_map(|b| {
            off_window(
                minutes_in_local_day(&b.start),
                minutes_in_local_day(&b.end),
                day_start,
                day_end,
            )
            .map(|side| (b.clone(), side))
        })
        .collect();

    // External events entirely outside the workday window: counted, not drawn.
    let (external_earlier, external_later) = blocks
        .iter()
        .filter(|b| matches!(b.source, BlockSource::External { .. }))
        .fold((0usize, 0usize), |(e, l), b| {
            match off_window(
                minutes_in_local_day(&b.start),
                minutes_in_local_day(&b.end),
                day_start,
                day_end,
            ) {
                Some(OffWindow::Early) => (e + 1, l),
                Some(OffWindow::After) => (e, l + 1),
                None => (e, l),
            }
        });
    let outside_label = fmt_outside_counts(external_earlier, external_later);

    let timeline_height = (day_end - day_start) as f64 / 60.0 * PX_PER_HOUR;
    let now_line_top = is_today
        .then(|| now_line_offset(minutes_in_local_day(&Utc::now()), day_start, day_end))
        .flatten();
    let hours: Vec<u32> = (day_start / 60..=day_end / 60)
        .filter(|h| h * 60 >= day_start && h * 60 <= day_end)
        .collect();

    // A block dropped on the calendar IS planned work: it creates a todo
    // scheduled for the day (estimate = block length) plus the linked block.
    // A bare, todo-less block would be invisible to the Today list and the
    // capacity math — the timeline and the list must stay one model.
    let block_minutes = settings.peek().planner_default_block_minutes.max(15);
    let mut create_block_at = move |minutes: u32| {
        let start_min = snap_to_quarter(minutes.clamp(day_start, day_end.saturating_sub(15)));

        let mut todo = Todo::new("Focus", planner.peek().next_sort_order());
        // Anytime, not Inbox: this todo is born triaged — clearing its date
        // later should not dump it back into the capture queue.
        todo.bucket = TodoBucket::Anytime;
        todo.scheduled_for = Some(day);
        todo.estimate_minutes = Some(block_minutes);

        let block = TimeBlock {
            id: uuid::Uuid::new_v4().to_string(),
            todo_id: Some(todo.id.clone()),
            title: todo.title.clone(),
            start: local_minutes_to_utc(day, start_min),
            end: local_minutes_to_utc(day, start_min + block_minutes),
            source: BlockSource::Manual,
        };

        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save block todo {}: {}", todo.id, e);
        }
        persist_block(&block);
        selected_block.set(Some(block.id.clone()));
        {
            let mut state = planner.write();
            state.upsert_todo(todo);
            state.blocks.push(block);
        }
    };

    // Auto-placement's busy set. Non-busy external blocks (free / focus-time
    // mirrors) are NOT busy — tasks may be placed over them.
    let busy: Vec<(u32, u32)> = blocks
        .iter()
        .filter(|b| !matches!(b.source, BlockSource::External { busy: false, .. }))
        .map(|b| {
            (
                minutes_in_local_day(&b.start).max(0) as u32,
                minutes_in_local_day(&b.end).max(0) as u32,
            )
        })
        .collect();

    // Overlap columns for the ruler: computed from the *stored* positions of
    // every block on the day (a mid-drag preview keeps its column — width
    // reflow mid-drag would jitter). Keyed by block id.
    let overlap_columns = assign_overlap_columns(
        &blocks
            .iter()
            .map(|b| {
                (
                    b.id.clone(),
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect::<Vec<_>>(),
    );

    // The agent lane: fleet working time on the viewed day, reconstructed
    // from the stored event log. Read through a memo because it costs two
    // SQLite queries — it re-runs when the day changes, when the live session
    // set changes, or on the minute tick (an open span grows with the clock).
    let agent_spans_memo = use_memo(move || {
        let day = viewing();
        let _tick = *now_tick.read();
        let live = fleet_state.read();
        let _live_count = live.sessions.len();
        if !crate::entitlement::pro_active() {
            return Vec::new();
        }
        let bound = if day == today() {
            Utc::now()
        } else {
            crate::fleet::local_day_bounds_utc(day).1
        };
        let spans = crate::fleet::replay::replay_day_spans(
            &crate::fleet::store::events_on(day),
            bound,
        );
        // Names + origin: the day's ended rows first, then the live map
        // (freshest title/origin) for sessions still open.
        let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut origins: std::collections::HashMap<String, crate::fleet::FleetOrigin> =
            std::collections::HashMap::new();
        for r in crate::fleet::store::sessions_active_on(day) {
            origins.insert(r.id.clone(), r.origin);
            names.insert(r.id.clone(), r.display_name().to_string());
        }
        for s in live.sessions.values() {
            origins.insert(s.id.clone(), s.origin);
            names.insert(s.id.clone(), s.display_name().to_string());
        }
        let spans = filter_external_spans(spans, &origins);
        let merged = merge_agent_spans(&spans, MAX_AGENT_LANE_SESSIONS);
        if merged.is_empty() {
            return Vec::new();
        }
        merged
            .into_iter()
            .map(|span| {
                let name = names
                    .get(&span.session_id)
                    .cloned()
                    .unwrap_or_else(|| "agent".to_string());
                (span, name)
            })
            .collect::<Vec<_>>()
    });
    let agent_spans: Vec<(crate::fleet::replay::DaySpan, String)> = agent_spans_memo.read().clone();
    let agent_lane_items = agent_lane_layout(&agent_spans);

    // The day's cached rollup, shared with the Fleet review band (same meta
    // key, same generator) — a day has one narrative, not two.
    let rollup_memo = use_memo(move || {
        let _bump = *rollup_bump.read();
        crate::fleet::briefs::load_rollup(viewing())
    });
    let rollup = rollup_memo.read().clone();
    // The person's blocks give up the right sub-column only while there is
    // something to put there.
    let lane_avail = if agent_spans.is_empty() {
        1.0
    } else {
        AGENT_LANE_SPLIT
    };

    // Free slots worth offering: gaps in the busy set, and on today only the
    // ones still ahead of you — the morning you already spent is not a slot.
    let gap_floor = if is_today {
        day_start.max((minutes_in_local_day(&Utc::now()).max(0) as u32).div_ceil(15) * 15)
    } else {
        day_start
    };
    let gaps = free_gaps(&busy, gap_floor, day_end, FREE_GAP_MIN_MINUTES);

    // Timeline drag: (block id, mode, anchor screen-y, orig start, orig end).
    // The preview holds the snapped (start, end) under the cursor; because it
    // only changes when the drag crosses a quarter-hour boundary, re-renders
    // are throttled to grid crossings for free.
    let mut block_drag =
        use_signal(|| Option::<(String, BlockDragMode, f64, u32, u32)>::None);
    let mut drag_preview = use_signal(|| Option::<(u32, u32)>::None);

    let mut commit_block_drag = move || {
        let Some((id, mode, _, orig_s, orig_e)) = block_drag.peek().clone() else {
            return;
        };
        let preview = *drag_preview.peek();
        block_drag.set(None);
        drag_preview.set(None);

        match preview {
            // The mouse never left the starting quarter: that's a click, and a
            // click on a block means select/deselect (the pre-drag behaviour).
            None => {
                if mode == BlockDragMode::Move {
                    let current = selected_block.peek().clone();
                    selected_block.set(if current.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id)
                    });
                }
            }
            Some((s, e)) if (s, e) == (orig_s, orig_e) => {}
            Some((s, e)) => {
                let mut linked_todo: Option<String> = None;
                {
                    let mut state = planner.write();
                    if let Some(b) = state.blocks.iter_mut().find(|b| b.id == id) {
                        b.start = local_minutes_to_utc(day, s);
                        b.end = local_minutes_to_utc(day, e);
                        if mode == BlockDragMode::ResizeEnd {
                            linked_todo = b.todo_id.clone();
                        }
                    }
                }
                if let Some(b) = planner.peek().blocks.iter().find(|b| b.id == id) {
                    persist_block(b);
                }
                // Resizing the block IS re-estimating the work: the capacity
                // bar follows the calendar, not a stale estimate.
                if let Some(tid) = linked_todo {
                    let minutes = e.saturating_sub(s).max(15);
                    mutate_todo(planner, &tid, |t| t.estimate_minutes = Some(minutes));
                }
                selected_block.set(Some(id));
            }
        }
    };

    // Snapshot for the render pass: which block is previewing where.
    let preview_for: Option<(String, u32, u32)> = block_drag
        .read()
        .as_ref()
        .and_then(|(id, _, _, _, _)| (*drag_preview.read()).map(|(s, e)| (id.clone(), s, e)));

    let (balance_label, over) = capacity_balance_label(&capacity);
    let done_label = capacity_done_label(capacity.done_minutes, capacity.planned_minutes);
    let caveat = (capacity.unestimated > 0).then(|| {
        format!(
            "{} unestimated \u{2014} the real total is higher",
            capacity.unestimated
        )
    });
    let nav_label = day_label(day, real_today);
    let nav_date = day_date_label(day);

    rsx! {
        div {
            class: "h-full w-full flex flex-col gap-4 overflow-y-auto border-l border-subtle bg-section p-4 outline-none",
            tabindex: "0",
            // `[` / `]` walk the day while the rail has focus (clicking
            // anywhere in it gives focus). Arrow keys are left alone — the
            // rail scrolls.
            onkeydown: move |evt| {
                let step = match evt.data.key() {
                    Key::Character(ref c) if c == "[" => -1,
                    Key::Character(ref c) if c == "]" => 1,
                    _ => return,
                };
                let d = *viewing.peek();
                let next = if step < 0 { d.pred_opt() } else { d.succ_opt() };
                if let Some(next) = next {
                    evt.prevent_default();
                    viewing.set(next);
                }
            },
            // Day navigation. The rail is a window onto one day; `Today` is
            // just the day it opens on.
            div {
                class: "flex items-center gap-2",
                button {
                    class: "h-6 w-6 shrink-0 rounded border border-faint text-[11px] text-fg hover:bg-input",
                    title: "Previous day",
                    onclick: move |_| {
                        let d = *viewing.peek();
                        if let Some(prev) = d.pred_opt() {
                            viewing.set(prev);
                        }
                    },
                    "\u{2039}"
                }
                button {
                    class: "min-w-0 truncate text-sm font-semibold text-fg",
                    title: "Back to today",
                    onclick: move |_| viewing.set(today()),
                    "{nav_label}"
                }
                span { class: "shrink-0 font-mono text-[11px] tabular-nums text-fg-muted", "{nav_date}" }
                button {
                    class: "h-6 w-6 shrink-0 rounded border border-faint text-[11px] text-fg hover:bg-input",
                    title: "Next day",
                    onclick: move |_| {
                        let d = *viewing.peek();
                        if let Some(next) = d.succ_opt() {
                            viewing.set(next);
                        }
                    },
                    "\u{203a}"
                }
                div { class: "flex-1" }
                button {
                    class: "shrink-0 rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input",
                    title: "Ask the assistant to plan this day",
                    onclick: move |_| {
                        let d = *viewing.peek();
                        let seed = if d == today() {
                            "Plan my day".to_string()
                        } else {
                            format!("Plan my day for {}", d.format("%A %-d %B"))
                        };
                        chat_command.set(Some(
                            crate::components::chat_input::ChatCommand::StartTodoInChat(seed),
                        ));
                    },
                    "Plan my day"
                }
            }
            CapacityBar {
                planned: capacity.planned_minutes,
                done: capacity.done_minutes,
                capacity: capacity.capacity_minutes,
                agent: capacity.agent_minutes,
                done_label,
                balance_label,
                over,
                caveat,
            }
            // Strategic-time readout v1 (Pro): person vs agent focus totals
            // and the usable gaps between person sittings in the workday.
            if let Some(line) = strategic_line {
                p { class: "text-xs text-fg-muted", "{line}" }
            }

            // All-day events: a thin banner row above the timeline. These are
            // cached calendar events, never TimeBlocks — a 24h block would
            // wreck the ruler geometry.
            if !all_day_events.is_empty() {
                div {
                    class: "flex flex-wrap gap-1",
                    for ev in all_day_events {
                        {
                            let color = subscription_color(&calendar_subs, Some(&ev.subscription_id))
                                .unwrap_or_else(|| "#94a3b8".to_string());
                            let url = ev.url.clone();
                            let has_url = url.is_some();
                            rsx! {
                                div {
                                    key: "allday-{ev.subscription_id}-{ev.uid}",
                                    class: if has_url {
                                        "flex min-w-0 items-center gap-1.5 rounded-full bg-input px-2 py-0.5 cursor-pointer hover:bg-card"
                                    } else {
                                        "flex min-w-0 items-center gap-1.5 rounded-full bg-input px-2 py-0.5"
                                    },
                                    title: if has_url { "All-day event — click to open" } else { "All-day event" },
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        if let Some(url) = &url {
                                            if let Err(e) = open::that_detached(url) {
                                                tracing::error!("planner: failed to open event link: {}", e);
                                            }
                                        }
                                    },
                                    span {
                                        class: "h-2 w-2 shrink-0 rounded-full",
                                        style: "background-color: {color};",
                                    }
                                    span { class: "min-w-0 truncate text-[11px] text-fg-muted", "{ev.title}" }
                                    if has_url {
                                        Icon { width: 10, height: 10, icon: fi_icons::FiExternalLink }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Hour-ruled timeline. All geometry is inline style — Tailwind
            // purges computed class names, so pixels never ride in classes.
            div {
                class: "relative rounded border border-faint bg-app",
                style: "height: {timeline_height}px;",
                onclick: move |evt| {
                    let y = evt.data.element_coordinates().y;
                    let minutes = day_start + (y / PX_PER_HOUR * 60.0).max(0.0) as u32;
                    create_block_at(minutes);
                },
                // External events the window can't show at all, as an honest
                // count. Non-interactive so it never eats a create-block click.
                if let Some(label) = outside_label {
                    span {
                        class: "pointer-events-none absolute right-1 top-0.5 z-10 rounded-full bg-input px-2 py-0.5 text-[10px] text-fg-muted",
                        "{label}"
                    }
                }
                for hour in hours {
                    {
                        let top = (hour * 60 - day_start) as f64 / 60.0 * PX_PER_HOUR;
                        rsx! {
                            div {
                                key: "hr-{hour}",
                                class: "pointer-events-none absolute left-0 right-0 border-t border-faint",
                                style: "top: {top}px;",
                                span {
                                    class: "pointer-events-none absolute left-1 -top-0.5 text-[10px] leading-none text-fg-muted",
                                    "{hour:02}:00"
                                }
                            }
                        }
                    }
                }
                // Current-time line (this rail IS today). Complementary warm
                // orange against the cool block palette; hidden outside the
                // workday window. Non-interactive so create-block clicks and
                // drags pass straight through it.
                if let Some(top) = now_line_top {
                    div {
                        class: "pointer-events-none absolute left-0 right-0 z-20",
                        style: "top: {top}px;",
                        div {
                            style: "position: absolute; left: 40px; right: 0; border-top: 2px solid {NOW_LINE_COLOR};",
                        }
                        div {
                            style: "position: absolute; left: 36px; top: -3px; width: 8px; height: 8px; border-radius: 9999px; background: {NOW_LINE_COLOR};",
                        }
                    }
                }
                // Free slots: the day's honest empty space, offered rather
                // than left blank. Clicking one creates a block at its start,
                // exactly as clicking bare ruler does.
                for (gs, ge) in gaps.iter().copied() {
                    if let Some((top, height)) = block_geometry(gs as i64, ge as i64, day_start, day_end) {
                        {
                            let mut style = format!("top: {top}px; height: {height}px;");
                            style.push_str(&overlap_column_style_scaled(0, 1, lane_avail));
                            let label = format!("free \u{b7} {}", model::format_minutes(ge - gs));
                            rsx! {
                                div {
                                    key: "gap-{gs}",
                                    class: "absolute flex items-center justify-center overflow-hidden rounded-md border border-dashed border-faint text-[11px] text-fg-muted hover:bg-input",
                                    style: "{style}",
                                    title: "{fmt_hhmm(gs)}\u{2013}{fmt_hhmm(ge)} \u{2014} click to plan something here",
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        create_block_at(gs);
                                    },
                                    span { class: "truncate px-1", "{label}" }
                                }
                            }
                        }
                    }
                }
                // Agent lane: external fleet working time, in its own
                // right-hand sub-column, sub-divided when sessions overlap.
                // Read-only and pointer-transparent — it never joins the
                // overlap columns and never eats a create click.
                for item in agent_lane_items.iter() {
                    if let Some((top, height)) = block_geometry(
                        minutes_in_local_day(&item.start),
                        minutes_in_local_day(&item.end),
                        day_start,
                        day_end,
                    ) {
                        {
                            let style =
                                agent_span_style(top, height, item.background, item.col, item.count);
                            rsx! {
                                div {
                                    key: "{item.key}",
                                    class: "pointer-events-none absolute overflow-hidden rounded-r border-l-[3px] border-accent-500 bg-accent-500/15 px-1 py-0.5 text-[11px] text-accent-300",
                                    style: "{style}",
                                    title: "{item.title}",
                                    span { class: "block truncate leading-4", "{item.label}" }
                                }
                            }
                        }
                    }
                }
                // Blocks fully outside the workday window are filtered before
                // rsx so the loop body positions unconditionally. External
                // blocks carry clamp flags for the dashed-edge cue; manual and
                // auto blocks keep the plain geometry (their off-window home
                // is the strip below the ruler).
                for (block, s_min, e_min, geo) in blocks.iter().filter_map(|b| {
                    // A block mid-drag renders at its previewed position, not
                    // its stored one.
                    let (s_min, e_min) = match &preview_for {
                        Some((pid, ps, pe)) if *pid == b.id => (*ps as i64, *pe as i64),
                        _ => (minutes_in_local_day(&b.start), minutes_in_local_day(&b.end)),
                    };
                    let geo = if matches!(b.source, BlockSource::External { .. }) {
                        clamped_block_geometry(s_min, e_min, day_start, day_end)
                    } else {
                        block_geometry(s_min, e_min, day_start, day_end).map(|(top, height)| {
                            ClampedGeometry { top, height, clamped_top: false, clamped_bottom: false }
                        })
                    };
                    geo.map(|geo| (b, s_min, e_min, geo))
                }) {
                    {
                        let is_external = matches!(block.source, BlockSource::External { .. });
                        // Subscription tint + deep link for mirrored calendar
                        // events. A block whose subscription is unknown (legacy
                        // or just removed) keeps the muted fallback style.
                        let (ext_color, ext_url, ext_busy, ext_tentative) = match &block.source {
                            BlockSource::External { subscription_id, url, busy, tentative, .. } => (
                                subscription_color(&calendar_subs, subscription_id.as_deref()),
                                url.clone(),
                                *busy,
                                *tentative,
                            ),
                            _ => (None, None, true, false),
                        };
                        let has_url = ext_url.is_some();
                        let click_url = ext_url.clone();
                        let ClampedGeometry { top, height, clamped_top, clamped_bottom } = geo;
                        let mut block_style = if is_external {
                            external_block_style(top, height, ext_color.as_deref(), clamped_top, clamped_bottom, ext_busy, ext_tentative)
                        } else {
                            format!("top: {top}px; height: {height}px;")
                        };
                        // Overlapping blocks share the rail width side by side;
                        // a lone block keeps the classic full-width footprint.
                        let (col, count) = overlap_columns
                            .get(&block.id)
                            .copied()
                            .unwrap_or((0, 1));
                        block_style.push_str(&overlap_column_style_scaled(col, count, lane_avail));
                        let is_selected = selected_block.read().as_deref() == Some(block.id.as_str());
                        let is_dragging_this = preview_for.as_ref().is_some_and(|(pid, _, _)| *pid == block.id);
                        let (display_title, is_done, is_focus_block) = linked_info
                            .get(&block.id)
                            .map(|(t, c, f)| (t.clone(), *c, *f))
                            .unwrap_or_else(|| (block.title.clone(), false, false));
                        let delete_id = block.id.clone();
                        let drag_id = block.id.clone();
                        let resize_id = block.id.clone();
                        // Original minutes anchor the drag math; labels follow
                        // the (possibly previewed) rendered position.
                        let orig_s = minutes_in_local_day(&block.start).max(0) as u32;
                        let orig_e = minutes_in_local_day(&block.end).max(0) as u32;
                        let start_label = fmt_hhmm(s_min.max(0) as u32);
                        let end_label = fmt_hhmm(e_min.max(0) as u32);
                        // Three-way conditional lives outside rsx: the macro's
                        // conditional-attribute form only supports if/else.
                        // Horizontal placement (left/width) arrives via the
                        // inline overlap-column style, never via classes.
                        let block_class = if is_external {
                            if ext_color.is_some() {
                                // Fill and left bar arrive via inline style.
                                "absolute overflow-hidden rounded border border-faint px-1.5 py-0.5 text-fg-muted"
                            } else {
                                "absolute overflow-hidden rounded border border-faint bg-input px-1.5 py-0.5 text-fg-muted"
                            }
                        } else if is_selected {
                            "absolute overflow-hidden rounded border border-subtle bg-btn-primary-hover px-1.5 py-0.5 text-fg cursor-grab"
                        } else {
                            "absolute overflow-hidden rounded border border-subtle bg-btn-primary px-1.5 py-0.5 text-fg cursor-grab"
                        };
                        rsx! {
                                div {
                                    key: "{block.id}",
                                    class: "{block_class}",
                                    class: if is_dragging_this { "z-10 ring-1 ring-primary-400" },
                                    class: if is_focus_block { "ring-1 ring-primary-300" },
                                    class: if is_external && has_url { "cursor-pointer" },
                                    // Tentative invitations announce themselves in
                                    // the hover tooltip; the dashed left bar is the
                                    // at-a-glance cue.
                                    title: if is_external && ext_tentative { "(tentative) {display_title}" },
                                    style: "{block_style}",
                                    // Selection also runs through the drag: a
                                    // press that never leaves its quarter-hour
                                    // commits as a click in commit_block_drag.
                                    onmousedown: move |evt| {
                                        // Mirrored calendar events are read-only:
                                        // never start a drag, and never let the
                                        // press reach the timeline surface.
                                        evt.stop_propagation();
                                        if is_external {
                                            return;
                                        }
                                        block_drag.set(Some((
                                            drag_id.clone(),
                                            BlockDragMode::Move,
                                            evt.data.screen_coordinates().y,
                                            orig_s,
                                            orig_e,
                                        )));
                                    },
                                    // Only external blocks handle click (deep
                                    // link); stopping propagation also keeps
                                    // the timeline's create-block onclick from
                                    // minting a phantom block under the event.
                                    onclick: move |evt| {
                                        if is_external {
                                            evt.stop_propagation();
                                            if let Some(url) = &click_url {
                                                if let Err(e) = open::that_detached(url) {
                                                    tracing::error!("planner: failed to open event link: {}", e);
                                                }
                                            }
                                        }
                                    },
                                    div {
                                        class: "flex items-start justify-between gap-1",
                                        p {
                                            class: "truncate text-xs font-medium leading-4",
                                            class: if is_done { "line-through opacity-60" },
                                            "{display_title}"
                                        }
                                        if is_external && has_url {
                                            span {
                                                class: "shrink-0 opacity-70",
                                                title: "Open in calendar",
                                                Icon { width: 10, height: 10, icon: fi_icons::FiExternalLink }
                                            }
                                        }
                                        if is_selected && !is_external {
                                            button {
                                                class: "shrink-0 text-red-400 hover:text-red-300",
                                                title: "Delete block",
                                                onmousedown: move |evt| evt.stop_propagation(),
                                                onclick: move |evt| {
                                                    evt.stop_propagation();
                                                    if let Err(e) = store::delete_block(&delete_id) {
                                                        tracing::error!(
                                                            "planner: failed to delete block {}: {}",
                                                            delete_id, e
                                                        );
                                                    }
                                                    planner.write().blocks.retain(|b| b.id != delete_id);
                                                    selected_block.set(None);
                                                },
                                                Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                            }
                                        }
                                    }
                                    if height >= 30.0 {
                                        p { class: "text-[10px] opacity-80", "{start_label}–{end_label}" }
                                    }
                                    if !is_external {
                                        div {
                                            class: "absolute inset-x-0 bottom-0 h-1.5 cursor-ns-resize",
                                            onmousedown: move |evt| {
                                                evt.stop_propagation();
                                                block_drag.set(Some((
                                                    resize_id.clone(),
                                                    BlockDragMode::ResizeEnd,
                                                    evt.data.screen_coordinates().y,
                                                    orig_s,
                                                    orig_e,
                                                )));
                                            },
                                        }
                                    }
                                }
                        }
                    }
                }
            }

            // Off-window strip: the visible home for blocks the ruler culls.
            // Same select/delete affordances as ruler blocks, minus dragging.
            if !off_window_blocks.is_empty() {
                div {
                    class: "flex flex-col gap-1",
                    for (block, side) in off_window_blocks {
                        {
                            let is_selected =
                                selected_block.read().as_deref() == Some(block.id.as_str());
                            let display_title = linked_info
                                .get(&block.id)
                                .map(|(t, _, _)| t.clone())
                                .unwrap_or_else(|| block.title.clone());
                            let start_label =
                                fmt_hhmm(minutes_in_local_day(&block.start).max(0) as u32);
                            let end_label =
                                fmt_hhmm(minutes_in_local_day(&block.end).max(0) as u32);
                            let side_label = match side {
                                OffWindow::Early => "Early",
                                OffWindow::After => "After hours",
                            };
                            let select_id = block.id.clone();
                            let delete_id = block.id.clone();
                            rsx! {
                                div {
                                    key: "{block.id}",
                                    class: if is_selected {
                                        "flex items-center gap-2 rounded bg-card px-2 py-1.5 ring-1 ring-primary-400 cursor-pointer"
                                    } else {
                                        "flex items-center gap-2 rounded bg-card px-2 py-1.5 cursor-pointer"
                                    },
                                    onclick: move |evt| {
                                        evt.stop_propagation();
                                        let current = selected_block.peek().clone();
                                        selected_block.set(
                                            if current.as_deref() == Some(select_id.as_str()) {
                                                None
                                            } else {
                                                Some(select_id.clone())
                                            },
                                        );
                                    },
                                    span {
                                        class: "shrink-0 rounded-full bg-input px-2 py-0.5 text-[10px] text-fg-muted",
                                        "{side_label}"
                                    }
                                    span {
                                        class: "shrink-0 text-[10px] text-fg-muted",
                                        "{start_label}–{end_label}"
                                    }
                                    span { class: "flex-1 min-w-0 truncate text-xs text-fg", "{display_title}" }
                                    if is_selected {
                                        button {
                                            class: "shrink-0 text-red-400 hover:text-red-300",
                                            title: "Delete block",
                                            onclick: move |evt| {
                                                evt.stop_propagation();
                                                if let Err(e) = store::delete_block(&delete_id) {
                                                    tracing::error!(
                                                        "planner: failed to delete block {}: {}",
                                                        delete_id, e
                                                    );
                                                }
                                                planner.write().blocks.retain(|b| b.id != delete_id);
                                                selected_block.set(None);
                                            },
                                            Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Fullscreen capture surface while a block drag is live — the same
            // convention as the column resizers. Snapping happens in the pure
            // helpers; commit persists exactly once on release.
            //
            // This MUST be a sibling of the timeline container, never a child:
            // after mousedown-on-block + mouseup-on-overlay the browser
            // synthesizes a click on their nearest common ancestor. As a child,
            // that ancestor was the container itself, whose onclick is
            // create_block_at — every drag also spawned a phantom "Focus"
            // block at the release point. As a sibling, the common ancestor is
            // this rail's root, which has no click handler.
            if block_drag.read().is_some() {
                div {
                    class: if block_drag.read().as_ref().is_some_and(|(_, m, _, _, _)| *m == BlockDragMode::ResizeEnd) {
                        "fixed inset-0 z-50 cursor-ns-resize"
                    } else {
                        "fixed inset-0 z-50 cursor-grabbing"
                    },
                    onmousemove: move |evt| {
                        let Some((_, mode, anchor_y, orig_s, orig_e)) = block_drag.peek().clone() else {
                            return;
                        };
                        let dy = evt.data.screen_coordinates().y - anchor_y;
                        let delta_min = (dy / PX_PER_HOUR * 60.0).round() as i64;
                        let next = match mode {
                            BlockDragMode::Move => {
                                dragged_move(orig_s, orig_e, delta_min, day_start, day_end)
                            }
                            BlockDragMode::ResizeEnd => {
                                dragged_resize(orig_s, orig_e, delta_min, day_end)
                            }
                        };
                        // Don't promote an unmoved press into a drag: the
                        // first preview is only set once the position
                        // actually differs, so commit can tell click from
                        // drag by preview presence.
                        let current = *drag_preview.peek();
                        if current.is_none() && next == (orig_s, orig_e) {
                            return;
                        }
                        if current != Some(next) {
                            drag_preview.set(Some(next));
                        }
                    },
                    onmouseup: move |_| commit_block_drag(),
                    onmouseleave: move |_| commit_block_drag(),
                }
            }
            p { class: "text-[11px] text-fg-muted", "Click an empty slot for a new block · drag a block to move it · drag its bottom edge to resize." }

            if !unblocked.is_empty() && !*end_day_open.read() {
                div {
                    class: "flex flex-col gap-1",
                    h3 { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Not on the timeline" }
                    for todo in unblocked {
                        {
                            let todo_id = todo.id.clone();
                            let title = todo.title.clone();
                            let estimate = todo.estimate_minutes;
                            // Unestimated work is placed at the default block
                            // length, which then becomes its estimate below.
                            let duration = estimate.unwrap_or(block_minutes).max(15);
                            let busy = busy.clone();
                            rsx! {
                                div {
                                    key: "{todo.id}",
                                    class: "flex items-center gap-2 rounded bg-card px-2 py-1.5",
                                    span { class: "flex-1 min-w-0 truncate text-xs text-fg", "{todo.title}" }
                                    if let Some(m) = estimate {
                                        span { class: "shrink-0 text-[10px] text-fg-muted", "{model::format_minutes(m)}" }
                                    } else {
                                        span {
                                            class: "shrink-0 text-[10px] text-amber-400/80",
                                            title: "No estimate — placing uses the default block length and sets it as the estimate",
                                            "{model::format_minutes(duration)}?"
                                        }
                                    }
                                    button {
                                        class: "shrink-0 rounded p-0.5 text-fg-muted hover:bg-input hover:text-fg",
                                        title: "Add to timeline at the first free slot",
                                        onclick: move |_| {
                                            // Auto-placement never lands before now on today
                                            // (this rail IS today): search from the current
                                            // minute, snapped forward to the quarter grid.
                                            let now_min =
                                                minutes_in_local_day(&Utc::now()).max(0) as u32;
                                            let search_from = placement_search_start(
                                                day_start,
                                                now_min,
                                                is_today,
                                            );
                                            let start_min = first_free_slot(&busy, search_from, duration);
                                            let block = TimeBlock {
                                                id: uuid::Uuid::new_v4().to_string(),
                                                todo_id: Some(todo_id.clone()),
                                                title: title.clone(),
                                                start: local_minutes_to_utc(day, start_min),
                                                end: local_minutes_to_utc(day, start_min + duration),
                                                source: BlockSource::Auto,
                                            };
                                            persist_block(&block);
                                            // Timeboxing the work today IS scheduling it
                                            // today: a todo rolled in from an earlier day
                                            // must not keep its stale date once placed.
                                            let (changed, pruned) = {
                                                let mut state = planner.write();
                                                state.blocks.push(block);
                                                state.schedule_todo_on(&todo_id, day, Utc::now())
                                            };
                                            if changed {
                                                persist_todo(&planner, &todo_id);
                                            }
                                            for b in &pruned {
                                                if let Err(e) = store::delete_block(&b.id) {
                                                    tracing::error!(
                                                        "planner: failed to delete stale block {}: {}",
                                                        b.id, e
                                                    );
                                                }
                                            }
                                            // Estimate ↔ block sync: the block was
                                            // born at `duration`, so stamping it as
                                            // the estimate needs no resize.
                                            if estimate.is_none() {
                                                mutate_todo(planner, &todo_id, |t| {
                                                    t.estimate_minutes = Some(duration)
                                                });
                                            }
                                        },
                                        Icon { width: 14, height: 14, icon: fi_icons::FiPlus }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── End the day ────────────────────────────────────────────
            div { class: "flex-1" }
            if let Some((at, said)) = day_closed.clone() {
                div {
                    class: "flex h-8 shrink-0 items-center rounded border border-faint px-2",
                    span {
                        class: "min-w-0 truncate font-mono text-[11px] tabular-nums text-fg-muted",
                        "{closed_day_line(at, &said)}"
                    }
                }
            } else if *end_day_open.read() {
                div {
                    class: "flex shrink-0 flex-col gap-2 rounded border border-subtle bg-card p-3",
                    div {
                        class: "flex items-baseline justify-between gap-2",
                        span { class: "text-sm font-semibold text-fg", "{nav_label}, {nav_date}" }
                    }
                    span {
                        class: "font-mono text-[11px] tabular-nums text-fg-muted",
                        {
                            format!(
                                "{} done \u{b7} {} carried \u{b7} {} you \u{b7} {} agents",
                                eod.done,
                                eod.carried,
                                fmt_hours(eod.you_minutes),
                                fmt_hours(eod.agent_minutes),
                            )
                        }
                    }

                    p { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Rollup" }
                    if let Some(r) = rollup.clone() {
                        p { class: "text-xs leading-relaxed text-fg", "{r.narrative}" }
                    } else {
                        div {
                            class: "flex items-center gap-2",
                            button {
                                class: "shrink-0 rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input disabled:opacity-50",
                                disabled: *rollup_running.read(),
                                onclick: move |_| {
                                    if *rollup_running.peek() {
                                        return;
                                    }
                                    rollup_running.set(true);
                                    rollup_error.set(None);
                                    let s = settings.peek().clone();
                                    let d = *viewing.peek();
                                    spawn(async move {
                                        match crate::fleet::briefs::generate_day_rollup(&s, d).await {
                                            Ok(_) => rollup_bump += 1,
                                            Err(e) => rollup_error.set(Some(e)),
                                        }
                                        rollup_running.set(false);
                                    });
                                },
                                if *rollup_running.read() { "Rolling up\u{2026}" } else { "Roll up" }
                            }
                            span {
                                class: "min-w-0 flex-1 truncate text-[11px] text-fg-muted",
                                "A short review of what every session got done."
                            }
                        }
                    }
                    if let Some(err) = rollup_error.read().clone() {
                        p { class: "text-[11px] text-red-300", "{err}" }
                    }

                    if !carry_candidates.is_empty() {
                        p { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Carry over" }
                        div {
                            class: "flex flex-wrap gap-1",
                            for (id, title) in carry_candidates.clone() {
                                {
                                    let checked = !carry_unchecked.read().contains(&id);
                                    let toggle_id = id.clone();
                                    rsx! {
                                        button {
                                            key: "carry-{id}",
                                            class: if checked {
                                                "max-w-full rounded-full border border-faint bg-input px-2 py-0.5 text-[11px] text-fg"
                                            } else {
                                                "max-w-full rounded-full border border-faint px-2 py-0.5 text-[11px] text-fg-muted line-through"
                                            },
                                            onclick: move |_| {
                                                let mut set = carry_unchecked.write();
                                                if !set.remove(&toggle_id) {
                                                    set.insert(toggle_id.clone());
                                                }
                                            },
                                            span { class: "truncate", if checked { "\u{2713} " } else { "" } "{title}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    input {
                        class: "w-full rounded border border-faint bg-input px-2 py-1 text-xs text-fg outline-none",
                        placeholder: "One line about the day\u{2026}",
                        value: "{reflection}",
                        oninput: move |e| reflection.set(e.value()),
                    }
                    div {
                        class: "flex items-center gap-2",
                        div { class: "flex-1" }
                        button {
                            class: "rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input",
                            onclick: move |_| end_day_open.set(false),
                            "Not yet"
                        }
                        button {
                            class: "rounded bg-btn-primary px-2 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                            onclick: move |_| {
                                let now = Utc::now();
                                let said = reflection.peek().trim().to_string();
                                // The day plan carries the shutdown: one row
                                // per day, created on the spot if the day was
                                // never explicitly planned.
                                let plan = {
                                    let mut state = planner.write();
                                    let capacity = state.capacity_for(day, default_capacity);
                                    match state.day_plans.iter_mut().find(|p| p.date == day) {
                                        Some(plan) => {
                                            plan.shutdown_at = Some(now);
                                            plan.reflection = said.clone();
                                            plan.clone()
                                        }
                                        None => {
                                            let mut plan = model::DayPlan::new(day, capacity);
                                            plan.shutdown_at = Some(now);
                                            plan.reflection = said.clone();
                                            state.day_plans.push(plan.clone());
                                            plan
                                        }
                                    }
                                };
                                if let Err(e) = store::save_day_plan(&plan) {
                                    tracing::error!("planner: failed to save day plan {}: {}", day, e);
                                }
                                // Carry-over: checked work moves to the next
                                // day, and its timebox follows the schedule
                                // (same rule the rows use).
                                if let Some(next) = day.succ_opt() {
                                    let unchecked = carry_unchecked.peek().clone();
                                    for (id, _) in carry_candidates.iter() {
                                        if unchecked.contains(id) {
                                            continue;
                                        }
                                        mutate_todo(planner, id, |t| t.scheduled_for = Some(next));
                                        let pruned =
                                            planner.write().prune_blocks_for_todo(id, Some(next));
                                        for b in &pruned {
                                            if let Err(e) = store::delete_block(&b.id) {
                                                tracing::error!(
                                                    "planner: failed to delete carried block {}: {}",
                                                    b.id, e
                                                );
                                            }
                                        }
                                    }
                                }
                                tracing::info!(
                                    "planner: closed {} \u{2014} {} done, {} carried",
                                    day, eod.done, eod.carried
                                );
                                carry_unchecked.write().clear();
                                reflection.set(String::new());
                                end_day_open.set(false);
                            },
                            "Close the day"
                        }
                    }
                }
            } else {
                button {
                    class: "flex h-8 shrink-0 items-center gap-2 rounded border border-faint px-2 text-left hover:bg-input",
                    onclick: move |_| end_day_open.set(true),
                    span { class: "shrink-0 text-[11px] text-fg", "End the day" }
                    span {
                        class: "min-w-0 flex-1 truncate font-mono text-[11px] tabular-nums text-fg-muted",
                        { format!("roll up \u{b7} reflect \u{b7} carry over {}", eod.carried) }
                    }
                }
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{CalendarSource, CalendarSubscription};
    use chrono::{DateTime, Duration};
    use std::collections::HashSet;

    fn sub(id: &str, color: &str) -> CalendarSubscription {
        CalendarSubscription {
            id: id.into(),
            name: format!("Sub {}", id),
            color: color.into(),
            enabled: true,
            source: CalendarSource::Ics {},
        }
    }

    /// A UTC instant at `hour`:00 **local** time on `day` — matches how the
    /// planner builds instants, so tests stay timezone-independent.
    fn local_instant(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cal_event(sub: &str, uid: &str, start: DateTime<Utc>, end: DateTime<Utc>, all_day: bool) -> model::CalendarEvent {
        model::CalendarEvent {
            uid: uid.into(),
            subscription_id: sub.into(),
            title: format!("Event {}", uid),
            start,
            end,
            all_day,
            url: None,
            location: None,
            busy: true,
            tentative: false,
        }
    }

    #[test]
    fn clamped_geometry_matches_block_geometry_when_fully_inside() {
        // 09:30–10:15 in a 09:00–17:00 window: same pixels, no clamp flags.
        let geo = clamped_block_geometry(570, 615, 540, 1020).unwrap();
        assert_eq!((geo.top, geo.height), block_geometry(570, 615, 540, 1020).unwrap());
        assert!(!geo.clamped_top && !geo.clamped_bottom);
    }

    #[test]
    fn clamped_geometry_flags_the_clamped_side() {
        // 08:00–10:00 against a 09:00 open: clamped at the top only.
        let geo = clamped_block_geometry(480, 600, 540, 1020).unwrap();
        assert_eq!((geo.top, geo.height), block_geometry(480, 600, 540, 1020).unwrap());
        assert!(geo.clamped_top && !geo.clamped_bottom);
        assert_eq!(geo.top, 0.0);

        // 16:30–18:00 against a 17:00 close: clamped at the bottom only.
        let geo = clamped_block_geometry(990, 1080, 540, 1020).unwrap();
        assert!(!geo.clamped_top && geo.clamped_bottom);

        // 08:00–18:00 spans the whole window: clamped on both sides.
        let geo = clamped_block_geometry(480, 1080, 540, 1020).unwrap();
        assert!(geo.clamped_top && geo.clamped_bottom);
        assert_eq!(geo.top, 0.0);
        assert_eq!(geo.height, 8.0 * PX_PER_HOUR);
    }

    #[test]
    fn clamped_geometry_excludes_fully_outside_and_zero_length() {
        // Entirely before the window / entirely after: not drawn (counted
        // instead, via off_window).
        assert_eq!(clamped_block_geometry(420, 540, 540, 1020), None);
        assert_eq!(clamped_block_geometry(1020, 1080, 540, 1020), None);
        assert_eq!(off_window(420, 540, 540, 1020), Some(OffWindow::Early));
        assert_eq!(off_window(1020, 1080, 540, 1020), Some(OffWindow::After));
        // Zero- and negative-length guard.
        assert_eq!(clamped_block_geometry(600, 600, 540, 1020), None);
        assert_eq!(clamped_block_geometry(600, 570, 540, 1020), None);
    }

    #[test]
    fn fmt_outside_counts_reads_naturally() {
        assert_eq!(fmt_outside_counts(0, 0), None);
        assert_eq!(fmt_outside_counts(2, 0), Some("2 earlier".into()));
        assert_eq!(fmt_outside_counts(0, 1), Some("1 later".into()));
        assert_eq!(fmt_outside_counts(2, 1), Some("2 earlier · 1 later".into()));
    }

    #[test]
    fn subscription_color_resolves_known_ids_and_falls_back() {
        let subs = vec![sub("s1", "#4f9cf9"), sub("s2", "#34d399")];
        assert_eq!(subscription_color(&subs, Some("s2")), Some("#34d399".into()));
        assert_eq!(subscription_color(&subs, Some("gone")), None);
        assert_eq!(subscription_color(&subs, None), None);
        assert_eq!(subscription_color(&[], Some("s1")), None);
    }

    #[test]
    fn external_block_style_carries_tint_and_clamp_cues() {
        let plain = external_block_style(24.0, 36.0, None, false, false, true, false);
        assert_eq!(plain, "top: 24px; height: 36px;");

        let tinted = external_block_style(0.0, 48.0, Some("#34d399"), true, false, true, false);
        assert!(tinted.contains("border-left: 3px solid #34d399;"));
        assert!(tinted.contains("background-color: #34d39926;"), "{}", tinted);
        assert!(tinted.contains("border-top-style: dashed;"));
        assert!(!tinted.contains("border-bottom-style"));
        assert!(!tinted.contains("opacity"), "busy blocks keep full opacity");

        let bottom = external_block_style(0.0, 48.0, None, false, true, true, false);
        assert!(bottom.contains("border-bottom-style: dashed;"));
        assert!(!bottom.contains("border-top-style"));
    }

    #[test]
    fn non_busy_external_block_style_reads_as_non_blocking() {
        let free = external_block_style(0.0, 48.0, Some("#34d399"), false, false, false, false);
        assert!(free.contains("border-left: 3px dashed #34d399;"), "{}", free);
        assert!(free.contains("background-color: #34d39914;"), "fainter fill: {}", free);
        assert!(free.contains("opacity: 0.65;"), "{}", free);

        // Colorless free block still gets the opacity cue.
        let plain_free = external_block_style(0.0, 48.0, None, false, false, false, false);
        assert!(plain_free.contains("opacity: 0.65;"), "{}", plain_free);
    }

    #[test]
    fn tentative_external_block_style_dashes_the_bar_but_keeps_the_busy_tint() {
        let maybe = external_block_style(0.0, 48.0, Some("#34d399"), false, false, true, true);
        assert!(maybe.contains("border-left: 3px dashed #34d399;"), "{}", maybe);
        assert!(
            maybe.contains("background-color: #34d39926;"),
            "tentative keeps the full busy fill: {}",
            maybe
        );
        assert!(
            !maybe.contains("opacity"),
            "tentative still blocks time — no free-style fade: {}",
            maybe
        );
    }

    #[test]
    fn all_day_partition_filters_by_flag_subscription_and_day() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let enabled: HashSet<String> = ["s1".to_string()].into();
        let midnight = local_instant(day, 0);

        let events = vec![
            // Today's all-day event on an enabled subscription: kept.
            cal_event("s1", "offsite", midnight, midnight + Duration::days(1), true),
            // Timed event: not an all-day banner.
            cal_event("s1", "standup", local_instant(day, 9), local_instant(day, 10), false),
            // Disabled subscription: dropped.
            cal_event("s2", "other", midnight, midnight + Duration::days(1), true),
            // Yesterday only (end is exclusive — it closes at today's
            // midnight and must not bleed into today).
            cal_event("s1", "yesterday", midnight - Duration::days(1), midnight, true),
            // Multi-day span covering today: kept.
            cal_event("s1", "conf", midnight - Duration::days(1), midnight + Duration::days(2), true),
        ];

        let uids: Vec<String> = all_day_events_for(&events, &enabled, day)
            .into_iter()
            .map(|e| e.uid)
            .collect();
        assert_eq!(uids, vec!["conf".to_string(), "offsite".to_string()]);
    }

    #[test]
    fn parse_hhmm_accepts_valid_and_rejects_junk() {
        assert_eq!(parse_hhmm("09:00"), Some(540));
        assert_eq!(parse_hhmm(" 17:30 "), Some(1050));
        assert_eq!(parse_hhmm("0:05"), Some(5));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("09:60"), None);
        assert_eq!(parse_hhmm("nine"), None);
        assert_eq!(parse_hhmm("09"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn now_line_appears_only_inside_the_workday_window() {
        // 09:00–17:00 window: before, at start, mid-day, at end, after.
        assert_eq!(now_line_offset(8 * 60, 540, 1020), None);
        assert_eq!(now_line_offset(540, 540, 1020), Some(0.0));
        assert_eq!(now_line_offset(12 * 60 + 30, 540, 1020), Some(3.5 * PX_PER_HOUR));
        assert_eq!(now_line_offset(1020, 540, 1020), Some(8.0 * PX_PER_HOUR));
        assert_eq!(now_line_offset(1021, 540, 1020), None);
    }

    #[test]
    fn workday_bounds_falls_back_on_bad_or_inverted_input() {
        assert_eq!(workday_bounds("08:00", "16:00"), (480, 960));
        assert_eq!(workday_bounds("garbage", "16:00"), (540, 1020));
        // End before start makes no timeline — fall back rather than invert.
        assert_eq!(workday_bounds("17:00", "09:00"), (540, 1020));
        assert_eq!(workday_bounds("09:00", "09:00"), (540, 1020));
    }

    #[test]
    fn block_geometry_positions_and_clamps() {
        // 09:30–10:15 in a 09:00–17:00 window at 48px/h.
        assert_eq!(block_geometry(570, 615, 540, 1020), Some((24.0, 36.0)));
        // Starting before the window clamps to its top.
        assert_eq!(block_geometry(480, 600, 540, 1020), Some((0.0, 48.0)));
        // Fully outside the window renders nothing.
        assert_eq!(block_geometry(300, 480, 540, 1020), None);
        assert_eq!(block_geometry(1080, 1140, 540, 1020), None);
        // Very short blocks keep a height that fits a one-line title.
        let (_, h) = block_geometry(540, 545, 540, 1020).unwrap();
        assert_eq!(h, 20.0);
    }

    #[test]
    fn off_window_classifies_exactly_what_the_ruler_culls() {
        let (ds, de) = (540, 1020); // 09:00–17:00

        // Fully before / fully after the window, edges touching included.
        assert_eq!(off_window(300, 480, ds, de), Some(OffWindow::Early));
        assert_eq!(off_window(480, 540, ds, de), Some(OffWindow::Early));
        assert_eq!(off_window(1020, 1080, ds, de), Some(OffWindow::After));
        assert_eq!(off_window(1080, 1140, ds, de), Some(OffWindow::After));

        // Anything the ruler draws (even partially) is not off-window.
        assert_eq!(off_window(570, 615, ds, de), None);
        assert_eq!(off_window(480, 600, ds, de), None);
        assert_eq!(off_window(1000, 1080, ds, de), None);

        // The complement of block_geometry: every block is drawn or stripped.
        for (s, e) in [(300, 480), (480, 600), (570, 615), (1000, 1080), (1020, 1080)] {
            assert_eq!(
                block_geometry(s, e, ds, de).is_none(),
                off_window(s, e, ds, de).is_some(),
                "({}, {}) must render in exactly one place",
                s,
                e
            );
        }
    }

    #[test]
    fn dragged_move_snaps_and_clamps() {
        // 09:30–10:15 dragged down 40min lands on the 10:00 grid line, 45m kept.
        assert_eq!(dragged_move(570, 615, 40, 540, 1020), (600, 645));
        // Dragging above the workday pins to its start.
        assert_eq!(dragged_move(570, 615, -600, 540, 1020), (540, 585));
        // Dragging below pins so the block still ends inside the day.
        assert_eq!(dragged_move(570, 615, 600, 540, 1020), (975, 1020));
        // Zero delta is the identity — a click must not look like a drag.
        assert_eq!(dragged_move(570, 615, 0, 540, 1020), (570, 615));
        // A zero-length block is treated as the 15m minimum, not a panic.
        assert_eq!(dragged_move(600, 600, 0, 540, 1020), (600, 615));
    }

    #[test]
    fn dragged_resize_keeps_a_minimum_and_stays_in_day() {
        // 09:30–10:15 pulled 30min longer.
        assert_eq!(dragged_resize(570, 615, 30, 1020), (570, 645));
        // Shrinking below 15 minutes stops at 15.
        assert_eq!(dragged_resize(570, 615, -600, 1020), (570, 585));
        // Growing past the workday stops at its end.
        assert_eq!(dragged_resize(570, 615, 600, 1020), (570, 1020));
        // Snap floors to the quarter grid.
        assert_eq!(dragged_resize(570, 615, 10, 1020), (570, 615));
    }

    #[test]
    fn fmt_hhmm_pads() {
        assert_eq!(fmt_hhmm(570), "09:30");
        assert_eq!(fmt_hhmm(0), "00:00");
        assert_eq!(fmt_hhmm(1020), "17:00");
    }

    // Date labels are covered by views::friendly_date's tests (U3.6): one
    // formatter, one test home.

    #[test]
    fn snap_to_quarter_rounds_down() {
        assert_eq!(snap_to_quarter(540), 540);
        assert_eq!(snap_to_quarter(554), 540);
        assert_eq!(snap_to_quarter(555), 555);
        assert_eq!(snap_to_quarter(569), 555);
    }

    #[test]
    fn first_free_slot_finds_gaps_in_order() {
        // Empty day: right at the start.
        assert_eq!(first_free_slot(&[], 540, 60), 540);
        // 09:00–10:00 busy: an hour-long task starts at 10:00.
        assert_eq!(first_free_slot(&[(540, 600)], 540, 60), 600);
        // A gap big enough between blocks is used.
        assert_eq!(first_free_slot(&[(540, 600), (660, 720)], 540, 60), 600);
        // A gap too small is skipped.
        assert_eq!(first_free_slot(&[(540, 600), (630, 720)], 540, 60), 720);
        // Unsorted input still resolves earliest-first.
        assert_eq!(first_free_slot(&[(630, 720), (540, 600)], 540, 30), 600);
        // Busy ends snap up to the next quarter hour.
        assert_eq!(first_free_slot(&[(540, 610)], 540, 30), 615);
    }

    /// Regression for Phase 5's busy-time verification: the timeline's
    /// auto-place ("Add to timeline") builds its busy set from `blocks_on`,
    /// which must include external calendar blocks (`todo_id: None`,
    /// `BlockSource::External`) — a meeting is not free time.
    #[test]
    fn auto_placement_treats_external_meetings_as_busy() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let mut state = PlannerState::default();
        state.blocks.push(TimeBlock {
            id: "own".into(),
            todo_id: None,
            title: "Deep work".into(),
            start: local_instant(day, 8),
            end: local_instant(day, 9),
            source: BlockSource::Manual,
        });
        state.blocks.push(TimeBlock {
            id: "ext".into(),
            todo_id: None,
            title: "Board meeting".into(),
            start: local_instant(day, 9),
            end: local_instant(day, 11),
            source: BlockSource::External {
                uid: "mtg-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        });

        // Exactly the component's construction (blocks_on → minute spans).
        let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
        let busy: Vec<(u32, u32)> = blocks
            .iter()
            .map(|b| {
                (
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect();
        // A 60-minute todo placed from 08:00 must skip both the own block and
        // the meeting, landing at 11:00 — not 09:00, which is mid-meeting.
        assert_eq!(first_free_slot(&busy, 8 * 60, 60), 11 * 60);
    }

    /// Mirror of the component's busy-set construction: non-busy external
    /// blocks (free / focus time) are excluded, so auto-placement may place
    /// tasks over them.
    #[test]
    fn auto_placement_ignores_non_busy_external_blocks() {
        let day: NaiveDate = "2026-08-12".parse().unwrap();
        let mut state = PlannerState::default();
        state.blocks.push(TimeBlock {
            id: "focus-hold".into(),
            todo_id: None,
            title: "Focus time".into(),
            start: local_instant(day, 9),
            end: local_instant(day, 11),
            source: BlockSource::External {
                uid: "ft-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: false,
                tentative: false,
            },
        });
        state.blocks.push(TimeBlock {
            id: "mtg".into(),
            todo_id: None,
            title: "Board meeting".into(),
            start: local_instant(day, 11),
            end: local_instant(day, 12),
            source: BlockSource::External {
                uid: "mtg-1".into(),
                subscription_id: Some("s1".into()),
                url: None,
                busy: true,
                tentative: false,
            },
        });

        let blocks: Vec<TimeBlock> = state.blocks_on(day).into_iter().cloned().collect();
        let busy: Vec<(u32, u32)> = blocks
            .iter()
            .filter(|b| !matches!(b.source, BlockSource::External { busy: false, .. }))
            .map(|b| {
                (
                    minutes_in_local_day(&b.start).max(0) as u32,
                    minutes_in_local_day(&b.end).max(0) as u32,
                )
            })
            .collect();
        // The free 9–11 hold doesn't block: a 60m task from 09:00 lands right
        // at 09:00. Only the real meeting (11–12) is busy.
        assert_eq!(first_free_slot(&busy, 9 * 60, 60), 9 * 60);
        assert_eq!(first_free_slot(&busy, 11 * 60, 60), 12 * 60);
    }

    #[test]
    fn placement_search_start_clamps_today_to_now() {
        let day_start = 540; // 09:00

        // Before the workday opens: the workday start wins.
        assert_eq!(placement_search_start(day_start, 7 * 60, true), 540);
        // Mid-day: now wins, snapped FORWARD to the quarter grid (13:07 → 13:15).
        assert_eq!(placement_search_start(day_start, 13 * 60 + 7, true), 13 * 60 + 15);
        // Already on the grid: no over-snap (13:15 stays 13:15).
        assert_eq!(placement_search_start(day_start, 13 * 60 + 15, true), 13 * 60 + 15);
        // One minute past a grid line still snaps forward, never back.
        assert_eq!(placement_search_start(day_start, 541, true), 555);
        // After the workday ends: the search starts past the day end — the
        // caller's first-fit overflows honestly rather than planning fiction.
        assert_eq!(placement_search_start(day_start, 18 * 60, true), 18 * 60);
        // Future days are untouched.
        assert_eq!(placement_search_start(day_start, 18 * 60, false), 540);
        assert_eq!(placement_search_start(day_start, 0, false), 540);
    }

    // ── Overlap columns ─────────────────────────────────────────────────────

    fn cols(entries: &[(&str, u32, u32)]) -> std::collections::HashMap<String, (usize, usize)> {
        let owned: Vec<(String, u32, u32)> = entries
            .iter()
            .map(|(id, s, e)| (id.to_string(), *s, *e))
            .collect();
        assign_overlap_columns(&owned)
    }

    #[test]
    fn disjoint_blocks_keep_full_width() {
        let map = cols(&[("a", 540, 600), ("b", 660, 720), ("c", 780, 840)]);
        for id in ["a", "b", "c"] {
            assert_eq!(map[id], (0, 1), "{id} keeps the classic footprint");
        }
    }

    #[test]
    fn two_overlapping_blocks_split_in_half() {
        let map = cols(&[("a", 540, 600), ("b", 570, 630)]);
        assert_eq!(map["a"], (0, 2));
        assert_eq!(map["b"], (1, 2));
    }

    #[test]
    fn staggered_chain_shares_one_cluster() {
        // A–B overlap and B–C overlap but A–C don't: transitively one cluster.
        // A and C never coexist, so greedy reuses column 0 for C.
        let map = cols(&[("a", 540, 600), ("b", 570, 660), ("c", 620, 700)]);
        assert_eq!(map["a"], (0, 2));
        assert_eq!(map["b"], (1, 2));
        assert_eq!(map["c"], (0, 2), "C reuses A's column; the cluster needs 2");
    }

    #[test]
    fn four_concurrent_blocks_cap_at_three_columns() {
        let map = cols(&[
            ("a", 540, 660),
            ("b", 540, 660),
            ("c", 540, 660),
            ("d", 540, 660),
        ]);
        assert_eq!(map["a"], (0, 3));
        assert_eq!(map["b"], (1, 3));
        assert_eq!(map["c"], (2, 3));
        assert_eq!(map["d"], (2, 3), "overflow shares column 2 (0-based)");
    }

    #[test]
    fn touching_edges_stay_separate_clusters() {
        // end == next start does not overlap (TimeBlock::overlaps parity).
        let map = cols(&[("a", 540, 600), ("b", 600, 660)]);
        assert_eq!(map["a"], (0, 1));
        assert_eq!(map["b"], (0, 1));

        // ...even inside a mixed set: the meeting overlapping a timebox splits,
        // the back-to-back one doesn't.
        let map = cols(&[("box", 540, 600), ("mtg", 570, 630), ("next", 630, 690)]);
        assert_eq!(map["box"], (0, 2));
        assert_eq!(map["mtg"], (1, 2));
        assert_eq!(map["next"], (0, 1));
    }

    #[test]
    fn overlap_columns_skip_degenerate_spans() {
        let map = cols(&[("zero", 600, 600), ("real", 540, 660)]);
        assert!(!map.contains_key("zero"));
        assert_eq!(map["real"], (0, 1));
    }

    #[test]
    fn overlap_column_style_composes_inline_geometry() {
        // Single column: the classic full-width footprint, unchanged look.
        assert_eq!(overlap_column_style(0, 1), " left: 44px; right: 4px;");

        // Two columns: halves with a gutter, offset by the column index.
        let left = overlap_column_style(0, 2);
        assert!(left.contains("left: calc(44px + (100% - 48px) * 0.0000)"), "{left}");
        assert!(left.contains("width: calc((100% - 48px) * 0.5000 - 3px)"), "{left}");
        let right = overlap_column_style(1, 2);
        assert!(right.contains("left: calc(44px + (100% - 48px) * 0.5000)"), "{right}");

        // Three columns: thirds.
        let mid = overlap_column_style(1, 3);
        assert!(mid.contains("* 0.3333)"), "{mid}");
        assert!(mid.contains("* 0.3333 - 3px)"), "{mid}");
    }

    #[test]
    fn local_minutes_round_trip() {
        let date: NaiveDate = "2026-08-12".parse().unwrap();
        let dt = local_minutes_to_utc(date, 570);
        assert_eq!(minutes_in_local_day(&dt), 570);
        // Out-of-range minutes clamp inside the day instead of panicking.
        let clamped = local_minutes_to_utc(date, 30_000);
        assert_eq!(minutes_in_local_day(&clamped), 23 * 60 + 59);
    }

    // ── Day navigation ──────────────────────────────────────────────────

    #[test]
    fn day_label_names_the_days_you_live_in() {
        let today: NaiveDate = "2026-09-16".parse().unwrap();
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(day_label("2026-09-17".parse().unwrap(), today), "Tomorrow");
        assert_eq!(day_label("2026-09-15".parse().unwrap(), today), "Yesterday");
        // Anything further out is just its weekday — the exact date rides in
        // its own span beside the label.
        assert_eq!(day_label("2026-09-18".parse().unwrap(), today), "Friday");
        assert_eq!(day_label("2026-09-14".parse().unwrap(), today), "Monday");
        assert_eq!(day_date_label(today), "Wed 16 Sep");
    }

    // ── Free gaps ───────────────────────────────────────────────────────

    #[test]
    fn free_gaps_finds_usable_space_between_busy_items() {
        // 09:00–17:00 with 10:00–11:00 and 13:00–13:30 busy.
        let busy = [(600, 660), (780, 810)];
        assert_eq!(
            free_gaps(&busy, 540, 1020, 30),
            vec![(540, 600), (660, 780), (810, 1020)]
        );
    }

    #[test]
    fn free_gaps_skips_short_slots_and_merges_overlaps() {
        // 09:00–09:20 free is changeover, not a slot.
        let busy = [(560, 660)];
        assert_eq!(free_gaps(&busy, 540, 1020, 30), vec![(660, 1020)]);

        // Overlapping and nested busy intervals collapse to one stretch.
        let busy = [(600, 720), (630, 660), (660, 780)];
        assert_eq!(free_gaps(&busy, 540, 1020, 30), vec![(540, 600), (780, 1020)]);
    }

    #[test]
    fn free_gaps_clips_to_the_window_and_is_total() {
        // Busy spilling past both edges leaves nothing.
        assert!(free_gaps(&[(0, 1440)], 540, 1020, 30).is_empty());
        // An empty day is one long gap.
        assert_eq!(free_gaps(&[], 540, 1020, 30), vec![(540, 1020)]);
        // Degenerate inputs never panic and never invent a slot.
        assert!(free_gaps(&[], 1020, 540, 30).is_empty());
        assert!(free_gaps(&[], 540, 1020, 0).is_empty());
        assert!(free_gaps(&[(700, 600)], 540, 1020, 1000).is_empty());
        // Raising the floor (today: no slots behind you) drops the morning.
        assert_eq!(free_gaps(&[], 840, 1020, 30), vec![(840, 1020)]);
    }

    // ── Agent lane ──────────────────────────────────────────────────────

    fn span(id: &str, day: NaiveDate, from: u32, to: u32, background: bool) -> crate::fleet::replay::DaySpan {
        crate::fleet::replay::DaySpan {
            session_id: id.into(),
            start: local_minutes_to_utc(day, from),
            end: local_minutes_to_utc(day, to),
            background,
        }
    }

    #[test]
    fn merge_agent_spans_joins_touching_spans_of_one_session() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            span("a", day, 540, 600, false),
            span("a", day, 600, 660, true), // touches: one stretch
            span("b", day, 570, 600, false),
        ];
        let merged = merge_agent_spans(&spans, 12);
        assert_eq!(merged.len(), 2, "{merged:?}");
        // Chronological, and the merged stretch spans both parts.
        assert_eq!(merged[0].session_id, "a");
        assert_eq!(merged[0].start, local_minutes_to_utc(day, 540));
        assert_eq!(merged[0].end, local_minutes_to_utc(day, 660));
        // A stretch containing driven work is not a background stretch.
        assert!(!merged[0].background);
        assert_eq!(merged[1].session_id, "b");
    }

    #[test]
    fn merge_agent_spans_keeps_gaps_and_all_background_stays_background() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            span("a", day, 540, 600, true),
            span("a", day, 660, 720, true), // an hour apart: two stretches
        ];
        let merged = merge_agent_spans(&spans, 12);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|s| s.background));
    }

    #[test]
    fn merge_agent_spans_caps_sessions_keeping_the_busiest() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans: Vec<_> = (0..20)
            .map(|i| span(&format!("s{i:02}"), day, 540 + i, 540 + i + i + 1, false))
            .collect();
        let merged = merge_agent_spans(&spans, 12);
        let sessions: std::collections::HashSet<&str> =
            merged.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(sessions.len(), 12, "capped at the lane's limit");
        // The busiest survive: s19 (39 min) in, s00 (1 min) out.
        assert!(sessions.contains("s19"));
        assert!(!sessions.contains("s00"));
        // Zero-length spans never reach the lane.
        assert!(merge_agent_spans(&[span("z", day, 600, 600, false)], 12).is_empty());
    }

    #[test]
    fn agent_lane_geometry_sits_in_the_right_sub_column() {
        let style = agent_span_style(24.0, 48.0, false, 0, 1);
        assert!(style.contains("left: calc(44px + (100% - 48px) * 0.6600)"), "{style}");
        assert!(style.contains("right: 4px"), "{style}");
        assert!(!style.contains("opacity"), "a driven span is not dimmed");
        assert!(agent_span_style(0.0, 10.0, true, 0, 1).contains("opacity: 0.65"));

        // A person's block yields the sub-column only when the lane exists.
        assert_eq!(overlap_column_style_scaled(0, 1, 1.0), overlap_column_style(0, 1));
        let narrowed = overlap_column_style_scaled(0, 1, AGENT_LANE_SPLIT);
        assert!(narrowed.contains("width: calc((100% - 48px) * 0.6600 - 3px)"), "{narrowed}");
        // Overlap columns split the narrowed width, not the full one.
        let col1 = overlap_column_style_scaled(1, 2, AGENT_LANE_SPLIT);
        assert!(col1.contains("* 0.3300)"), "{col1}");

        // Two sub-columns within the lane split its right-hand region, not
        // the whole block area.
        let sub0 = agent_span_style(0.0, 10.0, false, 0, 2);
        assert!(sub0.contains("left: calc(44px + (100% - 48px) * 0.6600)"), "{sub0}");
        assert!(sub0.contains("width: calc((100% - 48px) * 0.1700 - 2px)"), "{sub0}");
        let sub1 = agent_span_style(0.0, 10.0, false, 1, 2);
        assert!(sub1.contains("left: calc(44px + (100% - 48px) * 0.8300)"), "{sub1}");
    }

    fn fleet_origin_map(
        pairs: &[(&str, crate::fleet::FleetOrigin)],
    ) -> std::collections::HashMap<String, crate::fleet::FleetOrigin> {
        pairs.iter().map(|(id, o)| (id.to_string(), *o)).collect()
    }

    #[test]
    fn filter_external_spans_drops_hobbes_tabs_and_unresolved_sessions() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            span("ext", day, 540, 600, false),
            span("hobbes-tab", day, 540, 600, false),
            span("unknown", day, 540, 600, false),
        ];
        let origins = fleet_origin_map(&[
            ("ext", crate::fleet::FleetOrigin::External),
            ("hobbes-tab", crate::fleet::FleetOrigin::Hobbes),
            // "unknown" deliberately absent: no row resolves for it.
        ]);
        let kept = filter_external_spans(spans, &origins);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session_id, "ext");
    }

    #[test]
    fn agent_lane_layout_gives_two_concurrent_spans_distinct_sub_columns() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            (span("a", day, 540, 600, false), "alpha".to_string()),
            (span("b", day, 550, 610, false), "beta".to_string()),
        ];
        let items = agent_lane_layout(&spans);
        assert_eq!(items.len(), 2);
        let cols: std::collections::HashSet<usize> = items.iter().map(|i| i.col).collect();
        assert_eq!(cols, std::collections::HashSet::from([0, 1]));
        assert!(items.iter().all(|i| i.count == 2));
    }

    #[test]
    fn agent_lane_layout_collapses_four_concurrent_spans_into_a_band() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            (span("a", day, 540, 600, false), "alpha".to_string()),
            (span("b", day, 545, 605, false), "beta".to_string()),
            (span("c", day, 550, 610, false), "gamma".to_string()),
            (span("d", day, 555, 615, false), "delta".to_string()),
        ];
        let items = agent_lane_layout(&spans);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(items.len(), 1, "{labels:?}");
        assert_eq!(items[0].label, "4 agents");
        assert_eq!(items[0].start, local_minutes_to_utc(day, 540));
        assert_eq!(items[0].end, local_minutes_to_utc(day, 615));
        assert_eq!(items[0].col, 0);
        assert_eq!(items[0].count, 1);
        assert!(items[0].title.contains("alpha"));
        assert!(items[0].title.contains("delta"));
    }

    #[test]
    fn agent_lane_layout_keeps_full_width_when_nothing_overlaps() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let spans = vec![
            (span("a", day, 540, 600, false), "alpha".to_string()),
            (span("b", day, 600, 660, false), "beta".to_string()), // touches, doesn't overlap
        ];
        let items = agent_lane_layout(&spans);
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.col == 0 && i.count == 1));
    }

    // ── Capacity readout ────────────────────────────────────────────────

    fn capacity(planned: u32, done: u32, cap: u32) -> model::Capacity {
        model::Capacity {
            planned_minutes: planned,
            done_minutes: done,
            capacity_minutes: cap,
            unestimated: 0,
            agent_minutes: 0,
        }
    }

    #[test]
    fn capacity_labels_read_as_two_halves() {
        assert_eq!(capacity_done_label(45, 260), "45m done · 3h 35m to go");
        let (label, over) = capacity_balance_label(&capacity(180, 0, 255));
        assert_eq!((label.as_str(), over), ("1h 15m free", false));
        let (label, over) = capacity_balance_label(&capacity(270, 0, 240));
        assert_eq!((label.as_str(), over), ("over by 30m", true));
        // A finished day reads "all done", never a negative remainder.
        assert_eq!(capacity_done_label(120, 120), "2h done · 0m to go");
    }

    // ── End the day ─────────────────────────────────────────────────────

    fn todo_on(id: &str, day: Option<NaiveDate>, closed_on: Option<NaiveDate>) -> Todo {
        let mut t = Todo::new(id, 0.0);
        t.id = id.to_string();
        t.scheduled_for = day;
        if let Some(d) = closed_on {
            t.status = TodoStatus::Completed;
            t.completed_at = Some(local_minutes_to_utc(d, 600));
        }
        t
    }

    #[test]
    fn carry_over_defaults_to_the_days_open_todos() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let other: NaiveDate = "2026-09-17".parse().unwrap();
        let todos = vec![
            todo_on("open-today", Some(day), None),
            todo_on("done-today", Some(day), Some(day)),
            todo_on("open-tomorrow", Some(other), None),
            todo_on("unscheduled", None, None),
        ];
        let carried: Vec<String> = carry_over_candidates(&todos, day)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(carried, vec!["open-today"], "only open work on the day");
    }

    #[test]
    fn end_of_day_summary_counts_the_day() {
        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let other: NaiveDate = "2026-09-15".parse().unwrap();
        let todos = vec![
            todo_on("a", Some(day), None),
            todo_on("b", Some(day), None),
            todo_on("c", Some(day), Some(day)),
            // Closed today but scheduled last week: still today's win.
            todo_on("d", Some(other), Some(day)),
            // Closed on another day: not today's.
            todo_on("e", Some(other), Some(other)),
        ];
        let closed = |id: &str, todo: &str, from: u32, to: u32, actor: model::FocusActor| {
            let mut s = model::FocusSession::open(todo, local_minutes_to_utc(day, from), actor);
            s.id = id.to_string();
            s.ended_at = Some(local_minutes_to_utc(day, to));
            s.minutes = to - from;
            s
        };
        let sessions = vec![
            closed("f1", "a", 540, 630, model::FocusActor::Person),
            closed("f2", "b", 660, 690, model::FocusActor::Agent { session_id: None }),
        ];
        let s = end_of_day_summary(&todos, &sessions, day, 150);
        assert_eq!(s.done, 2, "c and d closed on the day");
        assert_eq!(s.carried, 2, "a and b are still open on the day");
        assert_eq!(s.you_minutes, 90, "agent sittings are not your hours");
        assert_eq!(s.agent_minutes, 150, "agent total is passed in, not derived");
    }

    #[test]
    fn shutdown_line_and_hours_round_for_a_day_you_are_closing() {
        assert_eq!(fmt_hours(0), "0h");
        assert_eq!(fmt_hours(90), "1.5h");
        assert_eq!(fmt_hours(120), "2h");
        assert_eq!(fmt_hours(115), "1.9h");

        let day: NaiveDate = "2026-09-16".parse().unwrap();
        let at = local_minutes_to_utc(day, 17 * 60 + 42);
        assert_eq!(closed_day_line(at, "   "), "Closed at 17:42");
        assert_eq!(
            closed_day_line(at, "good day"),
            "Closed at 17:42 · \u{201c}good day\u{201d}"
        );
    }
}
