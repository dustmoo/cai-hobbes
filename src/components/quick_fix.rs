use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

#[component]
pub fn QuickFix(
    suggestions: Vec<String>,
    on_select: EventHandler<String>,
) -> Element {
    if suggestions.is_empty() {
        return rsx!({});
    }

    rsx! {
        div {
            class: "flex flex-col space-y-2 mt-2 p-3 bg-red-900/20 border border-red-800/50 rounded-lg",
            div {
                class: "flex items-center space-x-2 text-xs text-red-300 font-semibold uppercase tracking-wider",
                Icon {
                    width: 14,
                    height: 14,
                    icon: fi_icons::FiTool,
                }
                span { "Suggested Fixes" }
            }
            div {
                class: "flex flex-wrap gap-2",
                for suggestion in suggestions {
                    button {
                        class: "px-3 py-1.5 text-sm bg-primary-600/80 hover:bg-primary-500 text-white rounded transition-colors flex items-center space-x-1 shadow-sm",
                        onclick: move |_| on_select.call(suggestion.clone()),
                        span { "{suggestion}" }
                        Icon {
                            width: 12,
                            height: 12,
                            icon: fi_icons::FiArrowRight,
                        }
                    }
                }
            }
        }
    }
}
