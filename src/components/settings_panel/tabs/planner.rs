use super::super::SettingsCtx;
use super::super::calendar::CalendarSubscriptionsSection;
use dioxus::prelude::*;

#[component]
pub(in super::super) fn PlannerTab(ctx: SettingsCtx) -> Element {
    let mut local_settings = ctx.local_settings;
    rsx! {
                            div {
                                class: "border border-subtle rounded-lg mb-4",
                                div {
                                    class: "p-4 bg-section rounded-lg",
                                    h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-3", "Planner" }
                                    p { class: "text-sm text-fg-muted mb-4",
                                        "The built-in to-do list and daily planner, shared between you and Hobbes."
                                    }

                                    div {
                                        class: "flex items-center justify-between mb-4",
                                        div {
                                            label { class: "text-sm font-medium text-fg-muted", "Enable Planner" }
                                            p { class: "text-xs text-fg-muted", "When off, the planner tools are withheld from the AI and the Planner view is hidden." }
                                        }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().planner_enabled}",
                                            onchange: move |e| {
                                                local_settings.write().planner_enabled = e.value() == "true";
                                            }
                                        }
                                    }

                                    div {
                                        class: "flex items-center justify-between mb-4",
                                        div {
                                            label { class: "text-sm font-medium text-fg-muted", "Share Today's Plan with the AI" }
                                            p { class: "text-xs text-fg-muted", "Inject today's todos, timeline, and capacity into the AI's context each turn (costs a few hundred tokens)." }
                                        }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().planner_inject_today_context}",
                                            onchange: move |e| {
                                                local_settings.write().planner_inject_today_context = e.value() == "true";
                                            }
                                        }
                                    }

                                    div {
                                        class: "flex items-center justify-between mb-4",
                                        div {
                                            label { class: "text-sm font-medium text-fg-muted", "Let the AI Choose a Profile When Dispatching" }
                                            p { class: "text-xs text-fg-muted", "A dispatched chat can run under another Composio profile (e.g. a client's accounts) when the work belongs there. Off: it always uses the profile of the tab that dispatched it." }
                                        }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().dispatch_profile_choice}",
                                            onchange: move |e| {
                                                local_settings.write().dispatch_profile_choice = e.value() == "true";
                                            }
                                        }
                                    }

                                    div {
                                        class: "flex items-center justify-between mb-4",
                                        div {
                                            label { class: "text-sm font-medium text-fg-muted", "Roll Unfinished Work Forward" }
                                            p { class: "text-xs text-fg-muted", "Todos scheduled for a past day move onto today when the planner opens." }
                                        }
                                        input {
                                            r#type: "checkbox",
                                            class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                                            checked: "{local_settings.read().planner_auto_rollover}",
                                            onchange: move |e| {
                                                local_settings.write().planner_auto_rollover = e.value() == "true";
                                            }
                                        }
                                    }

                                    div {
                                        class: "grid grid-cols-2 gap-4 mb-4",
                                        div {
                                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Workday Starts" }
                                            input {
                                                r#type: "text",
                                                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                                                placeholder: "09:00",
                                                value: "{local_settings.read().planner_workday_start}",
                                                oninput: move |e| {
                                                    local_settings.write().planner_workday_start = e.value();
                                                }
                                            }
                                        }
                                        div {
                                            label { class: "block text-sm font-medium text-fg-muted mb-1", "Workday Ends" }
                                            input {
                                                r#type: "text",
                                                class: "w-full rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                                                placeholder: "17:00",
                                                value: "{local_settings.read().planner_workday_end}",
                                                oninput: move |e| {
                                                    local_settings.write().planner_workday_end = e.value();
                                                }
                                            }
                                        }
                                    }
                                    p { class: "text-xs text-fg-muted -mt-2 mb-4",
                                        "24-hour HH:MM. The timeline falls back to 09:00–17:00 if these don't parse."
                                    }

                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Daily Capacity (minutes)" }
                                        p { class: "text-xs text-fg-muted mb-1", "Focused work the day is planned against — an honest number, not your hours at a desk. 360 = 6h." }
                                        input {
                                            r#type: "number",
                                            min: "0",
                                            class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                                            value: "{local_settings.read().planner_daily_capacity_minutes}",
                                            oninput: move |e| {
                                                if let Ok(v) = e.value().parse::<u32>() {
                                                    local_settings.write().planner_daily_capacity_minutes = v;
                                                }
                                            }
                                        }
                                    }

                                    div {
                                        class: "mb-4",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Default Estimate (minutes)" }
                                        p { class: "text-xs text-fg-muted mb-1", "Auto-filled onto quick-added todos with no ~duration token. 0 leaves them unestimated." }
                                        input {
                                            r#type: "number",
                                            min: "0",
                                            class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                                            value: "{local_settings.read().planner_default_estimate_minutes}",
                                            oninput: move |e| {
                                                if let Ok(v) = e.value().parse::<u32>() {
                                                    local_settings.write().planner_default_estimate_minutes = v;
                                                }
                                            }
                                        }
                                    }

                                    div {
                                        class: "mb-1",
                                        label { class: "block text-sm font-medium text-fg-muted mb-1", "Default Block Length (minutes)" }
                                        p { class: "text-xs text-fg-muted mb-1", "Blocks created by clicking the timeline, or by an @time token on an unestimated todo." }
                                        input {
                                            r#type: "number",
                                            min: "15",
                                            class: "w-32 rounded border border-subtle bg-input px-3 py-2 text-sm text-fg",
                                            value: "{local_settings.read().planner_default_block_minutes}",
                                            oninput: move |e| {
                                                if let Ok(v) = e.value().parse::<u32>() {
                                                    local_settings.write().planner_default_block_minutes = v.max(15);
                                                }
                                            }
                                        }
                                    }

                                    // Priority naming: display only — P1 is Q1, so switching
                                    // never rewrites a todo.
                                    {
                                        use crate::todo::model::{Priority, PriorityDisplay, PriorityScheme};
                                        let scheme = local_settings.read().planner_priority.scheme;
                                        rsx! {
                                            div {
                                                class: "mt-4 mb-1",
                                                label { class: "block text-sm font-medium text-fg-muted mb-1", "Priority Labels" }
                                                p { class: "text-xs text-fg-muted mb-1", "How the four priority levels read on rows, menus and band headers. Rename any level; blank uses the default. Rows take 1–4 (0 clears, p cycles) when focused." }
                                                select {
                                                    class: "mb-2 block w-64 px-3 py-2 bg-input border border-subtle rounded-md text-sm text-fg",
                                                    onchange: move |e| {
                                                        local_settings.write().planner_priority.scheme = if e.value() == "linear" {
                                                            PriorityScheme::Linear
                                                        } else {
                                                            PriorityScheme::Eisenhower
                                                        };
                                                    },
                                                    option { value: "eisenhower", selected: scheme == PriorityScheme::Eisenhower, "Eisenhower quadrants (Q1–Q4)" }
                                                    option { value: "linear", selected: scheme == PriorityScheme::Linear, "Linear scale (P1–P4)" }
                                                }
                                                div {
                                                    class: "grid grid-cols-[2.5rem_1fr] items-center gap-x-2 gap-y-1.5",
                                                    for p in Priority::ALL {
                                                        {
                                                            let i = p.quadrant() as usize - 1;
                                                            let display = local_settings.read().planner_priority.clone();
                                                            let value = display.names.get(i).cloned().unwrap_or_default();
                                                            rsx! {
                                                                span { key: "b-{i}", class: "font-mono text-xs text-fg-muted", "{display.badge(p)}" }
                                                                input {
                                                                    key: "n-{i}",
                                                                    r#type: "text",
                                                                    class: "w-64 rounded border border-subtle bg-input px-3 py-1.5 text-sm text-fg",
                                                                    placeholder: "{PriorityDisplay::default_name(scheme, p)}",
                                                                    value: "{value}",
                                                                    oninput: move |e| {
                                                                        let mut s = local_settings.write();
                                                                        let names = &mut s.planner_priority.names;
                                                                        if names.len() < 4 {
                                                                            names.resize(4, String::new());
                                                                        }
                                                                        names[i] = e.value();
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
                            }

                            div {
                                class: "border border-subtle rounded-lg mb-4",
                                div {
                                    class: "p-4 bg-section rounded-lg",
                                    h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-3", "Quick-Add Syntax" }
                                    p { class: "text-sm text-fg-muted mb-2",
                                        "Tokens anywhere in the quick-add input; anything that doesn't parse stays in the title."
                                    }
                                    div {
                                        class: "text-xs text-fg-muted space-y-1 font-mono",
                                        p { "~30m  ~1h30m  ~90     estimate" }
                                        p { "#tag                  tag (repeatable)" }
                                        p { "@2pm  @14:30  @9      place on today's timeline" }
                                        p { "@morning  @evening    time-of-day group" }
                                        p { "*mon  *tomorrow       scheduled day" }
                                        p { "!fri  !tomorrow       deadline" }
                                        p { "!!1…!!4  !!p1…!!p4    priority" }
                                        p { "+hobbes  +hobbes-rc   project (slug or word prefix)" }
                                    }
                                }
                            }

                            CalendarSubscriptionsSection {}
    }
}
