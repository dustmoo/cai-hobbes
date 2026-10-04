//! Original pixel-art icons implementing `dioxus_free_icons::IconShape`, so
//! they drop into `Icon {}` and `ChatBarIconButton` beside the feather set.
//! Deliberately not a copy of any existing character or sprite.
//!
//! The bow-tie tiger ([`TIGER_GRID`]) is the app mascot: `HobbesTiger` is the
//! colour mark used in the chat bar, Fleet tab and Fleet header,
//! `HobbesTigerStencil` the currentColor silhouette for tiny chips, and
//! [`tiger_rgba`] rasterises the same grid for the system tray at runtime
//! (no PNG asset). All three read the one grid — the stencil is derived from
//! it, never drawn separately.

use dioxus::prelude::*;
use dioxus_free_icons::IconShape;

/// The mascot: a bow-tie tiger on a 16×16 grid, row-major top to bottom.
///
/// One source of truth for every rendering of the mark — the colour SVG
/// ([`HobbesTiger`]), the menu-bar stencil ([`HobbesTigerStencil`]) and the
/// runtime rasteriser ([`tiger_rgba`]) all read these rows. The stencil is
/// *derived*, never drawn separately: ink is `{o, c, b}`, holes are
/// `{s, e, '.'}`, so stripes and eyes punch through the template silhouette.
///
/// Original design. Not a copy of any existing character or sprite.
pub const TIGER_GRID: [&str; 16] = [
    "................",
    "..oo........oo..",
    ".osoo......ooso.",
    ".oooooooooooooo.",
    ".ossoooooooosso.",
    ".oooeeooooeeooo.",
    ".oooeeooooeeooo.",
    ".ossoccssccosso.",
    ".ooocccsscccooo.",
    ".ossccsccsccsso.",
    "...ooccccccoo...",
    "................",
    "...bb..bb..bb...",
    "...bbbbbbbbbb...",
    "...bb..bb..bb...",
    "................",
];

/// One cell of [`TIGER_GRID`], classified for both the colour and the
/// stencil renderings.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TigerCell {
    /// Orange body.
    Body,
    /// Cream muzzle.
    Muzzle,
    /// Bow tie.
    Bow,
    /// Dark stripe.
    Stripe,
    /// Dark eye.
    Eye,
    /// Nothing drawn.
    Empty,
}

impl TigerCell {
    /// Classify one grid character. Unknown characters read as [`Self::Empty`].
    pub fn from_char(c: char) -> Self {
        match c {
            'o' => Self::Body,
            'c' => Self::Muzzle,
            'b' => Self::Bow,
            's' => Self::Stripe,
            'e' => Self::Eye,
            _ => Self::Empty,
        }
    }

    /// True for the cells that form the menu-bar silhouette.
    pub fn is_ink(self) -> bool {
        matches!(self, Self::Body | Self::Muzzle | Self::Bow)
    }

    /// Palette colour as `#RRGGBB`, or `None` for [`Self::Empty`].
    pub fn hex(self) -> Option<&'static str> {
        match self {
            Self::Body => Some("#E8913A"),
            Self::Muzzle => Some("#F6E7C8"),
            Self::Bow => Some("#C2412F"),
            Self::Stripe => Some("#3A2A1E"),
            Self::Eye => Some("#241A12"),
            Self::Empty => None,
        }
    }

    /// Palette colour as opaque RGBA, or a fully transparent pixel for
    /// [`Self::Empty`].
    pub fn rgba(self) -> [u8; 4] {
        match self {
            Self::Body => [0xE8, 0x91, 0x3A, 255],
            Self::Muzzle => [0xF6, 0xE7, 0xC8, 255],
            Self::Bow => [0xC2, 0x41, 0x2F, 255],
            Self::Stripe => [0x3A, 0x2A, 0x1E, 255],
            Self::Eye => [0x24, 0x1A, 0x12, 255],
            Self::Empty => [0, 0, 0, 0],
        }
    }
}

