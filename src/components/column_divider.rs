#![allow(non_snake_case)]
//! One divider for every resizable column.
//!
//! At rest it's a 1px hairline centred in an invisible 8px grab area that
//! takes no layout width (negative margins), so neighbouring columns abut.
//! Hover (after ~120ms) lights the line and shows a grip pill; dragging
//! thickens the line, fills the grip and shows a live "312 px" readout.
//! Double-click resets to the default; the grip is a keyboard-operable
//! `role="separator"` (←/→ 16px, Shift 64px, Enter resets).
//!
//! The divider never owns the width. It reports through `on_resize` (live,
//! during a drag) and `on_commit` (drag end, keyboard step, reset) — persist
//! only on commit. Styling lives in `tailwind.css` (`.col-divider*`).
//!
//! Columns that can snap to a rail (the tab sidebar) pass `rail`: dragging
//! below `rail.below` reports `railed: true` while keeping the full width,
//! and dragging back out restores that full width.

use dioxus::prelude::*;
use std::time::{Duration, Instant};

/// Which side of the divider the resized column sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnSide {
    /// Column is left of the divider: dragging right grows it.
    Left,
    /// Column is right of the divider: dragging left grows it (the Planner
    /// timeline).
    Right,
}

impl ColumnSide {
    /// Pointer/key movement (positive = rightward) as growth of the column.
    pub fn growth(self, dx: f64) -> f64 {
        match self {
            ColumnSide::Left => dx,
            ColumnSide::Right => -dx,
        }
    }
}

/// Optional snap-to-rail behaviour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RailSnap {
    /// Dragging the column narrower than this snaps it to the rail.
    pub below: f64,
    /// The rail's width (what the column shows while railed).
    pub rail_width: f64,
}

/// What the divider reports. `width` is always the column's *full* width
/// (clamped to `min..=max`); when `railed`, the column shows its rail and
/// `width` is the width to restore when it leaves the rail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnWidth {
    pub width: f64,
    pub railed: bool,
}

/// Size limits for one column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DividerBounds {
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub rail: Option<RailSnap>,
}

impl DividerBounds {
    pub fn clamp(&self, w: f64) -> f64 {
        w.clamp(self.min, self.max.max(self.min))
    }

    /// The width the column currently shows.
    pub fn visual(&self, current: ColumnWidth) -> f64 {
        match (current.railed, self.rail) {
            (true, Some(r)) => r.rail_width,
            _ => current.width,
        }
    }

    pub fn reset(&self) -> ColumnWidth {
        ColumnWidth { width: self.clamp(self.default), railed: false }
    }

    /// Resolve a drag: `start` is the state at mousedown, `growth` how far
    /// the pointer has moved in the column-growing direction.
    pub fn resolve_drag(&self, start: ColumnWidth, growth: f64) -> ColumnWidth {
        let raw = self.visual(start) + growth;
        if let Some(r) = self.rail {
            if raw < r.below {
                return ColumnWidth { width: self.clamp(start.width), railed: true };
            }
        }
        // Leaving the rail pops straight back to the last full width, then
        // follows the pointer if it keeps going.
        let w = if start.railed && self.rail.is_some() { raw.max(start.width) } else { raw };
        ColumnWidth { width: self.clamp(w), railed: false }
    }

    /// One keyboard step: `step` px in the growing direction (negative
    /// shrinks). Stepping out of the rail restores the full width at once.
    pub fn key_step(&self, current: ColumnWidth, step: f64) -> ColumnWidth {
        if current.railed && self.rail.is_some() && step > 0.0 {
            return ColumnWidth { width: self.clamp(current.width), railed: false };
        }
        self.resolve_drag(current, step)
    }
}

/// Keyboard step size: 16px, or 64px with Shift.
pub fn key_step_px(shift: bool) -> f64 {
    if shift {
        64.0
    } else {
        16.0
    }
}

