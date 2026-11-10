//! Floor control: who may speak now, and what waits until they can.
//!
//! The controller tracks when the person last showed speech (their input
//! transcript, or the mic level clearly above the room's noise) and whether
//! the model is mid-turn. Speech nobody asked for right now — a Heartbeat
//! note, a returned result, a brain answer — waits in a [`SpeechQueue`]
//! until the floor is free: the person isn't speaking (nor was within
//! [`FLOOR_QUIET`]), the push-to-talk key is up, nothing is playing, and the
//! model has finished its turn. Items go out one at a time.
//!
//! Without echo cancellation the model would hear itself on speakers, so the
//! mic isn't streamed while Hobbes talks; [`BargeIn`] watches the muted mic
//! for the person talking over it (the level held clearly above the echo
//! seen during playback) so their words are kept and sent instead of lost.
//!
//! Everything here is pure (no audio, no I/O) and unit-tested.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::voice::controller::AnnounceKind;

/// The person keeps the floor this long after their last speech: a short
/// thinking pause mid-sentence isn't an opening.
pub const FLOOR_QUIET: Duration = Duration::from_millis(1200);

/// After the person's transcribed speech the model owes them a reply;
/// queued speech waits for that reply's turn to finish — at most this long
/// (it may decide nothing needs saying).
pub const REPLY_WAIT: Duration = Duration::from_secs(6);

/// A model turn with no sign of life for this long is treated as over (a
/// lost `TurnComplete` must not hold the queue forever).
pub const MODEL_TURN_STALE: Duration = Duration::from_secs(10);

/// The longest a finished brain result is held for the floor before it goes
/// out anyway (as `WHEN_IDLE`). See [`SpeechQueue`].
pub const BRAIN_RESULT_CAP: Duration = Duration::from_secs(20);

/// Mic level that can count as speech, whatever the room (peak, 0–1).
pub const SPEECH_PEAK_MIN: f32 = 0.05;
/// Speech must also be this many times the tracked noise floor.
pub const SPEECH_NOISE_RATIO: f32 = 3.0;

/// How much muted mic audio is kept while Hobbes talks without echo
/// cancellation: the start of a barge-in, sent once it's detected.
pub const ECHO_RING: Duration = Duration::from_millis(1500);
/// A barge-in must be at least this loud (peak, 0–1)…
pub const BARGE_MIN_PEAK: f32 = 0.08;
/// …and this many times the loudest recent echo of Hobbes' own voice…
pub const BARGE_ECHO_RATIO: f32 = 2.0;
/// …held for this long net (frames above add, frames below subtract, so
/// the dips between syllables don't reset it but a click can't trigger it).
pub const BARGE_SUSTAIN_MS: u32 = 240;
/// The first playback this long only learns the echo level.
pub const BARGE_WARMUP_MS: u32 = 400;
/// The echo estimate halves over roughly this long (it follows the speaker
/// volume down; up immediately).
const ECHO_DECAY_MS: f32 = 3000.0;

/// Mic samples per millisecond (16 kHz input).
const SAMPLES_PER_MS: usize = (crate::voice::realtime::INPUT_SAMPLE_RATE / 1000) as usize;

/// Duration of a mic frame at the input rate, in ms.
pub fn frame_ms(samples: usize) -> u32 {
    (samples / SAMPLES_PER_MS) as u32
}

/// Speech-vs-noise on the mic level: a noise floor that follows quiet
/// frames down quickly and loud ones up slowly (a fan or a busy room
/// doesn't hold the floor forever).
#[derive(Clone, Debug, Default)]
pub struct NoiseGate {
    noise: f32,
}

impl NoiseGate {
    pub fn is_speech(&mut self, peak: f32) -> bool {
        let speech = peak >= SPEECH_PEAK_MIN && peak >= self.noise * SPEECH_NOISE_RATIO;
        let k = if peak < self.noise { 0.1 } else { 0.002 };
        self.noise += (peak - self.noise) * k;
        speech
    }
}

