use crate::mcp::composio_client::ComposioCategory;
use crate::settings::McpSource;
use dioxus::prelude::*;

#[component]
pub fn McpSearchForm(
    search_query: Signal<String>,
    trigger_search: Signal<i32>,
    filter_verified: Signal<bool>,
    filter_deployed: Signal<bool>,
    sort_by: Signal<String>,
    mcp_source: McpSource,
    composio_categories: Signal<Vec<String>>,
    available_categories: Signal<Vec<ComposioCategory>>,
    show_category_dropdown: Signal<bool>,
    #[props(default = false)] categories_loading: bool,
) -> Element {
    // Clone signals for use in closures - this ensures proper reactivity
    let mut sort_by_writer = sort_by;
    let mut trigger_search_writer = trigger_search;

    rsx! {
        div {
            class: "mb-4",
            form {
                class: "flex gap-2",
                onsubmit: move |evt| {
                    evt.prevent_default();
                    let current_val = *trigger_search.read();
                    trigger_search_writer.set(current_val + 1);
                },
                input {
                    class: "w-full px-3 py-2 bg-input border border-faint rounded-md text-fg placeholder-gray-400 focus:outline-none focus:border-primary-500",
                    placeholder: if mcp_source == McpSource::Composio { "Filter Composio toolkits..." } else { "Search MCP servers..." },
                    value: "{search_query}",
                    oninput: move |e| search_query.set(e.value()),
                }
                button {
                    "type": "submit",
                    class: "px-4 py-2 bg-btn-primary hover:bg-btn-primary-hover rounded font-medium transition-colors",
                    if mcp_source == McpSource::Composio { "Filter" } else { "Search" }
                }
            }
            div {
                class: "flex items-center justify-between mt-2",
                // Left side - Source-specific filters
                match mcp_source {
                    McpSource::Smithery => rsx! {
                        div {
                            class: "flex items-center space-x-4",
                            label {
                                class: "flex items-center space-x-2 text-sm text-fg-muted",
                                input {
                                    class: "form-checkbox bg-input border-faint text-primary-500 focus:ring-primary-500",
                                    "type": "checkbox",
                                    checked: "{filter_verified}",
                                    oninput: move |e| {
                                        filter_verified.set(e.value() == "true");
                                        let current = *trigger_search.read();
                                        trigger_search_writer.set(current + 1);
                                    }
                                }
                                span { "Verified" }
                            }
                            label {
                                class: "flex items-center space-x-2 text-sm text-fg-muted",
                                input {
                                    class: "form-checkbox bg-input border-faint text-primary-500 focus:ring-primary-500",
                                    "type": "checkbox",
                                    checked: "{filter_deployed}",
                                    oninput: move |e| {
                                        filter_deployed.set(e.value() == "true");
                                        let current = *trigger_search.read();
                                        trigger_search_writer.set(current + 1);
                                    }
                                }
                                span { "Deployed" }
                            }
                        }
                    },
                    McpSource::Composio => rsx! {
                        div {
                            class: "flex items-center gap-2 text-sm text-fg-muted",
                            span { "Type to filter results" }
                        }
                    }
                }
                // Right side - Sort dropdown (Smithery only - Composio API doesn't support sorting)
                if mcp_source == McpSource::Smithery {
                    div {
                        class: "flex items-center space-x-2",
                        label { class: "text-sm text-fg-muted", "Sort by:" }
                        select {
                            class: "bg-input border border-faint rounded text-sm text-fg px-2 py-1 focus:outline-none focus:border-primary-500",
                            value: "{sort_by}",
                            onchange: move |e| {
                                let new_value = e.value();
                                tracing::debug!("Smithery sort changed to: {}", new_value);
                                sort_by_writer.set(new_value);
                                let current = *trigger_search.read();
                                trigger_search_writer.set(current + 1);
                            },
                            option { value: "relevance", "Relevance" }
                            option { value: "use_count", "Most Used" }
                            option { value: "created_at", "Newest" }
                        }
                    }
                }
            }
        }
    }
}
