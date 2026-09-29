use super::super::license::LicenseSection;
use crate::components::markdown_renderer::MarkdownRenderer;
use crate::components::onboarding::TOS_CONTENT;
use dioxus::prelude::*;

#[component]
pub(in super::super) fn AboutTab(app_name: &'static str, app_version: String, mut show_tos_modal: Signal<bool>) -> Element {
    rsx! {
                // About & Legal Section
                div {
                    class: "border border-subtle rounded-lg mb-4",
                    div {
                        class: "p-4 bg-section rounded-lg",
                        h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "About & Legal" }
                        p { class: "text-sm text-fg-muted mb-3", "What Hobbes is, and where the license and legal terms live." }

                        // Version and app name
                        div {
                            class: "flex items-center gap-2 mb-3",
                            span { class: "text-sm text-fg-muted", "{app_name}" }
                            span { class: "font-mono text-[11px] text-fg-muted", "{app_version}" }
                        }

                        // Privacy statement
                        p {
                            class: "text-sm text-green-400 mb-3",
                            "🔒 Built without telemetry for your privacy."
                        }

                        // Attribution
                        p {
                            class: "text-xs text-fg-muted mb-4",
                            "{crate::settings::APP_ATTRIBUTION}"
                        }

                        // Legal links
                        div {
                            class: "flex gap-4",
                            button {
                                class: "border border-faint text-fg rounded px-3 py-1.5 text-sm hover:bg-card",
                                onclick: move |_| show_tos_modal.set(true),
                                "Terms of Service"
                            }
                            a {
                                class: "text-sm text-primary-400 hover:text-primary-300 underline",
                                href: "https://clearmirror.ai/privacy-policy",
                                target: "_blank",
                                "Privacy Policy"
                            }
                        }

                        // TOS Modal
                        if show_tos_modal() {
                            div {
                                class: "fixed inset-0 bg-black/60 flex items-center justify-center z-50",
                                onclick: move |_| show_tos_modal.set(false),
                                div {
                                    class: "bg-section rounded-lg shadow-xl max-w-lg w-full mx-4 max-h-[80vh] flex flex-col",
                                    onclick: move |e| e.stop_propagation(),
                                    // Header
                                    div {
                                        class: "flex justify-between items-center p-4 border-b border-subtle",
                                        h2 { class: "text-lg font-bold", "Terms of Service" }
                                        button {
                                            class: "text-fg-muted hover:text-fg text-xl font-bold w-8 h-8 flex items-center justify-center rounded hover:bg-input transition-colors",
                                            onclick: move |_| show_tos_modal.set(false),
                                            "×"
                                        }
                                    }
                                    // Content
                                    div {
                                        class: "flex-1 overflow-y-auto p-4 text-sm prose prose-sm dark:prose-invert max-w-none",
                                        MarkdownRenderer {
                                            content: TOS_CONTENT.to_string(),
                                            comments: None,
                                            pending_highlight: None,
                                        }
                                    }
                                    // Footer
                                    div {
                                        class: "p-4 border-t border-subtle",
                                        button {
                                            class: "w-full bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                                            onclick: move |_| show_tos_modal.set(false),
                                            "Close"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Pro license status + key entry
                LicenseSection {}
    }
}
