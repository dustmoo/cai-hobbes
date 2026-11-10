//! The voice controller: one live conversation at a time, driven by the
//! hotkey (press / release) and by the realtime session's events.
//!
//! **Activation.** A press opens the mic at once (speech before the
//! connection is ready is buffered by the channel). A short press (a tap)
//! starts — or ends — a hands-free conversation; a long press (a hold) is
//! push-to-talk: the mic closes on release and the provider's turn
//! detection takes it from there. Both use the provider's own voice-activity
//! detection, so one session setup serves both.
//!
//! **Barge-in.** The provider reports `Interrupted` when the person talks
//! over the voice; playback is flushed immediately. When the model could
//! hear itself (no echo cancellation and the output isn't headphones), the
//! mic isn't streamed while the voice speaks: the last moments are kept and
//! a local detector (`floor::BargeIn`) stops playback and sends them when
//! the person talks over it, so their words aren't lost.
//!
//! **Floor control.** Speech nobody just asked for — announcements, brain
//! results — waits in a queue (`floor::SpeechQueue`) until the person has
//! stopped talking and the model has finished its turn, then goes out one
//! item at a time.
//!
//! **Pipes.** The strip shows which mic is live and its level, and warns
//! within a few seconds when no audio (or only digital silence) arrives.
//! Each conversation logs counts — frames captured and sent, model audio,
//! transcript fragments — never content.
//!
//! **The brain.** `VOICE_ASK_BRAIN` runs in the background (`brain.rs`); its
//! result goes back as the tool response (`WHEN_IDLE`), or as injected
//! context when the model already abandoned the call.
//!
//! Runs on the Dioxus runtime: signals are only peeked/written between
//! awaits, never held across one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::components::builtin_tools::BuiltinToolCtx;
use crate::voice::audio::{self, AudioIo, MicPermission, VoiceEcho};
use crate::voice::floor::{
    self, BargeIn, Delivery, Floor, MicGate, MicRing, ModelTurn, NoiseGate, Pending, PlaybackTail, SpeechQueue,
};
use crate::voice::probe;
use crate::voice::brain::{self, BrainHandle, BrainOutcome};
use crate::voice::realtime::{
    RealtimeControl, RealtimeEvent, RealtimeSetup, RealtimeVoice, Scheduling,
};
use crate::voice::transcript::{PendingLine, Utterance};
use crate::voice::{session, tools};

/// What the voice is doing (drives the indicator).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Connecting,
    Listening,
    Speaking,
}

/// Hands-free conversation or push-to-talk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Conversation,
    PushToTalk,
}

/// Indicator state.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct VoiceUi {
    pub phase: Phase,
    pub mode: Option<Mode>,
    /// The push-to-talk key is down (mic open).
    pub held: bool,
    /// Brain requests in flight.
    pub brain_busy: usize,
    /// What the person is saying right now (live transcript tail).
    pub heard: String,
    /// The model can't hear itself (echo cancelled, or headphones).
    pub echo_safe: bool,
    /// The mic being captured from.
    pub mic: String,
    /// Recent mic peak level, 0.0–1.0.
    pub level: f32,
    /// The mic device is open (push-to-talk releases it between holds).
    pub mic_on: bool,
    /// The mic isn't delivering audio (shown while the conversation runs).
    pub mic_warning: Option<String>,
    /// A passing hint (shown where the mic warning goes, when there is none).
    pub notice: Option<String>,
    /// Last problem worth showing (cleared on the next successful start).
    pub error: Option<String>,
}

pub static VOICE_UI: GlobalSignal<VoiceUi> = Signal::global(VoiceUi::default);

/// Inputs to the controller.
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceCmd {
    /// Hotkey (or button) went down.
    Press,
    /// Hotkey went up after `held_ms`.
    Release { held_ms: u64 },
    /// End any live conversation.
    Stop,
    /// News that reached the Hobbes tab on its own (dispatched work
    /// finishing, a brain answer after the conversation ended, a Heartbeat
    /// wake with something for the person): said as "Excuse me…" in a live
    /// conversation, or in a short speaking-only session (mic off)
    /// otherwise.
    Announce(String, AnnounceKind),
}

/// What an announcement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceKind {
    /// A result of work handed off earlier.
    Result,
    /// What a Heartbeat wake found for the person.
    Heartbeat,
}

/// How a session starts.
#[derive(Clone, Debug, PartialEq)]
enum Start {
    /// The hotkey went down: mic open, tap or hold decided on release.
    Press,
    /// Hobbes speaks first, with the mic off; a press opens it.
    Announce(String, AnnounceKind),
}

/// The running controller's inbox (replaced if the root remounts).
static CMD_TX: Mutex<Option<mpsc::UnboundedSender<VoiceCmd>>> = Mutex::new(None);

/// Send a command to the controller (no-op before it starts).
pub fn send(cmd: VoiceCmd) {
    if let Some(tx) = CMD_TX.lock().ok().and_then(|g| g.clone()) {
        let _ = tx.send(cmd);
    }
}

/// Whether a conversation is live (connecting counts).
pub fn is_live() -> bool {
    VOICE_UI.peek().phase != Phase::Idle
}

/// Start the controller (once, from the app root).
pub fn use_voice(ctx: BuiltinToolCtx) {
    use_hook(move || {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut slot) = CMD_TX.lock() {
            *slot = Some(tx);
        }
        spawn(controller_loop(ctx, rx));
    });
}

async fn controller_loop(ctx: BuiltinToolCtx, mut rx: mpsc::UnboundedReceiver<VoiceCmd>) {
    while let Some(cmd) = rx.recv().await {
        let start = match cmd {
            VoiceCmd::Press => Start::Press,
            VoiceCmd::Announce(text, kind) => {
                let s = ctx.settings.peek();
                if !crate::voice::voice_active(&s) || !s.voice.announce_results {
                    continue;
                }
                Start::Announce(text, kind)
            }
            // Release / Stop / alerts while idle: nothing to do.
            _ => continue,
        };
        {
            run_session(ctx, &mut rx, start).await;
            // Keep the last error (read before the write: one borrow at a time).
            let error = VOICE_UI.peek().error.clone();
            *VOICE_UI.write() = VoiceUi { error, ..VoiceUi::default() };
        }
    }
}

/// What to say for a finished Hobbes-tab turn: the reply after its trigger
/// (the last message not written by Hobbes), whether that trigger was a
/// voice request, and whether the turn is held on an approval.
#[derive(Debug, PartialEq)]
pub struct TurnNews {
    pub voice_request: bool,
    pub reply: String,
    pub needs_approval: bool,
    /// The turn was a Heartbeat wake (not returned work, a relay or a timer).
    pub heartbeat_wake: bool,
    /// The chat tab the full details live in, when the turn answered a
    /// report from a chat worker (its tab, not the Hobbes tab's summary).
    pub details_tab: Option<String>,
}

pub fn turn_news(messages: &[crate::components::chat::Message]) -> Option<TurnNews> {
    let trigger = messages.iter().rposition(|m| m.author != "Hobbes")?;
    let trigger_text = messages[trigger].content.get_text_content();
    let voice_request = trigger_text.as_deref().is_some_and(|t| brain::is_voice_request(t.trim()));
    let heartbeat_wake = trigger_text
        .as_deref()
        .is_some_and(|t| t.trim_start().starts_with(crate::heartbeat::wake::WAKE_HEADER));
    let details_tab = match &messages[trigger].content {
        crate::components::shared::MessageContent::Relay(note)
            if note.direction == crate::todo::relay::RelayDirection::FromWorker =>
        {
            Some(note.peer_title.trim().to_string()).filter(|t| !t.is_empty())
        }
        _ => None,
    };
    Some(TurnNews {
        voice_request,
        heartbeat_wake,
        details_tab,
        reply: brain::collect_reply(messages, trigger),
        needs_approval: crate::heartbeat::wake::has_pending_approval(&messages[trigger + 1..]),
    })
}

/// Whether the Hobbes window has keyboard focus (fed by the window's focus
/// events in `main.rs`). A tab on screen behind other apps isn't being read.
static APP_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_app_focused(focused: bool) {
    APP_FOCUSED.store(focused, std::sync::atomic::Ordering::Relaxed);
}

