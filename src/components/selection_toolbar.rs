use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[component]
pub fn SelectionToolbar(
    on_copy: EventHandler<()>,
    on_comment: EventHandler<()>,
    on_mouseenter: EventHandler<()>,
    on_mouseleave: EventHandler<()>,
    position_top: f64,
    position_left: f64,
) -> Element {
    rsx! {
        div {
            id: "selection-toolbar",
            class: "fixed z-50 bg-card border border-faint rounded-lg shadow-2xl p-1.5 flex space-x-1.5 transition-all duration-200",
            style: "top: {position_top}px; left: {position_left}px;",
            onclick: move |e| e.stop_propagation(),
            onmouseenter: move |_| on_mouseenter.call(()),
            onmouseleave: move |_| on_mouseleave.call(()),

            button {
                class: "p-2 text-fg-muted hover:text-fg hover:bg-input rounded-md transition-colors",
                onclick: move |_| on_copy.call(()),
                title: "Copy",
                Icon { width: 14, height: 14, icon: fi_icons::FiCopy }
            }
            button {
                class: "p-2 text-fg-muted hover:text-fg hover:bg-input rounded-md transition-colors",
                onclick: move |_| on_comment.call(()),
                title: "Add Comment",
                Icon { width: 14, height: 14, icon: fi_icons::FiMessageSquare }
            }
        }
    }
}
