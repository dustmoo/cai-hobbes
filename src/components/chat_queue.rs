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
    /// A waking dispatch relay (`todo::relay`) instead of a user message:
    /// the drain appends this incoming relay bubble and starts a turn
    /// without a user message. `text` then only labels the queue chip.
    pub relay: Option<crate::components::chat::Message>,
    /// Who the turn this starts belongs to, stated by every producer (the
    /// constructor requires it, so no call site can forget). Resolved to a
    /// `TurnOrigin` at the drain ([`resolve_origin`]). Harness-set only;
    /// never from model output.
    pub origin: QueueOrigin,
}

/// Who a queued message's turn belongs to, as its producer knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOrigin {
    /// The person: typed while a turn ran, an answer they gave in the
    /// planner.
    Person,
    /// The person by voice: a request the realtime model relayed to the
    /// brain (`voice::brain::queue_request`). The person's turn, but the
    /// words are a paraphrase, so the call risk gate applies.
    Voice,
    /// A Heartbeat wake.
    HeartbeatWake,
    /// Nobody is present: a fired timer, a relay, returned work, a message
    /// an agent hands on. Unattended in the Hobbes session, the person's
    /// anywhere else (as before the merge).
    Unattended,
}

impl QueuedMessage {
    pub fn new(text: String, attachments: Vec<Attachment>, origin: QueueOrigin) -> Self {
        Self {
            id: Uuid::new_v4(),
            text,
            attachments,
            relay: None,
            origin,
        }
    }

    /// A relay entry: delivered as the relay itself, never as user text.
    pub fn relay(message: crate::components::chat::Message) -> Self {
        let text = match &message.content {
            crate::components::shared::MessageContent::Relay(note) => {
                crate::todo::relay::summary_line(note)
            }
            other => other.display_summary(),
        };
        Self {
            id: Uuid::new_v4(),
            text,
            attachments: Vec::new(),
            relay: Some(message),
            origin: QueueOrigin::Unattended,
        }
    }
}

/// The origin of a turn a queued message starts in a session: the person's
/// or a wake's as stated; an unattended one gets the Heartbeat's strict
/// rules in the Hobbes session and is the person's anywhere else.
pub fn resolve_origin(stated: QueueOrigin, is_hobbes_session: bool) -> crate::session::TurnOrigin {
    use crate::session::TurnOrigin;
    match stated {
        QueueOrigin::Person => TurnOrigin::User,
        QueueOrigin::Voice => TurnOrigin::Voice,
        QueueOrigin::HeartbeatWake => TurnOrigin::Heartbeat,
        QueueOrigin::Unattended if is_hobbes_session => TurnOrigin::Heartbeat,
        QueueOrigin::Unattended => TurnOrigin::User,
    }
}

/// The origin the drain gives the queued message `qm` popped for
/// `session_id` (the one call the drain makes; tests use it too).
pub fn drain_origin(qm: &QueuedMessage, session_id: &str) -> crate::session::TurnOrigin {
    resolve_origin(qm.origin, crate::system_sessions::is_current(session_id))
}

/// Origins the drain resolved for messages it handed to `submit`, keyed by
/// session, until the send path takes them ([`take_turn_origin`]). The
/// drain sets one immediately before its synchronous `submit` call, so the
/// send it hands over to is the one that takes it.
static PENDING_ORIGINS: std::sync::Mutex<Option<HashMap<String, crate::session::TurnOrigin>>> =
    std::sync::Mutex::new(None);

/// The drain is about to submit a queued message for `session_id`.
pub fn set_pending_origin(session_id: &str, origin: crate::session::TurnOrigin) {
    if let Ok(mut map) = PENDING_ORIGINS.lock() {
        map.get_or_insert_with(HashMap::new).insert(session_id.to_string(), origin);
    }
}

/// The origin of the send now starting in `session_id`: the one the drain
/// left, else the person's (a typed send).
pub fn take_turn_origin(session_id: &str) -> crate::session::TurnOrigin {
    PENDING_ORIGINS
        .lock()
        .ok()
        .and_then(|mut map| map.as_mut().and_then(|m| m.remove(session_id)))
        .unwrap_or_default()
}