/// Eyes closed: the stop-control mascot's blink frame (see
/// [`crate::components::chat_input`]'s animated tiger). Differs from
/// [`TIGER_GRID`] only in the two eye rows — lids down (`o`, fur) over a
/// thin closed line (`s`) — everything else, including the array's shape,
/// stays byte-identical so a diff against `TIGER_GRID` shows exactly the
/// change that matters.
pub const TIGER_GRID_BLINK: [&str; 16] = [
    "................",
    "..oo........oo..",
    ".osoo......ooso.",
    ".oooooooooooooo.",
    ".ossoooooooosso.",
    ".oooooooooooooo.",
    ".ooossoooossooo.",
    ".ossoccssccosso.",
    ".ooocccsscccooo.",
    ".ossccsccsccsso.",
    "...ooccccccoo...",
    "................",
    "...bb..bb..bb...",
    "...bbbbbbbbbb...",
    "...bb..bb..bb...",
    "................",
];

/// Ears perked: the stop-control mascot's alert frame. Adds a stripe accent
/// flanking each ear tip (row 1) — everything else matches [`TIGER_GRID`].
pub const TIGER_GRID_EARS_UP: [&str; 16] = [
    "................",
    ".soo........oos.",
    ".osoo......ooso.",
    ".oooooooooooooo.",
    ".ossoooooooosso.",
    ".oooeeooooeeooo.",
    ".oooeeooooeeooo.",
    ".ossoccssccosso.",
    ".ooocccsscccooo.",
    ".ossccsccsccsso.",
    "...ooccccccoo...",
    "................",
    "...bb..bb..bb...",
    "...bbbbbbbbbb...",
    "...bb..bb..bb...",
    "................",
];

/// Walk a 16×16 grid as `(x, y, cell)`, skipping empty cells. Generic over
/// the grid so [`HobbesTiger`]/[`HobbesTigerStencil`] (fixed to
/// [`TIGER_GRID`]) and [`HobbesTigerFrame`] (any grid, for the animated
/// mascot) share one walk instead of duplicating it per frame.
fn cells_of(grid: &'static [&'static str; 16]) -> impl Iterator<Item = (usize, usize, TigerCell)> {
    grid.iter().enumerate().flat_map(|(y, row)| {
        row.chars()
            .enumerate()
            .map(move |(x, c)| (x, y, TigerCell::from_char(c)))
            .filter(|(_, _, cell)| *cell != TigerCell::Empty)
    })
}

/// [`TIGER_GRID`] walked as `(x, y, cell)`, skipping empty cells.
fn tiger_cells() -> impl Iterator<Item = (usize, usize, TigerCell)> {
    cells_of(&TIGER_GRID)
}

/// The colour mascot: every non-empty cell as a 1×1 rect carrying its own
/// palette `fill`, so stripes and eyes are drawn dark rather than left as
/// holes. Original design, not a copy of any existing character.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct HobbesTiger;

impl IconShape for HobbesTiger {
    fn view_box(&self) -> &str {
        "0 0 16 16"
    }
    fn xmlns(&self) -> &str {
        "http://www.w3.org/2000/svg"
    }
    fn fill_and_stroke<'a>(&self, user_color: &'a str) -> (&'a str, &'a str, &'a str) {
        // The svg-level fill is the caller's colour; every rect below sets an
        // explicit `fill` of its own, which overrides the inherited value.
        (user_color, "none", "0")
    }
    fn child_elements(&self) -> Element {
        rsx! {
            for (x, y, cell) in tiger_cells() {
                rect {
                    key: "{x}-{y}",
                    x: "{x}",
                    y: "{y}",
                    width: "1",
                    height: "1",
                    fill: "{cell.hex().unwrap_or(\"none\")}",
                }
            }
        }
    }
}

/// The menu-bar stencil: the same grid in currentColor, ink cells only, so
/// the stripes, eyes and background read as holes in the silhouette.
///
/// Used where the mark is too small for colour to help (10px chips); the
/// menu bar takes the same silhouette through [`tiger_rgba`].
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct HobbesTigerStencil;

impl IconShape for HobbesTigerStencil {
    fn view_box(&self) -> &str {
        "0 0 16 16"
    }
    fn xmlns(&self) -> &str {
        "http://www.w3.org/2000/svg"
    }
    fn fill_and_stroke<'a>(&self, user_color: &'a str) -> (&'a str, &'a str, &'a str) {
        (user_color, "none", "0")
    }
    fn child_elements(&self) -> Element {
        rsx! {
            for (x, y, _) in tiger_cells().filter(|(_, _, c)| c.is_ink()) {
                rect {
                    key: "{x}-{y}",
                    x: "{x}",
                    y: "{y}",
                    width: "1",
                    height: "1",
                }
            }
        }
    }
}

