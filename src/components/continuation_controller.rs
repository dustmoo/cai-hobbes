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
/// always targets the correct tab, regardless of which tab is currently
/// visible, and the origin of the turn it continues: the origin travels with
/// the turn, so nothing that lands in the session between rounds can change
/// which rules the next round runs under.
pub type ContinuationCallback = Rc<dyn Fn(String, crate::session::TurnOrigin)>;

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
    pub fn trigger_continuation(&mut self, session_id: String, origin: crate::session::TurnOrigin) {
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
            (cb)(session_id, origin);
        } else {
            tracing::warn!("Continuation triggered, but no callback was registered.");
        }
    }

    /// Whether `session_id` has a continuation in flight (no subscription).
    pub fn is_in_flight(&self, session_id: &str) -> bool {
        self.in_flight.peek().contains(session_id)
    }

    /// Sessions with a continuation in flight (subscribes the caller).
    pub fn in_flight_sessions(&self) -> HashSet<String> {
        self.in_flight.read().clone()
    }

    /// Reset the in-flight guard for a specific session after its continuation
    /// stream completes. Called from the `on_complete` handler.
    pub fn clear_in_flight(&mut self, session_id: &str) {
        self.in_flight.write().remove(session_id);
        tracing::debug!(session_id = %session_id, "Continuation in_flight guard cleared.");
    }
}

/// What an approval handed to the resume path is: an approved message (a
/// skill), or the person's "analyse now".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApprovalKey {
    Message(uuid::Uuid),
    AnalyseNow,
}

type ApprovalOrigins = std::collections::HashMap<String, std::collections::HashMap<ApprovalKey, crate::session::TurnOrigin>>;

/// The origins of approvals waiting for the resume path, per session and
/// per approved message — so a second approval (or "analyse now") while the
/// first is deferred adds an entry instead of overwriting it. Every resume of
/// the session takes them all ([`take_approval_origins`]), whichever branch
/// it runs, so none outlives its resume.
static APPROVAL_ORIGINS: std::sync::Mutex<Option<ApprovalOrigins>> = std::sync::Mutex::new(None);

/// Record the origin of the approval `key` in `session_id`.
pub fn record_approval_origin(session_id: &str, key: ApprovalKey, origin: crate::session::TurnOrigin) {
    if let Ok(mut m) = APPROVAL_ORIGINS.lock() {
        m.get_or_insert_with(Default::default)
            .entry(session_id.to_string())
            .or_default()
            .insert(key, origin);
    }
}

/// Consume every recorded approval origin of `session_id` (the resume is
/// handling all of them).
pub fn take_approval_origins(session_id: &str) -> Vec<crate::session::TurnOrigin> {
    APPROVAL_ORIGINS
        .lock()
        .ok()
        .and_then(|mut m| m.as_mut().and_then(|m| m.remove(session_id)))
        .map(|e| e.into_values().collect())
        .unwrap_or_default()
}

/// The strictest of `origins` (unattended wins), if any.
pub fn strictest(origins: impl IntoIterator<Item = crate::session::TurnOrigin>) -> Option<crate::session::TurnOrigin> {
    origins.into_iter().reduce(|a, b| if a.is_unattended() { a } else { b })
}

/// Holds a session's continuation in-flight guard for one run of the
/// continuation flow, from the trigger until the next stream is handed off
/// ([`InFlightGuard::hand_off`]; `start_stream` then releases the guard once
/// the stream maps cover the turn). Dropped any other way — an early return,
/// an error, the task aborted — it releases the guard, so a continuation that
/// never reached a stream can't leave the chat looking busy for good.
pub struct InFlightGuard {
    controller: Signal<ContinuationController>,
    session_id: String,
    armed: bool,
}

impl InFlightGuard {
    pub fn new(controller: Signal<ContinuationController>, session_id: String) -> Self {
        Self { controller, session_id, armed: true }
    }

    /// The flow reached its stream: the stream takes over the guard.
    pub fn hand_off(mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cleared = match self.controller.try_write() {
            Ok(mut c) => {
                c.clear_in_flight(&self.session_id);
                true
            }
            Err(_) => false,
        };
        if !cleared {
            // Borrowed right now (dropped mid-write): clear on the next
            // tick instead of skipping it. No runtime left means the app is
            // shutting down and nothing waits on the flag.
            let mut controller = self.controller;
            let session_id = std::mem::take(&mut self.session_id);
            let _ = dioxus::prelude::spawn_forever(async move {
                if let Ok(mut c) = controller.try_write() {
                    c.clear_in_flight(&session_id);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard dropped while the controller is borrowed still clears the
    /// flag (on the next tick) instead of silently leaving the chat busy.
    #[tokio::test]
    async fn a_guard_dropped_mid_write_still_clears() {
        thread_local! {
            static CTRL: std::cell::Cell<Option<Signal<ContinuationController>>> = const { std::cell::Cell::new(None) };
        }
        let mut dom = VirtualDom::new(|| {
            let mut c = use_signal(ContinuationController::new);
            use_hook(|| {
                c.write().register_callback(Rc::new(|_, _| {}));
                c.write().trigger_continuation("s".into(), crate::session::TurnOrigin::User);
                {
                    let guard = InFlightGuard::new(c, "s".into());
                    let mut alias = c; // Signals are Copy: borrow through a copy
                    let _held = alias.write();
                    drop(guard);
                }
                assert!(c.peek().is_in_flight("s"), "deferred, not done in the drop");
                CTRL.with(|x| x.set(Some(c)));
            });
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
        for _ in 0..5 {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), dom.wait_for_work()).await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        let c = CTRL.with(|x| x.get()).unwrap();
        assert!(!dom.in_runtime(|| c.peek().is_in_flight("s")), "cleared on the next tick");
    }

    #[test]
    fn approval_origins_are_per_message_and_taken_together() {
        use crate::session::TurnOrigin;
        let wake_skill = ApprovalKey::Message(uuid::Uuid::new_v4());
        // A wake's skill approved while a turn ran (deferred), then the
        // person's "analyse now" in the same chat: both kept.
        record_approval_origin("ao", wake_skill, TurnOrigin::Heartbeat);
        record_approval_origin("ao", ApprovalKey::AnalyseNow, TurnOrigin::User);
        let got = take_approval_origins("ao");
        assert_eq!(got.len(), 2);
        assert_eq!(strictest(got), Some(TurnOrigin::Heartbeat));
        // Taken once: nothing left for a later resume.
        assert!(take_approval_origins("ao").is_empty());
        assert_eq!(strictest(Vec::new()), None);
    }

    #[test]
    fn a_continuation_that_never_streams_releases_its_guard() {
        let mut dom = VirtualDom::new(|| {
            let mut controller = use_signal(ContinuationController::new);
            controller.write().register_callback(Rc::new(|_, _| {}));
            // Bailed out (session gone, an error, the task aborted).
            controller.write().trigger_continuation("s".into(), crate::session::TurnOrigin::User);
            assert!(controller.peek().is_in_flight("s"));
            drop(InFlightGuard::new(controller, "s".into()));
            assert!(!controller.peek().is_in_flight("s"));
            // Handed to its stream: the stream releases it, not the flow.
            controller.write().trigger_continuation("s".into(), crate::session::TurnOrigin::User);
            InFlightGuard::new(controller, "s".into()).hand_off();
            assert!(controller.peek().is_in_flight("s"));
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
}