/// Who has the floor.
#[derive(Clone, Debug, Default)]
pub struct Floor {
    /// The person last showed speech (mic level or transcript).
    last_person_speech: Option<Instant>,
    /// The person's last transcript fragment (the model owes a reply).
    last_transcript: Option<Instant>,
    last_turn_end: Option<Instant>,
    model_turn_open: bool,
    last_model_event: Option<Instant>,
}

impl Floor {
    /// The mic heard the person (above noise, or a barge-in).
    pub fn person_spoke(&mut self, now: Instant) {
        self.last_person_speech = Some(now);
    }

    /// A fragment of the person's input transcript arrived.
    pub fn person_transcript(&mut self, now: Instant) {
        self.last_person_speech = Some(now);
        self.last_transcript = Some(now);
    }

    /// The model is producing a turn (audio, its transcript, or we just
    /// asked it to speak).
    pub fn model_active(&mut self, now: Instant) {
        self.model_turn_open = true;
        self.last_model_event = Some(now);
    }

    /// The model's turn ended (`TurnComplete`, or it was interrupted).
    pub fn model_turn_ended(&mut self, now: Instant) {
        self.model_turn_open = false;
        self.last_turn_end = Some(now);
    }

    pub fn person_has_floor(&self, held: bool, now: Instant) -> bool {
        held || self.last_person_speech.is_some_and(|t| now.saturating_duration_since(t) < FLOOR_QUIET)
    }

    /// The model is talking, mid-turn, or about to answer the person.
    pub fn model_busy(&self, playing: bool, now: Instant) -> bool {
        let mid_turn = self.model_turn_open
            && self.last_model_event.is_some_and(|t| now.saturating_duration_since(t) < MODEL_TURN_STALE);
        let owes_reply = self.last_transcript.is_some_and(|t| {
            self.last_turn_end.is_none_or(|e| t > e) && now.saturating_duration_since(t) < REPLY_WAIT
        });
        playing || mid_turn || owes_reply
    }

    /// Unsolicited speech may start now.
    pub fn is_free(&self, held: bool, playing: bool, now: Instant) -> bool {
        self.blocker(held, playing, now).is_none()
    }

    /// Which condition keeps the floor taken right now (for logs; no
    /// content), or `None` when it's free.
    pub fn blocker(&self, held: bool, playing: bool, now: Instant) -> Option<&'static str> {
        if held {
            return Some("key held");
        }
        if self.person_has_floor(false, now) {
            return Some("person speaking");
        }
        if playing {
            return Some("playing");
        }
        if self.model_busy(false, now) {
            let mid_turn = self.model_turn_open
                && self.last_model_event.is_some_and(|t| now.saturating_duration_since(t) < MODEL_TURN_STALE);
            return Some(if mid_turn { "model mid-turn" } else { "model owes a reply" });
        }
        None
    }
}

/// Playback is treated as ongoing this long after the output queue empties:
/// the speaker and the room still carry the last of it.
pub const PLAYBACK_HANGOVER: Duration = Duration::from_millis(250);

/// After the voice is cut off for a key press (no echo cancellation), mic
/// frames are dropped this long: the speaker's tail.
pub const SPEAKER_TAIL: Duration = Duration::from_millis(200);

/// Audio kept from before a barge-in's rise above the threshold.
pub const BARGE_PREROLL_MS: u32 = 200;

/// "Is Hobbes audible?" with a [`PLAYBACK_HANGOVER`] after the output queue
/// empties.
#[derive(Clone, Debug, Default)]
pub struct PlaybackTail {
    last_playing: Option<Instant>,
}

impl PlaybackTail {
    /// Note the output queue's state; true while playing or within the
    /// hangover after it stopped.
    pub fn observe(&mut self, playing: bool, now: Instant) -> bool {
        if playing {
            self.last_playing = Some(now);
            return true;
        }
        self.last_playing.is_some_and(|t| now.saturating_duration_since(t) < PLAYBACK_HANGOVER)
    }

