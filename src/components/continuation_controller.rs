//! # Continuation Controller
//!
//! This module defines the `ContinuationController`, a simple service responsible for
//! orchestrating the re-invocation of the chat stream when the AI indicates it should
//! continue the conversation.

use dioxus::prelude::*;
use std::rc::Rc;

/// A function that can be called to re-trigger the LLM prompt flow.
pub type ContinuationCallback = Rc<dyn Fn()>;

#[derive(Clone)]
pub struct ContinuationController {
    // Using an Option allows us to register the callback from the UI component
    // after the controller has been created and placed in context.
    callback: Signal<Option<ContinuationCallback>>,
}

impl ContinuationController {
    pub fn new() -> Self {
        Self {
            callback: Signal::new(None),
        }
    }

    /// Called by a UI component (e.g., ChatWindow) to provide the function
    /// that can restart the conversation stream.
    pub fn register_callback(&mut self, callback: ContinuationCallback) {
        self.callback.write().replace(callback);
        tracing::info!("Continuation callback registered.");
    }

    /// Called by the StreamManager when a continuation hint is detected.
    pub fn trigger_continuation(&self) {
        if let Some(cb) = self.callback.read().as_ref() {
            tracing::info!("Continuation triggered. Invoking callback.");
            (cb)();
        } else {
            tracing::warn!("Continuation triggered, but no callback was registered.");
        }
    }
}
