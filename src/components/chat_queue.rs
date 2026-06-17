//! Per-session FIFO queue for messages the user submits while a turn is still
//! streaming. The current turn drains one queued message when it completes; the
//! user can remove individual entries or clear a session's whole queue.
//!
//! The queue is **runtime-only** and intentionally NOT persisted: after a
//! restart the in-flight turn that made queuing necessary is gone, so
//! auto-sending a stale queued message on launch would be surprising.

use dioxus::prelude::*;
use hobbes_core::models::Attachment;
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

/// One user submission held while a session's turn is in flight.
#[derive(Clone, Debug, PartialEq)]
pub struct QueuedMessage {
    /// Stable identity so the UI can remove a specific entry even as others
    /// drain and shift position.
    pub id: Uuid,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

impl QueuedMessage {
    pub fn new(text: String, attachments: Vec<Attachment>) -> Self {
        Self {
            id: Uuid::new_v4(),
            text,
            attachments,
        }
    }
}

/// App-wide, per-session queue keyed by session id. A `GlobalSignal` (rather
/// than a context) so it survives `ChatWindow`/`ChatInput` remounts on tab
/// switches and holds queues for background sessions too.
pub static CHAT_QUEUE: GlobalSignal<HashMap<String, VecDeque<QueuedMessage>>> =
    Signal::global(HashMap::new);

// ── Pure map operations ──────────────────────────────────────────────────────
// All queue mutations go through these free functions so the FIFO/removal logic
// is unit-testable without a Dioxus runtime. Each keeps the invariant that an
// emptied session leaves no dangling key behind.

/// Append a message to the end of a session's queue (FIFO).
pub fn queue_push(
    map: &mut HashMap<String, VecDeque<QueuedMessage>>,
    session_id: &str,
    msg: QueuedMessage,
) {
    map.entry(session_id.to_string())
        .or_default()
        .push_back(msg);
}

/// Remove and return the next message to dispatch for a session (front of FIFO).
pub fn queue_pop_next(
    map: &mut HashMap<String, VecDeque<QueuedMessage>>,
    session_id: &str,
) -> Option<QueuedMessage> {
    let next = map.get_mut(session_id).and_then(|q| q.pop_front());
    if map.get(session_id).is_some_and(|q| q.is_empty()) {
        map.remove(session_id);
    }
    next
}

/// Remove a single queued message by id, preserving the order of the rest.
pub fn queue_remove(
    map: &mut HashMap<String, VecDeque<QueuedMessage>>,
    session_id: &str,
    id: Uuid,
) {
    if let Some(q) = map.get_mut(session_id) {
        q.retain(|m| m.id != id);
        if q.is_empty() {
            map.remove(session_id);
        }
    }
}

/// Drop a session's entire queue ("clear all", and on session delete / tab close).
pub fn queue_clear(map: &mut HashMap<String, VecDeque<QueuedMessage>>, session_id: &str) {
    map.remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> QueuedMessage {
        QueuedMessage::new(text.to_string(), Vec::new())
    }

    fn texts(map: &HashMap<String, VecDeque<QueuedMessage>>, sid: &str) -> Vec<String> {
        map.get(sid)
            .map(|q| q.iter().map(|m| m.text.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn push_then_pop_is_fifo() {
        let mut map = HashMap::new();
        queue_push(&mut map, "s1", msg("a"));
        queue_push(&mut map, "s1", msg("b"));
        queue_push(&mut map, "s1", msg("c"));

        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "a");
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "b");
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "c");
    }

    #[test]
    fn pop_on_empty_or_missing_is_none_and_leaves_no_key() {
        let mut map = HashMap::new();
        assert!(queue_pop_next(&mut map, "missing").is_none());

        queue_push(&mut map, "s1", msg("only"));
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "only");
        // Draining the last item removes the key so the map doesn't accumulate
        // empty deques for every session ever queued.
        assert!(!map.contains_key("s1"));
        assert!(queue_pop_next(&mut map, "s1").is_none());
    }

    #[test]
    fn remove_targets_the_id_and_keeps_order() {
        let mut map = HashMap::new();
        let (a, b, c) = (msg("a"), msg("b"), msg("c"));
        let b_id = b.id;
        queue_push(&mut map, "s1", a);
        queue_push(&mut map, "s1", b);
        queue_push(&mut map, "s1", c);

        queue_remove(&mut map, "s1", b_id);
        assert_eq!(texts(&map, "s1"), vec!["a", "c"]);
        // FIFO order of survivors is intact after the removal.
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "a");
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "c");
    }

    #[test]
    fn remove_unknown_id_is_a_no_op() {
        let mut map = HashMap::new();
        queue_push(&mut map, "s1", msg("a"));
        queue_remove(&mut map, "s1", Uuid::new_v4());
        assert_eq!(texts(&map, "s1"), vec!["a"]);
        queue_remove(&mut map, "missing", Uuid::new_v4()); // no panic on missing session
    }

    #[test]
    fn removing_last_item_drops_the_key() {
        let mut map = HashMap::new();
        let m = msg("a");
        let id = m.id;
        queue_push(&mut map, "s1", m);
        queue_remove(&mut map, "s1", id);
        assert!(!map.contains_key("s1"));
    }

    #[test]
    fn clear_empties_only_the_target_session() {
        let mut map = HashMap::new();
        queue_push(&mut map, "s1", msg("a"));
        queue_push(&mut map, "s1", msg("b"));
        queue_push(&mut map, "s2", msg("x"));

        queue_clear(&mut map, "s1");
        assert!(!map.contains_key("s1"));
        // A different session's queue is untouched.
        assert_eq!(texts(&map, "s2"), vec!["x"]);
    }

    #[test]
    fn sessions_are_isolated() {
        let mut map = HashMap::new();
        queue_push(&mut map, "s1", msg("a1"));
        queue_push(&mut map, "s2", msg("b1"));
        queue_push(&mut map, "s1", msg("a2"));

        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "a1");
        assert_eq!(queue_pop_next(&mut map, "s2").unwrap().text, "b1");
        assert_eq!(queue_pop_next(&mut map, "s1").unwrap().text, "a2");
    }
}
