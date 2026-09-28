// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use dioxus::prelude::*;
use futures_util::StreamExt;
use tokio::time::Duration;

use super::conversation_processor::ConversationProcessor;
use crate::{session::SessionState, settings::Settings};

const INACTIVITY_DELAY: Duration = Duration::from_secs(5);

/// An enum to signal activity to the scheduler.
pub enum SchedulerSignal {
    Activity,
    /// Force a summary refresh (e.g., after profile change)
    ForceRefresh,
}

/// A hook that schedules conversation summarization based on user inactivity.
pub fn use_summarization_scheduler() {
    let session_state = use_context::<Signal<SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let processor = use_context::<Signal<ConversationProcessor>>();
    let planner = use_context::<Signal<crate::todo::PlannerState>>();

    let coroutine = use_coroutine(move |mut rx: UnboundedReceiver<SchedulerSignal>| {
        let mut session_state = session_state.to_owned();
        let settings = settings.to_owned();
        let processor = processor.to_owned();
        let planner = planner.to_owned();
        async move {
            // Per session: one shared counter meant a long tab (say 100
            // messages) blocked idle summaries for every shorter tab until it
            // outgrew that count — so newer tabs never got a summary or a
            // tab emoji/topic.
            let mut last_summarized: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            loop {
                match tokio::time::timeout(INACTIVITY_DELAY, rx.next()).await {
                    Ok(Some(SchedulerSignal::Activity)) => {
                        // Activity occurred, loop again to reset the timer.
                        continue;
                    }
                    Ok(Some(SchedulerSignal::ForceRefresh)) => {
                        // Force refresh requested - summarize immediately regardless of message count
                        tracing::info!(
                            "ForceRefresh signal received. Summarizing conversation immediately."
                        );

                        let (session, settings_guard) =
                            (session_state.read().clone(), settings.read().clone());
                        let active_session = if let Some(s) = session.get_active_session() {
                            s.clone()
                        } else {
                            continue;
                        };

                        // For forced refresh, we ignore the message count check because the context might have changed (e.g. profile switch)
                        let current_message_count = active_session.messages.len();

                        let processor_guard = processor.read();
                        if let Some(summary) = processor_guard
                            .generate_summary(&active_session, &settings_guard)
                            .await
                        {
                            let summary_value =
                                serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
                            // Write the new summary back to the session, and
                            // apply any tab emoji/topic it carries (see
                            // `apply_tab_identity_from_summary`'s doc comment
                            // — this call site used to silently drop it).
                            // Look this up by the id we actually summarized
                            // (`active_session.id`, captured before the
                            // `.await` above), NOT via
                            // `get_active_session_mut()` — the user may have
                            // switched tabs (or opened a brand new one)
                            // while the LLM call was in flight, and
                            // `get_active_session_mut()` would silently
                            // apply this summary/tab_title/tab_emoji to
                            // whatever tab is active *now* instead of the
                            // one it was actually generated for.
                            let tab_titled = {
                                let mut state = session_state.write();
                                state.sessions.get_mut(&active_session.id).and_then(|session| {
                                    let titled = crate::session::apply_tab_identity_from_summary(
                                        session, &summary,
                                    );
                                    session.active_context.conversation_summary = summary;
                                    titled
                                })
                            };
                            crate::session_events::log_event(
                                &active_session.id,
                                crate::session_events::SessionEvent::SummaryComputed {
                                    summary: summary_value,
                                },
                            );
                            if let Some((emoji, title)) = tab_titled {
                                crate::session_events::log_event(
                                    &active_session.id,
                                    crate::session_events::SessionEvent::TabTitled {
                                        emoji,
                                        title,
                                    },
                                );
                            }
                            last_summarized.insert(active_session.id.clone(), current_message_count);
                            crate::services::project_tagger::maybe_auto_tag(
                                &active_session.id,
                                session_state,
                                planner,
                            );
                            tracing::info!("Conversation summary updated (Forced).");
                        }
                        continue;
                    }
                    Ok(None) => {
                        // Channel closed, component was dropped.
                        tracing::info!("Summarization scheduler shutting down.");
                        break;
                    }
                    Err(_) => {
                        // Timeout occurred, meaning user is idle. Time to summarize.
                        let (session, settings_guard) =
                            (session_state.read().clone(), settings.read().clone());
                        let active_session = if let Some(s) = session.get_active_session() {
                            s.clone()
                        } else {
                            continue; // No active session to summarize
                        };

                        let current_message_count = active_session.messages.len();

                        let last_count =
                            last_summarized.get(&active_session.id).copied().unwrap_or(0);
                        if current_message_count > last_count && settings_guard.enable_summarization {
                            tracing::info!("Inactivity detected. Summarizing conversation.");
                            let processor_guard = processor.read();
                            if let Some(summary) = processor_guard
                                .generate_summary(&active_session, &settings_guard)
                                .await
                            {
                                let summary_value = serde_json::to_value(&summary)
                                    .unwrap_or(serde_json::Value::Null);
                                // Write the new summary back to the session,
                                // and apply any tab emoji/topic it carries —
                                // this is the path an ordinary new tab (no
                                // tool calls) actually goes through first;
                                // see `apply_tab_identity_from_summary`'s doc
                                // comment for why this used to be silently
                                // dropped here.
                                //
                                // Look up by `active_session.id` (captured
                                // before the `.await` above), not via
                                // `get_active_session_mut()` — the user may
                                // have switched to (or created) a different
                                // tab while this LLM call was in flight, and
                                // `get_active_session_mut()` would silently
                                // apply this session's summary/tab_title/
                                // tab_emoji onto whatever tab is active now
                                // instead of the one it was generated for.
                                let tab_titled = {
                                    let mut state = session_state.write();
                                    state.sessions.get_mut(&active_session.id).and_then(|session| {
                                        let titled = crate::session::apply_tab_identity_from_summary(
                                            session, &summary,
                                        );
                                        session.active_context.conversation_summary = summary;
                                        titled
                                    })
                                };
                                crate::session_events::log_event(
                                    &active_session.id,
                                    crate::session_events::SessionEvent::SummaryComputed {
                                        summary: summary_value,
                                    },
                                );
                                if let Some((emoji, title)) = tab_titled {
                                    crate::session_events::log_event(
                                        &active_session.id,
                                        crate::session_events::SessionEvent::TabTitled {
                                            emoji,
                                            title,
                                        },
                                    );
                                }
                                last_summarized.insert(active_session.id.clone(), current_message_count);
                                crate::services::project_tagger::maybe_auto_tag(
                                    &active_session.id,
                                    session_state,
                                    planner,
                                );
                                tracing::info!("Conversation summary updated.");
                            }
                        }
                    }
                }
            }
        }
    });

    use_context_provider(|| coroutine);
}
