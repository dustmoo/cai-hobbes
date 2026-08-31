#![allow(non_snake_case)]
// Full-screen skill editor overlay: a friendly, Bear-like markdown editor for
// creating and editing skills. Frontmatter is owned by the metadata form (never
// edited as raw YAML), the body is a plain markdown textarea with a live
// preview toggle rendered by MarkdownRenderer.

use crate::components::confirm_delete_modal::ConfirmDeleteModal;
use crate::components::focus_context::FocusContext;
use crate::components::markdown_renderer::MarkdownRenderer;
use crate::session::SessionState;
use crate::settings::{Settings, SettingsManager};
use crate::skills::parser::{Skill, SkillMetadata};
use crate::skills::SkillRegistry;
use dioxus::prelude::*;
use std::path::PathBuf;

#[derive(Props, Clone, PartialEq)]
pub struct SkillEditorProps {
    /// None = creating a new skill; Some = editing an existing one.
    pub skill: Option<Skill>,
    pub on_close: EventHandler<()>,
}

/// Build a self-contained HTML document from a skill for export.
fn skill_to_html(name: &str, description: &str, instructions: &str) -> String {
    let mut body = String::new();
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(instructions, options);
    pulldown_cmark::html::push_html(&mut body, parser);

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>{name}</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; max-width: 46rem; margin: 3rem auto; padding: 0 1.5rem; line-height: 1.6; color: #1f2328; }}
h1, h2, h3 {{ line-height: 1.25; }}
code {{ background: #f0f1f3; padding: 0.15em 0.35em; border-radius: 4px; font-size: 0.9em; }}
pre {{ background: #f6f8fa; padding: 1rem; border-radius: 8px; overflow-x: auto; }}
pre code {{ background: none; padding: 0; }}
blockquote {{ border-left: 3px solid #d0d7de; margin-left: 0; padding-left: 1rem; color: #57606a; }}
table {{ border-collapse: collapse; }}
th, td {{ border: 1px solid #d0d7de; padding: 0.35rem 0.7rem; }}
.description {{ color: #57606a; font-style: italic; }}
</style>
</head>
<body>
<h1>{name}</h1>
<p class="description">{description}</p>
{body}
</body>
</html>
"#
    )
}

#[component]
pub fn SkillEditor(props: SkillEditorProps) -> Element {
    let skill_registry = use_context::<Signal<SkillRegistry>>();
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let session_state = use_context::<Signal<SessionState>>();
    let mut focus_context = use_context::<Signal<FocusContext>>();

    let original = props.skill.clone();
    let original_name = original.as_ref().map(|s| s.metadata.name.clone());
    let is_new = original.is_none();

    // Form state, seeded from the skill being edited (or blank for a new one)
    let mut name = use_signal(|| {
        original
            .as_ref()
            .map(|s| s.metadata.name.clone())
            .unwrap_or_default()
    });
    let mut description = use_signal(|| {
        original
            .as_ref()
            .map(|s| s.metadata.description.clone())
            .unwrap_or_default()
    });
    let mut argument_hint = use_signal(|| {
        original
            .as_ref()
            .and_then(|s| s.metadata.argument_hint.clone())
            .unwrap_or_default()
    });
    let mut allowed_tools = use_signal(|| {
        original
            .as_ref()
            .and_then(|s| s.metadata.allowed_tools.clone())
            .unwrap_or_default()
    });
    let mut tool_draft = use_signal(String::new);
    let mut user_invocable = use_signal(|| {
        original
            .as_ref()
            .map(|s| s.metadata.user_invocable)
            .unwrap_or(true)
    });
    let mut model_invocable = use_signal(|| {
        original
            .as_ref()
            .map(|s| !s.metadata.disable_model_invocation)
            .unwrap_or(true)
    });
    let mut instructions = use_signal(|| {
        original
            .as_ref()
            .map(|s| s.instructions.clone())
            .unwrap_or_default()
    });

    let mut show_preview = use_signal(|| false);
    let mut show_metadata = use_signal(|| is_new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut show_discard_confirm = use_signal(|| false);
    let mut saving = use_signal(|| false);

    // Claim keyboard focus while the editor is open; release on drop/close.
    use_effect(move || {
        focus_context.set(FocusContext::SkillEditorModal);
    });
    use_drop(move || {
        focus_context.set(FocusContext::ChatInput);
    });

    // Snapshot of the initial form values for dirty checking
    let initial_snapshot = use_hook(|| {
        (
            name.peek().clone(),
            description.peek().clone(),
            argument_hint.peek().clone(),
            allowed_tools.peek().clone(),
            *user_invocable.peek(),
            *model_invocable.peek(),
            instructions.peek().clone(),
        )
    });
    let is_dirty = {
        let snap = initial_snapshot.clone();
        move || {
            snap != (
                name.peek().clone(),
                description.peek().clone(),
                argument_hint.peek().clone(),
                allowed_tools.peek().clone(),
                *user_invocable.peek(),
                *model_invocable.peek(),
                instructions.peek().clone(),
            )
        }
    };

    let build_metadata = move || SkillMetadata {
        name: name.peek().trim().to_string(),
        description: description.peek().trim().to_string(),
        disable_model_invocation: !*model_invocable.peek(),
        user_invocable: *user_invocable.peek(),
        allowed_tools: {
            let tools = allowed_tools.peek().clone();
            if tools.is_empty() {
                None
            } else {
                Some(tools)
            }
        },
        argument_hint: {
            let hint = argument_hint.peek().trim().to_string();
            if hint.is_empty() {
                None
            } else {
                Some(hint)
            }
        },
    };

    let request_close = {
        let is_dirty = is_dirty.clone();
        let on_close = props.on_close;
        move || {
            if is_dirty() {
                let mut show = show_discard_confirm;
                show.set(true);
            } else {
                on_close.call(());
            }
        }
    };

    let on_save = {
        let original_name = original_name.clone();
        let on_close = props.on_close;
        move |_: MouseEvent| {
            if *saving.peek() {
                return;
            }
            let metadata = build_metadata();
            if let Err(errs) = metadata.validate() {
                error.set(Some(errs.join("; ")));
                return;
            }
            let skill = Skill {
                metadata,
                instructions: instructions.peek().clone(),
                path: PathBuf::new(),
                root_path: PathBuf::new(),
                scripts: Vec::new(),
                resources: Vec::new(),
            };
            let original_name = original_name.clone();
            let registry = skill_registry.peek().clone();
            saving.set(true);
            error.set(None);
            let mut session_state = session_state;
            spawn(async move {
                let result = tokio::task::spawn_blocking({
                    let skill = skill.clone();
                    let original_name = original_name.clone();
                    move || match &original_name {
                        Some(orig) => registry.save_skill(orig, &skill),
                        None => registry.create_skill(&skill),
                    }
                })
                .await;

                match result {
                    Ok(Ok(saved)) => {
                        let new_name = saved.metadata.name.clone();
                        // Rename: migrate the permission entry and the active
                        // session's loaded-skill payload to the new key.
                        if let Some(orig) = original_name.as_deref() {
                            if orig != new_name {
                                let migrated = {
                                    let mut s = settings.write();
                                    s.permission_settings
                                        .skill_permissions
                                        .remove(orig)
                                        .map(|allowed| {
                                            s.permission_settings
                                                .skill_permissions
                                                .insert(new_name.clone(), allowed)
                                        })
                                        .is_some()
                                };
                                if migrated {
                                    let sm = settings_manager.peek().clone();
                                    sm.save_async(settings.peek().clone(), None);
                                }
                                let (session_dirty, rename_log) = {
                                    let mut state = session_state.write();
                                    if let Some(session) = state.get_active_session_mut() {
                                        if let Some(payload) =
                                            session.loaded_skills.remove(orig)
                                        {
                                            session
                                                .loaded_skills
                                                .insert(new_name.clone(), payload.clone());
                                            (true, Some((session.id.clone(), payload)))
                                        } else {
                                            (false, None)
                                        }
                                    } else {
                                        (false, None)
                                    }
                                };
                                if let Some((sid, payload)) = rename_log {
                                    crate::session_events::log_events(
                                        &sid,
                                        vec![
                                            crate::session_events::SessionEvent::SkillUnloaded {
                                                name: orig.to_string(),
                                            },
                                            crate::session_events::SessionEvent::SkillLoaded {
                                                name: new_name.clone(),
                                                payload,
                                            },
                                        ],
                                    );
                                }
                                if session_dirty {
                                    SessionState::save_signal(&session_state, None);
                                }
                            }
                        }
                        SkillRegistry::reload_into_signal(skill_registry).await;
                        saving.set(false);
                        on_close.call(());
                    }
                    Ok(Err(e)) => {
                        saving.set(false);
                        error.set(Some(e));
                    }
                    Err(e) => {
                        saving.set(false);
                        error.set(Some(format!("Save task failed: {}", e)));
                    }
                }
            });
        }
    };

    let export_md = move |_: MouseEvent| {
        let metadata = build_metadata();
        let skill = Skill {
            metadata,
            instructions: instructions.peek().clone(),
            path: PathBuf::new(),
            root_path: PathBuf::new(),
            scripts: Vec::new(),
            resources: Vec::new(),
        };
        let file_name = if skill.metadata.name.is_empty() {
            "skill.md".to_string()
        } else {
            format!("{}.md", skill.metadata.name)
        };
        match skill.to_markdown() {
            Ok(markdown) => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(&file_name)
                    .add_filter("Markdown", &["md"])
                    .save_file()
                {
                    if let Err(e) = std::fs::write(&path, markdown) {
                        error.set(Some(format!("Export failed: {}", e)));
                    }
                }
            }
            Err(e) => error.set(Some(format!("Export failed: {}", e))),
        }
    };

    let export_html = move |_: MouseEvent| {
        let skill_name = name.peek().trim().to_string();
        let html = skill_to_html(
            if skill_name.is_empty() { "Untitled skill" } else { &skill_name },
            description.peek().trim(),
            &instructions.peek(),
        );
        let file_name = if skill_name.is_empty() {
            "skill.html".to_string()
        } else {
            format!("{}.html", skill_name)
        };
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(&file_name)
            .add_filter("HTML", &["html"])
            .save_file()
        {
            if let Err(e) = std::fs::write(&path, html) {
                error.set(Some(format!("Export failed: {}", e)));
            }
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center bg-black/60",
            onkeydown: {
                let request_close = request_close.clone();
                move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape {
                        evt.stop_propagation();
                        request_close();
                    }
                }
            },

            div {
                class: "bg-section border border-faint rounded-lg shadow-xl flex flex-col w-[90vw] h-[90vh] max-w-4xl",
                onclick: |evt| evt.stop_propagation(),

                // ── Header: title input + actions ─────────────────────────
                div {
                    class: "flex items-center gap-3 px-5 pt-4 pb-2",
                    input {
                        class: "flex-1 bg-transparent text-xl font-semibold text-fg placeholder-fg-muted outline-none border-none",
                        placeholder: "skill-name",
                        autofocus: is_new,
                        // Uncontrolled (initial_value): a controlled binding races
                        // re-renders and snaps the caret to the end mid-typing.
                        initial_value: "{name}",
                        oninput: move |evt| name.set(evt.value()),
                    }
                    button {
                        class: "px-2.5 py-1 text-xs text-fg-muted hover:text-fg border border-subtle rounded-md transition-colors",
                        onclick: export_md,
                        "Export MD"
                    }
                    button {
                        class: "px-2.5 py-1 text-xs text-fg-muted hover:text-fg border border-subtle rounded-md transition-colors",
                        onclick: export_html,
                        "Export HTML"
                    }
                    button {
                        class: "text-fg-muted hover:text-fg text-xl font-bold w-8 h-8 flex items-center justify-center rounded hover:bg-input transition-colors",
                        onclick: {
                            let request_close = request_close.clone();
                            move |_| request_close()
                        },
                        "×"
                    }
                }

                // ── Metadata strip (collapsible) ───────────────────────────
                div {
                    class: "px-5",
                    button {
                        class: "text-xs text-fg-muted hover:text-fg transition-colors mb-2",
                        onclick: move |_| {
                            let open = *show_metadata.peek();
                            show_metadata.set(!open);
                        },
                        if *show_metadata.read() { "▾ Metadata" } else { "▸ Metadata" }
                    }
                    if *show_metadata.read() {
                        div {
                            class: "space-y-2 mb-3 p-3 border border-subtle rounded-md bg-black/10",
                            div {
                                class: "flex flex-col gap-1",
                                label { class: "text-xs text-fg-muted", "Description" }
                                input {
                                    class: "bg-input text-sm text-fg rounded-md px-2 py-1.5 outline-none border border-subtle focus:border-primary-500",
                                    placeholder: "What this skill teaches Hobbes to do",
                                    initial_value: "{description}",
                                    oninput: move |evt| description.set(evt.value()),
                                }
                            }
                            div {
                                class: "flex flex-col gap-1",
                                label { class: "text-xs text-fg-muted", "Argument hint (shown in autocomplete)" }
                                input {
                                    class: "bg-input text-sm text-fg rounded-md px-2 py-1.5 outline-none border border-subtle focus:border-primary-500",
                                    placeholder: "<topic>",
                                    initial_value: "{argument_hint}",
                                    oninput: move |evt| argument_hint.set(evt.value()),
                                }
                            }
                            div {
                                class: "flex flex-col gap-1",
                                label { class: "text-xs text-fg-muted", "Allowed tools (press Enter to add)" }
                                div {
                                    class: "flex flex-wrap items-center gap-1.5",
                                    for (idx, tool) in allowed_tools.read().iter().enumerate() {
                                        span {
                                            class: "flex items-center gap-1 text-xs px-1.5 py-0.5 bg-primary-900/30 text-primary-300 rounded",
                                            "{tool}"
                                            button {
                                                class: "hover:text-fg font-bold",
                                                onclick: move |_| {
                                                    allowed_tools.write().remove(idx);
                                                },
                                                "×"
                                            }
                                        }
                                    }
                                    input {
                                        id: "skill-editor-tool-draft",
                                        class: "bg-input text-xs text-fg rounded-md px-2 py-1 outline-none border border-subtle focus:border-primary-500 w-40",
                                        placeholder: "tool name",
                                        initial_value: "{tool_draft}",
                                        oninput: move |evt| tool_draft.set(evt.value()),
                                        onkeydown: move |evt: KeyboardEvent| {
                                            if evt.key() == Key::Enter {
                                                evt.prevent_default();
                                                let tool = tool_draft.peek().trim().to_string();
                                                if !tool.is_empty()
                                                    && !allowed_tools.peek().contains(&tool)
                                                {
                                                    allowed_tools.write().push(tool);
                                                }
                                                tool_draft.set(String::new());
                                                // Uncontrolled input: clear the DOM value explicitly
                                                let _ = document::eval(r#"
                                                    const el = document.getElementById('skill-editor-tool-draft');
                                                    if (el) { el.value = ''; }
                                                "#);
                                            }
                                        },
                                    }
                                }
                            }
                            div {
                                class: "flex items-center gap-5 pt-1",
                                label {
                                    class: "flex items-center gap-2 text-xs text-fg-muted select-none cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        checked: *user_invocable.read(),
                                        onchange: move |evt| user_invocable.set(evt.checked()),
                                    }
                                    "User can invoke with /"
                                }
                                label {
                                    class: "flex items-center gap-2 text-xs text-fg-muted select-none cursor-pointer",
                                    input {
                                        r#type: "checkbox",
                                        checked: *model_invocable.read(),
                                        onchange: move |evt| model_invocable.set(evt.checked()),
                                    }
                                    "Model can invoke"
                                }
                            }
                        }
                    }
                }

                // ── Body: markdown editor / preview ────────────────────────
                div {
                    class: "flex-1 flex flex-col min-h-0 px-5",
                    div {
                        class: "flex justify-end mb-1",
                        button {
                            class: "px-2.5 py-1 text-xs text-fg-muted hover:text-fg border border-subtle rounded-md transition-colors",
                            onclick: move |_| {
                                let preview = *show_preview.peek();
                                show_preview.set(!preview);
                            },
                            if *show_preview.read() { "✎ Edit" } else { "👁 Preview" }
                        }
                    }
                    if *show_preview.read() {
                        div {
                            class: "flex-1 overflow-y-auto border border-subtle rounded-md p-4 bg-black/10 prose prose-sm dark:prose-invert max-w-none",
                            MarkdownRenderer {
                                content: instructions.read().clone(),
                                comments: None,
                                pending_highlight: None,
                            }
                        }
                    } else {
                        textarea {
                            class: "flex-1 resize-none bg-input text-sm text-fg rounded-md p-4 outline-none border border-subtle focus:border-primary-500 font-mono leading-relaxed",
                            placeholder: "# Instructions\n\nWrite the skill's instructions in Markdown…",
                            // Uncontrolled: re-renders (e.g. background state redraws)
                            // must never rewrite the value while the user is typing.
                            // Toggling to Preview unmounts this node; on remount the
                            // fresh initial_value picks up the current signal.
                            initial_value: "{instructions}",
                            oninput: move |evt| instructions.set(evt.value()),
                        }
                    }
                }

                // ── Footer: validation error + actions ─────────────────────
                div {
                    class: "flex items-center gap-3 px-5 py-4",
                    if let Some(err) = error.read().as_ref() {
                        span { class: "flex-1 text-xs text-red-400", "{err}" }
                    } else {
                        span { class: "flex-1" }
                    }
                    button {
                        class: "px-4 py-2 bg-input rounded-md text-fg text-sm font-semibold hover:bg-input transition-colors",
                        onclick: {
                            let request_close = request_close.clone();
                            move |_| request_close()
                        },
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-btn-primary hover:bg-btn-primary-hover rounded-md text-sm font-bold transition-colors disabled:opacity-50",
                        disabled: *saving.read(),
                        onclick: on_save,
                        if *saving.read() { "Saving…" } else { "Save" }
                    }
                }
            }
        }

        ConfirmDeleteModal {
            is_visible: show_discard_confirm,
            title: "Discard changes?".to_string(),
            message: "You have unsaved changes. Discard them and close the editor?".to_string(),
            confirm_button_text: "Discard".to_string(),
            show_dont_ask_again: false,
            on_confirm: {
                let on_close = props.on_close;
                move |_| {
                    show_discard_confirm.set(false);
                    on_close.call(());
                }
            },
            on_cancel: move |_| show_discard_confirm.set(false),
        }
    }
}
