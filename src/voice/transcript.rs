//! Where voice transcript lines go in the Hobbes session (which Heartbeat
//! wakes and typed turns share: each is a turn like the brain's).
//!
//! Transcript lines are buffered while the brain's turn runs (appending
//! mid-turn would split it) and written later. Appending them at write time
//! put what the person said *below* the brain's answer to it. Instead each
//! line carries the moment its utterance began and is inserted where that
//! moment falls — but only between turns, never inside one: a moment that
//! falls inside a turn (between its request and its last message) snaps to
//! the nearer edge of that turn, and an *open* turn at the tail (see
//! [`turn_open`]) only has its start edge. The vec stays strictly
//! `created_at`-ordered around each line (the stamp is fitted strictly inside
//! the gap it lands in), which is what keeps the renderer (it sorts by
//! `created_at`), the prompt builder (vec order) and journal replay
//! (`session_events::upsert_message`, sorted insertion) in agreement.
//!
//! Whether the last turn is open is read from the messages themselves, not
//! only from the streaming flag: that flag (a mirror) is false in the gap
//! before a stream starts, between continuation rounds, and while a call
//! waits on approval — and a line written into any of those gaps would land
//! inside the turn.

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::shared::{MessageContent, ToolCallStatus};
use crate::session::SessionState;
use crate::session_events::SessionEvent;
use crate::voice::session::{transcript_message, HEARD_PREFIX, SPOKE_PREFIX};

/// A tail that looks mid-turn (empty placeholder, a finished tool call with
/// no reply yet, a request with no answer) counts as open for this long
/// after its last message; past it the turn is taken as abandoned
/// (cancelled, crashed) so lines aren't held behind it forever. Pending
/// approvals and running calls stay open regardless.
const OPEN_TAIL_GRACE_MINUTES: i64 = 5;

/// The prefix of the synthetic "User" prompts auto-recovery pushes *inside*
/// a turn (stream_manager): they continue the turn rather than start one.
const RECOVERY_PREFIX: &str = "[System: Auto-recovery triggered";

/// A finished transcript line waiting to be written.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingLine {
    /// What the person said (`true`) or what the voice said.
    pub heard: bool,
    pub text: String,
    /// When the utterance began (its first transcript fragment).
    pub at: DateTime<Utc>,
}

impl PendingLine {
    pub fn new(heard: bool, text: String, at: Option<DateTime<Utc>>) -> Self {
        Self { heard, text, at: at.unwrap_or_else(Utc::now) }
    }
}

/// A Hobbes-authored message that stands alone rather than being part of a
/// turn's machinery: a spoken transcript line.
fn standalone(m: &Message) -> bool {
    m.author == "Hobbes" && m.content.get_text_content().is_some_and(|t| t.starts_with(SPOKE_PREFIX))
}

/// A voice transcript line ("🎙 …" heard or "🔊 …" spoken), as opposed to a
/// brain request (also "🎙"-prefixed, but framed) or turn machinery.
fn transcript_line(m: &Message) -> bool {
    standalone(m)
        || (m.author == "User"
            && m.content.get_text_content().is_some_and(|t| {
                t.starts_with(HEARD_PREFIX) && !crate::voice::brain::is_voice_request(t.trim())
            }))
}

/// Whether `m` opens a new unit: a request / relay / transcript line. An
/// auto-recovery prompt is "User"-authored but continues the turn it is in.
fn starts_unit(m: &Message) -> bool {
    if standalone(m) {
        return true;
    }
    m.author != "Hobbes"
        && !m.content.get_text_content().is_some_and(|t| t.starts_with(RECOVERY_PREFIX))
}

/// Whether `m` starts a turn of its own: a request, a relay or a wake —
/// not a voice transcript line and not an auto-recovery prompt inside a
/// turn. (The Heartbeat reads its wakes' windows with this.)
pub fn starts_turn(m: &Message) -> bool {
    starts_unit(m) && !transcript_line(m)
}

