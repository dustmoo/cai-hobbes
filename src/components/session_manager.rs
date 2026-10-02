#![allow(non_snake_case)]
use crate::{
    components::chat_input::ChatCommand, session::SessionState, session_store, settings::Settings,
};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use sublime_fuzzy::best_match;

#[derive(Props, PartialEq, Clone)]
pub struct SessionManagerProps {}

pub fn SessionManager(_props: SessionManagerProps) -> Element {
    let mut session_state = use_context::<Signal<SessionState>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let settings = use_context::<Signal<Settings>>();
    let mut editing_session_id = use_signal(|| None::<String>);
    let mut temp_session_name = use_signal(String::new);

    // Pagination and Filtering State
    let mut filter_query = use_signal(String::new);
    // Debounced version of filter_query — only updates 150ms after the last keystroke.
    // This prevents the expensive fuzzy search from running on every character.
    let mut debounced_query = use_signal(String::new);
    let mut current_page = use_signal(|| 0);
    let mut items_per_page = use_signal(|| 10);

    // Debounce effect: whenever filter_query changes, wait 150ms then push
    // the value into debounced_query.  A new keystroke cancels the previous timer.
    // CRITICAL: The signal read MUST happen inside the use_effect closure so Dioxus
    // tracks it as a reactive dependency and re-runs the effect on changes.
    use_effect(move || {
        let q = filter_query.read().clone();
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            debounced_query.set(q);
        });
    });

    // Listen for Global Commands (e.g., Delete Session Hotkey) removed - now handled in main.rs

    // Subscribe to session state so renames / new messages / deletions in
    // hydrated sessions re-render the list. The listing itself comes from the
    // SQLite store, so closed sessions never need to be loaded into memory.
    let sessions = session_state.read();
    let active_id = sessions.active_session_id.clone();

    let query = debounced_query.read().to_lowercase();
    let limit = *items_per_page.read();
    let offset = (*current_page.read() * limit) as i64;

    // Phase 1: DB-backed listing — exact substring match on name, summary,
    // and message text content (search_text column), newest first, paginated
    // in SQL. Phase 2 (only when exact finds nothing): fuzzy match on
    // name + summary over lightweight metadata rows.
    let (mut page_metas, total_items) = if query.is_empty() {
        let (metas, total) = session_store::list_metadata(None, offset, limit as i64);
        (metas, total as usize)
    } else {
        let (metas, total) = session_store::list_metadata(Some(&query), offset, limit as i64);
        if total > 0 {
            (metas, total as usize)
        } else {
            let mut scored: Vec<(String, isize)> = session_store::all_name_summaries()
                .into_iter()
                .filter_map(|(id, name, summary)| {
                    let name_score =
                        best_match(&query, &name.to_lowercase()).map(|m| m.score());
                    let summary_score =
                        best_match(&query, &summary.to_lowercase()).map(|m| m.score());
                    let best = match (name_score, summary_score) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                    best.map(|score| (id, score))
                })
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            let total = scored.len();
            let start = (offset as usize).min(total);
            let end = (start + limit).min(total);
            let page_ids: Vec<String> =
                scored[start..end].iter().map(|(id, _)| id.clone()).collect();
            (session_store::metadata_by_ids(&page_ids), total)
        }
    };

    // Overlay in-memory data for hydrated sessions: the async DB write can
    // lag a rename/new message by a beat, and memory is authoritative.
    for meta in page_metas.iter_mut() {
        if let Some(live) = sessions.sessions.get(&meta.id) {
            meta.name = live.name.clone();
            meta.message_count = live.messages.len() as i64;
            meta.tab_title = live.tab_title.clone();
            meta.tab_emoji = live.tab_emoji.clone();
            meta.name_user_set = live.name_user_set;
        }
    }

    let total_pages = (total_items as f64 / limit as f64).ceil() as usize;
    let total_pages = if total_pages == 0 { 1 } else { total_pages };

    // Ensure current page is valid
    if *current_page.read() >= total_pages {
        current_page.set(total_pages.saturating_sub(1));
    }

    let paginated_sessions = page_metas;
    let count_label = if total_items == 1 {
        "1 chat".to_string()
    } else {
        format!("{total_items} chats")
    };
    let hotkey_label = crate::hotkey::display_combo(&settings.read().hotkeys.toggle_history);

    rsx! {
    // Main content of the session manager
    div {
        class: "flex flex-col bg-app text-fg h-full w-full",

            // Header
            div {
                class: "h-11 flex items-baseline gap-2.5 px-4 border-b border-subtle shrink-0",
                span { class: "text-base font-semibold", "History" }
                span { class: "font-mono text-[11px] text-fg-muted", "{count_label}" }
                span { class: "ml-auto font-mono text-[11px] text-fg-muted", "{hotkey_label}" }
            }

            div {
                class: "flex flex-col flex-1 min-h-0 p-4",

            // Search Input
            div {
                class: "mb-4 relative",
                input {
                    class: "h-8 bg-app border border-faint rounded px-2.5 pl-8 text-sm text-fg focus:outline-none focus:ring-2 focus:ring-primary-500 w-full",
                    placeholder: "Search chats...",
                    value: "{filter_query}",
                    oninput: move |evt| {
                        filter_query.set(evt.value());
                        current_page.set(0); // Reset to first page on search
                    }
                }
                div {
                    class: "absolute left-2.5 top-1/2 -translate-y-1/2 text-fg-muted",
                    Icon {
                        icon: fi_icons::FiSearch,
                        width: 14,
                        height: 14,
                    }
                }
            }
            div {
                class: "flex-1 overflow-y-auto min-h-0",
                if paginated_sessions.is_empty() {
                    div { class: "text-sm text-fg-muted px-2 py-8", "No chats found." }
                } else {
                    span { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium px-2", "Chats" }
                }
                ul {
                    class: "space-y-0.5 mt-1",
                    {paginated_sessions.iter().map(|session| {
                        let active_class = if session.id == active_id { "bg-section" } else { "" };
                        let session_id = session.id.clone();
                        let session_name = session.display_title();
                        let session_emoji = session.display_emoji().map(str::to_string);
                        let session_summary = session.summary.clone();
                        let session_date = format_session_date(&session.last_updated);

                        let id_click = session_id.clone();
                        let id_edit = session_id.clone();
                        let id_delete = session_id.clone();
                        let id_keydown = session_id.clone();
                        let id_blur = session_id.clone();
                        let name_edit = session_name.clone();

                        rsx! {
                            li {
                                class: "group grid grid-cols-[minmax(0,1fr)_auto] gap-3 items-center py-2 px-2 rounded hover:bg-section cursor-pointer {active_class}",
                                key: "{session_id}",
                                onclick: move |_| {
                                    if editing_session_id.read().is_none() {
                                        chat_command.set(Some(ChatCommand::SwitchToSession(id_click.clone())));
                                    }
                                },
                                div {
                                    class: "min-w-0 flex items-start gap-1.5",
                                    // Fixed-width emoji column (same glyph treatment as
                                    // the tab sidebar) so titles stay aligned for
                                    // chats that have no emoji yet.
                                    span {
                                        class: "w-5 shrink-0 text-center text-[13px] leading-5",
                                        if let Some(e) = &session_emoji { "{e}" }
                                    }
                                    div {
                                        class: "min-w-0 flex-1",
                                    if editing_session_id.read().as_ref() == Some(&session_id) {
                                        input {
                                            class: "w-full bg-input text-fg rounded p-1 text-sm focus:outline-none focus:ring-2 focus:ring-primary-500",
                                            value: "{temp_session_name.read()}",
                                            oninput: move |evt| temp_session_name.set(evt.value()),
                                            onkeydown: move |evt| {
                                                if evt.key() == Key::Enter {
                                                    session_state.write().update_session_name(&id_keydown, temp_session_name.read().clone());
                                                    editing_session_id.set(None);
                                                } else if evt.key() == Key::Escape {
                                                    editing_session_id.set(None);
                                                }
                                            },
                                            onblur: move |_| {
                                                session_state.write().update_session_name(&id_blur, temp_session_name.read().clone());
                                                editing_session_id.set(None);
                                            }
                                        }
                                    } else {
                                        div {
                                            class: "text-sm text-fg break-words", title: "{session_name}", "{session_name}"
                                        }
                                        if !session_summary.is_empty() {
                                            div {
                                                class: "text-xs text-fg-muted truncate", "{session_summary}"
                                            }
                                        }
                                    }
                                    }
                                },
                                div {
                                    class: "flex items-center gap-2",
                                    span {
                                        class: "font-mono text-[11px] text-fg-muted tabular-nums whitespace-nowrap",
                                        "{session_date}"
                                    }
                                    div {
                                        class: "flex items-center opacity-0 group-hover:opacity-100",
                                        button {
                                            class: "p-1 rounded text-fg-muted hover:bg-card hover:text-fg mr-1",
                                            onclick: move |event| {
                                                event.stop_propagation();
                                                temp_session_name.set(name_edit.clone());
                                                editing_session_id.set(Some(id_edit.clone()));
                                            },
                                            Icon {
                                                icon: fi_icons::FiEdit2,
                                                width: 14,
                                                height: 14,
                                            }
                                        }
                                        button {
                                            class: "p-1 rounded text-fg-muted hover:bg-red-600 hover:text-fg",
                                            onclick: move |event| {
                                                event.stop_propagation();
                                                // Signal to main.rs that we want to delete this specific session
                                                // Pass the ID explicitly through the command payload
                                                chat_command.set(Some(ChatCommand::DeleteSession(id_delete.clone())));
                                            },
                                            Icon {
                                                icon: fi_icons::FiTrash2,
                                                width: 14,
                                                height: 14,
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    })}
                }
            }

            // Footer Section: Pagination + New Chat
            div {
                class: "mt-auto pt-4 border-t border-faint",

                // Controls Container
                div {
                    class: "flex flex-col gap-3 mb-4",

                    // Pagination Controls
                    div {
                        class: "flex justify-between items-center px-2 text-sm text-fg-muted",
                        button {
                            class: "p-2 rounded hover:bg-card disabled:opacity-50 disabled:cursor-not-allowed transition-colors",
                            disabled: *current_page.read() == 0,
                            onclick: move |_| {
                                let p = *current_page.read();
                                current_page.set(p - 1);
                            },
                            Icon { icon: fi_icons::FiChevronLeft, width: 18, height: 18 }
                        }
                        span {
                            class: "font-medium select-none",
                            "Page {*current_page.read() + 1} of {total_pages}"
                        }
                        button {
                            class: "p-2 rounded hover:bg-card disabled:opacity-50 disabled:cursor-not-allowed transition-colors",
                            disabled: *current_page.read() >= total_pages - 1,
                            onclick: move |_| {
                                let p = *current_page.read();
                                current_page.set(p + 1);
                            },
                            Icon { icon: fi_icons::FiChevronRight, width: 18, height: 18 }
                        }
                    }

                    // Items Per Page Selector
                    div {
                        class: "flex justify-between items-center px-2 text-xs text-fg-muted",
                        span { "Sessions per page:" }
                        select {
                            class: "bg-input border border-faint rounded px-2 py-1 text-fg text-xs focus:outline-none focus:ring-1 focus:ring-primary-500 cursor-pointer",
                            value: "{items_per_page.read()}",
                            onchange: move |evt| {
                                if let Ok(val) = evt.value().parse::<usize>() {
                                    items_per_page.set(val);
                                    current_page.set(0);
                                }
                            },
                            option { value: "10", "10" }
                            option { value: "20", "20" }
                            option { value: "50", "50" }
                            option { value: "100", "100" }
                        }
                    }
                }

                button {
                    class: "w-full px-4 py-2 bg-btn-primary rounded-md text-fg font-semibold hover:bg-btn-primary-hover focus:outline-none focus:ring-2 focus:ring-primary-500 transition-colors",
                    onclick: move |_| {
                        chat_command.set(Some(ChatCommand::NewChat));
                    },
                    "✨ New Chat"
                }
            }
            }
        }
    }
}

/// Formats a fixed-width RFC3339 `last_updated` timestamp as a short
/// human date for the row's right-aligned meta column (e.g. "Sep 16").
fn format_session_date(last_updated: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(last_updated)
        .map(|dt| dt.format("%b %-d").to_string())
        .unwrap_or_default()
}
