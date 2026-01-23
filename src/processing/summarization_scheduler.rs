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

    let coroutine = use_coroutine(move |mut rx: UnboundedReceiver<SchedulerSignal>| {
        let mut session_state = session_state.to_owned();
        let settings = settings.to_owned();
        let processor = processor.to_owned();
        async move {
            let mut last_summarized_message_count = 0;
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
                            // Write the new summary back to the session
                            session_state
                                .write()
                                .get_active_session_mut()
                                .unwrap()
                                .active_context
                                .conversation_summary = summary;
                            last_summarized_message_count = current_message_count;
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

                        if current_message_count > last_summarized_message_count {
                            tracing::info!("Inactivity detected. Summarizing conversation.");
                            let processor_guard = processor.read();
                            if let Some(summary) = processor_guard
                                .generate_summary(&active_session, &settings_guard)
                                .await
                            {
                                // Write the new summary back to the session
                                session_state
                                    .write()
                                    .get_active_session_mut()
                                    .unwrap()
                                    .active_context
                                    .conversation_summary = summary;
                                last_summarized_message_count = current_message_count;
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