/// Whether nobody is present for a queued message's turn: anything but
/// the person typing or speaking (a fired timer, a relay, returned work, a
/// Heartbeat wake). Unlike [`resolve_origin`], the same in every chat.
pub fn nobody_present(origin: QueueOrigin) -> bool {
    !matches!(origin, QueueOrigin::Person | QueueOrigin::Voice)
}

type SessionSet = std::sync::Mutex<Option<std::collections::HashSet<String>>>;

/// Sessions whose next send the drain started with nobody present, until
/// the send path takes it ([`take_pending_absent`]); mirrors
/// [`PENDING_ORIGINS`].
static PENDING_ABSENT: SessionSet = std::sync::Mutex::new(None);

/// Sessions whose current turn nobody is present for ([`turn_absent`]),
/// set when a turn starts. Read by the cross-company flow guard
/// (`profile_flow::flow_person`) and the call risk gate
/// (`builtin_tools::risk_gate_applies`); every other gate (the Heartbeat's
/// included) keeps reading the turn's `TurnOrigin`, under which such a turn
/// in an ordinary chat is the person's. Runtime only: a turn doesn't
/// outlive the process.
static ABSENT_TURNS: SessionSet = std::sync::Mutex::new(None);

fn set_in(set: &SessionSet, session_id: &str, on: bool) {
    if let Ok(mut s) = set.lock() {
        let s = s.get_or_insert_with(Default::default);
        if on {
            s.insert(session_id.to_string());
        } else {
            s.remove(session_id);
        }
    }
}

/// The drain is about to submit a queued message for `session_id`.
pub fn set_pending_absent(session_id: &str, absent: bool) {
    set_in(&PENDING_ABSENT, session_id, absent);
}

/// Whether the send now starting in `session_id` has nobody present: what
/// the drain left, else not (a typed send). Taken once.
pub fn take_pending_absent(session_id: &str) -> bool {
    PENDING_ABSENT
        .lock()
        .ok()
        .and_then(|mut s| s.as_mut().map(|s| s.remove(session_id)))
        .unwrap_or(false)
}

/// A turn starts in `session_id`, with nobody present or not.
pub fn set_turn_absent(session_id: &str, absent: bool) {
    set_in(&ABSENT_TURNS, session_id, absent);
}

/// Whether nobody is present for the turn running in `session_id`.
pub fn turn_absent(session_id: &str) -> bool {
    ABSENT_TURNS.lock().ok().is_some_and(|s| s.as_ref().is_some_and(|s| s.contains(session_id)))
}

/// App-wide, per-session queue keyed by session id. A `GlobalSignal` (rather
/// than a context) so it survives `ChatWindow`/`ChatInput` remounts on tab
/// switches and holds queues for background sessions too.
pub static CHAT_QUEUE: GlobalSignal<HashMap<String, VecDeque<QueuedMessage>>> =
    Signal::global(HashMap::new);

/// Bumped to nudge `ChatInput`'s drain effect to run even when neither the
/// stream state nor the active session changed — e.g. when a fired timer
/// enqueues a prompt for the session that is *already* active.
pub static QUEUE_DRAIN_TICK: GlobalSignal<u64> = Signal::global(|| 0);

/// Sessions whose queued message was popped and handed to `submit`, until
/// its stream starts (or a 5 s failsafe): the drain's in-flight guard.
/// Change it only through [`begin_dispatch`] / [`end_dispatch`] /
/// [`release_dispatch`], which keep [`DISPATCH_IDS`] in step.
pub static DISPATCHING: GlobalSignal<std::collections::HashSet<String>> =
    Signal::global(std::collections::HashSet::new);

/// Which popped message each `DISPATCHING` entry is for, so a pop's
/// failsafe only ever releases its own guard (a later pop in the same
/// session owns the entry from then on), and so a voice brain wait can tell
/// its message is still on its way into the session.
static DISPATCH_IDS: std::sync::Mutex<Option<HashMap<String, Uuid>>> = std::sync::Mutex::new(None);

