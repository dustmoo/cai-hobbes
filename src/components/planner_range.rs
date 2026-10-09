//! Visible-range model for the Planner's Today rail.
//!
//! The rail always shows a window `[start, end)` of the day (minutes since
//! local midnight) stretched to fill the rail body, so `px_per_hour` is derived
//! (`body_px / range_hours`) and changing what you see means changing the
//! range. Everything here is pure (range math, pixel <-> minute conversion,
//! day-strip positions) so it can be unit-tested without a Dioxus runtime;
//! the two small components at the bottom (`DayStrip`, `RangeControls`) only
//! wire those helpers to events.
//!
//! Geometry is inline `style:` only — Tailwind purges computed class names.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// Minutes in a day: the hard upper bound of any range.
pub const DAY_MINUTES: u32 = 24 * 60;
/// Narrowest range the rail will zoom to.
pub const MIN_SPAN: u32 = 2 * 60;
/// Everything the range does snaps to this grid.
pub const SNAP: u32 = 15;
/// Pre-measurement fallback scale.
pub const FALLBACK_PX_PER_HOUR: f64 = 48.0;
/// Zoom stepper factor.
pub const ZOOM_STEP: f64 = 1.5;
/// The "Now" preset: start this long before now, span this wide.
const NOW_LEAD: i64 = 60;
const NOW_SPAN: i64 = 5 * 60;
/// Colour used for the visible-range bracket and the primary block family.
pub const BRACKET_COLOR: &str = "#84A4D1";

/// A visible window onto the day, in minutes since local midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewRange {
    pub start: u32,
    pub end: u32,
}

impl ViewRange {
    pub fn span(self) -> u32 {
        self.end - self.start
    }
    pub fn hours(self) -> f64 {
        self.span() as f64 / 60.0
    }
}

/// What the rail remembers: a preset (which keeps following the settings /
/// the clock) or an explicit custom range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RailView {
    Workday,
    Now,
    FullDay,
    Custom { start: u32, end: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Workday,
    Now,
    FullDay,
}

fn snap_round(m: i64) -> i64 {
    let s = SNAP as i64;
    (m as f64 / s as f64).round() as i64 * s
}

fn snap_floor(m: i64) -> i64 {
    m.div_euclid(SNAP as i64) * SNAP as i64
}

/// Force any `(start, end)` into a valid range: span within `[MIN_SPAN, 24h]`
/// (kept as requested, widened/narrowed about the start when out of bounds),
/// then shifted so it lies inside `[0, 24h]`.
pub fn clamp_range(start: i64, end: i64) -> ViewRange {
    let span = (end - start).clamp(MIN_SPAN as i64, DAY_MINUTES as i64);
    let start = start.clamp(0, DAY_MINUTES as i64 - span);
    ViewRange {
        start: start as u32,
        end: (start + span) as u32,
    }
}

/// The range for a preset. `workday` is the settings bounds, `now_min` the
/// current local minute. "Now" starts an hour back (snapped down), spans five
/// hours and is shifted back inside the day near midnight.
pub fn preset_range(preset: Preset, workday: (u32, u32), now_min: u32) -> ViewRange {
    match preset {
        Preset::Workday => clamp_range(workday.0 as i64, workday.1 as i64),
        Preset::FullDay => ViewRange {
            start: 0,
            end: DAY_MINUTES,
        },
        Preset::Now => {
            let start = snap_floor(now_min as i64 - NOW_LEAD);
            clamp_range(start, start + NOW_SPAN)
        }
    }
}

/// Resolve a remembered view to the concrete range.
pub fn resolve_view(view: RailView, workday: (u32, u32), now_min: u32) -> ViewRange {
    match view {
        RailView::Workday => preset_range(Preset::Workday, workday, now_min),
        RailView::Now => preset_range(Preset::Now, workday, now_min),
        RailView::FullDay => preset_range(Preset::FullDay, workday, now_min),
        RailView::Custom { start, end } => clamp_range(start as i64, end as i64),
    }
}

/// How an edited range is remembered: ranges equal to a static preset
/// collapse back to it (so they keep following the settings), the rest are
/// custom.
pub fn view_from_range(range: ViewRange, workday: (u32, u32)) -> RailView {
    if range == preset_range(Preset::Workday, workday, 0) {
        RailView::Workday
    } else if range == preset_range(Preset::FullDay, workday, 0) {
        RailView::FullDay
    } else {
        RailView::Custom {
            start: range.start,
            end: range.end,
        }
    }
}

