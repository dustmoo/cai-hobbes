//! Where voice transcript lines go in the Voice session.
//!
//! Transcript lines are buffered while the brain's turn runs (appending
//! mid-turn would split it) and written later. Appending them at write time
//! put what the person said *below* the brain's answer to it. Instead each
//! line carries the moment its utterance began and is inserted where that
//! moment falls — but only between turns, never inside one: a moment that
//! falls inside a turn (between its request and its last message) snaps to
//! the nearer edge of that turn, and a turn still streaming at the tail only
//! has its start edge. The vec stays `created_at`-monotonic (the stamp is
//! clamped into the gap it lands in), which is what keeps the renderer (it
//! sorts by `created_at`), the prompt builder (vec order) and journal replay
//! (`session_events::upsert_message`, sorted insertion) in agreement.

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::session::SessionState;
use crate::session_events::SessionEvent;
use crate::voice::session::{transcript_message, SPOKE_PREFIX};

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
    let next = &messages[j];
    next.author != "Hobbes" || standalone(next) || standalone(&messages[j - 1])
}

/// Insert `lines` (already stamped, in spoken order) between turns at their
/// chronological positions; see the module docs. `tail_open`: the session's
/// last turn is still streaming, so nothing may follow it. Returns the
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
                    let until_end = messages[end - 1].created_at - ts;
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
        // Clamp into the gap so sorted insertion (journal replay) lands it here.
        let lo = j.checked_sub(1).map(|i| messages[i].created_at);
        let hi = messages.get(j).map(|m| m.created_at);
        if let Some(lo) = lo {
            ts = ts.max(lo);
        }
        if let Some(hi) = hi {
            if ts >= hi {
                let below = hi - Duration::milliseconds(1);
                ts = lo.map_or(below, |lo| below.max(lo));
            }
        }
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
    let tail_open = crate::session_streaming(session_id);
    let inserted = {
        let mut state = session_state.write();
        match state.sessions.get_mut(session_id) {
            Some(s) => insert_between_turns(&mut s.messages, lines, tail_open),
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
    use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus};

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
}
