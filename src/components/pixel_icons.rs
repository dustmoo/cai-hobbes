//! Original pixel-art icons implementing `dioxus_free_icons::IconShape`, so
//! they drop into `Icon {}` and `ChatBarIconButton` beside the feather set.
//!
//! The invader is Hobbes' own species — an original 11×8 design (narrow
//! head, hanging side arms, three straight legs). Deliberately NOT a copy of
//! any Taito Space Invaders sprite: those remain under copyright; the idea
//! of a blocky alien is free, the famous crab/squid/octopus expressions are
//! not.

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