/// The colour mascot rendered from an arbitrary grid — the same drawing as
/// [`HobbesTiger`], generalised so the stop-control mascot can cycle between
/// [`TIGER_GRID`], [`TIGER_GRID_BLINK`] and [`TIGER_GRID_EARS_UP`] as real
/// sprite-frame swaps rather than a CSS transform on one fixed icon.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct HobbesTigerFrame(pub &'static [&'static str; 16]);

impl IconShape for HobbesTigerFrame {
    fn view_box(&self) -> &str {
        "0 0 16 16"
    }
    fn xmlns(&self) -> &str {
        "http://www.w3.org/2000/svg"
    }
    fn fill_and_stroke<'a>(&self, user_color: &'a str) -> (&'a str, &'a str, &'a str) {
        (user_color, "none", "0")
    }
    fn child_elements(&self) -> Element {
        rsx! {
            for (x, y, cell) in cells_of(self.0) {
                rect {
                    key: "{x}-{y}",
                    x: "{x}",
                    y: "{y}",
                    width: "1",
                    height: "1",
                    fill: "{cell.hex().unwrap_or(\"none\")}",
                }
            }
        }
    }
}

/// A pixel speech bubble on a 16×15 grid, drawn to sit beside the tiger in
/// the empty-chat state: `#` is the outline, `1`–`3` are the three 2×2
/// "typing" dots (numbered so each can take its own animation delay), and the
/// tail at the bottom-left points back toward the mascot.
pub const CHAT_BUBBLE_GRID: [&str; 15] = [
    "..############..",
    ".#............#.",
    "#..............#",
    "#..............#",
    "#..............#",
    "#...11.22.33...#",
    "#...11.22.33...#",
    "#..............#",
    "#..............#",
    "#..............#",
    "#..............#",
    ".#............#.",
    "..##.#########..",
    "...#.#..........",
    "..###...........",
];

/// [`CHAT_BUBBLE_GRID`] as an inline svg. The outline takes `currentColor`
/// (so a `text-*` class tints it for either theme); the dots are the tiger's
/// body orange and, with `animate`, apply `.bubble-typing-dot` for a static
/// soft placeholder style (main.css).
///
/// Not an [`IconShape`]: the dots need per-cell classes.
#[component]
pub fn PixelChatBubble(
    width: u32,
    height: u32,
    #[props(default = true)] animate: bool,
) -> Element {
    let dot_fill = TigerCell::Body.hex().unwrap_or("currentColor");
    let cells = CHAT_BUBBLE_GRID.iter().enumerate().flat_map(|(y, row)| {
        row.chars()
            .enumerate()
            .filter(|(_, c)| *c != '.')
            .map(move |(x, c)| (x, y, c))
    });
    rsx! {
        svg {
            width: "{width}",
            height: "{height}",
            view_box: "0 0 16 15",
            xmlns: "http://www.w3.org/2000/svg",
            shape_rendering: "crispEdges",
            "aria-hidden": "true",
            for (x, y, c) in cells {
                if c.is_ascii_digit() {
                    rect {
                        key: "{x}-{y}",
                        x: "{x}",
                        y: "{y}",
                        width: "1",
                        height: "1",
                        fill: "{dot_fill}",
                        class: if animate { "bubble-typing-dot" } else { "" },
                    }
                } else {
                    rect {
                        key: "{x}-{y}",
                        x: "{x}",
                        y: "{y}",
                        width: "1",
                        height: "1",
                        fill: "currentColor",
                    }
                }
            }
        }
    }
}

/// Cells of air on each side of the 16×16 grid inside the raster canvas.
const TIGER_MARGIN_CELLS: u32 = 1;
/// Canvas edge in cells: the 16×16 mark plus one cell of air per side, so the
/// mark sits centred in the menu bar's 18pt slot.
const TIGER_CANVAS_CELLS: u32 = 16 + 2 * TIGER_MARGIN_CELLS;