/// Whether a standalone message may go at index `j` of `messages` without
/// splitting a turn. A turn is a non-Hobbes message (the request, a relay)
/// and the Hobbes messages (tool calls, bubbles, the reply) that follow it.
fn is_boundary(messages: &[Message], j: usize, tail_open: bool) -> bool {
    if j == 0 {
        return true;
    }
    if j == messages.len() {
        return !tail_open;
    }
    starts_unit(&messages[j]) || standalone(&messages[j - 1])
}

/// The last moment a message shows activity: its stamp, or a tool call's
/// completion when later.
fn last_activity(m: &Message) -> DateTime<Utc> {
    match &m.content {
        MessageContent::ToolCall(tc) => tc.completed_at.map_or(m.created_at, |c| c.max(m.created_at)),
        _ => m.created_at,
    }
}

/// Whether the session's last turn is still in progress, so nothing may be
/// written after it: `streaming` (the stream flag) or, read from the
/// messages, a call waiting on approval, a running call, or a tail that is
/// mid-turn (the empty reply placeholder, a tool call the model hasn't
/// answered yet, a request with no reply) within the grace period.
pub fn turn_open(messages: &[Message], streaming: bool, now: DateTime<Utc>) -> bool {
    if streaming {
        return true;
    }
    // The last turn, looking past any transcript lines written after it.
    let start = messages.iter().rposition(|m| starts_unit(m) && !transcript_line(m)).unwrap_or(0);
    let tail = &messages[start..];
    let Some(last) = tail.iter().rev().find(|m| !transcript_line(m)) else {
        return false;
    };
    let held_or_running = tail.iter().any(|m| match &m.content {
        MessageContent::PermissionRequest(_) | MessageContent::SkillPermissionRequest(_) => true,
        MessageContent::ToolCall(tc) => tc.status == ToolCallStatus::Running,
        _ => false,
    });
    if held_or_running {
        return true;
    }
    let mid_turn = if last.author == "Hobbes" {
        match &last.content {
            MessageContent::ToolCall(_) => true,
            MessageContent::Text { content, .. } => content.trim().is_empty() && last.attachments.is_empty(),
            _ => false,
        }
    } else {
        // A request (or relay) nobody has answered yet.
        true
    };
    mid_turn && now - last_activity(last) < Duration::minutes(OPEN_TAIL_GRACE_MINUTES)
}

/// [`turn_open`] for a session in `session_state` (peeked; false when the
/// session isn't loaded).
pub fn session_turn_open(session_state: Signal<SessionState>, session_id: &str) -> bool {
    let streaming = crate::session_streaming(session_id);
    session_state
        .peek()
        .sessions
        .get(session_id)
        .is_some_and(|s| turn_open(&s.messages, streaming, Utc::now()))
}

/// A stamp for a message inserted between `lo` and `hi` (its neighbours),
/// strictly inside the gap so sorted insertion reproduces the position:
/// `want` when it already fits, else 1 ms inside the nearer edge, else the
/// midpoint. A zero-width gap (two neighbours with the same nanosecond
/// stamp — not produced by `Utc::now()` in practice) has no such stamp; the
/// line then takes `lo`, and a replay may put it after `hi`.
fn fit_stamp(want: DateTime<Utc>, lo: Option<DateTime<Utc>>, hi: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let step = Duration::milliseconds(1);
    let mut ts = want;
    if let Some(lo) = lo {
        if ts <= lo {
            ts = lo + step;
        }
    }
    if let Some(hi) = hi {
        if ts >= hi {
            ts = hi - step;
        }
    }
    if let (Some(lo), Some(hi)) = (lo, hi) {
        if !(lo < ts && ts < hi) {
            ts = lo + (hi - lo) / 2;
            if !(lo < ts && ts < hi) {
                ts = lo;
            }
        }
    }
    ts
}

