//! # Continuation Controller
//!
//! This module defines the `ContinuationController`, a simple service responsible for
//! orchestrating the re-invocation of the chat stream when the AI indicates it should
//! continue the conversation.
//!
//! The `in_flight` guard prevents duplicate continuations per session: if a continuation
//! is already running for a given session_id, subsequent calls for that same session are
//! suppressed until it completes. Continuations for different sessions proceed independently.

use dioxus::prelude::*;
use std::collections::HashSet;
use std::rc::Rc;

/// A function that can be called to re-trigger the LLM prompt flow.
/// Accepts the `session_id` of the originating stream so the continuation
/// always targets the correct tab, regardless of which tab is currently visible.
pub type ContinuationCallback = Rc<dyn Fn(String)>;

#[derive(Clone)]
pub struct ContinuationController {
    // Using an Option allows us to register the callback from the UI component
    // after the controller has been created and placed in context.
    callback: Signal<Option<ContinuationCallback>>,
    /// Guard: tracks which sessions have a continuation in-flight.
    /// Prevents duplicate triggers from producing parallel LLM calls
    /// for the SAME session, while allowing concurrent sessions to
    /// each have their own independent continuation chain.
    in_flight: Signal<HashSet<String>>,
}

impl ContinuationController {
    pub fn new() -> Self {
        Self {
            callback: Signal::new(None),
            in_flight: Signal::new(HashSet::new()),
        }
    }

    /// Called by a UI component (e.g., ChatWindow) to provide the function
    /// that can restart the conversation stream.
    pub fn register_callback(&mut self, callback: ContinuationCallback) {
        self.callback.write().replace(callback);
        tracing::info!("Continuation callback registered.");
    }

    /// Called by the StreamManager when a continuation hint is detected.
    /// Suppressed if a continuation is already in-flight for this session.
    /// `session_id` is the originating session so the callback targets
    /// the correct tab even if the user has switched away.
    pub fn trigger_continuation(&mut self, session_id: String) {
        if self.in_flight.read().contains(&session_id) {
            tracing::warn!(
                session_id = %session_id,
                "Continuation suppressed: another continuation is already in-flight for this session."
            );
            return;
        }
        if let Some(cb) = self.callback.read().as_ref() {
            tracing::info!("Continuation triggered for session '{}'. Invoking callback.", session_id);
            self.in_flight.write().insert(session_id.clone());
            (cb)(session_id);
        } else {
            tracing::warn!("Continuation triggered, but no callback was registered.");
        }
    }

    /// Reset the in-flight guard for a specific session after its continuation
    /// stream completes. Called from the `on_complete` handler.
    pub fn clear_in_flight(&mut self, session_id: &str) {
        self.in_flight.write().remove(session_id);
        tracing::debug!(session_id = %session_id, "Continuation in_flight guard cleared.");
    }
}