fn app_focused() -> bool {
    APP_FOCUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether a finished Hobbes-tab turn should be spoken. A voice request the
/// live conversation is still waiting on is delivered by that wait; with no
/// conversation, a turn the person is reading as it lands (`viewing`: the
/// Hobbes tab active in the focused window) needs no announcement, whoever
/// started it, unless it answers an earlier voice request.
pub fn should_announce(news: &TurnNews, live: bool, waits_in_flight: usize, viewing: bool) -> bool {
    if news.reply.is_empty() && !news.needs_approval {
        return false;
    }
    if live && news.voice_request && waits_in_flight > 0 {
        return false;
    }
    live || !viewing || news.voice_request
}

/// Whether a finished turn's news should be spoken, given who started it:
/// an unattended turn never speaks its "nothing to report" line; a
/// Heartbeat wake speaks only with `speak_heartbeat` on. Returned work, a
/// relay or a timer is a result, whatever origin it ran under.
pub fn announce_kind(origin: crate::session::TurnOrigin, news: &TurnNews, speak_heartbeat: bool) -> Option<AnnounceKind> {
    if !origin.is_unattended() {
        return Some(AnnounceKind::Result);
    }
    if !news.needs_approval && crate::heartbeat::wake::is_quiet_reply(&news.reply) {
        return None;
    }
    if !news.heartbeat_wake {
        return Some(AnnounceKind::Result);
    }
    speak_heartbeat.then_some(AnnounceKind::Heartbeat)
}

/// What the voice would say about the Hobbes session's latest finished
/// turn right now, if anything: the one decision both the announcement
/// ([`on_turn_end`]) and the Fleet toast's suppression (`main.rs`) use, so a
/// toast is never swallowed for news that won't be spoken.
pub fn pending_announcement(
    session_state: Signal<crate::session::SessionState>,
    settings: Signal<crate::settings::Settings>,
    session_id: &str,
) -> Option<(String, AnnounceKind)> {
    if !crate::system_sessions::is_current(session_id) {
        return None;
    }
    let speak_heartbeat = {
        let s = settings.peek();
        if !crate::voice::voice_active(&s) || !s.voice.announce_results {
            return None;
        }
        s.voice.speak_heartbeat
    };
    let (news, viewing, origin) = {
        let state = session_state.peek();
        let session = state.sessions.get(session_id)?;
        let news = turn_news(&session.messages)?;
        (news, state.active_session_id == session_id && app_focused(), session.turn_origin)
    };
    if !should_announce(&news, is_live(), brain::waits_in_flight(), viewing) {
        return None;
    }
    let kind = announce_kind(origin, &news, speak_heartbeat)?;
    let text = match (news.needs_approval, kind) {
        (true, AnnounceKind::Heartbeat) => {
            "The Heartbeat needs your approval in the Hobbes tab before it can continue.".to_string()
        }
        (true, AnnounceKind::Result) => {
            "The brain needs your approval in the Hobbes tab before it can continue.".to_string()
        }
        (false, _) => match &news.details_tab {
            Some(tab) => point_at_tab(&news.reply, tab),
            None => news.reply,
        },
    };
    Some((text, kind))
}

/// A reply about a chat worker's report, pointing at the worker's tab for
/// the details rather than the Hobbes tab.
pub fn point_at_tab(reply: &str, tab: &str) -> String {
    let note = format!("(Full details are in the \"{tab}\" tab.)");
    let hobbes = "(Full details are in the Hobbes tab.)";
    if reply.contains(hobbes) {
        reply.replace(hobbes, &note)
    } else {
        format!("{} {note}", reply.trim_end())
    }
}

/// Called when any turn ends (stream_manager): speak Hobbes-tab news —
/// returned results and what a Heartbeat wake found — through the one
/// announce path.
pub fn on_turn_end(
    session_state: Signal<crate::session::SessionState>,
    settings: Signal<crate::settings::Settings>,
    session_id: &str,
) {
    super::brain::wake();
    let Some((text, kind)) = pending_announcement(session_state, settings, session_id) else { return };
    tracing::info!("voice: announcing Hobbes-tab news ({kind:?}, {} chars)", text.chars().count());
    send(VoiceCmd::Announce(text, kind));
}

/// The instruction wrapped around an announcement.
pub fn announce_prompt(text: &str, mid_conversation: bool, kind: AnnounceKind) -> String {
    announce_prompt_merged(text, mid_conversation, kind, 1)
}

/// As [`announce_prompt`], for `merged` queued notes folded into the newest.
pub fn announce_prompt_merged(text: &str, mid_conversation: bool, kind: AnnounceKind, merged: usize) -> String {
    let what = match kind {
        AnnounceKind::Result => "a result just came back from work handed off earlier",
        AnnounceKind::Heartbeat => "the Heartbeat, checking on their day in the background, found something for them",
    };
    let how = if mid_conversation {
        format!(
            "News: {what}. Politely get their attention — start with \"Excuse me\" — and tell them in \
             one or two short spoken sentences, then carry on with them."
        )
    } else {
        format!(
            "You are interrupting the person to deliver news: {what}. Start with \"Excuse me\", tell \
             them in one or two short spoken sentences, then stop. Only ask something if it needs \
             their decision."
        )
    };
    let earlier = if merged > 1 {
        format!(
            " This is the latest of {merged} updates while you were talking; it supersedes the \
             earlier ones, so only say this one."
        )
    } else {
        String::new()
    };
    format!(
        "[{how}{earlier} If there's more, say where: the tab the news names for its details, \
         else the Hobbes tab.]\n\n{}",
        text.trim()
    )
}

/// Shown once per app run when a conversation starts without echo
/// cancellation on speakers.
pub const ECHO_HINT: &str = "Hobbes may not hear you while it talks on speakers. Headphones, or Echo cancellation \"On\" in Settings → Voice, fixes it.";
/// How long the hint stays up.
const ECHO_HINT_FOR: Duration = Duration::from_secs(20);
static ECHO_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

/// Mic / speaker problems and provider failures end up here.
fn fail(msg: impl Into<String>) {
    let msg = msg.into();
    tracing::warn!("voice: {msg}");
    VOICE_UI.write().error = Some(msg);
}

/// What a session needs from settings, resolved up front.
struct Resolved {
    provider: Box<dyn RealtimeVoice>,
    setup: RealtimeSetup,
    threshold_ms: u64,
    idle: Duration,
    input_device: Option<String>,
    echo: VoiceEcho,
}

fn resolve(ctx: BuiltinToolCtx) -> Result<Resolved, String> {
    let settings = ctx.settings.peek();
    if !crate::voice::voice_active(&settings) {
        return Err("Voice is off. Turn it on in Settings → Voice.".into());
    }
    let (provider, setup) = realtime_for(&settings)?;
    Ok(Resolved {
        provider,
        setup,
        threshold_ms: settings.voice.hold_threshold_ms.max(100),
        idle: Duration::from_secs(settings.voice.idle_timeout_secs.max(15)),
        input_device: settings.voice.input_device_uid.clone(),
        echo: settings.voice.echo,
    })
}

/// The realtime provider and session setup for `settings`' voice profile
/// (also used by the Settings connection test).
pub fn realtime_for(settings: &crate::settings::Settings) -> Result<(Box<dyn RealtimeVoice>, RealtimeSetup), String> {
    let Some(conn) = settings.voice_connector() else {
        return Err("Pick a voice profile in Settings → Voice.".into());
    };
    if !crate::settings::realtime_capable(conn) {
        return Err(crate::settings::VOICE_NEEDS_REALTIME.into());
    }
    let key = match &conn.config {
        crate::settings::ProviderInstanceConfig::Gemini(g) => g.api_key.clone().filter(|k| !k.trim().is_empty()),
        _ => None,
    };
    let Some(key) = key else {
        return Err(format!("The voice profile \"{}\" has no API key.", conn.name));
    };
    let vp = conn.voice.clone().unwrap_or_default();
    let model = if vp.model.trim().is_empty() {
        crate::settings::default_realtime_model(conn.provider()).unwrap_or_default().to_string()
    } else {
        vp.model.trim().to_string()
    };
    let setup = RealtimeSetup {
        model,
        system_instruction: String::new(), // filled once the session is known
        voice_name: vp.voice_name.filter(|v| !v.trim().is_empty()),
        language: vp.language.filter(|v| !v.trim().is_empty()),
        tools: tools::voice_tool_decls(settings.planner_enabled),
        manual_activity: false,
        end_of_turn_ms: Some(settings.voice.end_of_turn()),
    };
    Ok((Box::new(crate::voice::gemini_live::GeminiLive::new(key)), setup))
}

/// The voice tier's instructions, with the recent conversation in the
/// Hobbes tab (typed, spoken, and the Heartbeat's wakes).
pub fn system_instruction(now: chrono::DateTime<chrono::Local>, tail: &str) -> String {
    let mut s = format!(
        "You are Hobbes, the person's chief-of-staff assistant, talking with them by voice. \
         Talk the way a person does, with no lists and no markdown. By default keep replies \
         short: one to three spoken sentences. When they ask for details, ask you to walk them \
         through something, or the answer genuinely needs it (a plan, a comparison, what an \
         email or result actually says), take the room to explain properly, in plain spoken \
         paragraphs, and lead with the point. Room to explain never means answering more \
         yourself: who answers is decided below, and length only shapes how you say it. If \
         they interrupt, stop and listen.\n\n\
         You are the voice layer. You can check their coding-agent Fleet, their planner and \
         calendar, timers, and the Heartbeat's notes yourself with your tools. For anything \
         else — doing or changing things, email, files, code, dispatching work, anything \
         you'd need to look up or explain in depth, anything from before the conversation \
         below, or questions that need careful thought — call {brain} with a complete request (the brain cannot \
         hear the conversation), tell them briefly that you're on it, and keep talking with \
         them. Call {brain} once per request and wait: its result always arrives on that same \
         call, however long it takes. Never call it again for something you've already \
         asked, not even reworded; only call it again when the person asks for something new. As you hand something to the brain, say a short holding phrase out loud — \
         \"let me think about that\", \"one moment, I'll check\" — so they're never left in \
         silence. When the result comes back, summarise it naturally, and give the detail if they \
         asked for it. Never read out code, URLs \
         or long ids; say it's in the Hobbes tab. When they say goodbye, call {end}.\n\n\
         You only see the end of the Hobbes tab below. The brain has the whole history, its \
         summary and their other tabs, so if they ask what you remember or about anything \
         earlier, ask the brain rather than saying you can't see it.\n\n\
         It is {time}.",
        brain = tools::ASK_BRAIN,
        end = tools::END_CONVERSATION,
        time = now.format("%A %-d %B %Y, %H:%M"),
    );
    if !tail.trim().is_empty() {
        s.push_str(
            "\n\nThe end of the Hobbes tab, where your voice conversations, typed messages and the \
             Heartbeat's background checks share one timeline:\n",
        );
        s.push_str(tail.trim());
    }
    s
}

/// One live conversation, from the press that opened it to its end.
async fn run_session(ctx: BuiltinToolCtx, rx: &mut mpsc::UnboundedReceiver<VoiceCmd>, start: Start) {
    let pressed_at = Instant::now();
    let announcing_text = match &start {
        Start::Announce(t, kind) => Some((t.clone(), *kind)),
        Start::Press => None,
    };
    let mut ui = VoiceUi { phase: Phase::Connecting, held: announcing_text.is_none(), ..VoiceUi::default() };
    *VOICE_UI.write() = ui.clone();

    let Resolved { provider, mut setup, threshold_ms, idle, input_device, echo } = match resolve(ctx) {
        Ok(r) => r,
        Err(e) => {
            fail(e);
            return;
        }
    };

    // Microphone permission (prompts the first time).
    match audio::mic_permission() {
        MicPermission::Granted => {}
        MicPermission::Undetermined => {
            if audio::request_mic_permission().await != MicPermission::Granted {
                fail("Hobbes needs microphone access for voice (System Settings → Privacy & Security → Microphone).");
                return;
            }
        }
        MicPermission::Denied => {
            fail("Microphone access is off for Hobbes (System Settings → Privacy & Security → Microphone).");
            return;
        }
    }

    let voice_session = {
        let settings = ctx.settings.peek().clone();
        crate::hobbes_session::ensure_session(ctx.session_state, &settings)
    };
    let Some(voice_session) = voice_session else {
        fail("The Hobbes tab's session couldn't be created.");
        return;
    };

    // Audio first, so speech during the connect is captured.
    let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<i16>>();
    // Opening and starting the engine blocks for a second or two (VPIO
    // setup): off the UI thread.
    let mic_first = announcing_text.is_none();
    let started = tokio::task::spawn_blocking(move || -> Result<audio::Opened, String> {
        let mut o = audio::open(input_device.as_deref(), echo).map_err(|e| format!("Couldn't open the microphone: {e}"))?;
        // An announcement starts with the mic off (a press opens it).
        o.io.set_capture(mic_first);
        o.io.start(mic_tx).map_err(|e| format!("Couldn't start audio on {}: {e}", o.input_name))?;
        Ok(o)
    })
    .await;
    let mut audio_io: Box<dyn AudioIo> = match started {
        Ok(Ok(o)) => {
            ui.mic = o.input_name;
            o.io
        }
        Ok(Err(e)) => {
            fail(e);
            return;
        }
        Err(e) => {
            fail(format!("Audio setup crashed: {e}"));
            return;
        }
    };
    ui.echo_safe = audio_io.echo_safe();

    // One session: the Heartbeat's notes and earlier conversations are its
    // own messages, so their cross-company reads already count there.
    setup.system_instruction =
        system_instruction(chrono::Local::now(), &session::session_tail(ctx.session_state, &voice_session, 10));
    // Commands during the connect: a stop (or a tap that ends the
    // conversation) is honoured at once; the rest wait for the loop.
    let mut early_cmds: std::collections::VecDeque<VoiceCmd> = std::collections::VecDeque::new();
    let connected = {
        let connect = provider.connect(setup);
        tokio::pin!(connect);
        let mut early_mode: Option<Mode> = announcing_text.as_ref().map(|_| Mode::PushToTalk);
        loop {
            tokio::select! {
                r = &mut connect => break Some(r),
                cmd = rx.recv() => match cmd {
                    None | Some(VoiceCmd::Stop) => break None,
                    Some(cmd) => {
                        if let VoiceCmd::Release { held_ms } = &cmd {
                            match release_step(early_mode, *held_ms < threshold_ms) {
                                ReleaseStep::End => break None,
                                ReleaseStep::Mode(m, _) => early_mode = Some(m),
                            }
                        }
                        early_cmds.push_back(cmd);
                    }
                },
            }
        }
    };
    let mut rt = match connected {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            audio_io.stop();
            fail(format!("Couldn't reach the voice model: {e}"));
            return;
        }
        None => {
            audio_io.stop();
            tracing::info!("voice: stopped while connecting");
            return;
        }
    };
    tracing::info!("voice: conversation started (mic '{}', echo-safe: {})", ui.mic, ui.echo_safe);
    let live_at = Instant::now();
    // The mic device is open from the press; push-to-talk releases it
    // between holds (see the end of the loop).
    let mut capture_on = announcing_text.is_none();
    let mut ever_captured = capture_on;
    // Speaking-only until the person presses the key.
    let mut announcing = announcing_text.is_some();
    let mut stats = Stats::default();
    let mut level_window = 0.0f32;
    // Who has the floor, and the speech waiting for it (see `floor.rs`).
    let mut floor_state = Floor::default();
    let mut queue = SpeechQueue::default();
    let mut noise = NoiseGate::default();
    // Echo-unsafe only: the muted mic while Hobbes talks, and the detector
    // that notices the person talking over it.
    let mut barge = BargeIn::default();
    let mut ring = MicRing::default();
    // After a local barge-in (or a key press over the voice) the rest of
    // the interrupted turn is neither played nor transcribed, until the
    // model ends it (`Interrupted` / `TurnComplete`); otherwise its next
    // audio would mute the mic again. Only a turn still open is muted.
    let mut model_turn = ModelTurn::default();
    // Playback with a short hangover (the speaker's tail), and the window
    // after a cut in which mic frames are dropped as that tail.
    let mut playback = PlaybackTail::default();
    let mut mic_drop_until: Option<Instant> = None;
    // When a queued item last went out (an announcement session's cap).
    let mut last_delivery = Instant::now();
    // Model turns, for telling a new ask from the model repeating one: bumps
    // at each turn end, and at the first model output after a tool call.
    let mut model_turn_seq: u64 = 0;
    let mut after_tool_call = false;
    // Voice-tier tool results (dispatched off the loop).
    let (tool_tx, mut tool_rx) = mpsc::unbounded_channel::<(String, String, Value)>();
    if let Some((text, kind)) = &announcing_text {
        // Speaking-only start: nobody is talking (the mic is off).
        let _ = rt.control.send(RealtimeControl::InjectContext { text: announce_prompt(text, false, *kind), respond: true });
        floor_state.model_active(Instant::now());
    }

    // Until the key comes up we don't know tap vs hold; treat as held. An
    // announcement behaves like push-to-talk between holds.
    let mut mode: Option<Mode> = announcing_text.as_ref().map(|_| Mode::PushToTalk);
    let mut held = announcing_text.is_none();
    let mut last_activity = Instant::now();
    // Each side's utterance in progress. A line is placed in the session by
    // when it was said, not when it is written (see transcript.rs).
    let mut heard = Utterance::new(true);
    let mut spoke = Utterance::new(false);
    let mut pending_lines: Vec<PendingLine> = Vec::new();
    let mut brain_cancel: HashMap<String, BrainHandle> = HashMap::new();
    let mut abandoned: HashSet<String> = HashSet::new();
    // Brain requests still running (not cancelled), for spotting repeats;
    // and when the person last said something (input transcript).
    let mut in_flight_asks: HashMap<String, InFlightAsk> = HashMap::new();
    let mut last_speech: Option<Instant> = None;
    let (brain_tx, mut brain_rx) = mpsc::unbounded_channel::<(String, BrainOutcome)>();
    let mut ending_at: Option<Instant> = None;
    // Goodbye audio seen since the end was asked for.
    let mut goodbye_audio = false;
    // When the voice last told them it was thinking (rate-limits the phrase).
    let mut last_holding: Option<Instant> = None;
    // When the voice last produced speech (any turn): Gemini often says its
    // holding phrase in one turn and calls the brain in the next.
    let mut last_spoke: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    // Once per app run: on speakers without echo cancellation Hobbes only
    // hears them over its own voice when they speak up.
    let mut notice_until: Option<Instant> = None;
    if !ui.echo_safe && announcing_text.is_none() && !ECHO_HINT_SHOWN.swap(true, Ordering::Relaxed) {
        ui.notice = Some(ECHO_HINT.to_string());
        notice_until = Some(Instant::now() + ECHO_HINT_FOR);
    }
    ui.phase = Phase::Listening;
    ui.error = None;
    *VOICE_UI.write() = ui.clone();

    let close_reason = loop {
        tokio::select! {
            frame = mic_rx.recv() => {
                let Some(frame) = frame else { break "audio stopped".to_string() };
                let p = probe::peak(&frame);
                stats.mic_frames += 1;
                stats.max_peak = stats.max_peak.max(p);
                level_window = level_window.max(p);
                let mic_open = held || mode == Some(Mode::Conversation);
                let now = Instant::now();
                // Without echo cancellation or headphones the model would
                // hear itself (playback, and its tail just after).
                let audible = playback.observe(audio_io.is_playing(), now);
                match floor::mic_gate(mic_open, held, ui.echo_safe, audible, mic_drop_until, now) {
                    MicGate::Closed => {}
                    MicGate::Drop => stats.echo_frames_dropped += 1,
                    MicGate::Hold => {
                        // Keep the muted frame; send it after all if the
                        // person is talking over Hobbes.
                        let ms = floor::frame_ms(frame.len());
                        ring.push(frame);
                        if let Some(rise_ms) = barge.observe(p, ms) {
                            audio_io.flush();
                            playback.reset();
                            model_turn.cut();
                            stats.barge_ins += 1;
                            spoke.finish_into(true, &mut pending_lines);
                            // From where their voice rose above the echo, with
                            // a short pre-roll; the rest was Hobbes' own echo.
                            let (buffered, echo) = ring.drain_recent(rise_ms + floor::BARGE_PREROLL_MS);
                            stats.echo_frames_dropped += echo as u64;
                            tracing::info!(
                                "voice: local barge-in; sending {} buffered mic frames ({echo} echo dropped)",
                                buffered.len()
                            );
                            for f in buffered {
                                stats.sent_frames += 1;
                                let _ = rt.control.send(RealtimeControl::Audio(f));
                            }
                            floor_state.person_spoke(now);
                            ui.phase = Phase::Listening;
                            last_activity = now;
                        }
                    }
                    MicGate::Send => {
                        // Playback over without a barge-in: what was kept was
                        // Hobbes' own echo.
                        if !ring.is_empty() {
                            stats.echo_frames_dropped += ring.clear() as u64;
                            barge.playback_ended();
                        }
                        // The level counts toward the floor only when it can't
                        // be Hobbes' own voice.
                        if noise.is_speech(p) && (ui.echo_safe || !audible) {
                            floor_state.person_spoke(now);
                        }
                        stats.sent_frames += 1;
                        let _ = rt.control.send(RealtimeControl::Audio(frame));
                    }
                }
            }
            ev = rt.events.recv() => {
                let Some(ev) = ev else { break "connection ended".to_string() };
                match ev {
                    RealtimeEvent::Ready => {}
                    RealtimeEvent::Reconnected => {
                        // The new connection may not hold the open calls:
                        // every result still owed goes back as context.
                        abandoned.extend(brain_cancel.keys().cloned());
                        let moved = queue.abandon_brain_results(|_| true);
                        // A turn cut off by the reconnect won't complete.
                        model_turn.ended();
                        floor_state.model_turn_ended(Instant::now());
                        tracing::info!(
                            "voice: reconnected; {} brain call(s) in flight and {moved} queued result(s) go as context",
                            brain_cancel.len()
                        );
                    }
                    RealtimeEvent::Audio { pcm, sample_rate } => {
                        floor_state.model_active(Instant::now());
                        model_turn.audio();
                        if std::mem::take(&mut after_tool_call) {
                            model_turn_seq += 1;
                        }
                        let audio_ms = (pcm.len() as u64 * 1000) / sample_rate.max(1) as u64;
                        if ending_at.is_some() && !model_turn.muting() {
                            goodbye_audio = true;
                        }
                        if model_turn.muting() {
                            // The rest of a turn the person talked over.
                            stats.muted_model_ms += audio_ms;
                        } else {
                            last_spoke = Some(Instant::now());
                            stats.model_audio_ms += audio_ms;
                            heard.finish_into(false, &mut pending_lines);
                            audio_io.play(&pcm, sample_rate);
                            last_activity = Instant::now();
                            ui.phase = Phase::Speaking;
                            ui.heard.clear();
                        }
                    }
                    RealtimeEvent::InputTranscript(t) => {
                        floor_state.person_transcript(Instant::now());
                        stats.heard_fragments += 1;
                        // The person saying something (not noise): what lets
                        // a new brain request through — a new utterance, or
                        // more of the same one after the ask.
                        if t.chars().any(char::is_alphanumeric) {
                            last_speech = Some(Instant::now());
                        }
                        heard.push(&t);
                        last_activity = Instant::now();
                        ui.heard = live_tail(heard.text());
                        ui.phase = Phase::Listening;
                    }
                    RealtimeEvent::OutputTranscript(t) => {
                        floor_state.model_active(Instant::now());
                        if std::mem::take(&mut after_tool_call) {
                            model_turn_seq += 1;
                        }
                        // Not while muting a talked-over turn: those words
                        // were never played (its line already ends in "—").
                        if !model_turn.muting() {
                            last_spoke = Some(Instant::now());
                            stats.spoke_fragments += 1;
                            spoke.push(&t);
                        }
                    }
                    RealtimeEvent::Interrupted => {
                        floor_state.model_turn_ended(Instant::now());
                        model_turn.ended();
                        model_turn_seq += 1;
                        after_tool_call = false;
                        audio_io.flush();
                        spoke.finish_into(true, &mut pending_lines);
                        ui.phase = Phase::Listening;
                        last_activity = Instant::now();
                    }
                    RealtimeEvent::TurnComplete => {
                        floor_state.model_turn_ended(Instant::now());
                        model_turn.ended();
                        model_turn_seq += 1;
                        after_tool_call = false;
                        stats.turns += 1;
                        finish_in_order(&mut heard, &mut spoke, &mut pending_lines);
                        ui.heard.clear();
                        last_activity = Instant::now();
                    }
                    RealtimeEvent::ToolCall { id, name, args } => {
                        stats.tool_calls += 1;
                        after_tool_call = true;
                        last_activity = Instant::now();
                        match name.as_str() {
                            tools::ASK_BRAIN => {
                                let request = args.get("request").and_then(Value::as_str).unwrap_or_default().to_string();
                                let norm = normalize_request(&request);
                                if request.trim().is_empty() {
                                    respond(&rt.control, &id, &name, json!({"error": "request is empty"}), None);
                                } else if is_duplicate_ask(in_flight_asks.values(), &norm, last_speech, model_turn_seq) {
                                    // The realtime model repeating itself while it
                                    // waits: absorb it silently, queue nothing.
                                    stats.duplicate_asks += 1;
                                    tracing::info!("voice: repeated brain request ignored ({} in flight)", in_flight_asks.len());
                                    respond(&rt.control, &id, &name, brain_duplicate_response(), Some(Scheduling::Silent));
                                } else {
                                    in_flight_asks.insert(id.clone(), InFlightAsk { asked_at: Instant::now(), norm, turn: model_turn_seq });
                                    // Transcript so far goes in before the brain's turn
                                    // (unless an earlier request's turn is in flight).
                                    if brain_cancel.is_empty() {
                                        flush_lines(ctx, &voice_session, &mut pending_lines, false);
                                    }
                                    // Say "one moment" — once: not if it already
                                    // spoke this turn (its own holding phrase), nor
                                    // again within the cooldown. The result follows
                                    // on this call either way.
                                    let spoke_recently = !spoke.is_blank()
                                        || last_spoke.is_some_and(|t| t.elapsed() < RECENT_SPEECH);
                                    let speak = holding_should_speak(spoke_recently, last_holding, Instant::now());
                                    last_holding = Some(Instant::now());
                                    let _ = rt.control.send(RealtimeControl::ToolResponse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        response: if speak { brain_started_response() } else { brain_started_quiet_response() },
                                        scheduling: Some(if speak { Scheduling::WhenIdle } else { Scheduling::Silent }),
                                        will_continue: true,
                                    });
                                    let handle = BrainHandle::default();
                                    brain_cancel.insert(id.clone(), handle.clone());
                                    ui.brain_busy = brain_cancel.len();
                                    let tx = brain_tx.clone();
                                    let sid = voice_session.clone();
                                    let st = ctx.session_state;
                                    spawn(async move {
                                        let outcome = brain::run(st, sid, request, handle, crate::session_streaming).await;
                                        let _ = tx.send((id, outcome));
                                    });
                                }
                            }
                            tools::CANCEL_BRAIN => {
                                for h in brain_cancel.values() {
                                    h.cancel.store(true, Ordering::SeqCst);
                                }
                                // A cancelled request no longer blocks a new one.
                                in_flight_asks.clear();
                                respond(&rt.control, &id, &name, json!({"result": "cancelled"}), None);
                            }
                            tools::END_CONVERSATION => {
                                respond(&rt.control, &id, &name, json!({"result": "ending"}), None);
                                if ending_at.is_none() {
                                    // A goodbye said just before the call counts.
                                    goodbye_audio = audio_io.is_playing()
                                        || last_spoke.is_some_and(|t| t.elapsed() < GOODBYE_RECENT);
                                }
                                ending_at = Some(Instant::now());
                            }
                            _ => {
                                // Off the loop: barge-in, results and Stop keep
                                // being handled while the tool runs.
                                let tx = tool_tx.clone();
                                let sid = voice_session.clone();
                                spawn(async move {
                                    let result = tools::dispatch(ctx, &sid, &name, &args).await;
                                    let _ = tx.send((id, name, result));
                                });
                            }
                        }
                    }
                    RealtimeEvent::ToolCallCancelled(ids) => {
                        // Brain work keeps going; its result is injected
                        // instead (also one already waiting in the queue).
                        queue.abandon_brain_results(|id| ids.iter().any(|c| c == id));
                        abandoned.extend(ids);
                    }
                    RealtimeEvent::Warning(w) => tracing::warn!("voice: provider warning: {}", crate::log_utils::truncate_for_log(&w, 200)),
                    RealtimeEvent::Closed { reason } => break reason,
                }
            }
            done = brain_rx.recv() => {
                if let Some((id, outcome)) = done {
                    brain_cancel.remove(&id);
                    in_flight_asks.remove(&id);
                    ui.brain_busy = brain_cancel.len();
                    last_activity = Instant::now();
                    let text = match outcome {
                        BrainOutcome::Done(t) => t,
                        BrainOutcome::NeedsApproval(t) | BrainOutcome::Failed(t) => t,
                        BrainOutcome::Cancelled => "Cancelled.".to_string(),
                    };
                    // Either way it makes the voice speak: wait for the floor.
                    let item = if abandoned.remove(&id) {
                        Pending::Abandoned { text }
                    } else {
                        Pending::BrainResult { id, text }
                    };
                    queue.push(item, Instant::now());
                }
            }
            Some((id, name, result)) = tool_rx.recv() => {
                last_activity = Instant::now();
                respond(&rt.control, &id, &name, result, None);
            }
            cmd = next_cmd(&mut early_cmds, &mut *rx) => {
                let Some(cmd) = cmd else { break "app closing".to_string() };
                match cmd {
                    // In a conversation a press is decided on release.
                    VoiceCmd::Press => {
                        let now = Instant::now();
                        // On speakers without echo cancellation, a held key
                        // would send Hobbes' own voice with theirs: cut it
                        // off, and drop the speaker's tail.
                        if !ui.echo_safe && playback.observe(audio_io.is_playing(), now) {
                            audio_io.flush();
                            playback.reset();
                            model_turn.cut();
                            spoke.finish_into(true, &mut pending_lines);
                            stats.echo_frames_dropped += ring.clear() as u64;
                            barge.playback_ended();
                            mic_drop_until = Some(now + floor::SPEAKER_TAIL);
                            ui.phase = Phase::Listening;
                        }
                        held = true;
                        announcing = false;
                    }
                    VoiceCmd::Release { held_ms } => {
                        // Speech captured while the key was down (including
                        // during the connect) goes out before the release,
                        // through the same echo gate as any mic frame.
                        while let Ok(frame) = mic_rx.try_recv() {
                            let now = Instant::now();
                            let audible = playback.observe(audio_io.is_playing(), now);
                            match floor::mic_gate(true, false, ui.echo_safe, audible, mic_drop_until, now) {
                                MicGate::Closed => {}
                                MicGate::Drop => stats.echo_frames_dropped += 1,
                                MicGate::Hold => ring.push(frame),
                                MicGate::Send => {
                                    stats.sent_frames += 1;
                                    let _ = rt.control.send(RealtimeControl::Audio(frame));
                                }
                            }
                        }
                        held = false;
                        match release_step(mode, held_ms < threshold_ms) {
                            ReleaseStep::End => break "ended".to_string(),
                            ReleaseStep::Mode(m, stream_end) => {
                                mode = Some(m);
                                if stream_end {
                                    let _ = rt.control.send(RealtimeControl::AudioStreamEnd);
                                }
                            }
                        }
                        last_activity = Instant::now();
                    }
                    VoiceCmd::Stop => break "stopped".to_string(),
                    VoiceCmd::Announce(text, kind) => {
                        last_activity = Instant::now();
                        queue.push(Pending::Announce { text, kind }, Instant::now());
                    }
                }
            }
            _ = tick.tick() => {
                ui.level = level_window;
                level_window = 0.0;
                if capture_on {
                    ui.mic_warning = mic_warning(live_at.elapsed(), stats.mic_frames, stats.max_peak, &ui.mic);
                }
                if notice_until.is_some_and(|t| Instant::now() >= t) {
                    notice_until = None;
                    ui.notice = None;
                }
                if ui.phase == Phase::Speaking && !audio_io.is_playing() {
                    ui.phase = Phase::Listening;
                }
                // An announcement nobody answered: hang up once it's said.
                if announcing {
                    let said = stats.turns >= 1
                        && queue.is_empty()
                        && !audio_io.is_playing()
                        && last_activity.elapsed() >= Duration::from_secs(5);
                    if said || last_delivery.elapsed() > ANNOUNCE_CAP {
                        break "announced".to_string();
                    }
                }
                if let Some(at) = ending_at {
                    // Let the goodbye arrive and play out (it may come after
                    // the tool response).
                    if goodbye_done(at.elapsed(), goodbye_audio, audio_io.is_playing()) {
                        break "goodbye".to_string();
                    }
                }
                let busy = !brain_cancel.is_empty() || !queue.is_empty() || audio_io.is_playing() || held;
                if !busy && last_activity.elapsed() > idle {
                    break "idle".to_string();
                }
                // The press that started a push-to-talk still down past the
                // threshold: show it as push-to-talk right away.
                if mode.is_none() && pressed_at.elapsed() >= Duration::from_millis(threshold_ms) && held {
                    ui.mode = Some(Mode::PushToTalk);
                }
                // Transcript lines wait while the brain's turn is running.
                if brain_cancel.is_empty() {
                    flush_lines(ctx, &voice_session, &mut pending_lines, false);
                }
            }
        }
        // Speech that waited for the floor: one item, once it's free.
        let now = Instant::now();
        let free = floor_state.is_free(held, audio_io.is_playing(), now);
        if let Some(d) = queue.next(free, held, now) {
            stats.queued_out += 1;
            last_activity = now;
            last_delivery = now;
            // A call the model gave up on after its result was queued: the
            // result goes as context, not to the dead call.
            let d = match d {
                Delivery::BrainResult { id, text, .. } if abandoned.remove(&id) => Delivery::Abandoned { text },
                d => d,
            };
            match d {
                Delivery::Announce { text, kind, merged } => {
                    tracing::info!("voice: queued news out ({kind:?}, {merged} merged, {} still queued)", queue.len());
                    let text = announce_prompt_merged(&text, !announcing, kind, merged);
                    let _ = rt.control.send(RealtimeControl::InjectContext { text, respond: true });
                    floor_state.model_active(now);
                }
                Delivery::Abandoned { text } => {
                    let _ = rt.control.send(RealtimeControl::InjectContext {
                        text: format!("[Result of your earlier request to the brain] {text}"),
                        respond: true,
                    });
                    floor_state.model_active(now);
                }
                Delivery::BrainResult { id, text, forced } => {
                    if forced {
                        tracing::info!(
                            "voice: brain result held {}s for the floor ({}); sending anyway",
                            floor::BRAIN_RESULT_CAP.as_secs(),
                            floor_state.blocker(held, audio_io.is_playing(), now).unwrap_or("free")
                        );
                    } else {
                        floor_state.model_active(now);
                    }
                    respond(&rt.control, &id, tools::ASK_BRAIN, json!({"result": text}), Some(Scheduling::WhenIdle));
                }
            }
        }
        // The mic is open only while the key is held, or for the whole of a
        // hands-free conversation (`mode` is undecided until the first
        // release, and the key is still down then).
        let want_capture = mic_wanted(held, mode);
        if want_capture != capture_on {
            audio_io.set_capture(want_capture);
            capture_on = want_capture;
            ever_captured |= want_capture;
            if !want_capture {
                level_window = 0.0;
                ui.level = 0.0;
            }
        }
        ui.mode = mode.or(ui.mode);
        ui.held = held;
        ui.mic_on = capture_on;
        if *VOICE_UI.peek() != ui {
            *VOICE_UI.write() = ui.clone();
        }
    };

    // Wind down: stop the brain waits (their turns keep running in the tab).
    for h in brain_cancel.values() {
        h.detach.store(true, Ordering::SeqCst);
    }
    let _ = rt.control.send(RealtimeControl::Close);
    audio_io.flush();
    audio_io.stop();
    finish_in_order(&mut heard, &mut spoke, &mut pending_lines);
    flush_lines(ctx, &voice_session, &mut pending_lines, true);
    if !queue.is_empty() {
        // Said afterwards, as announcements (the announce settings apply).
        // Not after an announcement that never found the floor (it would
        // only start the same one again), nor when the app is closing, nor
        // after Stop (they asked for quiet; the text is in the Hobbes tab).
        let unspoken = queue.drain_as_announcements();
        if matches!(close_reason.as_str(), "announced" | "app closing" | "stopped") {
            tracing::info!("voice: {} queued item(s) not spoken before the end", unspoken.len());
        } else {
            tracing::info!("voice: {} queued item(s) not spoken; announcing them afterwards", unspoken.len());
            for (text, kind) in unspoken {
                send(VoiceCmd::Announce(text, kind));
            }
        }
    }
    tracing::info!("voice: conversation ended ({close_reason}); {}", stats.summary());
    if let Some(w) = mic_warning(live_at.elapsed(), stats.mic_frames, stats.max_peak, &ui.mic).filter(|_| ever_captured) {
        fail(w);
        return;
    }
    if !matches!(close_reason.as_str(), "ended" | "stopped" | "goodbye" | "idle" | "app closing" | "announced") {
        fail(format!("The voice conversation ended: {close_reason}"));
    }
}