/// Insert `lines` (already stamped, in spoken order) between turns at their
/// chronological positions; see the module docs. `tail_open`: the session's
/// last turn is still in progress ([`turn_open`]), so nothing may follow
/// it. Returns the
/// inserted messages with the stamps they were given.
pub fn insert_between_turns(messages: &mut Vec<Message>, lines: Vec<Message>, tail_open: bool) -> Vec<Message> {
    let mut inserted = Vec::with_capacity(lines.len());
    // Spoken order wins over fragment arrival: never place a line before the
    // one written just ahead of it.
    let mut floor: Option<(usize, DateTime<Utc>)> = None;
    for mut msg in lines {
        let mut ts = msg.created_at;
        if let Some((_, prev_ts)) = floor {
            ts = ts.max(prev_ts);
        }
        let sorted = messages.iter().rposition(|m| m.created_at <= ts).map_or(0, |i| i + 1);
        let mut j = if is_boundary(messages, sorted, tail_open) {
            sorted
        } else {
            let start = (0..sorted).rev().find(|&b| is_boundary(messages, b, tail_open)).unwrap_or(0);
            match (sorted + 1..=messages.len()).find(|&b| is_boundary(messages, b, tail_open)) {
                // Only an open turn has no end edge: go before it.
                None => start,
                Some(end) => {
                    let since_start = ts - messages[start].created_at;
                    // The turn's real end: a prose reply finalized in place
                    // may still carry its turn-start stamp (older turns), so
                    // take the latest activity in the turn, not the last stamp.
                    let turn_end = messages[start..end].iter().map(last_activity).max().unwrap_or(ts);
                    let until_end = turn_end - ts;
                    if since_start <= until_end {
                        start
                    } else {
                        end
                    }
                }
            }
        };
        if let Some((prev_j, _)) = floor {
            j = j.max(prev_j + 1);
        }
        // Fit strictly into the gap so sorted insertion (journal replay)
        // lands it here.
        let lo = j.checked_sub(1).map(|i| messages[i].created_at);
        let hi = messages.get(j).map(|m| m.created_at);
        ts = fit_stamp(ts, lo, hi);
        msg.created_at = ts;
        messages.insert(j, msg.clone());
        floor = Some((j, ts));
        inserted.push(msg);
    }
    inserted
}

