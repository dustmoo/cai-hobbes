//! `VOICE_ASK_BRAIN`: hand a request from the voice tier to the global
//! connector. The request is queued into the Voice session through the
//! background chat queue (the same path a Heartbeat wake uses), so the brain
//! runs a normal turn — full prompt builder, tools, trust rules, permission
//! bubbles — on whatever `active_connector_id` points at (the Voice session
//! is never pinned). This module waits for that turn and returns its text.

use std::sync::atomic::{AtomicBool, Ordering};
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
/// Most text handed back to the voice tier.
const MAX_RESULT_CHARS: usize = 4000;

#[derive(Clone, Debug, PartialEq)]
pub enum BrainOutcome {
    Done(String),
    /// The turn is held on a permission bubble in the Voice tab.
    NeedsApproval(String),
    Failed(String),
    Cancelled,
}

/// The queued text: the request, framed for a spoken reply.
pub fn framed_request(request: &str) -> String {
    format!(
        "🎙 {}\n\n[Voice request, relayed by Hobbes' voice layer from a live spoken conversation. \
         Do what is asked. Your final reply is read aloud: answer in one to four short plain \
         sentences, no markdown, no code, no URLs; put anything long in the reply after a line \
         that says \"Details:\" and the voice will point the person to the Voice tab.]",
        request.trim()
    )
}

/// The brain's reply: assistant text after the request at `anchor`, up to
/// the next user message (a later request or a typed message belongs to its
/// own turn), skipping voice transcript lines, with any "Details:" tail
/// reduced to a pointer.
pub fn collect_reply(messages: &[Message], anchor: usize) -> String {
    let mut parts = Vec::new();
    for m in messages.iter().skip(anchor + 1) {
        if m.author == "User" {
            break;
        }
        if let MessageContent::Text { content, .. } = &m.content {
            let t = content.trim();
            if t.is_empty() || t.starts_with(crate::voice::session::SPOKE_PREFIX) {
                continue;
            }
            parts.push(t.to_string());
        }
    }
    let mut text = parts.join("\n\n");
    if let Some(idx) = text.find("Details:") {
        text.truncate(idx);
        text = format!("{} (Full details are in the Voice tab.)", text.trim_end());
    }
    if text.chars().count() > MAX_RESULT_CHARS {
        text = text.chars().take(MAX_RESULT_CHARS).collect::<String>() + "…";
    }
    text
}

/// Index of this request's own message (the drain appends it as a User
/// message with exactly the queued text), searching from `from`.
pub fn find_anchor(messages: &[Message], from: usize, framed: &str) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, m)| {
            m.author == "User" && m.content.get_text_content().is_some_and(|t| t.trim() == framed.trim())
        })
        .map(|(i, _)| i)
}

/// The turn that answers the request at `anchor` is held on a permission
/// bubble (only messages after the anchor count; an old unanswered bubble
/// elsewhere in the session does not).
fn turn_needs_approval(messages: &[Message], anchor: usize) -> bool {
    let end = messages
        .iter()
        .enumerate()
        .skip(anchor + 1)
        .find(|(_, m)| m.author == "User")
        .map(|(i, _)| i)
        .unwrap_or(messages.len());
    crate::heartbeat::wake::has_pending_approval(&messages[anchor + 1..end])
}

/// How the controller stops a wait.
#[derive(Clone, Default)]
pub struct BrainHandle {
    /// Stop the brain's turn too (VOICE_CANCEL_BRAIN).
    pub cancel: Arc<AtomicBool>,
    /// Only stop waiting; the turn keeps running in the Voice tab
    /// (the conversation ended).
    pub detach: Arc<AtomicBool>,
}

/// Brain waits in progress (not detached). While one is running, a turn
/// that answers a voice request is the wait's to deliver, not an
/// announcement (see `controller::on_turn_end`).
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

/// Whether `text` is a request the voice layer queued (see [`framed_request`]).
pub fn is_voice_request(text: &str) -> bool {
    text.starts_with(crate::voice::session::HEARD_PREFIX) && text.contains("[Voice request, relayed by Hobbes' voice layer")
}