    /// Playback was cut deliberately (barge-in, key press): no hangover.
    pub fn reset(&mut self) {
        self.last_playing = None;
    }
}

/// What to do with one mic frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicGate {
    /// Push-to-talk between holds: nothing goes out.
    Closed,
    /// The speaker's tail after a cut: discard.
    Drop,
    /// The model could hear itself: keep it muted (ring + barge-in detector).
    Hold,
    /// Stream it.
    Send,
}

/// The gate for a mic frame: `audible` is playback with its hangover;
/// `drop_until` the speaker-tail window after a cut.
pub fn mic_gate(mic_open: bool, held: bool, echo_safe: bool, audible: bool, drop_until: Option<Instant>, now: Instant) -> MicGate {
    if !mic_open {
        MicGate::Closed
    } else if drop_until.is_some_and(|t| now < t) {
        MicGate::Drop
    } else if !echo_safe && audible && !held {
        MicGate::Hold
    } else {
        MicGate::Send
    }
}

/// The model's current turn, for muting the rest of one the person cut off.
/// A cut after the model already finished its turn mutes nothing (its next
/// reply is a new turn and must be heard).
#[derive(Clone, Debug, Default)]
pub struct ModelTurn {
    open: bool,
    muting: bool,
}

impl ModelTurn {
    /// Model audio arrived: its turn is open.
    pub fn audio(&mut self) {
        self.open = true;
    }

    /// `TurnComplete` / `Interrupted` (or a reconnect): the turn is over.
    pub fn ended(&mut self) {
        self.open = false;
        self.muting = false;
    }

    /// The person cut the voice off locally: mute the rest of the turn, if
    /// one is still open.
    pub fn cut(&mut self) {
        self.muting = self.open;
    }

    /// The rest of a cut turn: neither played nor transcribed.
    pub fn muting(&self) -> bool {
        self.muting
    }
}

/// Speech waiting for the floor.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    /// News for the person ("Excuse me…").
    Announce { text: String, kind: AnnounceKind },
    /// A brain result whose call the model already abandoned.
    Abandoned { text: String },
    /// A brain result for a call still open (its final tool response).
    BrainResult { id: String, text: String },
}

/// What to send now.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// News; `merged` > 1 when several Heartbeat notes were folded into
    /// the newest one.
    Announce { text: String, kind: AnnounceKind, merged: usize },
    Abandoned { text: String },
    /// The final tool response; `forced` = it waited [`BRAIN_RESULT_CAP`]
    /// and goes out without a free floor.
    BrainResult { id: String, text: String, forced: bool },
}

/// Unsolicited speech, delivered one item at a time when the floor is free.
///
/// **Brain results.** The model got an interim response (`willContinue`)
/// when the brain started, so it already told the person it's working and
/// isn't waiting on the call; holding the final response a little longer is
/// safe. A `WHEN_IDLE` result only waits for the *model* to be idle, not the
/// person to be quiet, so it is held here until the floor is free — but no
/// longer than [`BRAIN_RESULT_CAP`]: a person who keeps talking for 20 s
/// isn't left wondering whether the request was lost, and `WHEN_IDLE` still
/// keeps the model from cutting across its own speech. Results that are
/// held keep their arrival order relative to the other news.
#[derive(Debug, Default)]
pub struct SpeechQueue {
    items: VecDeque<(Pending, Instant)>,
}

