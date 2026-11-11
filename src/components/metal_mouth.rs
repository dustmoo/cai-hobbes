//! Polished-steel mouth: the Hobbes tab's voice badge (`hobbes_icon`), the
//! voice indicator's mark, and the icon of pre-merge Voice archives (the
//! sibling of `metal_heart::MetalHeart`).

use dioxus::prelude::*;

#[component]
pub fn MetalMouth(#[props(default = 13)] size: u32) -> Element {
    rsx! {
        svg {
            class: "shrink-0",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            role: "img",
            "aria-label": "Voice",
            title { "Voice" }
            defs {
                linearGradient { id: "metal-mouth-fill", x1: "0", y1: "0", x2: "1", y2: "1",
                    stop { offset: "0", stop_color: "#f4f6f8" }
                    stop { offset: "0.3", stop_color: "#b8c0c8" }
                    stop { offset: "0.5", stop_color: "#6b7580" }
                    stop { offset: "0.65", stop_color: "#e9edf1" }
                    stop { offset: "1", stop_color: "#4a525b" }
                }
            }
            // Upper lip, with the cupid's bow.
            path {
                d: "M2.2 12.2C4.8 8.4 7.9 6.8 10 7.3C11 7.5 11.6 8.1 12 8.7C12.4 8.1 13 7.5 14 7.3C16.1 6.8 19.2 8.4 21.8 12.2C18.2 11.7 15.1 12.1 12 12.4C8.9 12.1 5.8 11.7 2.2 12.2Z",
                fill: "url(#metal-mouth-fill)",
                stroke: "#3a4048",
                stroke_width: "0.75",
                stroke_linejoin: "round",
            }
            // Lower lip.
            path {
                d: "M2.2 12.2C5.8 12.8 8.9 13.2 12 13.3C15.1 13.2 18.2 12.8 21.8 12.2C19.8 16.1 16.2 18 12 18C7.8 18 4.2 16.1 2.2 12.2Z",
                fill: "url(#metal-mouth-fill)",
                stroke: "#3a4048",
                stroke_width: "0.75",
                stroke_linejoin: "round",
            }
            ellipse {
                cx: "8.6",
                cy: "15",
                rx: "2.4",
                ry: "0.9",
                transform: "rotate(8 8.6 15)",
                fill: "#ffffff",
                opacity: "0.5",
            }
        }
    }
}