/// Per-conversation counts for the end-of-conversation log line (no content).
#[derive(Default)]
struct Stats {
    mic_frames: u64,
    sent_frames: u64,
    max_peak: f32,
    model_audio_ms: u64,
    heard_fragments: u64,
    spoke_fragments: u64,
    turns: u64,
    tool_calls: u64,
    /// Repeated brain requests absorbed without queuing a turn.
    duplicate_asks: u64,
    /// Local barge-ins (echo-unsafe only).
    barge_ins: u64,
    /// Muted mic frames discarded as Hobbes' own echo.
    echo_frames_dropped: u64,
    /// Model audio not played: the rest of talked-over turns.
    muted_model_ms: u64,
    /// Items that waited for the floor and went out.
    queued_out: u64,
}

impl Stats {
    fn summary(&self) -> String {
        format!(
            "mic frames {} (sent {}, echo dropped {}), peak {:.3}, model audio {:.1}s (cut {:.1}s), heard/spoke fragments {}/{}, turns {}, tool calls {} (repeated asks {}), barge-ins {}, queued out {}",
            self.mic_frames,
            self.sent_frames,
            self.echo_frames_dropped,
            self.max_peak,
            self.model_audio_ms as f32 / 1000.0,
            self.muted_model_ms as f32 / 1000.0,
            self.heard_fragments,
            self.spoke_fragments,
            self.turns,
            self.tool_calls,
            self.duplicate_asks,
            self.barge_ins,
            self.queued_out,
        )
    }
}

