//! `VOICE_ASK_BRAIN`: hand a request from the voice tier to the global
//! connector. The request is queued into the Hobbes session through the
//! background chat queue (the same path a Heartbeat wake uses), so the brain
//! runs a normal turn — full prompt builder, tools, trust rules, permission
//! bubbles — on whatever `active_connector_id` points at (the Hobbes session
//! is never pinned). This module waits for that turn and returns its text.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::chat_queue;
use crate::components::shared::MessageContent;
use crate::session::SessionState;

/// Longest the voice tier waits on one brain request.
const BRAIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_millis(300);
/// Fallback wake while waiting for the turn to end, in case a [`wake`] is missed.
const END_FALLBACK: Duration = Duration::from_millis(500);
/// The re-check after a first idle reading (see [`IDLE_POLLS`]).
const CONFIRM_POLL: Duration = Duration::from_millis(150);
/// Consecutive idle readings before a turn counts as over: the turn-in-flight
/// signal is a mirror one reactive tick behind the stream manager.
const IDLE_POLLS: u32 = 2;
/// Polls a request may be neither queued, dispatching nor in the session
/// before the wait gives up on it (Esc took it back, its tab was closed).
const MISSING_POLLS: u32 = 4;
/// Polls a cancel waits for a just-started turn's stream to appear, so it
/// can be stopped, before the wait returns anyway.
const CANCEL_POLLS: u32 = 20;

static TURN_END: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Signalled at every turn end (`voice::controller::on_turn_end`) and when
/// the turn-in-flight mirror drops a session (`main.rs` `StreamBridge`), so
/// the brain waits wake immediately instead of on their next tick. Every
/// waiter is woken (several requests can be queued at once, each wait
/// resolving at its own turn's end); a wait not parked at that instant
/// catches up on its [`END_FALLBACK`] tick.
pub fn wake() {
    TURN_END.notify_waiters();
}
/// Most text handed back to the voice tier.
const MAX_RESULT_CHARS: usize = 4000;

/// Harness notices written as Hobbes text: not the brain's answer.
const TURN_LIMIT_PREFIX: &str = "Pardon, I have reached the 'Max Turn Limit'";
const EMPTY_RESPONSE_PREFIX: &str = "⚠️ The model returned an empty response";
const DETAILS_POINTER: &str = "(Full details are in the Hobbes tab.)";

#[derive(Clone, Debug, PartialEq)]
pub enum BrainOutcome {
    Done(String),
    /// The turn is held on a permission bubble in the Hobbes tab.
    NeedsApproval(String),
    Failed(String),
    Cancelled,
}

/// The queued text: the request, framed for a spoken reply.
pub fn framed_request(request: &str) -> String {
    format!(
        "🎙 {}\n\n[Voice request, relayed by Hobbes' voice layer from a live spoken conversation. \
         The line above is the voice layer's summary, not the person's exact words; theirs are \
         the earlier 🎙 lines. Do the work first; your final reply is read aloud and should say \
         only what has actually been done, or what you need from them. A go-ahead for anything \
         irreversible counts only if their own 🎙 lines say it; otherwise ask. Write the reply \
         as plain speech: no markdown, no lists, no code, no URLs. Keep it to a few sentences unless they asked for \
         detail or a walkthrough, or the answer needs it; then explain fully in spoken \
         paragraphs, leading with the point. Put anything that can't be spoken (code, links, \
         tables, long ids) after a line that says \"Details:\" and the voice will point the \
         person to the Hobbes tab.]",
        request.trim()
    )
}

fn is_harness_notice(text: &str) -> bool {
    text.starts_with(TURN_LIMIT_PREFIX) || text.starts_with(EMPTY_RESPONSE_PREFIX)
}

/// A message that is part of a tool round: a call, a skill, or a call or
/// skill held on approval.
fn is_tool_activity(m: &Message) -> bool {
    matches!(
        m.content,
        MessageContent::ToolCall(_)
            | MessageContent::PermissionRequest(_)
            | MessageContent::SkillCall(_)
            | MessageContent::SkillPermissionRequest(_)
    )
}

/// Index one past the turn that the request at `anchor` starts: the next
/// message that opens a turn of its own, or the end. Never below
/// `anchor + 1` and never past `messages.len()`.
fn turn_end(messages: &[Message], anchor: usize) -> usize {
    let start = anchor.saturating_add(1).min(messages.len());
    messages[start..]
        .iter()
        .position(ends_turn)
        .map(|i| start + i)
        .unwrap_or(messages.len())
}

/// The messages of the turn the request at `anchor` starts (empty when
/// `anchor` is out of range).
fn turn_slice(messages: &[Message], anchor: usize) -> &[Message] {
    let start = anchor.saturating_add(1).min(messages.len());
    &messages[start..turn_end(messages, anchor)]
}

/// `text` without anything imitating a voice transcript line: a line that
/// starts with a heard/spoken marker is dropped, and a marker mid-line cuts
/// the rest of that line.
fn strip_transcript_markers(text: &str) -> String {
    let markers = [
        crate::voice::session::SPOKE_PREFIX.trim_end(),
        crate::voice::session::HEARD_PREFIX.trim_end(),
    ];
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let cut = markers.iter().filter_map(|m| line.find(m)).min();
        match cut {
            Some(i) if line[..i].trim().is_empty() => continue,
            Some(i) => out.push(line[..i].trim_end()),
            None => out.push(line),
        }
    }
    out.join("\n").trim().to_string()
}