/// Which preset chip (if any) is lit for the current range.
pub fn active_preset(range: ViewRange, workday: (u32, u32), now_min: u32) -> Option<Preset> {
    [Preset::Workday, Preset::Now, Preset::FullDay]
        .into_iter()
        .find(|p| preset_range(*p, workday, now_min) == range)
}

/// Shift the range by `delta_min` (snapped), keeping its span.
pub fn pan_by(range: ViewRange, delta_min: i64) -> ViewRange {
    let span = range.span() as i64;
    let start = snap_round(range.start as i64 + delta_min);
    clamp_range(start, start + span)
}

/// Zoom by `factor` (>1 zooms in) keeping `pivot_min` at the same fraction of
/// the range. Result snaps to 15 minutes and respects the span limits.
pub fn zoom_around(range: ViewRange, factor: f64, pivot_min: f64) -> ViewRange {
    if factor <= 0.0 || !factor.is_finite() {
        return range;
    }
    let span = range.span() as f64;
    let new_span = snap_round((span / factor).round() as i64)
        .clamp(MIN_SPAN as i64, DAY_MINUTES as i64);
    let frac = ((pivot_min - range.start as f64) / span).clamp(0.0, 1.0);
    let start = snap_round((pivot_min - frac * new_span as f64).round() as i64);
    clamp_range(start, start + new_span)
}

/// The − / + stepper: zoom about the centre of the range.
pub fn zoom_step(range: ViewRange, zoom_in: bool) -> ViewRange {
    let centre = (range.start + range.end) as f64 / 2.0;
    let factor = if zoom_in { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
    zoom_around(range, factor, centre)
}

/// Which end of the range a handle drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Start,
    End,
}

/// Move one end of the range to `minute` (snapped), keeping the other end and
/// the minimum span.
pub fn resize_edge(range: ViewRange, edge: Edge, minute: i64) -> ViewRange {
    let m = snap_round(minute);
    match edge {
        Edge::Start => {
            let start = m.clamp(0, range.end as i64 - MIN_SPAN as i64);
            ViewRange {
                start: start as u32,
                end: range.end,
            }
        }
        Edge::End => {
            let end = m.clamp(range.start as i64 + MIN_SPAN as i64, DAY_MINUTES as i64);
            ViewRange {
                start: range.start,
                end: end as u32,
            }
        }
    }
}

/// Keyboard grow / shrink: 15 minutes off (or onto) each end.
pub fn resize_symmetric(range: ViewRange, grow: bool) -> ViewRange {
    let d = SNAP as i64;
    let (s, e) = (range.start as i64, range.end as i64);
    if grow {
        clamp_range(s - d, e + d)
    } else if e - s - 2 * d >= MIN_SPAN as i64 {
        clamp_range(s + d, e - d)
    } else {
        range
    }
}

/// `"Fit"` at the workday preset range, else the zoom factor against the
/// workday span (`"2.7×"`, `"0.5×"` when wider).
pub fn zoom_label(range: ViewRange, workday: (u32, u32)) -> String {
    let wd = preset_range(Preset::Workday, workday, 0);
    if range == wd {
        return "Fit".to_string();
    }
    format!("{:.1}\u{d7}", wd.span() as f64 / range.span() as f64)
}

/// Scale for a measured body height. Unmeasured (`<= 0`) falls back to 48px/h.
pub fn px_per_hour(body_px: f64, range: ViewRange) -> f64 {
    if body_px <= 1.0 {
        FALLBACK_PX_PER_HOUR
    } else {
        body_px / range.hours()
    }
}

/// Pixel offset of `minute` inside the rail body.
pub fn minute_to_y(minute: f64, range: ViewRange, pph: f64) -> f64 {
    (minute - range.start as f64) / 60.0 * pph
}

/// Minute under a rail-body pixel offset (fractional, unclamped).
pub fn y_to_minute(y: f64, range: ViewRange, pph: f64) -> f64 {
    range.start as f64 + y / pph * 60.0
}

/// Minutes a vertical pixel delta stands for at this scale.
pub fn px_to_minutes(dy: f64, pph: f64) -> f64 {
    dy / pph * 60.0
}