/// Whether the mic device should be open: while the key is down, and
/// throughout a hands-free conversation.
pub fn mic_wanted(held: bool, mode: Option<Mode>) -> bool {
    held || mode == Some(Mode::Conversation)
}

/// After a grace period: no frames at all, or only digital silence, from the mic.
pub fn mic_warning(live_for: Duration, frames: u64, max_peak: f32, mic: &str) -> Option<String> {
    if live_for < Duration::from_secs(3) {
        return None;
    }
    match probe::mic_verdict(frames as usize, max_peak) {
        probe::MicVerdict::Ok(_) => None,
        v => Some(probe::verdict_text(v, mic)),
    }
}

fn respond(control: &mpsc::UnboundedSender<RealtimeControl>, id: &str, name: &str, response: Value, scheduling: Option<Scheduling>) {
    let _ = control.send(RealtimeControl::ToolResponse {
        id: id.to_string(),
        name: name.to_string(),
        response,
        scheduling,
        will_continue: false,
    });
}

/// The interim answer to `VOICE_ASK_BRAIN`, sent the moment the brain
/// starts: it prompts the voice to say it's thinking (instead of going
/// quiet), and keeps the call open for the real result (`willContinue`).
/// Don't announce "thinking" twice within this long.
pub const HOLDING_COOLDOWN: Duration = Duration::from_secs(30);