/// Cut a "Details:" tail (only where "Details:" starts a line). The pointer
/// to the Hobbes tab is added only when the cut removed real content.
fn cut_details(text: &str) -> String {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Details:") {
            let head = text[..offset].trim_end();
            let tail = format!("{}{}", rest, &text[offset + line.len()..]);
            let substantial = tail.chars().filter(|c| c.is_alphanumeric()).count() >= 2;
            return match (head.is_empty(), substantial) {
                (_, false) => head.to_string(),
                (true, true) => DETAILS_POINTER.to_string(),
                (false, true) => format!("{head} {DETAILS_POINTER}"),
            };
        }
        offset += line.len();
    }
    text.to_string()
}

/// The brain's reply to the request at `anchor`: the text of the turn's
/// final answer — the assistant text after the turn's last tool call (a
/// tool round's own narration, which sits right after its calls, is left
/// out; texts joined across an auto-recovery prompt are kept together), or
/// all of the turn's text when it made no tool calls. Voice transcript
/// lines, harness notices (turn limit, empty response) and anything after a
/// voice marker are skipped; a "Details:" tail becomes a pointer.
pub fn collect_reply(messages: &[Message], anchor: usize) -> String {
    let turn = turn_slice(messages, anchor);
    let last_tool = turn.iter().rposition(is_tool_activity);
    // (text, joined to the previous text by an auto-recovery prompt)
    let mut texts: Vec<(String, bool)> = Vec::new();
    let mut recovery_since_last = false;
    for m in &turn[last_tool.map_or(0, |i| i + 1)..] {
        if m.author == "User" {
            recovery_since_last = true; // only recovery prompts stay inside a turn
            continue;
        }
        let MessageContent::Text { content, .. } = &m.content else { continue };
        let t = content.trim();
        if t.is_empty() || is_harness_notice(t) {
            continue;
        }
        let t = strip_transcript_markers(t);
        if t.is_empty() {
            continue;
        }
        texts.push((t, recovery_since_last));
        recovery_since_last = false;
    }
    let chosen: Vec<String> = if last_tool.is_none() {
        texts.into_iter().map(|(t, _)| t).collect()
    } else {
        // The final round's text, plus earlier texts tied to it by recovery.
        let mut from = texts.len().saturating_sub(1);
        while from > 0 && texts[from].1 {
            from -= 1;
        }
        texts.into_iter().skip(from).map(|(t, _)| t).collect()
    };
    let mut text = cut_details(&chosen.join("\n\n"));
    if text.chars().count() > MAX_RESULT_CHARS {
        text = text.chars().take(MAX_RESULT_CHARS).collect::<String>() + "…";
    }
    text
}

/// Whether `m` closes the turn before it: a user-authored message (a later
/// request, a heard transcript line, a typed message) that is not an
/// auto-recovery prompt (those continue the turn they are in), or an
/// incoming dispatch relay that woke a turn of its own (no user message
/// starts that turn: `todo::relay::starts_turn`).
fn ends_turn(m: &Message) -> bool {
    (m.author == "User" && !crate::voice::transcript::is_recovery_prompt(m)) || crate::todo::relay::starts_turn(m)
}

/// The turn answering the request at `anchor` is over because a later
/// request or message follows it. True even while the session streams that
/// later turn (queued requests run back to back, so "the session is
/// streaming" says nothing about this one).
pub fn turn_ended_by_later_message(messages: &[Message], anchor: usize) -> bool {
    turn_end(messages, anchor) < messages.len()
}

/// Whether `m` is the message the drain appended for the queued `framed`
/// request. Matched by prefix: the send may append to it (a fleet
/// @-mention's context block).
fn is_request_message(m: &Message, framed: &str) -> bool {
    m.author == "User"
        && m.content.get_text_content().is_some_and(|t| t.trim().starts_with(framed.trim()))
}

/// Index of this request's own message (the drain appends it as a User
/// message starting with the queued text), searching from `from`. The wait
/// itself anchors by id ([`find_request_message`]); this is for tests.
#[cfg(test)]
pub fn find_anchor(messages: &[Message], from: usize, framed: &str) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, m)| is_request_message(m, framed))
        .map(|(i, _)| i)
}

/// The wait's anchor: the first message for `framed` appended no earlier
/// than the request was queued that no other wait has claimed. Found by
/// time and id, not index, so lines written into the session before it
/// (voice transcript) or removed from it can't misaim it.
fn find_request_message(
    messages: &[Message],
    framed: &str,
    queued_at: chrono::DateTime<chrono::Utc>,
    claimed: &HashSet<uuid::Uuid>,
) -> Option<uuid::Uuid> {
    let since = queued_at - chrono::Duration::milliseconds(500);
    messages
        .iter()
        .find(|m| m.created_at >= since && !claimed.contains(&m.id) && is_request_message(m, framed))
        .map(|m| m.id)
}

