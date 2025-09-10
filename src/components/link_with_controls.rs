use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};
use std::time::Duration;
use tokio::time::sleep;
use feature_clipboard::copy_to_clipboard;

#[component]
pub fn LinkWithControls(href: String, text_html: String) -> Element {
    let mut copied = use_signal(|| false);
    let mut draft = consume_context::<Signal<String>>();
    let mcp_context = use_context::<Signal<crate::mcp::manager::McpContext>>();

    let fetch_tool_available = use_memo(move || {
        mcp_context.read().servers.iter().any(|s| s.name == "fetch")
    });

    let href_clone_copy = href.clone();
    let copy_onclick = move |_| {
        let href_to_copy = href_clone_copy.clone();
        spawn(async move {
            if copy_to_clipboard(&href_to_copy).is_ok() {
                copied.set(true);
                sleep(Duration::from_secs(2)).await;
                copied.set(false);
            }
        });
    };

    let href_clone_summarize = href.clone();
    let summarize_onclick = move |_| {
        let summary_prompt = format!("Please fetch {} and summarize.", href_clone_summarize);
        draft.set(summary_prompt);
        // Focus the textarea after setting the draft
        let _ = document::eval(r#"
            const el = document.getElementById('chat-textarea');
            if (el) {
                el.focus();
                el.style.height = 'auto';
                el.style.height = (el.scrollHeight) + 'px';
            }
        "#);
    };

    rsx! {
        span {
            class: "relative group",
            a {
                href: "{href}",
                target: "_blank",
                rel: "noopener noreferrer",
                class: "text-purple-400 hover:underline",
                dangerous_inner_html: "{text_html}"
            }
            span {
                class: "inline-flex items-center absolute left-full ml-1 z-10 opacity-0 group-hover:opacity-100 transition-opacity duration-200 bg-gray-900 bg-opacity-75 border border-gray-700 rounded-full shadow-lg p-0.5 space-x-0.5",
                
                // Copy Button
                button {
                    class: "p-1.5 rounded text-gray-400 hover:bg-gray-700 hover:text-white transition-colors",
                    onclick: copy_onclick,
                    if *copied.read() {
                        Icon { width: 16, height: 16, icon: fi_icons::FiCheck }
                    } else {
                        Icon { width: 16, height: 16, icon: fi_icons::FiClipboard }
                    }
                }

                // Summarize Button
                if *fetch_tool_available.read() {
                    button {
                        class: "p-1.5 rounded text-gray-400 hover:bg-gray-700 hover:text-white transition-colors",
                        onclick: summarize_onclick,
                        Icon { width: 16, height: 16, icon: fi_icons::FiFileText }
                    }
                }
            }
        }
    }
}