use dioxus::prelude::*;

#[component]
pub fn McpSearchForm(
    search_query: Signal<String>,
    trigger_search: Signal<i32>,
    filter_verified: Signal<bool>,
    filter_deployed: Signal<bool>,
    sort_by: Signal<String>,
) -> Element {
    rsx! {
        div {
            class: "mb-4",
            form {
                class: "flex gap-2",
                onsubmit: move |evt| {
                    evt.prevent_default();
                    let current_val = *trigger_search.read();
                    trigger_search.set(current_val + 1);
                },
                input {
                    class: "w-full px-3 py-2 bg-dark-input border border-gray-600 rounded-md text-white placeholder-gray-400 focus:outline-none focus:border-primary-500",
                    placeholder: "Search MCP servers...",
                    value: "{search_query}",
                    oninput: move |e| search_query.set(e.value()),
                }
                button {
                    "type": "submit",
                    class: "px-4 py-2 bg-primary-600 hover:bg-primary-500 rounded font-medium transition-colors",
                    "Search"
                }
            }
            div {
                class: "flex items-center justify-between mt-2",
                div {
                    class: "flex items-center space-x-4",
                    label {
                        class: "flex items-center space-x-2 text-sm text-gray-400",
                        input {
                            class: "form-checkbox bg-dark-input border-gray-600 text-primary-500 focus:ring-primary-500",
                            "type": "checkbox",
                            checked: "{filter_verified}",
                            oninput: move |e| {
                                filter_verified.set(e.value() == "true");
                                let current = *trigger_search.read();
                                trigger_search.set(current + 1);
                            }
                        }
                        span { "Verified" }
                    }
                    label {
                        class: "flex items-center space-x-2 text-sm text-gray-400",
                        input {
                            class: "form-checkbox bg-dark-input border-gray-600 text-primary-500 focus:ring-primary-500",
                            "type": "checkbox",
                            checked: "{filter_deployed}",
                            oninput: move |e| {
                                filter_deployed.set(e.value() == "true");
                                let current = *trigger_search.read();
                                trigger_search.set(current + 1);
                            }
                        }
                        span { "Deployed" }
                    }
                }
                div {
                    class: "flex items-center space-x-2",
                    label { class: "text-sm text-gray-400", "Sort by:" }
                    select {
                        class: "bg-dark-input border border-gray-600 rounded text-sm text-white px-2 py-1 focus:outline-none focus:border-primary-500",
                        value: "{sort_by}",
                        onchange: move |e| {
                            sort_by.set(e.value());
                            let current = *trigger_search.read();
                            trigger_search.set(current + 1);
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