/// Rasterise [`TIGER_GRID`] to RGBA8 at `cell_px` pixels per cell.
///
/// The canvas is 18×18 cells — the mark plus one cell of air on every side —
/// so the returned image is `18 * cell_px` square. With `colour == false` the
/// result is the menu-bar *template*: ink cells opaque black, everything else
/// transparent, for AppKit to tint. With `colour == true` every non-empty
/// cell is painted its palette colour at full alpha.
///
/// Pure: no I/O, no globals.
pub fn tiger_rgba(cell_px: u32, colour: bool) -> (Vec<u8>, u32, u32) {
    let edge = TIGER_CANVAS_CELLS * cell_px;
    let mut buf = vec![0u8; (edge * edge * 4) as usize];
    if cell_px == 0 {
        return (buf, edge, edge);
    }
    for (x, y, cell) in tiger_cells() {
        let px = if colour {
            cell.rgba()
        } else if cell.is_ink() {
            [0, 0, 0, 255]
        } else {
            continue;
        };
        let x0 = (x as u32 + TIGER_MARGIN_CELLS) * cell_px;
        let y0 = (y as u32 + TIGER_MARGIN_CELLS) * cell_px;
        for dy in 0..cell_px {
            for dx in 0..cell_px {
                let i = (((y0 + dy) * edge + x0 + dx) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&px);
            }
        }
    }
    (buf, edge, edge)
}