/// Two no-move clicks this close together count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Props, Clone, PartialEq)]
pub struct ColumnDividerProps {
    /// The column's full width (the width to restore while railed). Pass the
    /// *effective* width when a layout solver has shrunk the column, so a
    /// drag starts from what's on screen.
    pub width: f64,
    /// The column is currently showing its rail.
    #[props(default)]
    pub railed: bool,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub side: ColumnSide,
    /// Accessible name, e.g. "Resize tab sidebar".
    pub label: String,
    #[props(default)]
    pub rail: Option<RailSnap>,
    /// Live updates while dragging.
    #[props(default)]
    pub on_resize: Option<EventHandler<ColumnWidth>>,
    /// Fired at drag end, on each keyboard step and on reset — persist here.
    pub on_commit: EventHandler<ColumnWidth>,
}

pub fn ColumnDivider(props: ColumnDividerProps) -> Element {
    let bounds = DividerBounds {
        min: props.min,
        max: props.max,
        default: props.default,
        rail: props.rail,
    };
    let current = ColumnWidth { width: props.width, railed: props.railed };
    // (pointer start x, state at mousedown)
    let mut drag = use_signal(|| None::<(f64, ColumnWidth)>);
    let mut live = use_signal(|| None::<ColumnWidth>);
    let mut last_click = use_signal(|| None::<Instant>);

    let dragging = drag.read().is_some();
    let shown = live.read().unwrap_or(current);
    let readout = bounds.visual(shown).round() as i64;
    let side = props.side;
    let on_resize = props.on_resize;
    let on_commit = props.on_commit;

    let reset = move || {
        let r = bounds.reset();
        if let Some(cb) = on_resize {
            cb.call(r);
        }
        on_commit.call(r);
    };

    let mut finish = move || {
        let Some(_) = drag.take() else { return };
        match live.take() {
            Some(final_w) => {
                last_click.set(None);
                on_commit.call(final_w);
            }
            None => {
                // A click without movement: two in quick succession reset.
                let now = Instant::now();
                let is_double = last_click
                    .peek()
                    .is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK);
                if is_double {
                    last_click.set(None);
                    reset();
                } else {
                    last_click.set(Some(now));
                }
            }
        }
    };

    let readout_side = match side {
        ColumnSide::Left => "right",
        ColumnSide::Right => "left",
    };
    let (vmin, vmax) = match bounds.rail {
        Some(r) => (r.rail_width, bounds.max),
        None => (bounds.min, bounds.max),
    };

    rsx! {
        div {
            class: "col-divider",
            "data-state": if dragging { "dragging" } else { "idle" },
            onmousedown: move |evt| {
                evt.prevent_default();
                drag.set(Some((evt.data.screen_coordinates().x, current)));
                live.set(None);
            },
            div { class: "col-divider-line" }
            div {
                class: "col-divider-grip",
                tabindex: "0",
                role: "separator",
                "aria-orientation": "vertical",
                "aria-label": "{props.label}",
                "aria-valuenow": "{bounds.visual(current).round() as i64}",
                "aria-valuemin": "{vmin.round() as i64}",
                "aria-valuemax": "{vmax.round() as i64}",
                title: "{props.label}",
                onkeydown: move |evt| {
                    let dir = match evt.key() {
                        Key::ArrowLeft => -1.0,
                        Key::ArrowRight => 1.0,
                        Key::Enter => {
                            evt.prevent_default();
                            reset();
                            return;
                        }
                        _ => return,
                    };
                    evt.prevent_default();
                    let step = side.growth(dir * key_step_px(evt.modifiers().shift()));
                    let next = bounds.key_step(current, step);
                    if next != current {
                        if let Some(cb) = on_resize {
                            cb.call(next);
                        }
                        on_commit.call(next);
                    }
                },
                span {}
                span {}
                span {}
            }
            if dragging {
                div {
                    class: "col-divider-readout",
                    "data-side": readout_side,
                    "{readout} px"
                }
                div {
                    class: "col-divider-overlay",
                    onmousemove: move |evt| {
                        let Some((start_x, start)) = *drag.peek() else { return };
                        let dx = evt.data.screen_coordinates().x - start_x;
                        // Ignore sub-pixel jitter so a click stays a click.
                        if live.peek().is_none() && dx.abs() < 2.0 {
                            return;
                        }
                        let next = bounds.resolve_drag(start, side.growth(dx));
                        if *live.peek() != Some(next) {
                            live.set(Some(next));
                            if let Some(cb) = on_resize {
                                cb.call(next);
                            }
                        }
                    },
                    onmouseup: move |_| finish(),
                    onmouseleave: move |_| finish(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: DividerBounds = DividerBounds { min: 420.0, max: 800.0, default: 420.0, rail: None };
    const SIDEBAR: DividerBounds = DividerBounds {
        min: 180.0,
        max: 400.0,
        default: 256.0,
        rail: Some(RailSnap { below: 140.0, rail_width: 48.0 }),
    };

    fn full(w: f64) -> ColumnWidth {
        ColumnWidth { width: w, railed: false }
    }
    fn railed(w: f64) -> ColumnWidth {
        ColumnWidth { width: w, railed: true }
    }

    #[test]
    fn side_maps_pointer_motion_to_growth() {
        assert_eq!(ColumnSide::Left.growth(10.0), 10.0);
        assert_eq!(ColumnSide::Right.growth(10.0), -10.0);
    }

    #[test]
    fn drag_clamps_to_range() {
        assert_eq!(PLAIN.resolve_drag(full(500.0), 1000.0), full(800.0));
        assert_eq!(PLAIN.resolve_drag(full(500.0), -1000.0), full(420.0));
        assert_eq!(PLAIN.resolve_drag(full(500.0), 12.0), full(512.0));
    }

    #[test]
    fn plain_column_never_rails() {
        assert_eq!(PLAIN.resolve_drag(full(420.0), -400.0), full(420.0));
        assert_eq!(PLAIN.key_step(full(420.0), -64.0), full(420.0));
    }

    #[test]
    fn sidebar_snaps_to_rail_below_threshold_keeping_full_width() {
        // 256 − 100 = 156: above the snap line, clamped to the 180 floor.
        assert_eq!(SIDEBAR.resolve_drag(full(256.0), -100.0), full(180.0));
        // 256 − 117 = 139: below 140 → rail, remembering 256.
        assert_eq!(SIDEBAR.resolve_drag(full(256.0), -117.0), railed(256.0));
        // Exactly at the threshold doesn't snap.
        assert_eq!(SIDEBAR.resolve_drag(full(256.0), -116.0), full(180.0));
    }

    #[test]
    fn dragging_out_of_rail_restores_last_full_width() {
        // From the 48px rail, the pointer must cross 140 to leave it.
        assert_eq!(SIDEBAR.resolve_drag(railed(300.0), 50.0), railed(300.0));
        assert_eq!(SIDEBAR.resolve_drag(railed(300.0), 100.0), full(300.0));
        // …then follows the pointer past the restored width.
        assert_eq!(SIDEBAR.resolve_drag(railed(300.0), 272.0), full(320.0));
        assert_eq!(SIDEBAR.resolve_drag(railed(300.0), 500.0), full(400.0));
    }

    #[test]
    fn keyboard_steps() {
        assert_eq!(key_step_px(false), 16.0);
        assert_eq!(key_step_px(true), 64.0);
        assert_eq!(PLAIN.key_step(full(500.0), 16.0), full(516.0));
        assert_eq!(PLAIN.key_step(full(500.0), -64.0), full(436.0));
        assert_eq!(PLAIN.key_step(full(790.0), 16.0), full(800.0));
        // 180 − 16 = 164 stays at the floor; Shift (−64 → 116) rails.
        assert_eq!(SIDEBAR.key_step(full(180.0), -16.0), full(180.0));
        assert_eq!(SIDEBAR.key_step(full(180.0), -64.0), railed(180.0));
        // Any growing step leaves the rail at the remembered width.
        assert_eq!(SIDEBAR.key_step(railed(300.0), 16.0), full(300.0));
        // Shrinking a rail does nothing.
        assert_eq!(SIDEBAR.key_step(railed(300.0), -16.0), railed(300.0));
    }

    #[test]
    fn reset_and_visual() {
        assert_eq!(SIDEBAR.reset(), full(256.0));
        assert_eq!(SIDEBAR.visual(railed(300.0)), 48.0);
        assert_eq!(SIDEBAR.visual(full(300.0)), 300.0);
        assert_eq!(PLAIN.visual(railed(500.0)), 500.0);
        let odd = DividerBounds { min: 100.0, max: 200.0, default: 999.0, rail: None };
        assert_eq!(odd.reset(), full(200.0));
    }
}