impl SpeechQueue {
    pub fn push(&mut self, item: Pending, now: Instant) {
        self.items.push_back((item, now));
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The next thing to say, if any may be said now. Several queued
    /// Heartbeat notes go as one: the newest (its information supersedes the
    /// older ones), noting how many there were.
    /// `held`: the push-to-talk key is down — nothing is forced over it.
    pub fn next(&mut self, floor_free: bool, held: bool, now: Instant) -> Option<Delivery> {
        if !floor_free {
            if held {
                return None;
            }
            let overdue = self.items.iter().position(|(p, at)| {
                matches!(p, Pending::BrainResult { .. }) && now.saturating_duration_since(*at) >= BRAIN_RESULT_CAP
            })?;
            let (Pending::BrainResult { id, text }, _) = self.items.remove(overdue)? else { return None };
            return Some(Delivery::BrainResult { id, text, forced: true });
        }
        let (front, _) = self.items.pop_front()?;
        Some(match front {
            Pending::Announce { text, kind: AnnounceKind::Heartbeat } => {
                let mut newest = text;
                let mut merged = 1;
                self.items.retain(|(p, _)| match p {
                    Pending::Announce { text, kind: AnnounceKind::Heartbeat } => {
                        newest = text.clone();
                        merged += 1;
                        false
                    }
                    _ => true,
                });
                Delivery::Announce { text: newest, kind: AnnounceKind::Heartbeat, merged }
            }
            Pending::Announce { text, kind } => Delivery::Announce { text, kind, merged: 1 },
            Pending::Abandoned { text } => Delivery::Abandoned { text },
            Pending::BrainResult { id, text } => Delivery::BrainResult { id, text, forced: false },
        })
    }

    /// The model no longer holds these calls (`abandoned(id)`: cancelled,
    /// or every call after a reconnect): their queued results go as
    /// injected context instead. Order is kept.
    pub fn abandon_brain_results(&mut self, abandoned: impl Fn(&str) -> bool) -> usize {
        let mut n = 0;
        for (p, _) in self.items.iter_mut() {
            if let Pending::BrainResult { id, text } = p {
                if !abandoned(id) {
                    continue;
                }
                *p = Pending::Abandoned { text: std::mem::take(text) };
                n += 1;
            }
        }
        n
    }

    /// Everything still waiting, as announcements (the conversation is
    /// ending: they're said afterwards instead). Oldest first.
    pub fn drain_as_announcements(&mut self) -> Vec<(String, AnnounceKind)> {
        self.items
            .drain(..)
            .map(|(p, _)| match p {
                Pending::Announce { text, kind } => (text, kind),
                Pending::Abandoned { text } | Pending::BrainResult { text, .. } => (text, AnnounceKind::Result),
            })
            .collect()
    }
}

/// Local barge-in on a muted mic (no echo cancellation, Hobbes talking on
/// speakers): the person talking over the echo, loudly enough and long
/// enough to be them and not the speaker.
#[derive(Clone, Debug, Default)]
pub struct BargeIn {
    /// Loudest recent echo (peak, decaying).
    echo_peak: f32,
    /// Playback heard so far this conversation (for the warm-up).
    heard_ms: u32,
    /// Net time above the threshold.
    above_ms: u32,
    /// Audio since the current rise above the threshold began (while
    /// `above_ms` > 0).
    rise_ms: u32,
}

impl BargeIn {
    pub fn threshold(&self) -> f32 {
        BARGE_MIN_PEAK.max(self.echo_peak * BARGE_ECHO_RATIO)
    }

    /// One muted mic frame during playback. `Some(ms)` = the person barged
    /// in, their speech having risen above the threshold `ms` ago (this
    /// frame included); the detector re-arms itself.
    pub fn observe(&mut self, peak: f32, ms: u32) -> Option<u32> {
        self.heard_ms = self.heard_ms.saturating_add(ms);
        if self.heard_ms <= BARGE_WARMUP_MS {
            self.echo_peak = self.echo_peak.max(peak);
            return None;
        }
        if peak > self.threshold() {
            if self.above_ms == 0 {
                self.rise_ms = 0;
            }
            self.above_ms += ms;
        } else {
            self.above_ms = self.above_ms.saturating_sub(ms);
            let decay = 0.5f32.powf(ms as f32 / ECHO_DECAY_MS);
            self.echo_peak = peak.max(self.echo_peak * decay);
        }
        if self.above_ms > 0 {
            self.rise_ms = self.rise_ms.saturating_add(ms);
        } else {
            self.rise_ms = 0;
        }
        if self.above_ms >= BARGE_SUSTAIN_MS {
            self.above_ms = 0;
            return Some(std::mem::take(&mut self.rise_ms));
        }
        None
    }

