use super::super::SettingsCtx;
use crate::components::confirm_delete_modal::ConfirmDeleteModal;
use dioxus::prelude::*;

#[component]
pub(in super::super) fn SkillsTab(
    ctx: SettingsCtx,
    mut editing_skill: Signal<Option<Option<crate::skills::Skill>>>,
    mut deleting_skill: Signal<Option<String>>,
    mut skill_delete_visible: Signal<bool>,
) -> Element {
    let mut settings = ctx.settings;
    let settings_manager = ctx.settings_manager;
    let session_state = ctx.session_state;
    let save_error = ctx.save_error;
    let skill_registry = ctx.skill_registry;
    rsx! {
                        div {
                            class: "border border-subtle rounded-lg mb-4",
                            div {
                                class: "p-4 bg-section rounded-lg",
                                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-3", "Skills" }
                                p { class: "text-sm text-fg-muted mb-4",
                                    "Skills are instruction sets that teach Hobbes how to perform specific tasks using available tools."
                                }

                                // Action row: create + reload
                                div {
                                    class: "mb-4 flex items-center gap-2",
                                    button {
                                        class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                                        onclick: move |_| editing_skill.set(Some(None)),
                                        "+ New Skill"
                                    }
                                    button {
                                        class: "px-3 py-1.5 bg-primary-600 hover:bg-primary-500 text-white text-sm rounded-md transition-colors",
                                        onclick: move |_| {
                                            let skill_registry = skill_registry;
                                            spawn(async move {
                                                crate::skills::SkillRegistry::reload_into_signal(skill_registry).await;
                                            });
                                        },
                                        "↻ Reload Skills"
                                    }
                                }

                                // Skills that failed to parse (bad frontmatter etc.)
                                {
                                    let registry = skill_registry.read();
                                    let errors = registry.load_errors.read().unwrap_or_else(|p| p.into_inner()).clone();
                                    if errors.is_empty() {
                                        rsx! {}
                                    } else {
                                        rsx! {
                                            div {
                                                class: "mb-4 p-3 border border-red-800/60 rounded-md bg-red-900/10",
                                                p { class: "text-xs font-semibold text-red-400 mb-2", "Failed to load" }
                                                div {
                                                    class: "space-y-1",
                                                    for err in errors {
                                                        div {
                                                            class: "text-xs text-fg-muted",
                                                            code { class: "text-red-300", {err.path.display().to_string()} }
                                                            span { " — {err.error}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Skills list
                                {
                                    let registry = skill_registry.read();
                                    let skills_lock = registry.skills.read().unwrap_or_else(|p| p.into_inner());
                                    let mut skill_entries: Vec<_> = skills_lock.iter().map(|(n, s)| (n.clone(), s.clone())).collect();
                                    skill_entries.sort_by_key(|(name, _)| name.to_string());
                                    drop(skills_lock);

                                    if skill_entries.is_empty() {
                                        rsx! {
                                            div {
                                                class: "text-sm text-fg-muted italic p-3 border border-subtle rounded-md",
                                                "No skills yet. Create one with \"+ New Skill\", or add skill directories to "
                                                code { class: "text-xs bg-black/20 px-1 py-0.5 rounded", "~/.hobbes/skills/" }
                                                " or "
                                                code { class: "text-xs bg-black/20 px-1 py-0.5 rounded", "~/Library/Application Support/com.clearmirror.hobbes/skills/" }
                                            }
                                        }
                                    } else {
                                        rsx! {
                                            div {
                                                class: "space-y-2",
                                                for (name, skill) in skill_entries {
                                                    div {
                                                        class: "p-3 border border-subtle rounded-md bg-black/10",
                                                        div {
                                                            class: "flex items-center gap-2 mb-1",
                                                            span { class: "text-sm font-medium text-fg flex-1", "{name}" }
                                                            button {
                                                                class: "px-2 py-0.5 text-xs text-fg-muted hover:text-fg border border-subtle rounded transition-colors",
                                                                onclick: {
                                                                    let skill = skill.clone();
                                                                    move |_| editing_skill.set(Some(Some(skill.clone())))
                                                                },
                                                                "Edit"
                                                            }
                                                            button {
                                                                class: "px-2 py-0.5 text-xs text-red-400 hover:text-red-300 border border-subtle rounded transition-colors",
                                                                onclick: {
                                                                    let name = name.clone();
                                                                    move |_| {
                                                                        deleting_skill.set(Some(name.clone()));
                                                                        skill_delete_visible.set(true);
                                                                    }
                                                                },
                                                                "Delete"
                                                            }
                                                        }
                                                        if !skill.metadata.description.is_empty() {
                                                            p { class: "text-xs text-fg-muted mb-1", "{skill.metadata.description}" }
                                                        }
                                                        if let Some(tools) = &skill.metadata.allowed_tools {
                                                            div {
                                                                class: "flex flex-wrap gap-1 mt-1",
                                                                for tool in tools.clone() {
                                                                    span {
                                                                        class: "text-xs px-1.5 py-0.5 bg-primary-900/30 text-primary-300 rounded",
                                                                        "{tool}"
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
                        }

                        // Skill editor overlay
                        if let Some(draft) = editing_skill.read().clone() {
                            crate::components::skill_editor::SkillEditor {
                                skill: draft,
                                on_close: move |_| editing_skill.set(None),
                            }
                        }

                        // Delete confirmation
                        ConfirmDeleteModal {
                            is_visible: skill_delete_visible,
                            title: "Delete skill?".to_string(),
                            message: {
                                let name = deleting_skill.read().clone().unwrap_or_default();
                                let dir = skill_registry.read().get_skill(&name)
                                    .map(|s| s.root_path.display().to_string())
                                    .unwrap_or_default();
                                format!("This permanently deletes '{}' and its directory ({}).", name, dir)
                            },
                            confirm_button_text: "Yes, Delete".to_string(),
                            show_dont_ask_again: false,
                            on_confirm: move |_| {
                                skill_delete_visible.set(false);
                                let Some(name) = deleting_skill.peek().clone() else { return; };
                                deleting_skill.set(None);
                                let registry = skill_registry.peek().clone();
                                let mut session_state = session_state;
                                spawn(async move {
                                    let delete_result = tokio::task::spawn_blocking({
                                        let name = name.clone();
                                        move || registry.delete_skill(&name)
                                    }).await;
                                    match delete_result {
                                        Ok(Ok(())) => {
                                            // Purge the permission entry so it doesn't linger in the UI
                                            let removed = settings.write()
                                                .permission_settings.skill_permissions.remove(&name).is_some();
                                            if removed {
                                                let sm = settings_manager.peek().clone();
                                                sm.save_async(settings.peek().clone(), None);
                                            }
                                            // Purge the active session's loaded payload; persisted
                                            // sessions keep their self-contained snapshots by design.
                                            let (session_dirty, unload_sid) = {
                                                let mut state = session_state.write();
                                                state.get_active_session_mut()
                                                    .map(|s| (s.loaded_skills.remove(&name).is_some(), Some(s.id.clone())))
                                                    .unwrap_or((false, None))
                                            };
                                            if session_dirty {
                                                if let Some(sid) = &unload_sid {
                                                    crate::session_events::log_event(
                                                        sid,
                                                        crate::session_events::SessionEvent::SkillUnloaded {
                                                            name: name.clone(),
                                                        },
                                                    );
                                                }
                                                crate::session::SessionState::save_signal(&session_state, None);
                                            }
                                            crate::skills::SkillRegistry::reload_into_signal(skill_registry).await;
                                        }
                                        Ok(Err(e)) => {
                                            tracing::error!("Failed to delete skill '{}': {}", name, e);
                                            let mut save_error = save_error;
                                            save_error.set(Some(format!("Failed to delete skill: {}", e)));
                                        }
                                        Err(e) => {
                                            tracing::error!("Delete task failed for skill '{}': {}", name, e);
                                        }
                                    }
                                });
                            },
                            on_cancel: move |_| {
                                skill_delete_visible.set(false);
                                deleting_skill.set(None);
                            },
                        }
    }
}