/// Write `lines` into the Voice session at their chronological positions,
/// journal them (so a rewind's replay keeps them) and persist.
pub fn write_lines(mut session_state: Signal<SessionState>, session_id: &str, lines: Vec<PendingLine>) {
    let lines: Vec<Message> = lines
        .into_iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| {
            let mut m = transcript_message(l.heard, &l.text);
            m.created_at = l.at;
            m
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let streaming = crate::session_streaming(session_id);
    let inserted = {
        let mut state = session_state.write();
        match state.sessions.get_mut(session_id) {
            Some(s) => {
                let tail_open = turn_open(&s.messages, streaming, Utc::now());
                insert_between_turns(&mut s.messages, lines, tail_open)
            }
            None => return,
        }
    };
    let events = inserted
        .into_iter()
        .map(|message| {
            if message.author == "User" {
                SessionEvent::UserMessage { message }
            } else {
                SessionEvent::AssistantMessage { message }
            }
        })
        .collect();
    crate::session_events::log_events(session_id, events);
    SessionState::save_signal(&session_state, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::ToolCall;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-03T10:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn at(secs: i64) -> DateTime<Utc> {
        t0() + Duration::seconds(secs)
    }

    fn text(author: &str, body: &str, secs: i64) -> Message {
        let mut m = transcript_message(author == "User", "x");
        m.author = author.into();
        m.content = MessageContent::Text {
            content: body.into(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        };
        m.created_at = at(secs);
        m
    }

    fn call(secs: i64) -> Message {
        let mut m = text("Hobbes", "", secs);
        m.content = MessageContent::ToolCall(ToolCall {
            tool_name: "HOBBES_TODO_LIST".into(),
            status: ToolCallStatus::Completed,
            ..Default::default()
        });
        m
    }

    fn line(heard: bool, body: &str, secs: i64) -> Message {
        let mut m = transcript_message(heard, body);
        m.created_at = at(secs);
        m
    }

    fn bodies(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| m.content.get_text_content().unwrap_or_else(|| "<call>".into()))
            .collect()
    }

    fn monotonic(messages: &[Message]) -> bool {
        messages.windows(2).all(|w| w[0].created_at <= w[1].created_at)
    }

    /// A brain turn: request at 10s, a tool call, the reply finalized at 30s.
    fn brain_turn() -> Vec<Message> {
        vec![
            text("User", "🎙 earlier", 0),
            text("Hobbes", "Earlier reply.", 1),
            text("User", &crate::voice::brain::framed_request("what's blocked?"), 10),
            call(12),
            text("Hobbes", "Two sessions are blocked.", 30),
        ]
    }

    /// The person's question and the voice's "one moment" were buffered
    /// while the brain ran; they go above the request, not below the reply.
    #[test]
    fn deferred_lines_land_before_the_brain_turn_they_preceded() {
        let mut msgs = brain_turn();
        let inserted = insert_between_turns(
            &mut msgs,
            vec![line(true, "what's blocked?", 8), line(false, "Let me check, one moment.", 11)],
            false,
        );
        assert_eq!(
            bodies(&msgs)[2..5],
            ["🎙 what's blocked?", "🔊 Let me check, one moment.", crate::voice::brain::framed_request("what's blocked?").as_str()]
        );
        assert_eq!(bodies(&msgs).last().unwrap(), "Two sessions are blocked.");
        assert!(monotonic(&msgs));
        // Heard at its own time; the hand-off phrase clamped just before the request.
        assert_eq!(inserted[0].created_at, at(8));
        assert!(inserted[1].created_at < at(10));
    }

    /// What was said after the brain's turn ended follows it.
    #[test]
    fn lines_after_the_turn_follow_it() {
        let mut msgs = brain_turn();
        insert_between_turns(&mut msgs, vec![line(false, "Two sessions are blocked.", 31), line(true, "thanks", 35)], false);
        assert_eq!(bodies(&msgs)[5..], ["🔊 Two sessions are blocked.", "🎙 thanks"]);
        assert!(monotonic(&msgs));
    }

    /// A line from late in a long turn snaps to the turn's end, and never
    /// between the request and its tool calls or reply.
    #[test]
    fn lines_inside_a_turn_snap_to_the_nearer_edge() {
        let mut msgs = brain_turn();
        insert_between_turns(&mut msgs, vec![line(true, "still there?", 28)], false);
        assert_eq!(bodies(&msgs)[5], "🎙 still there?");
        assert!(msgs[5].created_at >= at(30));
        assert!(monotonic(&msgs));
    }

    /// Spoken order holds even when a later line's fragments arrived first,
    /// and even across a snap to a turn's end.
    #[test]
    fn spoken_order_is_kept() {
        let mut msgs = brain_turn();
        insert_between_turns(&mut msgs, vec![line(true, "first", 29), line(false, "second", 20)], false);
        assert_eq!(bodies(&msgs)[5..], ["🎙 first", "🔊 second"]);
        assert!(monotonic(&msgs));
    }

    /// At the end of a conversation the brain's turn may still be streaming:
    /// lines go before that turn, never into its tool round.
    #[test]
    fn an_open_turn_is_never_split() {
        let mut msgs = brain_turn();
        msgs.pop(); // reply not written yet: the tail is the tool round
        msgs.push(text("Hobbes", "", 13)); // the continuation placeholder
        insert_between_turns(&mut msgs, vec![line(true, "bye", 40)], true);
        assert_eq!(bodies(&msgs)[2], "🎙 bye");
        assert!(bodies(&msgs)[3].contains("[Voice request"));
        assert!(monotonic(&msgs));
    }

    /// The Hobbes session interleaves Heartbeat wakes with voice turns: a
    /// wake is a turn like any other, so lines said while it ran land
    /// before or after it — chronologically, never inside it.
    #[test]
    fn heartbeat_wakes_are_turns_lines_never_split() {
        let wake = |msgs: &mut Vec<Message>| {
            msgs.push(text("User", &format!("{}\n\nWhy: CI failed", crate::heartbeat::wake::WAKE_HEADER), 40));
            msgs.push(call(42));
            msgs.push(text("Hobbes", "CI is red on main.", 60));
        };
        let mut msgs = brain_turn();
        wake(&mut msgs);
        assert!(starts_turn(&msgs[5]), "a wake starts a turn");
        insert_between_turns(
            &mut msgs,
            vec![line(true, "and the release?", 35), line(false, "Checking.", 45), line(true, "thanks", 61)],
            false,
        );
        let b = bodies(&msgs);
        let wake_at = b.iter().position(|x| x.starts_with(crate::heartbeat::wake::WAKE_HEADER)).unwrap();
        // The wake's turn stays whole: request, call, reply.
        assert_eq!(b[wake_at + 1], "<call>");
        assert_eq!(b[wake_at + 2], "CI is red on main.");
        // Before it: what was said before it (and, snapped to its nearer
        // edge, what was said early in it); after it: what came later.
        assert_eq!(b[wake_at - 2..wake_at], ["🎙 and the release?", "🔊 Checking."]);
        assert_eq!(b[wake_at + 3], "🎙 thanks");
        assert!(monotonic(&msgs));

        // A wake still running at the tail is open: lines go before it.
        let mut open = brain_turn();
        wake(&mut open);
        open.pop();
        assert!(turn_open(&open, false, at(50)));
        insert_between_turns(&mut open, vec![line(true, "bye", 70)], true);
        let b = bodies(&open);
        let wake_at = b.iter().position(|x| x.starts_with(crate::heartbeat::wake::WAKE_HEADER)).unwrap();
        assert_eq!(b[wake_at - 1], "🎙 bye");
        assert!(monotonic(&open));
    }

    /// Lines can sit between a spoken line and a turn that has no request
    /// of its own (a continuation the voice's announcement preceded).
    #[test]
    fn spoken_lines_are_boundaries() {
        let mut msgs = vec![text("Hobbes", "🔊 Excuse me, CI is red.", 0), call(5), text("Hobbes", "Fixed.", 9)];
        insert_between_turns(&mut msgs, vec![line(true, "go ahead", 2)], false);
        assert_eq!(bodies(&msgs)[..2], ["🔊 Excuse me, CI is red.", "🎙 go ahead"]);
    }

    /// The journal projector reproduces the live layout (sorted insertion
    /// by `created_at`), so a rewind's replay keeps the lines in place.
    #[test]
    fn journal_replay_reproduces_the_placement() {
        use crate::session_events::project;
        use crate::session_store::LoadedSessionEvent;
        let base = brain_turn();
        let mut msgs = base.clone();
        let inserted = insert_between_turns(
            &mut msgs,
            vec![line(true, "what's blocked?", 8), line(false, "One moment.", 11), line(false, "Two sessions.", 31)],
            false,
        );
        // Journal order: the turn as it happened, the lines when written.
        let mut events = vec![SessionEvent::SessionCreated { id: "v".into(), name: "Voice".into() }];
        for m in base {
            events.push(if m.author == "User" {
                SessionEvent::UserMessage { message: m }
            } else {
                SessionEvent::AssistantMessage { message: m }
            });
        }
        for m in inserted {
            events.push(SessionEvent::UserMessage { message: m });
        }
        let loaded: Vec<LoadedSessionEvent> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoadedSessionEvent { seq: i as i64 + 1, ts: at(100), event })
            .collect();
        let replayed = project(None, &loaded);
        assert_eq!(
            replayed.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            msgs.iter().map(|m| m.id).collect::<Vec<_>>()
        );
    }

    /// The brain's reply is still found after the request (the lines sit
    /// before it), and the announcement logic still sees the brain's turn.
    #[test]
    fn brain_reply_and_turn_news_still_work() {
        let mut msgs = brain_turn();
        insert_between_turns(&mut msgs, vec![line(true, "what's blocked?", 8), line(false, "One moment.", 11)], false);
        let framed = crate::voice::brain::framed_request("what's blocked?");
        let anchor = crate::voice::brain::find_anchor(&msgs, 0, &framed).unwrap();
        assert_eq!(crate::voice::brain::collect_reply(&msgs, anchor), "Two sessions are blocked.");
        let news = crate::voice::controller::turn_news(&msgs).unwrap();
        assert!(news.voice_request);
        assert_eq!(news.reply, "Two sessions are blocked.");
    }

    fn bubble(secs: i64) -> Message {
        let mut m = text("Hobbes", "", secs);
        m.content = MessageContent::PermissionRequest(ToolCall { tool_name: "SEND_EMAIL".into(), ..Default::default() });
        m
    }

    /// A prose-only turn: its reply is stamped when it finished (stream_manager
    /// restamps it), so "one moment" heard mid-turn goes above the request,
    /// and a line near the end goes after the reply.
    #[test]
    fn a_turn_without_tool_calls_has_a_real_end() {
        let base = vec![text("User", &crate::voice::brain::framed_request("weather?"), 10), text("Hobbes", "Sunny.", 20)];
        let mut msgs = base.clone();
        insert_between_turns(&mut msgs, vec![line(true, "weather?", 9), line(false, "One moment.", 11)], false);
        assert_eq!(bodies(&msgs)[..2], ["🎙 weather?", "🔊 One moment."]);
        assert_eq!(bodies(&msgs)[3], "Sunny.");
        let mut msgs = base;
        insert_between_turns(&mut msgs, vec![line(true, "great", 19)], false);
        assert_eq!(bodies(&msgs)[2], "🎙 great");
        assert!(monotonic(&msgs));
    }

    /// The streaming flag is false while a call waits on approval; the
    /// messages say the turn is open, and lines never go after the bubble.
    #[test]
    fn a_held_turn_is_open() {
        let now = at(100);
        let mut msgs = vec![text("User", &crate::voice::brain::framed_request("email Sam"), 10), bubble(12)];
        assert!(turn_open(&msgs, false, now));
        // Held for a day: still open.
        assert!(turn_open(&msgs, false, at(86_400)));
        let open = turn_open(&msgs, false, now);
        insert_between_turns(&mut msgs, vec![line(false, "It needs your approval.", 13)], open);
        assert_eq!(bodies(&msgs)[0], "🔊 It needs your approval.");
        assert!(matches!(msgs[2].content, MessageContent::PermissionRequest(_)));
        // Approved and running: still open.
        let mut running = vec![text("User", "go", 0), call(1)];
        if let MessageContent::ToolCall(tc) = &mut running[1].content {
            tc.status = ToolCallStatus::Running;
        }
        assert!(turn_open(&running, false, at(3_600)));
    }

    /// The pre-stream gap (request + empty placeholder) and the gap between
    /// continuation rounds (a finished call, no reply yet) are open; a
    /// finished prose reply is not; a mid-turn tail long abandoned is not.
    #[test]
    fn turn_open_reads_the_gaps_from_the_messages() {
        let pre_stream = vec![text("User", "hi", 0), text("Hobbes", "", 0)];
        assert!(turn_open(&pre_stream, false, at(2)));
        let between_rounds = vec![text("User", "hi", 0), call(1)];
        assert!(turn_open(&between_rounds, false, at(3)));
        assert!(!turn_open(&between_rounds, false, at(3_600)), "an abandoned turn doesn't hold lines forever");
        let unanswered = vec![text("User", "hi", 0)];
        assert!(turn_open(&unanswered, false, at(1)));
        let done = vec![text("User", "hi", 0), call(1), text("Hobbes", "Done.", 2)];
        assert!(!turn_open(&done, false, at(3)));
        assert!(turn_open(&done, true, at(3)), "the stream flag still counts");
        // Transcript lines after a finished turn don't make it look open.
        let mut after = done.clone();
        after.push(line(true, "thanks", 4));
        assert!(!turn_open(&after, false, at(5)));
        assert!(!turn_open(&[], false, at(0)));
    }

    /// Auto-recovery prompts are "User" messages inside a turn: no line may
    /// be inserted next to them, and they don't end the turn.
    #[test]
    fn recovery_prompts_are_not_turn_starts() {
        let mut msgs = vec![
            text("User", "do it", 0),
            call(2),
            text("User", "[System: Auto-recovery triggered — the previous response was interrupted by a stream error]", 4),
            call(6),
            text("Hobbes", "Done.", 30),
        ];
        insert_between_turns(&mut msgs, vec![line(true, "hm", 5)], false);
        assert_eq!(bodies(&msgs)[0], "🎙 hm");
        let open = vec![text("User", "do it", 0), call(2), text("User", "[System: Auto-recovery triggered — x]", 4)];
        assert!(turn_open(&open, false, at(5)));
    }

    /// Stamps are fitted strictly inside the gap, so a replay's sorted
    /// insertion never puts a line after a neighbour it shares a stamp with.
    #[test]
    fn stamps_are_strictly_inside_the_gap() {
        let lo = at(10);
        let hi = at(11);
        assert_eq!(fit_stamp(at(9), Some(lo), Some(hi)), lo + Duration::milliseconds(1));
        assert_eq!(fit_stamp(at(12), Some(lo), Some(hi)), hi - Duration::milliseconds(1));
        assert_eq!(fit_stamp(hi, None, Some(hi)), hi - Duration::milliseconds(1));
        assert_eq!(fit_stamp(lo, Some(lo), None), lo + Duration::milliseconds(1));
        // A gap narrower than the step: the midpoint.
        let narrow = lo + Duration::nanoseconds(4);
        let ts = fit_stamp(lo, Some(lo), Some(narrow));
        assert!(lo < ts && ts < narrow);
        // Zero-width gap: the documented fallback.
        assert_eq!(fit_stamp(at(30), Some(lo), Some(lo)), lo);
    }

    /// Ties: neighbours that share stamps with the line's wish, across two
    /// separate flushes, still replay in the live order.
    #[test]
    fn ties_replay_in_live_order_across_flushes() {
        use crate::session_events::project;
        use crate::session_store::LoadedSessionEvent;
        let base = vec![
            text("User", "🎙 a", 10),
            text("User", &crate::voice::brain::framed_request("b"), 10),
            text("Hobbes", "B.", 10),
        ];
        let mut msgs = base.clone();
        let first = insert_between_turns(&mut msgs, vec![line(false, "x", 10)], false);
        let second = insert_between_turns(&mut msgs, vec![line(false, "y", 10)], false);
        let stamps: Vec<_> = msgs.iter().map(|m| m.created_at).collect();
        assert!(stamps.windows(2).all(|w| w[0] <= w[1]));
        let mut events = vec![SessionEvent::SessionCreated { id: "v".into(), name: "Voice".into() }];
        events.extend(base.into_iter().map(|message| SessionEvent::UserMessage { message }));
        events.extend(first.into_iter().chain(second).map(|message| SessionEvent::AssistantMessage { message }));
        let loaded: Vec<LoadedSessionEvent> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoadedSessionEvent { seq: i as i64 + 1, ts: at(100), event })
            .collect();
        let replayed = project(None, &loaded);
        assert_eq!(
            replayed.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            msgs.iter().map(|m| m.id).collect::<Vec<_>>()
        );
    }
}