/// The turn that answers the request at `anchor` is held on a permission
/// bubble (only messages after the anchor count; an old unanswered bubble
/// elsewhere in the session does not).
fn turn_needs_approval(messages: &[Message], anchor: usize) -> bool {
    crate::heartbeat::wake::has_pending_approval(turn_slice(messages, anchor))
}

/// The turn stopped on the turn-limit notice (it needs the person to go on).
fn turn_hit_limit(messages: &[Message], anchor: usize) -> bool {
    turn_slice(messages, anchor).iter().any(|m| {
        m.author != "User" && m.content.get_text_content().is_some_and(|t| t.trim().starts_with(TURN_LIMIT_PREFIX))
    })
}

/// How the controller stops a wait.
#[derive(Clone, Default)]
pub struct BrainHandle {
    /// Stop the brain's turn too (VOICE_CANCEL_BRAIN).
    pub cancel: Arc<AtomicBool>,
    /// Only stop waiting; the turn keeps running in the Hobbes tab
    /// (the conversation ended).
    pub detach: Arc<AtomicBool>,
}

/// Brain waits in progress (not detached). Kept for session rotation's
/// quiet-moment check; announcement suppression uses [`wait_owns`].
static WAITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn waits_in_flight() -> usize {
    WAITS.load(Ordering::SeqCst)
}

struct WaitGuard;
impl WaitGuard {
    fn new() -> Self {
        WAITS.fetch_add(1, Ordering::SeqCst);
        WaitGuard
    }
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        WAITS.fetch_sub(1, Ordering::SeqCst);
    }
}

// ── Per-request ownership of the turn-end announcement ───────────────────────
//
// The rule: a voice request's turn is announced at its end (controller
// `on_turn_end`) unless a wait owns it. A wait owns its request from the
// moment it queues it until the wait returns, as long as it is not detached;
// it then gives ownership up on every exit except `Done`, which leaves a
// "delivered" mark on the request message so that turn is never announced
// later either. So:
// - a wait that delivers `Done` returns only after the real turn end (two
//   idle readings, or a later message), by which time `on_turn_end` has
//   already run and found it owned — not announced, spoken once by the wait;
// - a wait that gives up early (NeedsApproval, timeout, cancel, detach,
//   failure) has released it before the turn finally ends — announced then;
// - a detach takes effect at once (the flag is read here), even before the
//   wait notices it.

/// One request a live wait will still deliver.
struct OwnedRequest {
    token: u64,
    session_id: String,
    framed: String,
    anchor_id: Option<uuid::Uuid>,
    detach: Arc<AtomicBool>,
}

#[derive(Default)]
struct Owners {
    live: Vec<OwnedRequest>,
    /// Request messages whose turn a wait delivered as `Done` (bounded).
    delivered: VecDeque<(String, uuid::Uuid)>,
}

const DELIVERED_KEPT: usize = 64;

static OWNERS: std::sync::Mutex<Option<Owners>> = std::sync::Mutex::new(None);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn with_owners<R>(f: impl FnOnce(&mut Owners) -> R) -> R {
    let mut guard = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Owners::default))
}

impl Owners {
    fn owns(&self, session_id: &str, trigger_text: &str, trigger_id: Option<uuid::Uuid>) -> bool {
        if let Some(id) = trigger_id {
            if self.delivered.iter().any(|(s, d)| s == session_id && *d == id) {
                return true;
            }
        }
        let text = trigger_text.trim();
        self.live.iter().any(|o| {
            if o.session_id != session_id || o.detach.load(Ordering::SeqCst) {
                return false;
            }
            match (o.anchor_id, trigger_id) {
                (Some(a), Some(id)) => a == id,
                // Not anchored yet, or no id to compare: by the request text.
                _ => text.starts_with(o.framed.trim()),
            }
        })
    }

    fn claimed(&self, session_id: &str) -> HashSet<uuid::Uuid> {
        self.live
            .iter()
            .filter(|o| o.session_id == session_id)
            .filter_map(|o| o.anchor_id)
            .chain(self.delivered.iter().filter(|(s, _)| s == session_id).map(|(_, id)| *id))
            .collect()
    }

    /// Drop the live entry `token`; when `delivered`, remember its anchor.
    fn release(&mut self, token: u64, delivered: bool) {
        let Some(pos) = self.live.iter().position(|o| o.token == token) else { return };
        let o = self.live.remove(pos);
        if let (true, Some(id)) = (delivered, o.anchor_id) {
            self.delivered.push_back((o.session_id, id));
            while self.delivered.len() > DELIVERED_KEPT {
                self.delivered.pop_front();
            }
        }
    }
}

/// Whether a live (not detached) brain wait will deliver — or already
/// delivered as its `Done` — the turn started by the request message
/// `trigger_message_id` with text `trigger_text` in `session_id`. The
/// turn-end announcement must stay silent for such a turn and speak every
/// other one (see the rule above). Pass the trigger message's id whenever
/// it is known; the text alone matches requests not yet anchored.
pub fn wait_owns(session_id: &str, trigger_text: &str, trigger_message_id: Option<uuid::Uuid>) -> bool {
    with_owners(|o| o.owns(session_id, trigger_text, trigger_message_id))
}