/// Wheel handling: add `dy` to the pixel accumulator and take out whole
/// 15-minute steps. Returns `(delta_min, new_accumulator)`; the remainder keeps
/// smooth trackpad deltas from being lost to rounding.
pub fn wheel_steps(acc_px: f64, dy: f64, pph: f64) -> (i64, f64) {
    let acc = acc_px + dy;
    let steps = (px_to_minutes(acc, pph) / SNAP as f64).trunc();
    let delta = steps as i64 * SNAP as i64;
    (delta, acc - delta as f64 / 60.0 * pph)
}

/// Ctrl/Cmd + wheel zoom: pixels of wheel travel per zoom step, and the
/// factor one step applies.
pub const ZOOM_WHEEL_PX: f64 = 50.0;
pub const ZOOM_WHEEL_FACTOR: f64 = 1.2;

/// Accumulate wheel travel into whole zoom steps. Positive steps mean the
/// wheel moved down (zoom out); callers apply `FACTOR.powi(-steps)`.
pub fn zoom_wheel_steps(acc_px: f64, dy: f64) -> (i32, f64) {
    let acc = acc_px + dy;
    let steps = (acc / ZOOM_WHEEL_PX).trunc();
    (steps as i32, acc - steps * ZOOM_WHEEL_PX)
}

/// Spacing of hour gridlines so labels never crowd: every hour at >= 28px/h,
/// every 2h at >= 16, otherwise every 3h.
pub fn hour_step(pph: f64) -> u32 {
    if pph >= 28.0 {
        1
    } else if pph >= 16.0 {
        2
    } else {
        3
    }
}

/// Hour marks (in hours) to draw inside the range.
pub fn hour_marks(range: ViewRange, pph: f64) -> Vec<u32> {
    let step = hour_step(pph);
    let first = range.start.div_ceil(60);
    (first..=range.end / 60).filter(|h| h % step == 0).collect()
}

/// A shaded stretch of the range outside the workday.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShadeBand {
    pub start: u32,
    pub end: u32,
    /// True for the band before the workday opens.
    pub before: bool,
}

pub fn shade_bands(range: ViewRange, workday: (u32, u32)) -> Vec<ShadeBand> {
    let mut out = Vec::new();
    if range.start < workday.0 {
        out.push(ShadeBand {
            start: range.start,
            end: range.end.min(workday.0),
            before: true,
        });
    }
    if range.end > workday.1 {
        out.push(ShadeBand {
            start: range.start.max(workday.1),
            end: range.end,
            before: false,
        });
    }
    out.retain(|b| b.end > b.start);
    out
}

/// Where a span sits relative to the visible range (fully outside only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutOfRange {
    Earlier,
    Later,
}

pub fn out_of_range(start_min: i64, end_min: i64, range: ViewRange) -> Option<OutOfRange> {
    if end_min <= range.start as i64 {
        Some(OutOfRange::Earlier)
    } else if start_min >= range.end as i64 {
        Some(OutOfRange::Later)
    } else {
        None
    }
}

/// Counts of spans fully before / after the range.
pub fn out_of_range_counts(spans: &[(i64, i64)], range: ViewRange) -> (usize, usize) {
    spans
        .iter()
        .fold((0, 0), |(e, l), (s, en)| match out_of_range(*s, *en, range) {
            Some(OutOfRange::Earlier) => (e + 1, l),
            Some(OutOfRange::Later) => (e, l + 1),
            None => (e, l),
        })
}

/// Pan target that reveals the nearest span on the given side (with a 15
/// minute margin), keeping the span width. `None` when nothing is there.
pub fn reveal_target(
    spans: &[(i64, i64)],
    range: ViewRange,
    side: OutOfRange,
) -> Option<ViewRange> {
    let span = range.span() as i64;
    match side {
        OutOfRange::Earlier => {
            let (s, _) = spans
                .iter()
                .filter(|(s, e)| out_of_range(*s, *e, range) == Some(OutOfRange::Earlier))
                .max_by_key(|(_, e)| *e)?;
            let start = snap_floor(*s) - SNAP as i64;
            Some(clamp_range(start, start + span))
        }
        OutOfRange::Later => {
            let (s, e) = spans
                .iter()
                .filter(|(s, e)| out_of_range(*s, *e, range) == Some(OutOfRange::Later))
                .min_by_key(|(s, _)| *s)?;
            let end = snap_floor(*e + SNAP as i64 - 1) + SNAP as i64;
            let mut start = end - span;
            if (*s - SNAP as i64) < start {
                start = snap_floor(*s) - SNAP as i64;
            }
            Some(clamp_range(start, start + span))
        }
    }
}

