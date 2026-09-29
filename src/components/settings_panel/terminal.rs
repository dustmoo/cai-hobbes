use super::shared::Toggle;
use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::*;

/// The native terminal card: one opt-in toggle, committed immediately (the
/// calendar-row idiom). Registration happens at MCP manager init, so
/// enabling takes effect at next launch; disabling also kills any live
/// shells right away.
#[component]
pub(super) fn TerminalSection() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();

    let enabled = settings.read().terminal_enabled;
    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-lg",
                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-1", "Terminal" }
                p {
                    class: "text-xs text-fg-muted mb-3",
                    "A persistent shell per chat the AI can run commands in — every command goes through the tool-approval prompt, and the working directory follows the chat's tagged project."
                }
                div {
                    class: "grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center py-2.5",
                    div {
                        div { class: "text-sm text-fg", "Enable Terminal" }
                        p { class: "text-xs text-fg-muted", "AI can run shell commands (with your approval). Takes effect at next launch; turning it off kills any running shells now." }
                    }
                    div {
                        class: "justify-self-end",
                        Toggle {
                            checked: enabled,
                            onchange: move |on: bool| {
                                settings.write().terminal_enabled = on;
                                settings_manager
                                    .peek()
                                    .save_async(settings.peek().clone(), Some(save_error));
                                if !on {
                                    let mgr = mcp_manager.peek().clone();
                                    spawn(async move {
                                        mgr.shutdown_terminal().await;
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