/// Queue `request` into the Voice session and wait for the brain's turn.
/// Logs the outcome (kind, duration, reply length; never content).
/// `streaming` reports whether a session is mid-turn. Runs on the Dioxus
/// runtime (it peeks signals); no guard crosses an await.
///
/// Completion needs the turn to have been seen streaming: `send_message`
/// appends the request (and an empty placeholder) and only then, after an
/// await, starts the stream — an idle-looking session right after the pop is
/// not a finished turn. A turn that never visibly streams is accepted once
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

async fn run_inner(
    session_state: Signal<SessionState>,
    session_id: String,
    request: String,
    handle: BrainHandle,
    streaming: fn(&str) -> bool,
) -> BrainOutcome {
    let before = match session_state.peek().sessions.get(&session_id) {
        Some(s) => s.messages.len(),
        None => return BrainOutcome::Failed("The Voice session isn't loaded.".into()),
    };
    let framed = framed_request(&request);
    let queued_id = {
        let msg = chat_queue::QueuedMessage::new(framed.clone(), Vec::new());
        let id = msg.id;
        chat_queue::queue_push(&mut chat_queue::CHAT_QUEUE.write(), &session_id, msg);
        id
    };
    chat_queue::request_drain();
    tracing::info!("voice: brain request queued");

    let started_at = Instant::now();
    let mut anchor: Option<usize> = None;
    let mut seen_stream = false;
    let mut quiet_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(POLL).await;
        if handle.detach.load(Ordering::SeqCst) {
            return BrainOutcome::Cancelled;
        }
        if handle.cancel.load(Ordering::SeqCst) {
            if anchor.is_none() {
                chat_queue::queue_remove(&mut chat_queue::CHAT_QUEUE.write(), &session_id, queued_id);
            } else if streaming(&session_id) {
                crate::request_stream_cancel(&session_id);
            }
            return BrainOutcome::Cancelled;
        }
        if started_at.elapsed() > BRAIN_TIMEOUT {
            return BrainOutcome::Failed("The brain took too long; it may still finish in the Voice tab.".into());
        }
        let is_streaming = streaming(&session_id);
        let (reply, pending) = {
            let state = session_state.peek();
            let Some(s) = state.sessions.get(&session_id) else {
                return BrainOutcome::Failed("The Voice session was closed.".into());
            };
            if anchor.is_none() {
                anchor = find_anchor(&s.messages, before, &framed);
            }
            match anchor {
                Some(a) => (collect_reply(&s.messages, a), turn_needs_approval(&s.messages, a)),
                None => (String::new(), false),
            }
        };
        let Some(_) = anchor else {
            continue; // still queued (another turn may be ahead of it)
        };
        if is_streaming {
            seen_stream = true;
            quiet_since = None;
            continue;
        }
        if pending {
            return BrainOutcome::NeedsApproval("The brain is waiting for your approval in the Voice tab.".into());
        }
        if seen_stream {
            return if reply.is_empty() {
                BrainOutcome::Failed("The brain finished without a reply; check the Voice tab.".into())
            } else {
                BrainOutcome::Done(reply)
            };
        }
        // Not seen streaming yet: the pre-stream gap, or a turn too quick
        // for the poll. Accept a reply once it has been quiet a while.
        let quiet = *quiet_since.get_or_insert_with(Instant::now);
        if !reply.is_empty() && quiet.elapsed() >= NO_STREAM_GRACE {
            return BrainOutcome::Done(reply);
        }
        // Appended but never started (e.g. the turn limit was reached).
        if reply.is_empty() && quiet.elapsed() >= NO_START_TIMEOUT {
            return BrainOutcome::Failed("The brain didn't start; check the Voice tab.".into());
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
        assert_eq!(collect_reply(&msgs, 0), "Two PRs need review. (Full details are in the Voice tab.)");
    }

    #[test]
    fn framing_keeps_the_request() {
        assert!(framed_request("  email Sam ").starts_with("🎙 email Sam\n"));
        assert!(is_voice_request(&framed_request("email Sam")));
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
    fn old_bubbles_dont_block_new_requests() {
        use crate::components::shared::ToolCall;
        let mut bubble = text("Hobbes", "");
        bubble.content = MessageContent::PermissionRequest(ToolCall { tool_name: "X".into(), ..Default::default() });
        let msgs = vec![bubble.clone(), text("User", &framed_request("A")), text("Hobbes", "ok")];
        assert!(!turn_needs_approval(&msgs, 1));
        let msgs = vec![text("User", &framed_request("A")), bubble];
        assert!(turn_needs_approval(&msgs, 0));
    }
}