fn with_dispatch_ids<R>(f: impl FnOnce(&mut HashMap<String, Uuid>) -> R) -> R {
    let mut guard = DISPATCH_IDS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Pure bookkeeping for [`end_dispatch`]: whether the entry for
/// `session_id` belongs to the pop of `id` (and so may be released by it).
pub fn dispatch_owned_by(ids: &HashMap<String, Uuid>, session_id: &str, id: Uuid) -> bool {
    ids.get(session_id) == Some(&id)
}

/// The drain popped `id` for `session_id` and is handing it to `submit`.
pub fn begin_dispatch(session_id: &str, id: Uuid) {
    with_dispatch_ids(|ids| ids.insert(session_id.to_string(), id));
    DISPATCHING.write().insert(session_id.to_string());
}

/// The failsafe of the pop of `id`: release the guard only if it is still
/// that pop's (a later pop owns it otherwise). Returns whether it released.
pub fn end_dispatch(session_id: &str, id: Uuid) -> bool {
    let owned = with_dispatch_ids(|ids| {
        let owned = dispatch_owned_by(ids, session_id, id);
        if owned {
            ids.remove(session_id);
        }
        owned
    });
    if owned && DISPATCHING.peek().contains(session_id) {
        DISPATCHING.write().remove(session_id);
    }
    owned
}

/// The session's stream registered: whatever was dispatched has started.
pub fn release_dispatch(session_id: &str) {
    with_dispatch_ids(|ids| ids.remove(session_id));
    if DISPATCHING.peek().contains(session_id) {
        DISPATCHING.write().remove(session_id);
    }
}

/// Whether the queued message `id` is still on its way into `session_id`:
/// waiting in the queue, or popped and handed to `submit` but not started.
pub fn is_pending(session_id: &str, id: Uuid) -> bool {
    let queued = CHAT_QUEUE.peek().get(session_id).is_some_and(|q| q.iter().any(|m| m.id == id));
    queued || with_dispatch_ids(|ids| dispatch_owned_by(ids, session_id, id))
}

/// Ask the active session's queue to drain on the next reactive tick.
pub fn request_drain() {
    *QUEUE_DRAIN_TICK.write() += 1;
}

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
    // A rotated-out system session is a read-only archive: whatever is
    // still addressed to it runs in its successor (`system_sessions`).
    let target = crate::system_sessions::current_for(session_id);
    if target != session_id {
        tracing::debug!(from = session_id, to = %target, "chat queue: archived system session; queued in its successor");
    }
    map.entry(target)
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
/// Returns whether it was there to remove.
pub fn queue_remove(
    map: &mut HashMap<String, VecDeque<QueuedMessage>>,
    session_id: &str,
    id: Uuid,
) -> bool {
    let mut removed = false;
    if let Some(q) = map.get_mut(session_id) {
        let before = q.len();
        q.retain(|m| m.id != id);
        removed = q.len() != before;
        if q.is_empty() {
            map.remove(session_id);
        }
    }
    removed
}

/// Take a session's queued user messages back out (Esc puts them in the input
/// box), oldest first. Queued relays stay: they aren't the person's words.
/// Nor do voice brain requests: a live voice wait is tied to each, and the
/// framed request is not text the person typed.
pub fn queue_take_user_messages(
    map: &mut HashMap<String, VecDeque<QueuedMessage>>,
    session_id: &str,
) -> Vec<QueuedMessage> {
    let Some(q) = map.get_mut(session_id) else {
        return Vec::new();
    };
    let (relays, mine): (VecDeque<_>, VecDeque<_>) = q
        .drain(..)
        .partition(|m| m.relay.is_some() || crate::voice::brain::is_voice_request(m.text.trim()));
    *q = relays;
    if q.is_empty() {
        map.remove(session_id);
    }
    mine.into()
}

/// Drop a session's entire queue ("clear all", and on session delete / tab close).
pub fn queue_clear(map: &mut HashMap<String, VecDeque<QueuedMessage>>, session_id: &str) {
    map.remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> QueuedMessage {
        QueuedMessage::new(text.to_string(), Vec::new(), QueueOrigin::Person)
    }

    fn texts(map: &HashMap<String, VecDeque<QueuedMessage>>, sid: &str) -> Vec<String> {
        map.get(sid)
            .map(|q| q.iter().map(|m| m.text.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn taking_user_messages_back_leaves_relays_queued() {
        let mut map = HashMap::new();
        let relay = QueuedMessage::relay(crate::components::chat::Message {
            id: Uuid::new_v4(),
            author: "Relay".into(),
            content: crate::components::shared::MessageContent::Text {
                content: "from the worker".into(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            },
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: chrono::Utc::now(),
            usage: None,
        });
        queue_push(&mut map, "s", msg("a"));
        queue_push(&mut map, "s", relay);
        queue_push(&mut map, "s", msg("b"));
        let taken: Vec<String> = queue_take_user_messages(&mut map, "s").into_iter().map(|m| m.text).collect();
        assert_eq!(taken, ["a", "b"]);
        assert_eq!(map["s"].len(), 1, "the relay stays");
        assert!(queue_take_user_messages(&mut map, "s").is_empty());
        assert!(queue_take_user_messages(&mut map, "missing").is_empty());
    }

    #[test]
    fn esc_leaves_voice_requests_queued_and_remove_reports_success() {
        let mut map = HashMap::new();
        let voice = msg(&crate::voice::brain::framed_request("check CI"));
        let voice_id = voice.id;
        queue_push(&mut map, "s", msg("typed"));
        queue_push(&mut map, "s", voice);
        let taken: Vec<String> = queue_take_user_messages(&mut map, "s").into_iter().map(|m| m.text).collect();
        assert_eq!(taken, ["typed"]);
        assert_eq!(map["s"][0].id, voice_id, "the voice request stays queued");
        assert!(queue_remove(&mut map, "s", voice_id));
        assert!(!queue_remove(&mut map, "s", voice_id), "already gone");
    }

    #[test]
    fn a_pop_s_failsafe_only_releases_its_own_guard() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ids = HashMap::new();
        ids.insert("s".to_string(), b); // pop B took over after pop A
        assert!(!dispatch_owned_by(&ids, "s", a), "A's timer must not clear B's guard");
        assert!(dispatch_owned_by(&ids, "s", b));
        assert!(!dispatch_owned_by(&ids, "other", b));
    }

    /// The flow guard's view of who is present: only the person typing or
    /// speaking; a timer, relay or returned work is nobody, in any chat,
    /// while `resolve_origin` keeps calling it the person's outside the
    /// Hobbes session.
    #[test]
    fn nobody_is_present_for_timers_relays_and_returned_work() {
        assert!(!nobody_present(QueueOrigin::Person));
        assert!(!nobody_present(QueueOrigin::Voice));
        assert!(nobody_present(QueueOrigin::Unattended));
        assert!(nobody_present(QueueOrigin::HeartbeatWake));
        assert_eq!(resolve_origin(QueueOrigin::Unattended, false), crate::session::TurnOrigin::User, "other gates unchanged");
        set_pending_absent("absent-test", true);
        assert!(take_pending_absent("absent-test"));
        assert!(!take_pending_absent("absent-test"), "taken once");
        set_turn_absent("absent-test", true);
        assert!(turn_absent("absent-test"));
        set_turn_absent("absent-test", false);
        assert!(!turn_absent("absent-test"));
    }

    #[test]
    fn unstated_origins_are_unattended_only_in_the_hobbes_session() {
        use crate::session::TurnOrigin;
        assert_eq!(resolve_origin(QueueOrigin::Unattended, true), TurnOrigin::Heartbeat);
        assert_eq!(resolve_origin(QueueOrigin::Unattended, false), TurnOrigin::User);
        assert_eq!(resolve_origin(QueueOrigin::Person, true), TurnOrigin::User);
        assert_eq!(resolve_origin(QueueOrigin::Voice, true), TurnOrigin::Voice);
        assert_eq!(resolve_origin(QueueOrigin::Voice, false), TurnOrigin::Voice);
        assert_eq!(resolve_origin(QueueOrigin::HeartbeatWake, true), TurnOrigin::Heartbeat);
        assert_eq!(resolve_origin(QueueOrigin::HeartbeatWake, false), TurnOrigin::Heartbeat);
        // The drain hands its resolution to the send; a typed send finds none.
        set_pending_origin("origin-test", TurnOrigin::Heartbeat);
        assert_eq!(take_turn_origin("origin-test"), TurnOrigin::Heartbeat);
        assert_eq!(take_turn_origin("origin-test"), TurnOrigin::User);
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
        assert!(!queue_remove(&mut map, "s1", Uuid::new_v4()));
        assert_eq!(texts(&map, "s1"), vec!["a"]);
        assert!(!queue_remove(&mut map, "missing", Uuid::new_v4())); // no panic on missing session
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