/// Speech this recent counts as the voice having just told them (its own
/// holding phrase, possibly in the turn before the brain call).
pub const RECENT_SPEECH: Duration = Duration::from_secs(8);

/// Whether the interim response should prompt a holding phrase: not if the
/// voice spoke recently (it said its own — this turn or the one before), nor
/// within the cooldown.
pub fn holding_should_speak(spoke_recently: bool, last_holding: Option<Instant>, now: Instant) -> bool {
    !spoke_recently && last_holding.is_none_or(|t| now.duration_since(t) >= HOLDING_COOLDOWN)
}

/// The interim response when a holding phrase was already said: absorbed
/// silently (`SILENT`), keeping the call open for the result.
pub fn brain_started_quiet_response() -> Value {
    json!({
        "status": "working",
        "note": "The brain is working on this and they already know. Say nothing more about it \
                 now; the result arrives later on this same call.",
    })
}

pub fn brain_started_response() -> Value {
    json!({
        "status": "working",
        "note": "The brain is working on this; it can take a minute. Say a few words to let them \
                 know you're thinking about it and will be right back (for example \"Let me \
                 think about that, one moment.\"). Don't guess the answer. The result arrives \
                 later on this same call.",
    })
}

/// The answer to a repeated `VOICE_ASK_BRAIN` (see [`is_duplicate_ask`]):
/// sent silently, and nothing is queued.
pub fn brain_duplicate_response() -> Value {
    json!({
        "status": "already_working",
        "note": "Already working on that request; its result will arrive on the earlier call when \
                 it's done. Don't ask again, and say nothing more about it now.",
    })
}