/// A wait's hold on its request (released on drop; see the rule above).
struct Ownership {
    token: u64,
    delivered: bool,
}

impl Ownership {
    fn new(session_id: &str, framed: &str, detach: Arc<AtomicBool>) -> Self {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::SeqCst);
        with_owners(|o| {
            o.live.push(OwnedRequest {
                token,
                session_id: session_id.to_string(),
                framed: framed.to_string(),
                anchor_id: None,
                detach,
            })
        });
        Self { token, delivered: false }
    }

    fn anchor(&self, id: uuid::Uuid) {
        with_owners(|o| {
            if let Some(e) = o.live.iter_mut().find(|e| e.token == self.token) {
                e.anchor_id = Some(id);
            }
        });
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let (token, delivered) = (self.token, self.delivered);
        with_owners(|o| o.release(token, delivered));
    }
}

/// Whether `text` is a request the voice layer queued (see [`framed_request`]).
pub fn is_voice_request(text: &str) -> bool {
    text.starts_with(crate::voice::session::HEARD_PREFIX) && text.contains("[Voice request, relayed by Hobbes' voice layer")
}

/// Queue a framed voice request into `session_id` for the drain. The
/// person asked for it out loud: their turn (`TurnOrigin::Voice`), under
/// the normal interactive rules (never the Heartbeat's) plus the call risk
/// gate, since the text is the realtime model's paraphrase
/// (`builtin_tools::risk_gate`: destructive calls held, malformed refused).
pub fn queue_request(session_id: &str, framed: &str) -> uuid::Uuid {
    let msg = chat_queue::QueuedMessage::new(framed.to_string(), Vec::new(), chat_queue::QueueOrigin::Voice);
    let id = msg.id;
    chat_queue::queue_push(&mut chat_queue::CHAT_QUEUE.write(), session_id, msg);
    chat_queue::request_drain();
    id
}

/// Queue `request` into the Hobbes session and wait for the brain's turn.
/// Logs the outcome (kind, duration, reply length; never content).
/// `streaming` reports whether a session is mid-turn; the wait also reads
/// `crate::session_turn_in_flight`, which stays set between tool rounds.
/// Runs on the Dioxus runtime (it peeks signals); no guard crosses an await.
///
/// Completion needs the turn to have been seen running: `send_message`
/// appends the request (and an empty placeholder) and only then, after an
/// await, starts the stream — an idle-looking session right after the pop is
/// not a finished turn. A turn that never visibly runs is accepted once
/// it has a reply and has been quiet for [`NO_STREAM_GRACE`].
pub async fn run(
    session_state: Signal<SessionState>,
    session_id: String,
    request: String,
    handle: BrainHandle,
    streaming: fn(&str) -> bool,
) -> BrainOutcome {
    let started = Instant::now();
    let outcome = {
        let _wait = WaitGuard::new();
        run_inner(session_state, session_id, request, handle, streaming).await
    };
    let (kind, chars) = match &outcome {
        BrainOutcome::Done(t) => ("done", t.chars().count()),
        BrainOutcome::NeedsApproval(_) => ("needs approval", 0),
        BrainOutcome::Failed(_) => ("failed", 0),
        BrainOutcome::Cancelled => ("cancelled / detached", 0),
    };
    tracing::info!("voice: brain {kind} after {:.0}s ({chars} chars)", started.elapsed().as_secs_f32());
    outcome
}

/// What one poll found about the request's turn.
#[derive(Debug, Default, PartialEq)]
struct TurnStatus {
    reply: String,
    pending: bool,
    later_turn: bool,
    turn_limit: bool,
}

fn turn_status(messages: &[Message], anchor: usize) -> TurnStatus {
    TurnStatus {
        reply: collect_reply(messages, anchor),
        pending: turn_needs_approval(messages, anchor),
        later_turn: turn_ended_by_later_message(messages, anchor),
        turn_limit: turn_hit_limit(messages, anchor),
    }
}

enum View {
    Unanchored,
    /// The anchored request message is no longer in the session.
    Gone,
    Anchored(TurnStatus),
}

const NEEDS_APPROVAL: &str = "The brain is waiting for your approval in the Hobbes tab.";
const TURN_LIMIT: &str = "The brain reached its turn limit; it needs your go-ahead in the Hobbes tab.";
const NO_REPLY: &str = "The brain finished without a reply; check the Hobbes tab.";

/// The outcome of a turn known to be over.
fn finished_outcome(status: TurnStatus) -> BrainOutcome {
    if status.pending {
        BrainOutcome::NeedsApproval(NEEDS_APPROVAL.into())
    } else if !status.reply.is_empty() {
        BrainOutcome::Done(status.reply)
    } else if status.turn_limit {
        BrainOutcome::Failed(TURN_LIMIT.into())
    } else {
        BrainOutcome::Failed(NO_REPLY.into())
    }
}

/// The wait's turn-progress bookkeeping between polls (pure, so the
/// "two idle readings" rule is unit-testable).
#[derive(Debug, Default)]
struct Progress {
    /// The turn was seen running (streaming or between rounds).
    seen_running: bool,
    idle_polls: u32,
    quiet_since: Option<Instant>,
}

