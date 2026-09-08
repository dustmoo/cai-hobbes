//! Original pixel-art icons implementing `dioxus_free_icons::IconShape`, so
//! they drop into `Icon {}` and `ChatBarIconButton` beside the feather set.
//!
//! The invader is Hobbes' own species — an original 11×8 design (narrow
//! head, hanging side arms, three straight legs). Deliberately NOT a copy of
//! any Taito Space Invaders sprite: those remain under copyright; the idea
//! of a blocky alien is free, the famous crab/squid/octopus expressions are
//! not.
//!
//! The bow-tie tiger ([`TIGER_GRID`]) is the app mascot: `HobbesTiger` is the
//! colour mark used in the chat bar, `HobbesTigerStencil` the currentColor
//! silhouette, and [`tiger_rgba`] rasterises the same grid for the system
//! tray at runtime (no PNG asset). All three read the one grid — the stencil
//! is derived from it, never drawn separately.

use dioxus::prelude::*;
use dioxus_free_icons::IconShape;

/// The fleet's mascot: one filled 1×1 rect per pixel, drawn in currentColor.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct HobbesInvader;

/// (x, y) lit pixels on an 11×8 grid.
const INVADER_PIXELS: &[(u8, u8)] = &[
    // antennae
    (2, 0),
    (8, 0),
    (3, 1),
    (7, 1),
    // head
    (3, 2),
    (4, 2),
    (5, 2),
    (6, 2),
    (7, 2),
    // face (eyes unlit at x=3 and x=7)
    (1, 3),
    (2, 3),
    (4, 3),
    (5, 3),
    (6, 3),
    (8, 3),
    (9, 3),
    // shoulders
    (0, 4),
    (1, 4),
    (2, 4),
    (3, 4),
    (4, 4),
    (5, 4),
    (6, 4),
    (7, 4),
    (8, 4),
    (9, 4),
    (10, 4),
    // body with detached side arms
    (0, 5),
    (2, 5),
    (3, 5),
    (4, 5),
    (5, 5),
    (6, 5),
    (7, 5),
    (8, 5),
    (10, 5),
    // arm tips + leg roots
    (0, 6),
    (3, 6),
    (5, 6),
    (7, 6),
    (10, 6),
    // three straight legs
    (3, 7),
    (5, 7),
    (7, 7),
];

impl IconShape for HobbesInvader {
    fn view_box(&self) -> &str {
        "0 0 11 8"
    }
    fn xmlns(&self) -> &str {
        "http://www.w3.org/2000/svg"
    }
    fn fill_and_stroke<'a>(&self, user_color: &'a str) -> (&'a str, &'a str, &'a str) {
        // Filled pixels, no stroke — the inverse of the feather line icons.
        (user_color, "none", "0")
    }
    fn child_elements(&self) -> Element {
        rsx! {
            for (x, y) in INVADER_PIXELS.iter().copied() {
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

/// Walk [`TIGER_GRID`] as `(x, y, cell)`, skipping empty cells.
fn tiger_cells() -> impl Iterator<Item = (usize, usize, TigerCell)> {
    TIGER_GRID.iter().enumerate().flat_map(|(y, row)| {
        row.chars()
            .enumerate()
            .map(move |(x, c)| (x, y, TigerCell::from_char(c)))
            .filter(|(_, _, cell)| *cell != TigerCell::Empty)
    })
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
/// Nothing in the UI renders the stencil as SVG yet — the menu bar takes the
/// same silhouette through [`tiger_rgba`] — but it is the DOM-side half of
/// the template rule and is kept in step with the grid by the tests.
#[allow(dead_code)]
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
