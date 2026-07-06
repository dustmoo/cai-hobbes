use crate::mcp::composio_client::ComposioCategory;
use crate::mcp::glama_client::GlamaHosting;
use crate::settings::McpSource;
use dioxus::prelude::*;

#[component]
pub fn McpSearchForm(
    search_query: Signal<String>,
    // Bumped on submit (Enter / Search button) to commit the query and trigger a
    // fetch. The parent owns pagination reset + refetch; the form only commits.
    search_commit: Signal<i32>,
    mcp_source: McpSource,
    composio_categories: Signal<Vec<String>>,
    available_categories: Signal<Vec<ComposioCategory>>,
    show_category_dropdown: Signal<bool>,
    glama_hosting_filter: Signal<Option<GlamaHosting>>,
    glama_official_only: Signal<bool>,
    glama_licensed_only: Signal<bool>,
    #[props(default = false)] categories_loading: bool,
) -> Element {
    let mut search_commit = search_commit;
    let mut hosting_filter = glama_hosting_filter;
    let mut official_only = glama_official_only;
    let mut licensed_only = glama_licensed_only;

    let hosting_chip = |value: Option<GlamaHosting>, label: &'static str| {
        let selected = *hosting_filter.read() == value;
        rsx! {
            button {
                r#type: "button",
                class: if selected {
                    "px-3 py-1 rounded-full text-xs font-medium bg-btn-primary text-fg"
                } else {
                    "px-3 py-1 rounded-full text-xs font-medium bg-input text-fg-muted hover:text-fg transition-colors"
                },
                // The marketplace's fetch effect watches this signal —
                // updating it resets pagination and refetches.
                onclick: move |_| hosting_filter.set(value),
                "{label}"
            }
        }
    };

    rsx! {
        div {
            class: "mb-4",
            form {
                class: "flex gap-2",
                onsubmit: move |evt| {
                    evt.prevent_default();
                    // Commit the current query; the parent resets pagination and refetches.
                    let current_val = *search_commit.peek();
                    search_commit.set(current_val + 1);
                },
                input {
                    class: "w-full px-3 py-2 bg-input border border-faint rounded-md text-fg placeholder-gray-400 focus:outline-none focus:border-primary-500",
                    placeholder: if mcp_source == McpSource::Composio { "Filter Composio toolkits..." } else { "Search the Glama MCP registry..." },
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
                    McpSource::Glama => rsx! {
                        div {
                            class: "flex items-center gap-2 flex-wrap",
                            {hosting_chip(None, "All")}
                            {hosting_chip(Some(GlamaHosting::LocalOnly), "Local")}
                            {hosting_chip(Some(GlamaHosting::RemoteCapable), "Remote")}
                            {hosting_chip(Some(GlamaHosting::Hybrid), "Hybrid")}
                            label {
                                class: "flex items-center gap-1.5 ml-2 text-xs text-fg-muted cursor-pointer",
                                input {
                                    r#type: "checkbox",
                                    class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                    checked: *official_only.read(),
                                    // Watched by the marketplace's fetch effect (refetch + pagination reset).
                                    oninput: move |e| official_only.set(e.value() == "true"),
                                }
                                span { "Official only" }
                            }
                            label {
                                class: "flex items-center gap-1.5 ml-2 text-xs text-fg-muted cursor-pointer",
                                title: "Hide servers Glama marks as not installable (no license found)",
                                input {
                                    r#type: "checkbox",
                                    class: "form-checkbox bg-input border-faint text-primary-500 h-3.5 w-3.5",
                                    checked: *licensed_only.read(),
                                    // Watched by the marketplace's fetch effect (refetch + pagination reset).
                                    oninput: move |e| licensed_only.set(e.value() == "true"),
                                }
                                span { "Installable only" }
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
            }
        }
    }
}