    /// Playback stopped: start the next detection fresh (the echo level is
    /// kept: same speaker, same room).
    pub fn playback_ended(&mut self) {
        self.above_ms = 0;
        self.rise_ms = 0;
    }
}

/// The muted mic's last [`ECHO_RING`] of audio.
#[derive(Debug, Default)]
pub struct MicRing {
    frames: VecDeque<Vec<i16>>,
    samples: usize,
}

impl MicRing {
    pub fn push(&mut self, frame: Vec<i16>) {
        let cap = ECHO_RING.as_millis() as usize * SAMPLES_PER_MS;
        self.samples += frame.len();
        self.frames.push_back(frame);
        while self.samples > cap && self.frames.len() > 1 {
            if let Some(f) = self.frames.pop_front() {
                self.samples -= f.len();
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The last `ms` of audio kept (whole frames, oldest first; at least the
    /// newest frame), and how many older frames were discarded; the ring is
    /// left empty.
    pub fn drain_recent(&mut self, ms: u32) -> (Vec<Vec<i16>>, usize) {
        let want = ms as usize * SAMPLES_PER_MS;
        let mut kept = 0usize;
        let mut start = self.frames.len();
        while start > 0 && (kept < want || start == self.frames.len()) {
            start -= 1;
            kept += self.frames[start].len();
        }
        let dropped = start;
        let out = self.frames.drain(..).skip(start).collect();
        self.samples = 0;
        (out, dropped)
    }

    /// Discard (no barge-in: it was Hobbes' own echo). Returns the count.
    pub fn clear(&mut self) -> usize {
        let n = self.frames.len();
        self.frames.clear();
        self.samples = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_person_keeps_the_floor_through_a_pause() {
        let t0 = Instant::now();
        let mut f = Floor::default();
        assert!(!f.person_has_floor(false, t0));
        // Holding the key is having the floor.
        assert!(f.person_has_floor(true, t0));
        f.person_spoke(t0);
        assert!(f.person_has_floor(false, t0 + ms(1000)));
        assert!(!f.is_free(false, false, t0 + ms(1000)));
        assert!(!f.person_has_floor(false, t0 + FLOOR_QUIET));
        // Level alone (no transcript) owes no reply: free after the quiet window.
        assert!(f.is_free(false, false, t0 + FLOOR_QUIET));
    }

    #[test]
    fn the_model_must_finish_its_turn_and_its_reply() {
        let t0 = Instant::now();
        let mut f = Floor::default();
        // Playing: busy.
        assert!(f.model_busy(true, t0));
        // Mid-turn without audio playing yet.
        f.model_active(t0);
        assert!(f.model_busy(false, t0 + ms(500)));
        // A lost TurnComplete doesn't hold it forever.
        assert!(!f.model_busy(false, t0 + MODEL_TURN_STALE));
        f.model_turn_ended(t0 + ms(600));
        assert!(!f.model_busy(false, t0 + ms(700)));
        // The person said something: the model owes a reply.
        let t1 = t0 + ms(1000);
        f.person_transcript(t1);
        let after_quiet = t1 + FLOOR_QUIET + ms(10);
        assert!(!f.person_has_floor(false, after_quiet));
        assert!(f.model_busy(false, after_quiet));
        assert!(!f.is_free(false, false, after_quiet));
        // It answered and finished: free.
        f.model_active(t1 + ms(1500));
        f.model_turn_ended(t1 + ms(3000));
        assert!(f.is_free(false, false, t1 + ms(3100)));
        // It never answers: free after REPLY_WAIT.
        let t2 = t1 + ms(5000);
        f.person_transcript(t2);
        assert!(!f.is_free(false, false, t2 + ms(3000)));
        assert!(f.is_free(false, false, t2 + REPLY_WAIT));
    }

    #[test]
    fn the_noise_gate_adapts_to_a_steady_room() {
        let mut g = NoiseGate::default();
        assert!(!g.is_speech(0.01));
        assert!(g.is_speech(0.2));
        // A loud fan for a while stops counting as speech.
        let mut last = true;
        for _ in 0..5000 {
            last = g.is_speech(0.08);
        }
        assert!(!last);
        // Speech over it still does.
        assert!(g.is_speech(0.4));
    }

    #[test]
    fn queue_waits_for_the_floor_and_goes_one_at_a_time() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::Announce { text: "result".into(), kind: AnnounceKind::Result }, t0);
        q.push(Pending::Abandoned { text: "late".into() }, t0);
        assert_eq!(q.next(false, false, t0), None);
        assert_eq!(q.len(), 2);
        assert_eq!(
            q.next(true, false, t0),
            Some(Delivery::Announce { text: "result".into(), kind: AnnounceKind::Result, merged: 1 })
        );
        assert_eq!(q.next(true, false, t0), Some(Delivery::Abandoned { text: "late".into() }));
        assert!(q.is_empty());
        assert_eq!(q.next(true, false, t0), None);
    }

    #[test]
    fn heartbeat_notes_coalesce_newest_wins() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::Announce { text: "hb1".into(), kind: AnnounceKind::Heartbeat }, t0);
        q.push(Pending::Announce { text: "res".into(), kind: AnnounceKind::Result }, t0);
        q.push(Pending::Announce { text: "hb2".into(), kind: AnnounceKind::Heartbeat }, t0);
        q.push(Pending::Announce { text: "hb3".into(), kind: AnnounceKind::Heartbeat }, t0);
        assert_eq!(
            q.next(true, false, t0),
            Some(Delivery::Announce { text: "hb3".into(), kind: AnnounceKind::Heartbeat, merged: 3 })
        );
        assert_eq!(
            q.next(true, false, t0),
            Some(Delivery::Announce { text: "res".into(), kind: AnnounceKind::Result, merged: 1 })
        );
        assert!(q.is_empty());
    }

    #[test]
    fn brain_results_wait_for_the_floor_but_not_forever() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::Announce { text: "hb".into(), kind: AnnounceKind::Heartbeat }, t0);
        q.push(Pending::BrainResult { id: "c1".into(), text: "done".into() }, t0 + ms(100));
        // Person still talking: held.
        assert_eq!(q.next(false, false, t0 + ms(5000)), None);
        // Past the cap: the brain result goes out anyway; the announcement waits.
        assert_eq!(
            q.next(false, false, t0 + ms(100) + BRAIN_RESULT_CAP),
            Some(Delivery::BrainResult { id: "c1".into(), text: "done".into(), forced: true })
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q.next(false, false, t0 + ms(60_000)), None);
        // Free floor: in order, not forced.
        q.push(Pending::BrainResult { id: "c2".into(), text: "two".into() }, t0);
        assert!(matches!(q.next(true, false, t0), Some(Delivery::Announce { .. })));
        assert_eq!(q.next(true, false, t0), Some(Delivery::BrainResult { id: "c2".into(), text: "two".into(), forced: false }));
    }