/// Day-strip fraction (0..1) of a minute on the 24h bar.
pub fn strip_frac(minute: f64) -> f64 {
    (minute / DAY_MINUTES as f64).clamp(0.0, 1.0)
}

/// `(left%, width%)` of a span on the day strip. Widths have a floor so a
/// 15-minute block stays a visible tick.
pub fn strip_span(start: i64, end: i64, min_width_pct: f64) -> (f64, f64) {
    let l = strip_frac(start as f64);
    let r = strip_frac(end as f64);
    (l * 100.0, ((r - l) * 100.0).max(min_width_pct))
}

/// What a day-strip drag is doing to the bracket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripDrag {
    Pan,
    Resize(Edge),
}

/// Range after dragging by `dx_px` on a strip `width_px` wide.
pub fn strip_drag_range(kind: StripDrag, orig: ViewRange, dx_px: f64, width_px: f64) -> ViewRange {
    if width_px <= 1.0 {
        return orig;
    }
    let dmin = dx_px / width_px * DAY_MINUTES as f64;
    match kind {
        StripDrag::Pan => pan_by(orig, dmin.round() as i64),
        StripDrag::Resize(Edge::Start) => {
            resize_edge(orig, Edge::Start, orig.start as i64 + dmin.round() as i64)
        }
        StripDrag::Resize(Edge::End) => {
            resize_edge(orig, Edge::End, orig.end as i64 + dmin.round() as i64)
        }
    }
}

/// Centre the range on a clicked strip position.
pub fn strip_click_range(orig: ViewRange, x_px: f64, width_px: f64) -> ViewRange {
    if width_px <= 1.0 {
        return orig;
    }
    let centre = x_px / width_px * DAY_MINUTES as f64;
    let start = snap_round((centre - orig.span() as f64 / 2.0).round() as i64);
    clamp_range(start, start + orig.span() as i64)
}

/// `"09:00 to 17:00"` for assistive tech.
pub fn range_text(range: ViewRange) -> String {
    format!("{} to {}", hhmm(range.start), hhmm(range.end))
}

fn hhmm(m: u32) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

// ── Components ──────────────────────────────────────────────────────────────

/// One tick on the day strip.
#[derive(Clone, Debug, PartialEq)]
pub struct StripTick {
    pub start: i64,
    pub end: i64,
    pub color: String,
}

