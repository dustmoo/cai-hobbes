//! Polished-steel heart that stands in for the Heartbeat session's tab emoji.

use dioxus::prelude::*;

/// The dedicated Heartbeat chat session's id, as a memo: a component that
/// reads it re-renders when the id is first assigned, not on every write to
/// the Heartbeat state (the runtime writes it every tick).
pub fn use_heartbeat_session_id() -> Memo<Option<String>> {
    use_memo(|| crate::heartbeat::HEARTBEAT.read().session_id.clone())
}

#[component]
pub fn MetalHeart(#[props(default = 13)] size: u32) -> Element {
    rsx! {
        svg {
            class: "shrink-0",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            role: "img",
            "aria-label": "Heartbeat",
            title { "Heartbeat" }
            defs {
                linearGradient { id: "metal-heart-fill", x1: "0", y1: "0", x2: "1", y2: "1",
                    stop { offset: "0", stop_color: "#f4f6f8" }
                    stop { offset: "0.3", stop_color: "#b8c0c8" }
                    stop { offset: "0.5", stop_color: "#6b7580" }
                    stop { offset: "0.65", stop_color: "#e9edf1" }
                    stop { offset: "1", stop_color: "#4a525b" }
                }
            }
            path {
                d: "M12 21.2C5 15.6 2.2 12.2 2.2 8.4C2.2 5.6 4.3 3.6 6.9 3.6C9.1 3.6 10.9 4.8 12 6.8C13.1 4.8 14.9 3.6 17.1 3.6C19.7 3.6 21.8 5.6 21.8 8.4C21.8 12.2 19 15.6 12 21.2Z",
                fill: "url(#metal-heart-fill)",
                stroke: "#3a4048",
                stroke_width: "0.75",
                stroke_linejoin: "round",
            }
            ellipse {
                cx: "7.6",
                cy: "7.6",
                rx: "2.6",
                ry: "1.5",
                transform: "rotate(-35 7.6 7.6)",
                fill: "#ffffff",
                opacity: "0.55",
            }
        }
    }
}