    #[test]
    fn echo_alone_never_barges_in() {
        let mut b = BargeIn::default();
        // Hobbes' voice through the speakers into the mic: varying, up to 0.15.
        for i in 0..500 {
            let p = 0.05 + 0.1 * ((i % 7) as f32 / 6.0);
            assert!(b.observe(p, 20).is_none(), "echo frame {i} triggered");
        }
        b.playback_ended();
        // A single loud click doesn't either.
        for _ in 0..3 {
            assert!(b.observe(0.9, 20).is_none());
        }
        for _ in 0..20 {
            assert!(b.observe(0.05, 20).is_none());
        }
    }

    #[test]
    fn talking_over_the_echo_barges_in() {
        let mut b = BargeIn::default();
        for _ in 0..50 {
            b.observe(0.1, 20);
        }
        assert!(b.threshold() >= 0.2);
        // The person speaks clearly over it, with dips between syllables.
        let mut fired_at = None;
        for i in 0..40 {
            let p = if i % 4 == 3 { 0.12 } else { 0.5 };
            if b.observe(p, 20).is_some() {
                fired_at = Some(i);
                break;
            }
        }
        let at = fired_at.expect("barge-in detected");
        // ~240 ms net: under half a second of speech with dips.
        assert!((11..24).contains(&at), "fired at frame {at}");
    }