/// The 24h overview bar: workday band, block ticks, now marker and the
/// draggable bracket for the visible range. `on_change` fires continuously
/// during a drag (cheap local state); the parent decides how to persist.
#[component]
pub fn DayStrip(
    range: ViewRange,
    workday: (u32, u32),
    ticks: Vec<StripTick>,
    now_min: Option<u32>,
    now_color: String,
    on_change: EventHandler<ViewRange>,
) -> Element {
    let mut track: Signal<Option<std::rc::Rc<MountedData>>> = use_signal(|| None);
    // (kind, anchor screen x, original range, track width)
    let mut drag = use_signal(|| Option::<(StripDrag, f64, ViewRange, f64)>::None);

    let wd_l = strip_frac(workday.0 as f64) * 100.0;
    let wd_w = (strip_frac(workday.1 as f64) * 100.0 - wd_l).max(0.0);
    let br_l = strip_frac(range.start as f64) * 100.0;
    let br_w = strip_frac(range.span() as f64) * 100.0;
    let value_text = range_text(range);
    let value_now = range.start;

    let begin = move |kind: StripDrag, anchor_x: f64| {
        let orig = range;
        spawn(async move {
            let m = track.peek().clone();
            let Some(m) = m else { return };
            let width = m.get_client_rect().await.map(|r| r.size.width).unwrap_or(0.0);
            drag.set(Some((kind, anchor_x, orig, width)));
        });
    };

    rsx! {
        div {
            class: "flex flex-col gap-0.5",
            div {
                class: "relative h-7 select-none rounded border border-faint bg-app",
                onmounted: move |evt| track.set(Some(evt.data())),
                onmousedown: move |evt| {
                    let x = evt.data.element_coordinates().x;
                    let ax = evt.data.screen_coordinates().x;
                    let orig = range;
                    spawn(async move {
                        let m = track.peek().clone();
                        let Some(m) = m else { return };
                        let width = m.get_client_rect().await.map(|r| r.size.width).unwrap_or(0.0);
                        let next = strip_click_range(orig, x, width);
                        on_change.call(next);
                        drag.set(Some((StripDrag::Pan, ax, next, width)));
                    });
                },
                // Workday band.
                div {
                    class: "pointer-events-none absolute inset-y-0 bg-input",
                    style: "left: {wd_l}%; width: {wd_w}%;",
                }
                for (i, t) in ticks.iter().enumerate() {
                    {
                        let (l, w) = strip_span(t.start, t.end, 0.5);
                        let c = t.color.clone();
                        rsx! {
                            div {
                                key: "tick-{i}",
                                class: "pointer-events-none absolute rounded-sm",
                                style: "left: {l}%; width: {w}%; top: 15px; height: 9px; background: {c};",
                            }
                        }
                    }
                }
                if let Some(n) = now_min {
                    div {
                        class: "pointer-events-none absolute inset-y-0",
                        style: "left: {strip_frac(n as f64) * 100.0}%; width: 2px; background: {now_color};",
                    }
                }
                // The bracket: body pans, end grips resize.
                div {
                    class: "absolute inset-y-0 cursor-grab rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-primary-300",
                    style: "left: {br_l}%; width: {br_w}%; border: 2px solid {BRACKET_COLOR};",
                    tabindex: "0",
                    role: "slider",
                    "aria-label": "Visible time range",
                    "aria-valuemin": "0",
                    "aria-valuemax": "{DAY_MINUTES}",
                    "aria-valuenow": "{value_now}",
                    "aria-valuetext": "{value_text}",
                    title: "Drag to pan, drag an end to zoom ({value_text})",
                    onmousedown: move |evt| {
                        evt.stop_propagation();
                        begin(StripDrag::Pan, evt.data.screen_coordinates().x);
                    },
                    onkeydown: move |evt| {
                        let shift = evt.data.modifiers().shift();
                        let next = match evt.data.key() {
                            Key::ArrowLeft if shift => resize_symmetric(range, false),
                            Key::ArrowRight if shift => resize_symmetric(range, true),
                            Key::ArrowLeft => pan_by(range, -30),
                            Key::ArrowRight => pan_by(range, 30),
                            Key::Character(ref c) if c == "-" => resize_symmetric(range, true),
                            Key::Character(ref c) if c == "+" || c == "=" => {
                                resize_symmetric(range, false)
                            }
                            _ => return,
                        };
                        evt.prevent_default();
                        evt.stop_propagation();
                        if next != range {
                            on_change.call(next);
                        }
                    },
                    div {
                        class: "absolute inset-y-0 left-0 w-1.5 cursor-ew-resize rounded-l-sm",
                        style: "background: {BRACKET_COLOR};",
                        title: "Drag to change the start",
                        onmousedown: move |evt| {
                            evt.stop_propagation();
                            begin(StripDrag::Resize(Edge::Start), evt.data.screen_coordinates().x);
                        },
                    }
                    div {
                        class: "absolute inset-y-0 right-0 w-1.5 cursor-ew-resize rounded-r-sm",
                        style: "background: {BRACKET_COLOR};",
                        title: "Drag to change the end",
                        onmousedown: move |evt| {
                            evt.stop_propagation();
                            begin(StripDrag::Resize(Edge::End), evt.data.screen_coordinates().x);
                        },
                    }
                }
            }
            div {
                class: "relative h-3 font-mono text-[10px] leading-3 tabular-nums text-fg-muted",
                for h in [0u32, 6, 12, 18, 24] {
                    span {
                        key: "lbl-{h}",
                        class: "absolute",
                        style: if h == 0 {
                            "left: 0;".to_string()
                        } else if h == 24 {
                            "right: 0;".to_string()
                        } else {
                            format!("left: {}%; transform: translateX(-50%);", h as f64 / 24.0 * 100.0)
                        },
                        "{h:02}"
                    }
                }
            }
        }
        // Fullscreen capture while dragging (same convention as the column
        // resizers). A sibling of the strip, never a child.
        if drag.read().is_some() {
            div {
                class: "fixed inset-0 z-50 cursor-ew-resize",
                onmousemove: move |evt| {
                    let d = *drag.peek();
                    let Some((kind, ax, orig, width)) = d else { return };
                    let dx = evt.data.screen_coordinates().x - ax;
                    let next = strip_drag_range(kind, orig, dx, width);
                    on_change.call(next);
                },
                onmouseup: move |_| drag.set(None),
                onmouseleave: move |_| drag.set(None),
            }
        }
    }
}