impl Progress {
    /// Fold in one anchored poll. `started` = the turn itself is running
    /// (streaming, or between tool rounds); `busy` = that, or the drain is
    /// still handing the request over. Returns the outcome once the turn is
    /// over.
    fn observe(&mut self, status: TurnStatus, started: bool, busy: bool, now: Instant) -> Option<BrainOutcome> {
        // A later message follows this request: its own turn is over, even
        // if the session is already running the next queued one.
        if status.later_turn {
            return Some(finished_outcome(status));
        }
        if started {
            self.seen_running = true;
        }
        if busy {
            self.idle_polls = 0;
            self.quiet_since = None;
            return None;
        }
        self.idle_polls += 1;
        if self.idle_polls < IDLE_POLLS {
            return None;
        }
        // An approval pause is a real stop: the stream manager drops the
        // turn from flight while the bubble waits.
        if status.pending || self.seen_running {
            return Some(finished_outcome(status));
        }
        // Never seen running: the pre-stream gap, or a turn too quick for
        // the poll. Accept a reply once it has been quiet a while.
        let quiet = *self.quiet_since.get_or_insert(now);
        if status.turn_limit {
            return Some(finished_outcome(status));
        }
        if !status.reply.is_empty() && now.duration_since(quiet) >= NO_STREAM_GRACE {
            return Some(BrainOutcome::Done(status.reply));
        }
        // Appended but never started.
        if status.reply.is_empty() && now.duration_since(quiet) >= NO_START_TIMEOUT {
            return Some(BrainOutcome::Failed("The brain didn't start; check the Hobbes tab.".into()));
        }
        None
    }
}

async fn run_inner(
    session_state: Signal<SessionState>,
    session_id: String,
    request: String,
    handle: BrainHandle,
    streaming: fn(&str) -> bool,
) -> BrainOutcome {
    // A rotated-out Hobbes session queues into its successor (queue_push);
    // wait where the request will actually run.
    let session_id = crate::system_sessions::current_for(&session_id);
    if !session_state.peek().sessions.contains_key(&session_id) {
        return BrainOutcome::Failed("The Hobbes session isn't loaded.".into());
    }
    let framed = framed_request(&request);
    let queued_at = chrono::Utc::now();
    let mut ownership = Ownership::new(&session_id, &framed, handle.detach.clone());
    let queued_id = queue_request(&session_id, &framed);
    tracing::info!("voice: brain request queued");

    let started_at = Instant::now();
    let mut anchor_id: Option<uuid::Uuid> = None;
    let mut progress = Progress::default();
    let mut missing_polls = 0u32;
    let mut cancel_polls = 0u32;
    loop {
        if progress.idle_polls > 0 && progress.idle_polls < IDLE_POLLS {
            // Confirm an idle reading (the mirror may lag a tick).
            tokio::time::sleep(CONFIRM_POLL).await;
        } else if progress.seen_running {
            // Mid-turn: sleep until the turn ends (or the fallback tick,
            // which also bounds cancel/detach latency).
            let _ = tokio::time::timeout(END_FALLBACK, TURN_END.notified()).await;
        } else {
            // Waiting for the turn to be picked up and start.
            tokio::time::sleep(POLL).await;
        }
        if started_at.elapsed() > BRAIN_TIMEOUT {
            return BrainOutcome::Failed("The brain took too long; it may still finish in the Hobbes tab.".into());
        }
        let streaming_now = streaming(&session_id);
        let started = streaming_now || crate::session_turn_started(&session_id);
        let busy = started || crate::session_turn_in_flight(&session_id);
        let view = {
            let state = session_state.peek();
            let Some(s) = state.sessions.get(&session_id) else {
                return BrainOutcome::Failed("The Hobbes session was closed.".into());
            };
            if anchor_id.is_none() {
                let claimed = with_owners(|o| o.claimed(&session_id));
                anchor_id = find_request_message(&s.messages, &framed, queued_at, &claimed);
                if let Some(id) = anchor_id {
                    ownership.anchor(id);
                }
            }
            match anchor_id {
                None => View::Unanchored,
                Some(id) => match s.messages.iter().position(|m| m.id == id) {
                    None => View::Gone,
                    Some(a) => View::Anchored(turn_status(&s.messages, a)),
                },
            }
        };
        let cancel = handle.cancel.load(Ordering::SeqCst);
        let detach = handle.detach.load(Ordering::SeqCst);
        let status = match view {
            View::Gone => {
                return BrainOutcome::Failed("The request was removed from the Hobbes tab before it finished.".into());
            }
            View::Unanchored => {
                if cancel && chat_queue::queue_remove(&mut chat_queue::CHAT_QUEUE.write(), &session_id, queued_id) {
                    return BrainOutcome::Cancelled;
                }
                if detach {
                    return BrainOutcome::Cancelled;
                }
                // Still queued (another turn may be ahead of it) or being
                // handed to the send: keep waiting. Neither, and not in the
                // session: it was taken back or dropped with its tab.
                if chat_queue::is_pending(&session_id, queued_id) {
                    missing_polls = 0;
                } else {
                    missing_polls += 1;
                    if missing_polls >= MISSING_POLLS {
                        return if cancel {
                            BrainOutcome::Cancelled
                        } else {
                            BrainOutcome::Failed(
                                "The request was taken out of the Hobbes queue before it ran, so nothing was done."
                                    .into(),
                            )
                        };
                    }
                }
                continue;
            }
            View::Anchored(status) => status,
        };
        // A finished turn wins over a detach: its reply is already there.
        if let Some(outcome) = progress.observe(status, started, busy, Instant::now()) {
            if matches!(outcome, BrainOutcome::Done(_)) {
                ownership.delivered = true;
            }
            return outcome;
        }
        if detach {
            return BrainOutcome::Cancelled;
        }
        if cancel {
            // The turn is in the session. While it runs, stop the next
            // round (the gap between tool rounds has no stream to cancel)
            // and the stream if one runs. Never on an idle turn: a stale
            // flag would drop a later approval's resume.
            if busy {
                crate::components::stream_manager::request_turn_abort(&session_id);
            }
            if started {
                crate::request_stream_cancel(&session_id);
                return BrainOutcome::Cancelled;
            }
            // Not streaming yet (its first prompt is still being built):
            // wait briefly for the stream so it can be stopped too.
            cancel_polls += 1;
            if cancel_polls >= CANCEL_POLLS {
                return BrainOutcome::Cancelled;
            }
        }
    }
}

