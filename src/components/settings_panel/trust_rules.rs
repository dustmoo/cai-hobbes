use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::*;

/// Trust rules: the allow-rules authored from permission prompts. Read-only
/// list plus removal (rules are only ever created at the moment of an
/// approval, where the user has full context); hit counts derive from the
/// trust_decisions log.
#[component]
pub(super) fn TrustRulesSection() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;
    let planner = use_context::<Signal<crate::todo::PlannerState>>();
    let mut stats = use_signal(std::collections::HashMap::<String, (u32, String)>::new);
    let mut armed_delete = use_signal(|| Option::<String>::None);
    use_effect(move || {
        stats.set(crate::context::trust_store::rule_stats());
    });

    let rules = settings.read().permission_settings.trust_rules.clone();
    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-lg",
                onmouseleave: move |_| armed_delete.set(None),
                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-1", "Trust Rules" }
                p { class: "text-xs text-fg-muted mb-3",
                    "Tools these rules match run without a permission prompt. Rules are created from permission prompts (\"Always allow…\") — remove any you no longer trust."
                }
                if rules.is_empty() {
                    p { class: "text-sm text-fg-muted", "No rules yet. Check \"Always allow…\" on a permission prompt to create one." }
                }
                div {
                    class: "space-y-2",
                    for rule in rules {
                        {
                            let scope = rule
                                .project_id
                                .as_deref()
                                .map(|pid| {
                                    crate::services::project_tagger::project_title(
                                        &planner.read().projects,
                                        pid,
                                    )
                                    .unwrap_or("(missing project)")
                                    .to_string()
                                })
                                .unwrap_or_else(|| "any project".to_string());
                            let subject = rule
                                .command_prefix
                                .clone()
                                .map(|p| format!("`{p}`"))
                                .or_else(|| rule.tool.clone())
                                .unwrap_or_else(|| "any tool".to_string());
                            let hits = stats
                                .read()
                                .get(&rule.id)
                                .map(|(n, _)| format!("{n} auto-approval{}", if *n == 1 { "" } else { "s" }))
                                .unwrap_or_else(|| "never used".to_string());
                            let rid = rule.id.clone();
                            let is_armed = armed_delete.read().as_deref() == Some(rule.id.as_str());
                            rsx! {
                                div {
                                    key: "{rule.id}",
                                    class: "flex items-center gap-2 rounded border border-faint bg-input/30 px-3 py-2 text-sm",
                                    span { class: "font-mono text-xs text-fg", "{rule.server}" }
                                    span { class: "text-fg-muted", "·" }
                                    span { class: "font-mono text-xs text-fg truncate", "{subject}" }
                                    span { class: "text-fg-muted", "·" }
                                    span { class: "text-xs text-fg-muted", "{scope}" }
                                    div { class: "flex-1" }
                                    span { class: "text-xs text-fg-muted shrink-0", "{hits}" }
                                    button {
                                        class: if is_armed {
                                            "shrink-0 px-2 py-0.5 rounded text-xs font-bold bg-red-900/60 text-red-200 hover:bg-red-800/60"
                                        } else {
                                            "shrink-0 px-2 py-0.5 rounded text-xs text-fg-muted hover:text-red-400 hover:bg-red-500/10"
                                        },
                                        onclick: move |_| {
                                            if is_armed {
                                                settings.write().remove_trust_rule(&rid);
                                                settings_manager
                                                    .peek()
                                                    .save_async(settings.peek().clone(), Some(save_error));
                                                armed_delete.set(None);
                                            } else {
                                                armed_delete.set(Some(rid.clone()));
                                            }
                                        },
                                        if is_armed { "Confirm remove" } else { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