/// Preset chips and the − / + zoom stepper.
#[component]
pub fn RangeControls(
    range: ViewRange,
    workday: (u32, u32),
    now_min: u32,
    on_preset: EventHandler<Preset>,
    on_zoom: EventHandler<bool>,
) -> Element {
    let active = active_preset(range, workday, now_min);
    let label = zoom_label(range, workday);
    let chip = |on: bool| {
        if on {
            "rounded-full border border-subtle bg-btn-primary px-2 py-0.5 text-[11px] text-fg"
        } else {
            "rounded-full border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input"
        }
    };
    let can_out = range.span() < DAY_MINUTES;
    let can_in = range.span() > MIN_SPAN;
    rsx! {
        div {
            class: "flex flex-wrap items-center gap-1.5",
            button {
                class: chip(active == Some(Preset::Workday)),
                title: "Show your workday, fitted to the rail",
                onclick: move |_| on_preset.call(Preset::Workday),
                "Workday"
            }
            button {
                class: chip(active == Some(Preset::Now)),
                title: "From an hour ago, five hours ahead",
                onclick: move |_| on_preset.call(Preset::Now),
                "Now"
            }
            button {
                class: chip(active == Some(Preset::FullDay)),
                title: "Show all 24 hours",
                onclick: move |_| on_preset.call(Preset::FullDay),
                "Full day"
            }
            div { class: "flex-1" }
            button {
                class: "h-6 w-6 shrink-0 rounded border border-faint text-[11px] text-fg hover:bg-input disabled:opacity-40",
                title: "Zoom out",
                disabled: !can_out,
                onclick: move |_| on_zoom.call(false),
                "\u{2212}"
            }
            span {
                class: "w-9 text-center font-mono text-[11px] tabular-nums text-fg-muted",
                title: "Zoom compared with your workday",
                "{label}"
            }
            button {
                class: "h-6 w-6 shrink-0 rounded border border-faint text-[11px] text-fg hover:bg-input disabled:opacity-40",
                title: "Zoom in",
                disabled: !can_in,
                onclick: move |_| on_zoom.call(true),
                "+"
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const WD: (u32, u32) = (540, 1020);

    fn r(s: u32, e: u32) -> ViewRange {
        ViewRange { start: s, end: e }
    }

    #[test]
    fn presets() {
        assert_eq!(preset_range(Preset::Workday, WD, 600), r(540, 1020));
        assert_eq!(preset_range(Preset::FullDay, WD, 600), r(0, 1440));
        // 10:07 -> start 09:00 (snapped down from 09:07), 5h span.
        assert_eq!(preset_range(Preset::Now, WD, 10 * 60 + 7), r(540, 840));
        // Near midnight the window slides back inside the day.
        assert_eq!(preset_range(Preset::Now, WD, 23 * 60 + 30), r(1140, 1440));
        // Early morning clamps at 00:00.
        assert_eq!(preset_range(Preset::Now, WD, 20), r(0, 300));
    }

    #[test]
    fn tiny_workday_widens_to_min_span() {
        let rr = preset_range(Preset::Workday, (600, 630), 0);
        assert_eq!(rr.span(), MIN_SPAN);
    }

    #[test]
    fn clamp_limits() {
        assert_eq!(clamp_range(-60, 60), r(0, 120));
        assert_eq!(clamp_range(1400, 1500), r(1320, 1440));
        assert_eq!(clamp_range(100, 5000), r(0, 1440));
        assert_eq!(clamp_range(600, 650), r(600, 720));
    }

    #[test]
    fn pan_snaps_and_clamps() {
        assert_eq!(pan_by(r(540, 1020), 30), r(570, 1050));
        assert_eq!(pan_by(r(540, 1020), 7), r(540, 1020));
        assert_eq!(pan_by(r(540, 1020), 9), r(555, 1035));
        assert_eq!(pan_by(r(60, 300), -600), r(0, 240));
        assert_eq!(pan_by(r(1200, 1440), 600), r(1200, 1440));
    }

    #[test]
    fn zoom_around_pivot() {
        // Zoom in 2x about the start keeps the start fixed.
        assert_eq!(zoom_around(r(540, 1020), 2.0, 540.0), r(540, 780));
        // About the centre.
        assert_eq!(zoom_around(r(540, 1020), 2.0, 780.0), r(660, 900));
        // Zoom out beyond the day clamps to full.
        assert_eq!(zoom_around(r(540, 1020), 0.1, 780.0), r(0, 1440));
        // Zoom in never narrower than 2h.
        assert_eq!(zoom_around(r(540, 700), 4.0, 600.0).span(), MIN_SPAN);
        assert_eq!(zoom_around(r(540, 1020), 0.0, 600.0), r(540, 1020));
    }

    #[test]
    fn stepper_zooms_about_centre() {
        let z = zoom_step(r(540, 1020), true);
        // 480 / 1.5 = 320 snaps to 315; centred on 13:00.
        assert_eq!(z, r(630, 945));
        let o = zoom_step(r(540, 1020), false);
        assert_eq!(o.span(), 720);
    }

    #[test]
    fn labels() {
        assert_eq!(zoom_label(r(540, 1020), WD), "Fit");
        assert_eq!(zoom_label(r(540, 780), WD), "2.0\u{d7}");
        assert_eq!(zoom_label(r(0, 1440), WD), "0.3\u{d7}");
        assert_eq!(zoom_label(r(600, 1080), WD), "1.0\u{d7}");
        assert_eq!(zoom_label(r(540, 840), WD), "1.6\u{d7}");
    }

    #[test]
    fn active_chip() {
        assert_eq!(active_preset(r(540, 1020), WD, 600), Some(Preset::Workday));
        assert_eq!(active_preset(r(0, 1440), WD, 600), Some(Preset::FullDay));
        assert_eq!(active_preset(r(540, 840), WD, 10 * 60 + 7), Some(Preset::Now));
        assert_eq!(active_preset(r(100, 400), WD, 600), None);
    }

    #[test]
    fn remembered_view_roundtrip() {
        assert_eq!(view_from_range(r(540, 1020), WD), RailView::Workday);
        assert_eq!(view_from_range(r(0, 1440), WD), RailView::FullDay);
        assert_eq!(
            view_from_range(r(480, 720), WD),
            RailView::Custom { start: 480, end: 720 }
        );
        assert_eq!(resolve_view(RailView::Custom { start: 480, end: 720 }, WD, 0), r(480, 720));
        assert_eq!(resolve_view(RailView::Workday, WD, 0), r(540, 1020));
        let json = serde_json::to_string(&RailView::Custom { start: 1, end: 2 }).unwrap();
        assert_eq!(serde_json::from_str::<RailView>(&json).unwrap(), RailView::Custom { start: 1, end: 2 });
    }

    #[test]
    fn edge_resize_and_keys() {
        assert_eq!(resize_edge(r(540, 1020), Edge::End, 1000), r(540, 1005));
        assert_eq!(resize_edge(r(540, 1020), Edge::End, 600), r(540, 660));
        assert_eq!(resize_edge(r(540, 1020), Edge::Start, 0), r(0, 1020));
        assert_eq!(resize_edge(r(540, 1020), Edge::Start, 1000), r(900, 1020));
        assert_eq!(resize_symmetric(r(540, 1020), true), r(525, 1035));
        assert_eq!(resize_symmetric(r(540, 1020), false), r(555, 1005));
        assert_eq!(resize_symmetric(r(540, 660), false), r(540, 660));
        assert_eq!(resize_symmetric(r(0, 1440), true), r(0, 1440));
    }

    #[test]
    fn pixel_minute_conversion() {
        let rr = r(540, 1020);
        let pph = px_per_hour(720.0, rr); // 90
        assert_eq!(pph, 90.0);
        assert_eq!(minute_to_y(600.0, rr, pph), 90.0);
        assert_eq!(y_to_minute(90.0, rr, pph), 600.0);
        assert_eq!(px_to_minutes(45.0, pph), 30.0);
        assert_eq!(px_per_hour(0.0, rr), FALLBACK_PX_PER_HOUR);
        let full = r(0, 1440);
        assert_eq!(px_per_hour(720.0, full), 30.0);
    }

    #[test]
    fn wheel_accumulates_small_deltas() {
        let (d, acc) = wheel_steps(0.0, 5.0, 90.0);
        assert_eq!(d, 0);
        let (d, acc) = wheel_steps(acc, 18.0, 90.0); // 23px -> 15.3 min
        assert_eq!(d, 15);
        assert!((acc - (23.0 - 22.5)).abs() < 1e-9);
        assert_eq!(wheel_steps(0.0, -90.0, 90.0).0, -60);
        assert_eq!(wheel_steps(0.0, 90.0, 30.0).0, 180);
    }

    #[test]
    fn wheel_zoom_steps() {
        assert_eq!(zoom_wheel_steps(0.0, 20.0), (0, 20.0));
        assert_eq!(zoom_wheel_steps(20.0, 40.0), (1, 10.0));
        assert_eq!(zoom_wheel_steps(0.0, -120.0), (-2, -20.0));
    }

    #[test]
    fn hour_marks_thin_out() {
        assert_eq!(hour_step(90.0), 1);
        assert_eq!(hour_step(20.0), 2);
        assert_eq!(hour_step(10.0), 3);
        assert_eq!(hour_marks(r(540, 780), 90.0), vec![9, 10, 11, 12, 13]);
        assert_eq!(hour_marks(r(0, 1440), 10.0), vec![0, 3, 6, 9, 12, 15, 18, 21, 24]);
    }

    #[test]
    fn shading() {
        assert_eq!(shade_bands(r(540, 1020), WD), vec![]);
        assert_eq!(
            shade_bands(r(0, 1440), WD),
            vec![
                ShadeBand { start: 0, end: 540, before: true },
                ShadeBand { start: 1020, end: 1440, before: false }
            ]
        );
        assert_eq!(
            shade_bands(r(600, 1200), WD),
            vec![ShadeBand { start: 1020, end: 1200, before: false }]
        );
        assert_eq!(
            shade_bands(r(60, 300), WD),
            vec![ShadeBand { start: 60, end: 300, before: true }]
        );
        assert_eq!(
            shade_bands(r(1100, 1400), WD),
            vec![ShadeBand { start: 1100, end: 1400, before: false }]
        );
    }

    #[test]
    fn earlier_later() {
        let rng = r(540, 1020);
        let spans = [(420, 540), (480, 600), (1020, 1080), (1100, 1160), (600, 660)];
        assert_eq!(out_of_range_counts(&spans, rng), (1, 2));
        assert_eq!(out_of_range(480, 600, rng), None);
        // Earlier reveal: block 07:00-09:00 becomes visible with a margin.
        let t = reveal_target(&spans, rng, OutOfRange::Earlier).unwrap();
        assert_eq!(t, r(405, 885));
        assert!(t.start as i64 <= 420);
        // Later reveal: nearest later block 17:00-18:00.
        let t = reveal_target(&spans, rng, OutOfRange::Later).unwrap();
        assert!(t.end >= 1080 && t.start <= 1020);
        assert_eq!(t.span(), 480);
        assert_eq!(reveal_target(&[(600, 660)], rng, OutOfRange::Later), None);
    }

    #[test]
    fn strip_positions() {
        assert_eq!(strip_frac(720.0), 0.5);
        assert_eq!(strip_span(720, 1440, 0.5), (50.0, 50.0));
        let (l, w) = strip_span(0, 5, 0.5);
        assert_eq!((l, w), (0.0, 0.5));
    }

    #[test]
    fn strip_drags() {
        let orig = r(540, 1020);
        // 1440 min over 288px = 5 min/px: 12px = 60 min.
        assert_eq!(strip_drag_range(StripDrag::Pan, orig, 12.0, 288.0), r(600, 1080));
        assert_eq!(strip_drag_range(StripDrag::Pan, orig, -1000.0, 288.0), r(0, 480));
        assert_eq!(
            strip_drag_range(StripDrag::Resize(Edge::End), orig, 12.0, 288.0),
            r(540, 1080)
        );
        assert_eq!(
            strip_drag_range(StripDrag::Resize(Edge::Start), orig, -12.0, 288.0),
            r(480, 1020)
        );
        assert_eq!(strip_drag_range(StripDrag::Pan, orig, 10.0, 0.0), orig);
        // Click centres the bracket.
        assert_eq!(strip_click_range(orig, 144.0, 288.0), r(480, 960));
    }

    #[test]
    fn range_text_is_hhmm() {
        assert_eq!(range_text(r(540, 1020)), "09:00 to 17:00");
    }
}
