//! # Continuation Controller
//!
//! This module defines the `ContinuationController`, a simple service responsible for
//! orchestrating the re-invocation of the chat stream when the AI indicates it should
//! continue the conversation.
//!
//! The `in_flight` guard prevents duplicate continuations: if a continuation is already
//! running (e.g., from a skill trigger), subsequent calls from the stream_manager are
//! suppressed until the current continuation completes.

use dioxus::prelude::*;
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
    /// Guard: true while a continuation is in-flight (LLM stream running).
    /// Prevents duplicate triggers from producing parallel LLM calls.
    in_flight: Signal<bool>,
}

impl ContinuationController {
    pub fn new() -> Self {
        Self {
            callback: Signal::new(None),
            in_flight: Signal::new(false),
        }
    }

    /// Called by a UI component (e.g., ChatWindow) to provide the function
    /// that can restart the conversation stream.
    pub fn register_callback(&mut self, callback: ContinuationCallback) {
        self.callback.write().replace(callback);
        tracing::info!("Continuation callback registered.");
    }

    /// Called by the StreamManager when a continuation hint is detected.
    /// Suppressed if a continuation is already in-flight.
    /// `session_id` is the originating session so the callback targets
    /// the correct tab even if the user has switched away.
    pub fn trigger_continuation(&mut self, session_id: String) {
        if *self.in_flight.read() {
            tracing::warn!(
                "Continuation suppressed: another continuation is already in-flight."
            );
            return;
        }
        if let Some(cb) = self.callback.read().as_ref() {
            tracing::info!("Continuation triggered for session '{}'. Invoking callback.", session_id);
            self.in_flight.set(true);
            (cb)(session_id);
        } else {
            tracing::warn!("Continuation triggered, but no callback was registered.");
        }
    }

    /// Reset the in-flight guard after a continuation stream completes.
    /// Called from the `on_complete` handler or after `send_prompt_to_llm` finishes.
    pub fn clear_in_flight(&mut self) {
        self.in_flight.set(false);
        tracing::debug!("Continuation in_flight guard cleared.");
    }

    /// Unconditionally reset the in_flight guard.
    /// Called on session/tab switch to prevent stale guards from blocking new work.
    pub fn force_reset(&mut self) {
        if *self.in_flight.read() {
            tracing::warn!("ContinuationController: force-resetting stale in_flight guard.");
            self.in_flight.set(false);
        }
    }
}