    #[test]
    fn the_warm_up_learns_the_echo() {
        let mut b = BargeIn::default();
        // Loud echo from the first frame: learned, not mistaken for speech.
        for _ in 0..(BARGE_WARMUP_MS / 20) {
            assert!(b.observe(0.3, 20).is_none());
        }
        assert!(b.threshold() >= 0.6 - f32::EPSILON);
        for _ in 0..30 {
            assert!(b.observe(0.3, 20).is_none());
        }
    }

    #[test]
    fn the_ring_keeps_the_last_second_and_a_half() {
        let mut r = MicRing::default();
        for i in 0..100 {
            r.push(vec![i as i16; 320]); // 20 ms frames
        }
        let (frames, dropped) = r.drain_recent(ECHO_RING.as_millis() as u32);
        assert_eq!((frames.len(), dropped), (75, 0));
        assert_eq!(frames.first().unwrap()[0], 25);
        assert_eq!(frames.last().unwrap()[0], 99);
        assert!(r.is_empty());
        r.push(vec![0; 320]);
        assert_eq!(r.clear(), 1);
        assert_eq!(frame_ms(320), 20);
    }

    #[test]
    fn a_held_key_is_never_talked_over_by_a_forced_result() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::BrainResult { id: "c1".into(), text: "done".into() }, t0);
        let late = t0 + BRAIN_RESULT_CAP + ms(1);
        assert_eq!(q.next(false, true, late), None);
        assert!(matches!(q.next(false, false, late), Some(Delivery::BrainResult { forced: true, .. })));
    }

    #[test]
    fn the_floor_names_what_blocks_it() {
        let t0 = Instant::now();
        let mut f = Floor::default();
        assert_eq!(f.blocker(false, false, t0), None);
        assert_eq!(f.blocker(true, false, t0), Some("key held"));
        assert_eq!(f.blocker(false, true, t0), Some("playing"));
        f.model_active(t0);
        assert_eq!(f.blocker(false, false, t0 + ms(10)), Some("model mid-turn"));
        f.model_turn_ended(t0 + ms(20));
        f.person_transcript(t0 + ms(30));
        assert_eq!(f.blocker(false, false, t0 + ms(40)), Some("person speaking"));
        assert_eq!(f.blocker(false, false, t0 + ms(30) + FLOOR_QUIET), Some("model owes a reply"));
    }

    #[test]
    fn a_reconnect_turns_queued_results_into_context() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::Announce { text: "hb".into(), kind: AnnounceKind::Heartbeat }, t0);
        q.push(Pending::BrainResult { id: "c1".into(), text: "done".into() }, t0);
        q.push(Pending::BrainResult { id: "c2".into(), text: "two".into() }, t0);
        // A cancelled call: only its result moves.
        assert_eq!(q.abandon_brain_results(|id| id == "c2"), 1);
        // A reconnect: all of them.
        assert_eq!(q.abandon_brain_results(|_| true), 1);
        assert!(matches!(q.next(true, false, t0), Some(Delivery::Announce { .. })));
        assert_eq!(q.next(true, false, t0), Some(Delivery::Abandoned { text: "done".into() }));
        assert_eq!(q.next(true, false, t0), Some(Delivery::Abandoned { text: "two".into() }));
    }

    #[test]
    fn unspoken_items_become_announcements() {
        let t0 = Instant::now();
        let mut q = SpeechQueue::default();
        q.push(Pending::Announce { text: "hb".into(), kind: AnnounceKind::Heartbeat }, t0);
        q.push(Pending::BrainResult { id: "c1".into(), text: "done".into() }, t0);
        q.push(Pending::Abandoned { text: "late".into() }, t0);
        assert_eq!(
            q.drain_as_announcements(),
            vec![
                ("hb".to_string(), AnnounceKind::Heartbeat),
                ("done".to_string(), AnnounceKind::Result),
                ("late".to_string(), AnnounceKind::Result),
            ]
        );
        assert!(q.is_empty());
    }

    #[test]
    fn a_barge_in_sends_from_where_the_person_started() {
        let mut b = BargeIn::default();
        let mut r = MicRing::default();
        // Echo for a second: learned, kept muted.
        for _ in 0..50 {
            r.push(vec![1; 320]);
            assert!(b.observe(0.1, 20).is_none());
        }
        // The person starts talking over it.
        let mut rise = None;
        for _ in 0..40 {
            r.push(vec![2; 320]);
            if let Some(ms) = b.observe(0.5, 20) {
                rise = Some(ms);
                break;
            }
        }
        let rise = rise.expect("barge-in");
        assert_eq!(rise, BARGE_SUSTAIN_MS);
        let (frames, dropped) = r.drain_recent(rise + BARGE_PREROLL_MS);
        // Their speech, plus ~200 ms of what came just before.
        assert_eq!(frames.len() as u32, (rise + BARGE_PREROLL_MS) / 20);
        assert_eq!(frames.iter().filter(|f| f[0] == 2).count() as u32, rise / 20);
        assert_eq!(frames.len() + dropped, 62);
        assert!(r.is_empty());
        // Always at least the newest frame.
        r.push(vec![3; 320]);
        assert_eq!(r.drain_recent(0), (vec![vec![3; 320]], 0));
    }

    #[test]
    fn playback_lingers_briefly_after_the_queue_empties() {
        let t0 = Instant::now();
        let mut p = PlaybackTail::default();
        assert!(!p.observe(false, t0));
        assert!(p.observe(true, t0));
        assert!(p.observe(false, t0 + ms(100)));
        assert!(!p.observe(false, t0 + PLAYBACK_HANGOVER));
        p.observe(true, t0);
        p.reset();
        assert!(!p.observe(false, t0 + ms(10)));
    }

    #[test]
    fn the_mic_gate() {
        let t0 = Instant::now();
        assert_eq!(mic_gate(false, false, false, true, None, t0), MicGate::Closed);
        // The speaker's tail after a cut: dropped, held key or not.
        assert_eq!(mic_gate(true, true, false, false, Some(t0 + SPEAKER_TAIL), t0), MicGate::Drop);
        assert_eq!(mic_gate(true, true, false, false, Some(t0), t0), MicGate::Send);
        // Hobbes audible on speakers: muted unless the key is held.
        assert_eq!(mic_gate(true, false, false, true, None, t0), MicGate::Hold);
        assert_eq!(mic_gate(true, true, false, true, None, t0), MicGate::Send);
        assert_eq!(mic_gate(true, false, true, true, None, t0), MicGate::Send);
    }

    #[test]
    fn a_cut_after_the_turn_ended_mutes_nothing() {
        let mut m = ModelTurn::default();
        m.audio();
        m.cut();
        assert!(m.muting());
        m.ended();
        assert!(!m.muting());
        // Server already finished the turn; only the local queue was playing.
        m.audio();
        m.ended();
        m.cut();
        assert!(!m.muting());
    }
}