/// Input transcription can trail the audio: fragments of the speech that
/// prompted an ask may arrive just after the call. Speech that starts within
/// this long after an ask doesn't count as "spoken since".
pub const TRANSCRIPT_LAG: Duration = Duration::from_millis(2000);

/// A `VOICE_ASK_BRAIN` whose brain turn is still running.
#[derive(Debug, Clone)]
pub struct InFlightAsk {
    pub asked_at: Instant,
    /// [`normalize_request`] of its text.
    pub norm: String,
    /// The model turn it was asked in (several asks in one turn are the
    /// person asking for several things, not the model repeating itself).
    pub turn: u64,
}

/// Case, punctuation and whitespace folded, for comparing request texts.
pub fn normalize_request(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a new ask repeats one already in flight: the same text (whatever
/// was said since), or — from a later model turn than that ask — any text
/// when the person hasn't said anything since it (`last_speech`: their
/// latest transcribed speech, a new utterance or more of the same one). The
/// realtime model re-issues the call while it waits; only the person can
/// start a genuinely new request. Different asks in one model turn (one
/// sentence, two requests) all go through.
pub fn is_duplicate_ask<'a>(
    in_flight: impl IntoIterator<Item = &'a InFlightAsk>,
    norm: &str,
    last_speech: Option<Instant>,
    turn: u64,
) -> bool {
    in_flight.into_iter().any(|a| {
        let spoken_since = last_speech.is_some_and(|s| s > a.asked_at + TRANSCRIPT_LAG);
        a.norm == norm || (a.turn != turn && !spoken_since)
    })
}

/// What a key release does to the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseStep {
    /// The mode from here on; `true` = end the audio stream (a hold ended).
    Mode(Mode, bool),
    /// A tap in a hands-free conversation: end it.
    End,
}