/// See [`run`]: how long a reply must sit unchanged with no stream seen.
const NO_STREAM_GRACE: Duration = Duration::from_secs(3);
/// See [`run`]: a request that is in the session but never started a turn.
const NO_START_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice request goes through the real queue and the drain's own
    /// origin resolution into the Hobbes session as the person's turn —
    /// never as an unattended (Heartbeat) one.
    #[test]
    fn a_voice_request_is_the_person_s_turn_through_the_queue() {
        use crate::components::chat_queue::{self, CHAT_QUEUE};
        use crate::session::TurnOrigin;
        let mut dom = VirtualDom::new(|| {
            crate::system_sessions::set_current("brain-hobbes");
            let framed = framed_request("check CI");
            let id = queue_request("brain-hobbes", &framed);
            // The drain: pop, resolve the origin, hand it to the send.
            let qm = chat_queue::queue_pop_next(&mut CHAT_QUEUE.write(), "brain-hobbes").unwrap();
            assert_eq!(qm.id, id);
            let origin = chat_queue::drain_origin(&qm, "brain-hobbes");
            assert_eq!(origin, TurnOrigin::Voice);
            assert!(!origin.is_unattended());
            chat_queue::set_pending_origin("brain-hobbes", origin);
            assert_eq!(chat_queue::take_turn_origin("brain-hobbes"), TurnOrigin::Voice);
            // By contrast a fired timer queued into the same session is
            // unattended there.
            let timer = chat_queue::QueuedMessage::new("timer".into(), vec![], chat_queue::QueueOrigin::Unattended);
            assert_eq!(chat_queue::drain_origin(&timer, "brain-hobbes"), TurnOrigin::Heartbeat);
            assert_eq!(chat_queue::drain_origin(&timer, "an-ordinary-chat"), TurnOrigin::User);
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
    use crate::voice::session::transcript_message;

    fn text(author: &str, t: &str) -> Message {
        let mut m = transcript_message(author == "User", "x");
        m.author = author.into();
        m.content = MessageContent::Text {
            content: t.into(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        };
        m
    }

    #[test]
    fn reply_skips_user_and_transcript_lines() {
        let msgs = vec![
            text("User", "old"),
            text("User", &framed_request("check CI")),
            text("Hobbes", "🔊 On it."),
            text("Hobbes", "CI is green on main."),
        ];
        assert_eq!(find_anchor(&msgs, 0, &framed_request("check CI")), Some(1));
        assert_eq!(collect_reply(&msgs, 1), "CI is green on main.");
    }

    #[test]
    fn details_become_a_pointer() {
        let msgs = vec![text("User", "ask"), text("Hobbes", "Two PRs need review.\nDetails:\n- #12\n- #14")];
        assert_eq!(collect_reply(&msgs, 0), "Two PRs need review. (Full details are in the Hobbes tab.)");
    }

    #[test]
    fn framing_keeps_the_request() {
        assert!(framed_request("  email Sam ").starts_with("🎙 email Sam\n"));
        // Detail is allowed when asked for; unspeakable material still goes after Details:.
        assert!(framed_request("x").contains("unless they asked for detail"));
        assert!(!framed_request("x").contains("one to four short"));
        // The head is the voice layer's summary; consent comes from the person's own lines.
        let f = framed_request("x");
        assert!(f.contains("not the person's exact words"));
        assert!(f.contains("Do the work first"));
        assert!(f.contains("only what has actually been done, or what you need from them"));
        assert!(f.contains("irreversible counts only if their own 🎙 lines say it"));
        assert!(f.contains("\"Details:\""));
        assert!(is_voice_request(&framed_request("email Sam")));
        assert_eq!(crate::voice::display::display_text(&framed_request("email Sam")), "🎙 email Sam");
        assert!(!is_voice_request("🎙 a heard transcript line"));
    }

    #[test]
    fn reply_stops_at_the_next_request() {
        let msgs = vec![
            text("User", &framed_request("A")),
            text("Hobbes", "Answer A."),
            text("User", &framed_request("B")),
            text("Hobbes", "Answer B."),
        ];
        assert_eq!(collect_reply(&msgs, 0), "Answer A.");
        assert_eq!(find_anchor(&msgs, 1, &framed_request("B")), Some(2));
        assert_eq!(collect_reply(&msgs, 2), "Answer B.");
    }

    #[test]
    fn a_turn_is_over_once_a_later_message_follows_it() {
        let msgs = vec![
            text("User", &framed_request("A")),
            text("Hobbes", "Answer A."),
            text("User", &framed_request("B")),
            text("Hobbes", ""),
        ];
        assert!(turn_ended_by_later_message(&msgs, 0));
        assert!(!turn_ended_by_later_message(&msgs, 2));
        // An auto-recovery prompt continues the turn it is in.
        let msgs = vec![
            text("User", &framed_request("A")),
            text("Hobbes", "Part one."),
            text("User", "[System: Auto-recovery triggered, carry on]"),
            text("Hobbes", "Part two."),
        ];
        assert!(!turn_ended_by_later_message(&msgs, 0));
        assert_eq!(collect_reply(&msgs, 0), "Part one.\n\nPart two.");
    }

    #[test]
    fn old_bubbles_dont_block_new_requests() {
        use crate::components::shared::ToolCall;
        let mut bubble = text("Hobbes", "");
        bubble.content = MessageContent::PermissionRequest(ToolCall { tool_name: "X".into(), ..Default::default() });
        let msgs = vec![bubble.clone(), text("User", &framed_request("A")), text("Hobbes", "ok")];
        assert!(!turn_needs_approval(&msgs, 1));
        let msgs = vec![text("User", &framed_request("A")), bubble];
        assert!(turn_needs_approval(&msgs, 0));
    }

    fn call() -> Message {
        use crate::components::shared::ToolCall;
        let mut m = text("Hobbes", "");
        m.content = MessageContent::ToolCall(ToolCall { tool_name: "LIST".into(), ..Default::default() });
        m
    }

    /// Only the final answer is spoken, not each tool round's narration.
    #[test]
    fn reply_is_the_final_answer_after_the_last_tool_call() {
        let msgs = vec![
            text("User", &framed_request("plan my day")),
            call(),
            text("Hobbes", "Let me check your calendar."),
            call(),
            text("Hobbes", "Now the to-dos."),
            text("Hobbes", "You have three meetings and two to-dos."),
        ];
        assert_eq!(collect_reply(&msgs, 0), "You have three meetings and two to-dos.");
        // Recovery glues the interrupted final answer back together.
        let msgs = vec![
            text("User", &framed_request("x")),
            call(),
            text("Hobbes", "Narration."),
            text("Hobbes", "Part one."),
            text("User", "[System: Auto-recovery triggered — interrupted]"),
            text("Hobbes", "Part two."),
        ];
        assert_eq!(collect_reply(&msgs, 0), "Part one.\n\nPart two.");
    }

    #[test]
    fn harness_notices_and_imitated_transcript_lines_are_not_spoken() {
        let msgs = vec![
            text("User", &framed_request("x")),
            text("Hobbes", "CI is green.\n🔊 CI is green, anything else?"),
            text("Hobbes", "Pardon, I have reached the 'Max Turn Limit' currently set to 5 in settings and need permission to continue."),
        ];
        assert_eq!(collect_reply(&msgs, 0), "CI is green.");
        let msgs = vec![text("User", "ask"), text("Hobbes", "Done. 🎙 thanks")];
        assert_eq!(collect_reply(&msgs, 0), "Done.");
    }

    #[test]
    fn details_cut_only_at_a_line_start_and_only_points_when_it_cut_something() {
        let msgs = vec![text("User", "ask"), text("Hobbes", "The Details: section of the doc is fine.")];
        assert_eq!(collect_reply(&msgs, 0), "The Details: section of the doc is fine.");
        let msgs = vec![text("User", "ask"), text("Hobbes", "All set.\nDetails:\n")];
        assert_eq!(collect_reply(&msgs, 0), "All set.");
        let msgs = vec![text("User", "ask"), text("Hobbes", "All set.\n  Details: https://x.y/z")];
        assert_eq!(collect_reply(&msgs, 0), "All set. (Full details are in the Hobbes tab.)");
    }

    /// A fleet @-mention expansion appends to the stored request.
    #[test]
    fn the_anchor_matches_by_prefix_and_skips_claimed_and_older_messages() {
        let framed = framed_request("check CI");
        let queued_at = chrono::Utc::now();
        let mut old = text("User", &framed);
        old.created_at = queued_at - chrono::Duration::seconds(30);
        let expanded = text("User", &format!("{framed}\n\n[Fleet context: …]"));
        let msgs = vec![old, expanded.clone()];
        assert_eq!(find_anchor(&msgs, 1, &framed), Some(1));
        assert_eq!(find_request_message(&msgs, &framed, queued_at, &HashSet::new()), Some(expanded.id));
        let claimed: HashSet<_> = [expanded.id].into();
        assert_eq!(find_request_message(&msgs, &framed, queued_at, &claimed), None);
    }

    #[test]
    fn out_of_range_anchors_never_panic() {
        let msgs = vec![text("User", "a")];
        assert!(!turn_needs_approval(&msgs, 5));
        assert_eq!(collect_reply(&msgs, 5), "");
        assert!(!turn_ended_by_later_message(&msgs, 5));
        assert!(!turn_needs_approval(&[], 0));
    }

    /// A relay that woke its own turn (no user message) ends the one before.
    #[test]
    fn a_waking_relay_starts_a_new_turn() {
        use crate::todo::relay::{RelayDirection, RelayKind, RelayNote};
        let mut relay = text("Relay", "");
        relay.content = MessageContent::Relay(RelayNote {
            direction: RelayDirection::FromWorker,
            kind: RelayKind::Result,
            peer_session_id: "w".into(),
            peer_title: "Worker".into(),
            todo_id: None,
            text: "done".into(),
            hop: 0,
            woke_turn: true,
        });
        let msgs = vec![text("User", &framed_request("A")), text("Hobbes", "Answer A."), relay, text("Hobbes", "Worker done.")];
        assert!(turn_ended_by_later_message(&msgs, 0));
        assert_eq!(collect_reply(&msgs, 0), "Answer A.");
    }

    fn status(reply: &str) -> TurnStatus {
        TurnStatus { reply: reply.into(), ..Default::default() }
    }

    /// The gap between tool rounds reads busy; one idle reading is not an end.
    #[test]
    fn a_turn_is_over_only_after_two_idle_readings() {
        let now = Instant::now();
        let mut p = Progress::default();
        assert_eq!(p.observe(status("Let me look."), true, true, now), None);
        // Between rounds: not streaming but the continuation is in flight.
        assert_eq!(p.observe(status("Let me look."), false, true, now), None);
        // One idle reading (mirror lag) — not yet.
        assert_eq!(p.observe(status("Let me look."), false, false, now), None);
        assert_eq!(p.observe(status("Let me look."), true, true, now), None);
        assert_eq!(p.observe(status("Final."), false, false, now), None);
        assert_eq!(p.observe(status("Final."), false, false, now), Some(BrainOutcome::Done("Final.".into())));
    }

    #[test]
    fn an_approval_pause_and_a_later_message_end_the_wait() {
        let now = Instant::now();
        let mut p = Progress::default();
        let held = TurnStatus { pending: true, ..Default::default() };
        assert_eq!(p.observe(held, true, true, now), None);
        let held = || TurnStatus { pending: true, ..Default::default() };
        assert_eq!(p.observe(held(), false, false, now), None);
        assert!(matches!(p.observe(held(), false, false, now), Some(BrainOutcome::NeedsApproval(_))));
        // A later message closes the turn at once, busy or not.
        let mut p = Progress::default();
        let later = TurnStatus { reply: "A.".into(), later_turn: true, ..Default::default() };
        assert_eq!(p.observe(later, false, true, now), Some(BrainOutcome::Done("A.".into())));
        // The turn limit stops a turn that never streamed.
        let mut p = Progress::default();
        let limit = || TurnStatus { turn_limit: true, ..Default::default() };
        assert_eq!(p.observe(limit(), false, false, now), None);
        assert_eq!(p.observe(limit(), false, false, now), Some(BrainOutcome::Failed(TURN_LIMIT.into())));
    }

    #[test]
    fn a_wait_owns_its_request_until_it_gives_up_or_delivers() {
        let mut owners = Owners::default();
        let framed = framed_request("check CI");
        let detach = Arc::new(AtomicBool::new(false));
        let id = uuid::Uuid::new_v4();
        owners.live.push(OwnedRequest { token: 1, session_id: "h".into(), framed: framed.clone(), anchor_id: None, detach: detach.clone() });
        // Before anchoring: by text (with an appended context block too).
        assert!(owners.owns("h", &format!("{framed}\n\nmore"), Some(id)));
        assert!(!owners.owns("other", &framed, Some(id)));
        assert!(!owners.owns("h", "🎙 something else", Some(id)));
        // Anchored: by id only.
        owners.live[0].anchor_id = Some(id);
        assert!(owners.owns("h", &framed, Some(id)));
        assert!(!owners.owns("h", &framed, Some(uuid::Uuid::new_v4())), "a repeat of the request is its own turn");
        assert!(owners.owns("h", &framed, None));
        // Detached: announced at the turn end.
        detach.store(true, Ordering::SeqCst);
        assert!(!owners.owns("h", &framed, Some(id)));
        detach.store(false, Ordering::SeqCst);
        // Gave up early (not delivered): announced.
        owners.release(1, false);
        assert!(!owners.owns("h", &framed, Some(id)));
        // Delivered: never announced, and claimed against re-anchoring.
        owners.live.push(OwnedRequest { token: 2, session_id: "h".into(), framed: framed.clone(), anchor_id: Some(id), detach });
        owners.release(2, true);
        assert!(owners.owns("h", &framed, Some(id)));
        assert!(owners.claimed("h").contains(&id));
        assert!(owners.live.is_empty());
    }
}