/// The mascot, alive: idles through a small sprite loop — blink, ear-perk —
/// as real frame swaps between [`TIGER_GRID`]/[`TIGER_GRID_BLINK`]/
/// [`TIGER_GRID_EARS_UP`] rather than animation. Always looping for as long
/// as this component stays mounted: the loop is a `use_future` owned by this
/// component's scope, so it is cancelled on unmount and every instance (app
/// menu, chat empty state) runs its own independent timer. `pouncing` (set
/// briefly by the caller's click handler) overrides the loop with a one-shot
/// squash-and-settle reaction — see the `.tiger-pounce` keyframe in
/// `assets/main.css`.
#[component]
pub fn AnimatedTiger(pouncing: Signal<bool>, width: u32, height: u32) -> Element {
    #[derive(Copy, Clone, PartialEq)]
    enum Frame {
        Idle,
        Blink,
        EarsUp,
    }
    impl Frame {
        fn grid(self) -> &'static [&'static str; 16] {
            match self {
                Frame::Idle => &TIGER_GRID,
                Frame::Blink => &TIGER_GRID_BLINK,
                Frame::EarsUp => &TIGER_GRID_EARS_UP,
            }
        }
    }

    let mut frame = use_signal(|| Frame::Idle);

    use_future(move || async move {
        {
            // An irregular idle beat (blink sooner, ear-perk further out, a
            // held return to Idle in between) reads as attentive rather than
            // a metronome — real mascots don't blink on a fixed tempo.
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1700)).await;
                frame.set(Frame::Blink);
                tokio::time::sleep(std::time::Duration::from_millis(140)).await;
                frame.set(Frame::Idle);
                tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
                frame.set(Frame::EarsUp);
                tokio::time::sleep(std::time::Duration::from_millis(450)).await;
                frame.set(Frame::Idle);
            }
        }
    });

    rsx! {
        div {
            class: if *pouncing.read() { "tiger-pounce" } else { "tiger-idle" },
            dioxus_free_icons::Icon {
                width,
                height,
                icon: HobbesTigerFrame(if *pouncing.read() { &TIGER_GRID_EARS_UP } else { frame.read().grid() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_char(x: usize, y: usize) -> char {
        TIGER_GRID[y].chars().nth(x).unwrap()
    }

    /// Sample the alpha of the pixel at the centre of grid cell (x, y).
    fn alpha_at(buf: &[u8], edge: u32, cell_px: u32, x: usize, y: usize) -> u8 {
        let px = (x as u32 + TIGER_MARGIN_CELLS) * cell_px + cell_px / 2;
        let py = (y as u32 + TIGER_MARGIN_CELLS) * cell_px + cell_px / 2;
        buf[((py * edge + px) * 4 + 3) as usize]
    }

    #[test]
    fn grid_is_16x16() {
        assert_eq!(TIGER_GRID.len(), 16);
        for row in TIGER_GRID {
            assert_eq!(row.chars().count(), 16, "row {row:?}");
        }
    }

    #[test]
    fn chat_bubble_grid_is_16x15_with_three_2x2_dots() {
        assert_eq!(CHAT_BUBBLE_GRID.len(), 15);
        for row in CHAT_BUBBLE_GRID {
            assert_eq!(row.chars().count(), 16, "row {row:?}");
            assert!(row.chars().all(|c| matches!(c, '.' | '#' | '1' | '2' | '3')), "row {row:?}");
        }
        for dot in ['1', '2', '3'] {
            let n: usize = CHAT_BUBBLE_GRID.iter().map(|r| r.matches(dot).count()).sum();
            assert_eq!(n, 4, "dot {dot}");
        }
    }

    #[test]
    fn frame_grids_are_16x16() {
        for grid in [&TIGER_GRID, &TIGER_GRID_BLINK, &TIGER_GRID_EARS_UP] {
            assert_eq!(grid.len(), 16);
            for row in grid {
                assert_eq!(row.chars().count(), 16, "row {row:?}");
            }
        }
    }

    #[test]
    fn idle_loop_frames_are_visibly_distinct() {
        assert_ne!(TIGER_GRID, TIGER_GRID_BLINK);
        assert_ne!(TIGER_GRID, TIGER_GRID_EARS_UP);
        assert_ne!(TIGER_GRID_BLINK, TIGER_GRID_EARS_UP);
    }

    #[test]
    fn stencil_ink_count_is_114() {
        let ink = TIGER_GRID
            .iter()
            .flat_map(|r| r.chars())
            .filter(|c| TigerCell::from_char(*c).is_ink())
            .count();
        assert_eq!(ink, 114, "the template silhouette is 114 of 256 cells");
    }

    #[test]
    fn raster_dimensions_are_18_cells_square() {
        for cell_px in [1u32, 2, 3, 8] {
            let (buf, w, h) = tiger_rgba(cell_px, false);
            assert_eq!((w, h), (18 * cell_px, 18 * cell_px));
            assert_eq!(buf.len(), (w * h * 4) as usize);
        }
        // The macOS tray slot: 18pt @2x.
        let (_, w, h) = tiger_rgba(2, false);
        assert_eq!((w, h), (36, 36));
    }

    #[test]
    fn template_paints_exactly_the_ink_cells_black() {
        let cell_px = 3;
        let (buf, edge, _) = tiger_rgba(cell_px, false);
        let mut opaque = 0;
        for i in (0..buf.len()).step_by(4) {
            if buf[i + 3] != 0 {
                assert_eq!(
                    &buf[i..i + 4],
                    &[0, 0, 0, 255],
                    "template ink is pure black"
                );
                opaque += 1;
            }
        }
        assert_eq!(opaque, 114 * (cell_px * cell_px) as usize);
        for y in 0..16 {
            for x in 0..16 {
                let want = TigerCell::from_char(grid_char(x, y)).is_ink();
                assert_eq!(
                    alpha_at(&buf, edge, cell_px, x, y) != 0,
                    want,
                    "cell ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn colour_raster_leaves_empty_cells_transparent() {
        let cell_px = 3;
        let (buf, edge, _) = tiger_rgba(cell_px, true);
        for y in 0..16 {
            for x in 0..16 {
                let cell = TigerCell::from_char(grid_char(x, y));
                let lit = alpha_at(&buf, edge, cell_px, x, y) != 0;
                assert_eq!(lit, cell != TigerCell::Empty, "cell ({x},{y})");
            }
        }
        // Nothing outside the grid is painted: the one-cell margin is air.
        for i in (0..buf.len()).step_by(4) {
            let p = (i / 4) as u32;
            let (px, py) = (p % edge, p / edge);
            let inside =
                px >= cell_px && py >= cell_px && px < edge - cell_px && py < edge - cell_px;
            if !inside {
                assert_eq!(
                    buf[i + 3],
                    0,
                    "margin pixel ({px},{py}) must be transparent"
                );
            }
        }
    }

    #[test]
    fn colour_and_template_agree_except_on_holes() {
        let cell_px = 2;
        let (colour, edge, _) = tiger_rgba(cell_px, true);
        let (template, _, _) = tiger_rgba(cell_px, false);
        for y in 0..16 {
            for x in 0..16 {
                let cell = TigerCell::from_char(grid_char(x, y));
                let c_lit = alpha_at(&colour, edge, cell_px, x, y) != 0;
                let t_lit = alpha_at(&template, edge, cell_px, x, y) != 0;
                if matches!(cell, TigerCell::Stripe | TigerCell::Eye) {
                    // Holes: drawn dark in colour, punched out of the stencil.
                    assert!(c_lit && !t_lit, "hole ({x},{y})");
                } else {
                    assert_eq!(c_lit, t_lit, "cell ({x},{y})");
                }
            }
        }
    }
}