/// The release of a key held in `mode` (`None`: the press that opened the
/// conversation); `tap` = shorter than the hold threshold.
pub fn release_step(mode: Option<Mode>, tap: bool) -> ReleaseStep {
    match (mode, tap) {
        // First release: decides the mode.
        (None, true) => ReleaseStep::Mode(Mode::Conversation, false),
        (None, false) => ReleaseStep::Mode(Mode::PushToTalk, true),
        (Some(Mode::Conversation), true) => ReleaseStep::End,
        (Some(Mode::Conversation), false) => ReleaseStep::Mode(Mode::Conversation, false),
        // Tap during push-to-talk: become a conversation.
        (Some(Mode::PushToTalk), true) => ReleaseStep::Mode(Mode::Conversation, false),
        (Some(Mode::PushToTalk), false) => ReleaseStep::Mode(Mode::PushToTalk, true),
    }
}

/// The next command: those that arrived during the connect first.
async fn next_cmd(early: &mut std::collections::VecDeque<VoiceCmd>, rx: &mut mpsc::UnboundedReceiver<VoiceCmd>) -> Option<VoiceCmd> {
    match early.pop_front() {
        Some(cmd) => Some(cmd),
        None => rx.recv().await,
    }
}

/// Finish both sides' utterances into `lines`, in the order they began.
fn finish_in_order(heard: &mut Utterance, spoke: &mut Utterance, lines: &mut Vec<PendingLine>) {
    let mut done = Vec::new();
    heard.finish_into(false, &mut done);
    spoke.finish_into(false, &mut done);
    lines.extend(by_start(done));
}

/// Lines sorted by when each began (stable for equal starts).
fn by_start(mut lines: Vec<PendingLine>) -> Vec<PendingLine> {
    lines.sort_by_key(|l| l.at);
    lines
}

/// An announcement session (nobody pressed the key) hangs up this long after
/// its last queued item went out.
pub const ANNOUNCE_CAP: Duration = Duration::from_secs(60);

/// Voice speech this recent when the conversation is ended counts as the
/// goodbye (said before the call).
pub const GOODBYE_RECENT: Duration = Duration::from_secs(2);

/// Whether the goodbye has been said: some of it arrived and finished
/// playing after a short grace, or the hard cap passed.
pub fn goodbye_done(since_end: Duration, goodbye_audio: bool, playing: bool) -> bool {
    let grace = since_end >= Duration::from_millis(1500) && goodbye_audio && !playing;
    grace || since_end > Duration::from_secs(8)
}

/// Write buffered transcript lines into the Hobbes session, unless its last
/// turn is still in progress (streaming, between rounds, or held on an
/// approval: `transcript::turn_open`) — `force` writes regardless (session
/// end), and an open turn then only gets lines before it. Each line goes in
/// at the time it was said, between turns: before the brain's request it
/// preceded, never inside a turn (see `transcript::insert_between_turns`).
fn flush_lines(ctx: BuiltinToolCtx, session_id: &str, lines: &mut Vec<PendingLine>, force: bool) {
    if lines.is_empty() || (!force && crate::voice::transcript::session_turn_open(ctx.session_state, session_id)) {
        return;
    }
    crate::voice::transcript::write_lines(ctx.session_state, session_id, std::mem::take(lines));
}

/// The last ~80 characters of the live transcript, for the indicator.
fn live_tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= 80 {
        t.to_string()
    } else {
        format!("…{}", t.chars().skip(n - 80).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_names_the_tools_and_context() {
        let now = chrono::Local::now();
        let s = system_instruction(now, "User: 🎙 hi\nHobbes: CI is red");
        assert!(s.contains(tools::ASK_BRAIN) && s.contains(tools::END_CONVERSATION));
        assert!(s.contains("CI is red") && s.contains("🎙 hi"));
        let bare = system_instruction(now, "");
        assert!(!bare.contains("The end of the Hobbes tab"));
    }

    #[test]
    fn instruction_allows_detail_when_asked() {
        let s = system_instruction(chrono::Local::now(), "");
        assert!(s.contains("By default keep replies short"));
        assert!(s.contains("When they ask for details"));
        assert!(!s.contains("Keep replies short and conversational: one to three"));
        // Length never moves work off the brain.
        assert!(s.contains("Room to explain never means answering more"));
        assert!(s.contains("explain in depth"));
        assert!(s.contains("ask the brain rather than saying you can't see it"));
    }

    fn msg(author: &str, text: &str) -> crate::components::chat::Message {
        let mut m = session::transcript_message(author == "User", "x");
        m.author = author.into();
        m.content = crate::components::shared::MessageContent::Text {
            content: text.into(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
        };
        m
    }

    #[test]
    fn dispatched_work_coming_back_is_announced() {
        let msgs = vec![
            msg("User", "The background agent you dispatched for \"rollup\" has finished."),
            msg("Hobbes", "The rollup is done: 14 items filed."),
        ];
        let news = turn_news(&msgs).unwrap();
        assert!(!news.voice_request);
        assert_eq!(news.reply, "The rollup is done: 14 items filed.");
        // Live conversation: say it now. Not live and not looking: announce.
        assert!(should_announce(&news, true, 0, false));
        assert!(should_announce(&news, false, 0, false));
        // Not live, reading the Hobbes tab as it lands: no need to say it.
        assert!(!should_announce(&news, false, 0, true));
        // It speaks as returned work, not as the Heartbeat.
        use crate::session::TurnOrigin;
        assert_eq!(announce_kind(TurnOrigin::Heartbeat, &news, false), Some(AnnounceKind::Result));
    }


    #[test]
    fn voice_requests_are_left_to_the_live_wait() {
        let msgs = vec![msg("User", &brain::framed_request("check CI")), msg("Hobbes", "CI is green.")];
        let news = turn_news(&msgs).unwrap();
        assert!(news.voice_request);
        // The live conversation's wait delivers it.
        assert!(!should_announce(&news, true, 1, false));
        // The conversation ended first: announce it, even in the Hobbes tab.
        assert!(should_announce(&news, false, 0, true));
        // Nothing to say: nothing announced.
        let empty = TurnNews { voice_request: false, reply: String::new(), needs_approval: false, heartbeat_wake: false, details_tab: None };
        assert!(!should_announce(&empty, true, 0, false));
    }

    #[test]
    fn a_chat_workers_report_points_at_its_tab() {
        use crate::todo::relay::{RelayDirection, RelayKind, RelayNote};
        let mut report = msg("Relay", "");
        report.content = crate::components::shared::MessageContent::Relay(RelayNote {
            direction: RelayDirection::FromWorker,
            kind: RelayKind::Report,
            peer_session_id: "w".into(),
            peer_title: "NMS VR: adjust elevation".into(),
            todo_id: None,
            text: "Finished.".into(),
            hop: 0,
            woke_turn: true,
        });
        let msgs = vec![report, msg("Hobbes", "The foundation parts are raised 0.2m.")];
        let news = turn_news(&msgs).unwrap();
        assert_eq!(news.details_tab.as_deref(), Some("NMS VR: adjust elevation"));
        assert_eq!(
            point_at_tab(&news.reply, "NMS VR: adjust elevation"),
            "The foundation parts are raised 0.2m. (Full details are in the \"NMS VR: adjust elevation\" tab.)"
        );
        assert_eq!(
            point_at_tab("Two PRs. (Full details are in the Hobbes tab.)", "CI"),
            "Two PRs. (Full details are in the \"CI\" tab.)"
        );
        // Returned work with no worker tab keeps the Hobbes tab.
        let plain = turn_news(&[msg("User", "The background agent you dispatched has finished."), msg("Hobbes", "Done.")]).unwrap();
        assert_eq!(plain.details_tab, None);
    }

    #[test]
    fn announcements_start_with_excuse_me() {
        assert!(announce_prompt("CI is green.", true, AnnounceKind::Result).contains("Excuse me"));
        assert!(announce_prompt("CI is green.", false, AnnounceKind::Result).ends_with("CI is green."));
        let hb = announce_prompt("CI is red on main.", false, AnnounceKind::Heartbeat);
        assert!(hb.contains("Excuse me") && hb.contains("Heartbeat") && hb.ends_with("CI is red on main."));
    }

    #[test]
    fn a_heartbeat_wake_speaks_only_when_it_found_something() {
        use crate::session::TurnOrigin;
        let msgs = vec![
            msg("User", "[Heartbeat wake]\n\nWhy: CI failed"),
            msg("Hobbes", "CI is red on main; I asked to dispatch a fix."),
        ];
        let news = turn_news(&msgs).unwrap();
        assert!(!news.voice_request);
        // Same rule as a returned result: live, or not looking at the tab.
        assert!(news.heartbeat_wake);
        assert!(should_announce(&news, false, 0, false));
        assert_eq!(announce_kind(TurnOrigin::Heartbeat, &news, true), Some(AnnounceKind::Heartbeat));
        // Off by the Heartbeat-specific switch.
        assert_eq!(announce_kind(TurnOrigin::Heartbeat, &news, false), None);
        // Nothing for the person: stay quiet.
        let quiet = TurnNews { voice_request: false, reply: "Nothing to report.".into(), needs_approval: false, heartbeat_wake: true, details_tab: None };
        assert_eq!(announce_kind(TurnOrigin::Heartbeat, &quiet, true), None);
        // Held on an approval: say so.
        let held = TurnNews { voice_request: false, reply: "Nothing to report.".into(), needs_approval: true, heartbeat_wake: true, details_tab: None };
        assert_eq!(announce_kind(TurnOrigin::Heartbeat, &held, true), Some(AnnounceKind::Heartbeat));
        // A turn the person started is a result, whatever the switch.
        assert_eq!(announce_kind(TurnOrigin::User, &news, false), Some(AnnounceKind::Result));
    }

    #[test]
    fn the_thinking_phrase_is_said_once() {
        let t0 = Instant::now();
        // First hand-off, nothing said yet: say it.
        assert!(holding_should_speak(false, None, t0));
        // It already said its own holding phrase — this turn, or (as Gemini
        // does) in the turn just before the brain call: stay quiet.
        assert!(!holding_should_speak(true, None, t0));
        // A second request ten seconds later: quiet.
        assert!(!holding_should_speak(false, Some(t0), t0 + Duration::from_secs(10)));
        // After the cooldown: say it again.
        assert!(holding_should_speak(false, Some(t0), t0 + HOLDING_COOLDOWN));
        assert_eq!(brain_started_quiet_response()["status"], "working");
    }

    #[test]
    fn repeated_brain_requests_are_absorbed() {
        let t0 = Instant::now();
        let first = InFlightAsk { asked_at: t0, norm: normalize_request("Check my email for the Puget invoice."), turn: 1 };
        let inflight = [first];
        let after_lag = Some(t0 + TRANSCRIPT_LAG + Duration::from_millis(500));
        // Nothing in flight: never a duplicate.
        assert!(!is_duplicate_ask(&[], "anything", None, 2));
        assert!(!is_duplicate_ask(&[], "anything", after_lag, 2));
        // No new speech since: a duplicate, whatever its wording.
        assert!(is_duplicate_ask(&inflight, &normalize_request("Look for the Puget invoice email"), None, 2));
        // Speech that started before the ask (or trailing transcript just
        // after it) isn't new speech.
        assert!(is_duplicate_ask(&inflight, "something else", Some(t0), 2));
        assert!(is_duplicate_ask(&inflight, "something else", Some(t0 + Duration::from_millis(800)), 2));
        // The same text after new speech: still a duplicate.
        assert!(is_duplicate_ask(&inflight, &normalize_request("  check MY email for the puget invoice "), after_lag, 2));
        // A different request after new speech: goes through.
        assert!(!is_duplicate_ask(&inflight, &normalize_request("What's on my calendar tomorrow?"), after_lag, 2));
        assert_eq!(brain_duplicate_response()["status"], "already_working");
    }

    #[test]
    fn two_requests_in_one_turn_both_go_through() {
        let t0 = Instant::now();
        let inflight = [InFlightAsk { asked_at: t0, norm: normalize_request("Check my email"), turn: 3 }];
        // Same model turn (one toolCall batch), different text: a new request.
        assert!(!is_duplicate_ask(&inflight, &normalize_request("Book a table for two"), None, 3));
        // Same text in the same turn: still a repeat.
        assert!(is_duplicate_ask(&inflight, &normalize_request("check my email."), None, 3));
        // A later turn with nothing said since: the model repeating itself.
        assert!(is_duplicate_ask(&inflight, &normalize_request("Book a table for two"), None, 4));
    }

    #[test]
    fn talking_on_after_an_ask_lets_a_new_one_through() {
        let t0 = Instant::now();
        let inflight = [InFlightAsk { asked_at: t0, norm: normalize_request("Check my email"), turn: 1 }];
        // More of the same utterance, well after the ask: new speech.
        let later_fragment = Some(t0 + TRANSCRIPT_LAG + Duration::from_millis(300));
        assert!(!is_duplicate_ask(&inflight, &normalize_request("and move my 3pm"), later_fragment, 2));
        assert!(is_duplicate_ask(&inflight, &normalize_request("check my email"), later_fragment, 2));
    }

    #[test]
    fn releases_decide_the_mode() {
        assert_eq!(release_step(None, true), ReleaseStep::Mode(Mode::Conversation, false));
        assert_eq!(release_step(None, false), ReleaseStep::Mode(Mode::PushToTalk, true));
        assert_eq!(release_step(Some(Mode::Conversation), true), ReleaseStep::End);
        assert_eq!(release_step(Some(Mode::Conversation), false), ReleaseStep::Mode(Mode::Conversation, false));
        assert_eq!(release_step(Some(Mode::PushToTalk), true), ReleaseStep::Mode(Mode::Conversation, false));
        assert_eq!(release_step(Some(Mode::PushToTalk), false), ReleaseStep::Mode(Mode::PushToTalk, true));
    }

    #[test]
    fn the_goodbye_waits_for_its_audio() {
        let s = Duration::from_secs;
        // Nothing arrived yet: keep waiting past the grace.
        assert!(!goodbye_done(s(3), false, false));
        // Arrived and played out.
        assert!(goodbye_done(s(2), true, false));
        // Still playing, or too early.
        assert!(!goodbye_done(s(2), true, true));
        assert!(!goodbye_done(Duration::from_millis(500), true, false));
        // The hard cap.
        assert!(goodbye_done(s(9), false, true));
    }

    #[test]
    fn transcript_lines_go_in_by_start_time() {
        let t0 = chrono::Utc::now();
        let lines = vec![
            PendingLine::new(true, "heard".into(), Some(t0 + chrono::Duration::seconds(2))),
            PendingLine::new(false, "spoke".into(), Some(t0)),
        ];
        let sorted = by_start(lines);
        assert_eq!(sorted.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["spoke", "heard"]);
        // Both sides finished together keep that order.
        let mut heard = Utterance::new(true);
        let mut spoke = Utterance::new(false);
        spoke.push("Hello.");
        std::thread::sleep(Duration::from_millis(5));
        heard.push("Hi.");
        let mut out = Vec::new();
        finish_in_order(&mut heard, &mut spoke, &mut out);
        assert_eq!(out.iter().map(|l| l.heard).collect::<Vec<_>>(), [false, true]);
    }

    #[test]
    fn requests_normalise_case_space_and_punctuation() {
        assert_eq!(normalize_request("  Check   my\nEmail, please! "), "check my email please");
        assert_eq!(normalize_request("Check my email please"), normalize_request("check my email, please."));
        let s = system_instruction(chrono::Local::now(), "");
        assert!(s.contains("once per request"));
    }

    #[test]
    fn the_brain_handoff_is_announced() {
        let s = system_instruction(chrono::Local::now(), "");
        assert!(s.contains("holding phrase"));
        let r = brain_started_response();
        assert_eq!(r["status"], "working");
        assert!(r["note"].as_str().unwrap().contains("one moment"));
    }

    #[test]
    fn push_to_talk_opens_the_mic_only_while_held() {
        // The opening press: open until we know tap vs hold.
        assert!(mic_wanted(true, None));
        // Push-to-talk: open while held, released between holds.
        assert!(mic_wanted(true, Some(Mode::PushToTalk)));
        assert!(!mic_wanted(false, Some(Mode::PushToTalk)));
        // Hands-free: open throughout.
        assert!(mic_wanted(false, Some(Mode::Conversation)));
    }

    #[test]
    fn mic_warning_waits_then_names_the_mic() {
        assert_eq!(mic_warning(Duration::from_secs(1), 0, 0.0, "BRIO"), None);
        assert!(mic_warning(Duration::from_secs(4), 0, 0.0, "BRIO").unwrap().contains("No audio is arriving from BRIO"));
        assert!(mic_warning(Duration::from_secs(4), 200, 0.0, "BRIO").unwrap().contains("sending silence"));
        assert_eq!(mic_warning(Duration::from_secs(4), 200, 0.1, "BRIO"), None);
    }

    #[test]
    fn live_tail_keeps_the_end() {
        let long = "a".repeat(100) + "END";
        let t = live_tail(&long);
        assert!(t.starts_with('…') && t.ends_with("END"));
        assert_eq!(live_tail(" short "), "short");
    }
}
