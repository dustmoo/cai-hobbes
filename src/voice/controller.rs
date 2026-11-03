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
//! mic is muted while the voice speaks.
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
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::components::builtin_tools::BuiltinToolCtx;
use crate::voice::audio::{self, AudioIo, MicPermission, VoiceEcho};
use crate::voice::probe;
use crate::voice::brain::{self, BrainHandle, BrainOutcome};
use crate::voice::realtime::{
    RealtimeControl, RealtimeEvent, RealtimeSetup, RealtimeVoice, Scheduling,
};
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
    /// A Heartbeat alert to speak while a conversation is live.
    HeartbeatAlert(String),
    /// A result that reached the Voice tab on its own (dispatched work
    /// finishing, a brain answer after the conversation ended): said as
    /// "Excuse me…" in a live conversation, or in a short speaking-only
    /// session (mic off) otherwise.
    Announce(String),
}

/// How a session starts.
#[derive(Clone, Debug, PartialEq)]
enum Start {
    /// The hotkey went down: mic open, tap or hold decided on release.
    Press,
    /// Hobbes speaks first, with the mic off; a press opens it.
    Announce(String),
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

/// Speak a Heartbeat alert if a conversation is live and the setting allows.
pub fn heartbeat_alert(text: String) {
    if is_live() {
        send(VoiceCmd::HeartbeatAlert(text));
    }
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
            VoiceCmd::Announce(text) => {
                let s = ctx.settings.peek();
                if !crate::voice::voice_active(&s) || !s.voice.announce_results {
                    continue;
                }
                Start::Announce(text)
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

/// What to say for a finished Voice-tab turn: the reply after its trigger
/// (the last message not written by Hobbes), whether that trigger was a
/// voice request, and whether the turn is held on an approval.
#[derive(Debug, PartialEq)]
pub struct TurnNews {
    pub voice_request: bool,
    pub reply: String,
    pub needs_approval: bool,
}

pub fn turn_news(messages: &[crate::components::chat::Message]) -> Option<TurnNews> {
    let trigger = messages.iter().rposition(|m| m.author != "Hobbes")?;
    let voice_request = messages[trigger]
        .content
        .get_text_content()
        .is_some_and(|t| brain::is_voice_request(t.trim()));
    Some(TurnNews {
        voice_request,
        reply: brain::collect_reply(messages, trigger),
        needs_approval: crate::heartbeat::wake::has_pending_approval(&messages[trigger + 1..]),
    })
}

/// Whether a finished Voice-tab turn should be spoken. A voice request the
/// live conversation is still waiting on is delivered by that wait; with no
/// conversation, a turn in the tab the person is looking at is theirs (they
/// typed it), unless it answers an earlier voice request.
pub fn should_announce(news: &TurnNews, live: bool, waits_in_flight: usize, viewing: bool) -> bool {
    if news.reply.is_empty() && !news.needs_approval {
        return false;
    }
    if live && news.voice_request && waits_in_flight > 0 {
        return false;
    }
    !(!live && viewing && !news.voice_request)
}

/// Called when any turn ends (stream_manager): speak Voice-tab news.
pub fn on_turn_end(
    session_state: Signal<crate::session::SessionState>,
    settings: Signal<crate::settings::Settings>,
    session_id: &str,
) {
    if session::VOICE_SESSION_ID.peek().as_deref() != Some(session_id) {
        return;
    }
    {
        let s = settings.peek();
        if !crate::voice::voice_active(&s) || !s.voice.announce_results {
            return;
        }
    }
    let (news, viewing) = {
        let state = session_state.peek();
        let Some(news) = state.sessions.get(session_id).and_then(|s| turn_news(&s.messages)) else {
            return;
        };
        (news, state.active_session_id == session_id)
    };
    if !should_announce(&news, is_live(), brain::waits_in_flight(), viewing) {
        return;
    }
    let text = if news.needs_approval {
        "The brain needs your approval in the Voice tab before it can continue.".to_string()
    } else {
        news.reply
    };
    tracing::info!("voice: announcing a Voice-tab result ({} chars)", text.chars().count());
    send(VoiceCmd::Announce(text));
}

/// The instruction wrapped around an announced result.
pub fn announce_prompt(text: &str, mid_conversation: bool) -> String {
    let how = if mid_conversation {
        "A result just came back from work handed off earlier. Politely get their attention — \
         start with \"Excuse me\" — and tell them in one or two short spoken sentences, then \
         carry on with them."
    } else {
        "You are interrupting the person to deliver news: a result just came back from work \
         handed off earlier. Start with \"Excuse me\", tell them in one or two short spoken \
         sentences, then stop. Only ask something if the result needs their decision."
    };
    format!("[{how} If there's more, say it's in the Voice tab.]\n\n{}", text.trim())
}

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
    speak_heartbeat: bool,
    profile: Option<String>,
    /// Hobbes' home profile (system sessions are kept on it).
    home: Option<String>,
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
        speak_heartbeat: settings.voice.speak_heartbeat,
        profile: settings
            .home_composio_profile
            .clone()
            .or_else(|| settings.get_active_profile().map(|p| p.id.clone())),
        home: settings.home_composio_profile.clone(),
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
    };
    Ok((Box::new(crate::voice::gemini_live::GeminiLive::new(key)), setup))
}

/// The voice tier's instructions, with recent context from both sessions.
pub fn system_instruction(now: chrono::DateTime<chrono::Local>, voice_tail: &str, heartbeat_tail: &str) -> String {
    let mut s = format!(
        "You are Hobbes, the person's chief-of-staff assistant, talking with them by voice. \
         Keep replies short and conversational: one to three spoken sentences, no lists, no \
         markdown. If they interrupt, stop and listen.\n\n\
         You are the voice layer. You can check their coding-agent Fleet, their planner and \
         calendar, timers, and the Heartbeat's notes yourself with your tools. For anything \
         else — doing or changing things, email, files, code, dispatching work, or questions \
         that need careful thought — call {brain} with a complete request (the brain cannot \
         hear the conversation), tell them briefly that you're on it, and keep talking with \
         them. As you hand something to the brain, say a short holding phrase out loud — \
         \"let me think about that\", \"one moment, I'll check\" — so they're never left in \
         silence. When the result comes back, summarise it naturally. Never read out code, URLs \
         or long ids; say it's in the Voice tab. When they say goodbye, call {end}.\n\n\
         It is {time}.",
        brain = tools::ASK_BRAIN,
        end = tools::END_CONVERSATION,
        time = now.format("%A %-d %B %Y, %H:%M"),
    );
    if !heartbeat_tail.trim().is_empty() {
        s.push_str("\n\nRecent Heartbeat notes (background monitoring):\n");
        s.push_str(heartbeat_tail.trim());
    }
    if !voice_tail.trim().is_empty() {
        s.push_str("\n\nThe end of your previous voice conversation:\n");
        s.push_str(voice_tail.trim());
    }
    s
}

fn tail_of(session_state: Signal<crate::session::SessionState>, peer: session::Peer, limit: usize) -> String {
    session::peer_tail(session_state, peer, limit).unwrap_or_default()
}

/// One live conversation, from the press that opened it to its end.
async fn run_session(ctx: BuiltinToolCtx, rx: &mut mpsc::UnboundedReceiver<VoiceCmd>, start: Start) {
    let pressed_at = Instant::now();
    let announcing_text = match &start {
        Start::Announce(t) => Some(t.clone()),
        Start::Press => None,
    };
    let mut ui = VoiceUi { phase: Phase::Connecting, held: announcing_text.is_none(), ..VoiceUi::default() };
    *VOICE_UI.write() = ui.clone();

    let Resolved { provider, mut setup, threshold_ms, idle, speak_heartbeat, profile, home, input_device, echo } = match resolve(ctx) {
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

    let Some(voice_session) = session::ensure_session(ctx.session_state, profile, home.as_deref()) else {
        fail("The Voice session couldn't be created.");
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

    setup.system_instruction = system_instruction(
        chrono::Local::now(),
        &tail_of(ctx.session_state, session::Peer::Voice, 6),
        &tail_of(ctx.session_state, session::Peer::Heartbeat, 4),
    );
    let mut rt = match provider.connect(setup).await {
        Ok(s) => s,
        Err(e) => {
            audio_io.stop();
            fail(format!("Couldn't reach the voice model: {e}"));
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
    if let Some(text) = &announcing_text {
        let _ = rt.control.send(RealtimeControl::InjectContext { text: announce_prompt(text, false), respond: true });
    }
    let mut stats = Stats::default();
    let mut level_window = 0.0f32;

    // Until the key comes up we don't know tap vs hold; treat as held. An
    // announcement behaves like push-to-talk between holds.
    let mut mode: Option<Mode> = announcing_text.as_ref().map(|_| Mode::PushToTalk);
    let mut held = announcing_text.is_none();
    let mut last_activity = Instant::now();
    let mut heard_buf = String::new();
    let mut spoke_buf = String::new();
    let mut pending_lines: Vec<(bool, String)> = Vec::new();
    let mut brain_cancel: HashMap<String, BrainHandle> = HashMap::new();
    let mut abandoned: HashSet<String> = HashSet::new();
    let (brain_tx, mut brain_rx) = mpsc::unbounded_channel::<(String, BrainOutcome)>();
    let mut ending_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
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
                // Without echo cancellation or headphones the model would hear itself.
                let echo_risk = !ui.echo_safe && audio_io.is_playing() && !held;
                if mic_open && !echo_risk {
                    stats.sent_frames += 1;
                    let _ = rt.control.send(RealtimeControl::Audio(frame));
                }
            }
            ev = rt.events.recv() => {
                let Some(ev) = ev else { break "connection ended".to_string() };
                match ev {
                    RealtimeEvent::Ready | RealtimeEvent::Reconnected => {}
                    RealtimeEvent::Audio { pcm, sample_rate } => {
                        stats.model_audio_ms += (pcm.len() as u64 * 1000) / sample_rate.max(1) as u64;
                        if !heard_buf.trim().is_empty() {
                            pending_lines.push((true, std::mem::take(&mut heard_buf)));
                        }
                        audio_io.play(&pcm, sample_rate);
                        last_activity = Instant::now();
                        ui.phase = Phase::Speaking;
                        ui.heard.clear();
                    }
                    RealtimeEvent::InputTranscript(t) => {
                        stats.heard_fragments += 1;
                        heard_buf.push_str(&t);
                        last_activity = Instant::now();
                        ui.heard = live_tail(&heard_buf);
                        ui.phase = Phase::Listening;
                    }
                    RealtimeEvent::OutputTranscript(t) => {
                        stats.spoke_fragments += 1;
                        spoke_buf.push_str(&t);
                    }
                    RealtimeEvent::Interrupted => {
                        audio_io.flush();
                        if !spoke_buf.trim().is_empty() {
                            pending_lines.push((false, format!("{} —", std::mem::take(&mut spoke_buf).trim_end())));
                        }
                        ui.phase = Phase::Listening;
                        last_activity = Instant::now();
                    }
                    RealtimeEvent::TurnComplete => {
                        stats.turns += 1;
                        if !heard_buf.trim().is_empty() {
                            pending_lines.push((true, std::mem::take(&mut heard_buf)));
                        }
                        if !spoke_buf.trim().is_empty() {
                            pending_lines.push((false, std::mem::take(&mut spoke_buf)));
                        }
                        ui.heard.clear();
                        last_activity = Instant::now();
                    }
                    RealtimeEvent::ToolCall { id, name, args } => {
                        stats.tool_calls += 1;
                        last_activity = Instant::now();
                        match name.as_str() {
                            tools::ASK_BRAIN => {
                                let request = args.get("request").and_then(Value::as_str).unwrap_or_default().to_string();
                                if request.trim().is_empty() {
                                    respond(&rt.control, &id, &name, json!({"error": "request is empty"}), None);
                                } else {
                                    // Transcript so far goes in before the brain's turn
                                    // (unless an earlier request's turn is in flight).
                                    if brain_cancel.is_empty() {
                                        flush_lines(ctx, &voice_session, &mut pending_lines, false);
                                    }
                                    // Say "one moment" now; the result follows on this call.
                                    let _ = rt.control.send(RealtimeControl::ToolResponse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        response: brain_started_response(),
                                        scheduling: Some(Scheduling::WhenIdle),
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
                                respond(&rt.control, &id, &name, json!({"result": "cancelled"}), None);
                            }
                            tools::END_CONVERSATION => {
                                respond(&rt.control, &id, &name, json!({"result": "ending"}), None);
                                ending_at = Some(Instant::now());
                            }
                            _ => {
                                let result = tools::dispatch(ctx, &voice_session, &name, &args).await;
                                respond(&rt.control, &id, &name, result, None);
                            }
                        }
                    }
                    RealtimeEvent::ToolCallCancelled(ids) => {
                        // Brain work keeps going; its result is injected instead.
                        abandoned.extend(ids);
                    }
                    RealtimeEvent::Warning(w) => tracing::warn!("voice: provider warning: {}", crate::log_utils::truncate_for_log(&w, 200)),
                    RealtimeEvent::Closed { reason } => break reason,
                }
            }
            done = brain_rx.recv() => {
                if let Some((id, outcome)) = done {
                    brain_cancel.remove(&id);
                    ui.brain_busy = brain_cancel.len();
                    last_activity = Instant::now();
                    let text = match outcome {
                        BrainOutcome::Done(t) => t,
                        BrainOutcome::NeedsApproval(t) | BrainOutcome::Failed(t) => t,
                        BrainOutcome::Cancelled => "Cancelled.".to_string(),
                    };
                    if abandoned.remove(&id) {
                        let _ = rt.control.send(RealtimeControl::InjectContext {
                            text: format!("[Result of your earlier request to the brain] {text}"),
                            respond: true,
                        });
                    } else {
                        respond(&rt.control, &id, tools::ASK_BRAIN, json!({"result": text}), Some(Scheduling::WhenIdle));
                    }
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break "app closing".to_string() };
                match cmd {
                    // In a conversation a press is decided on release.
                    VoiceCmd::Press => {
                        held = true;
                        announcing = false;
                    }
                    VoiceCmd::Release { held_ms } => {
                        // Speech captured while the key was down (including
                        // during the connect) goes out before the release.
                        while let Ok(frame) = mic_rx.try_recv() {
                            let _ = rt.control.send(RealtimeControl::Audio(frame));
                        }
                        held = false;
                        let tap = held_ms < threshold_ms;
                        match (mode, tap) {
                            // First release: decides the mode.
                            (None, true) => mode = Some(Mode::Conversation),
                            (None, false) => {
                                mode = Some(Mode::PushToTalk);
                                let _ = rt.control.send(RealtimeControl::AudioStreamEnd);
                            }
                            (Some(Mode::Conversation), true) => break "ended".to_string(),
                            (Some(Mode::Conversation), false) => {}
                            (Some(Mode::PushToTalk), true) => {
                                // Tap during push-to-talk: become a conversation.
                                mode = Some(Mode::Conversation);
                            }
                            (Some(Mode::PushToTalk), false) => {
                                let _ = rt.control.send(RealtimeControl::AudioStreamEnd);
                            }
                        }
                        last_activity = Instant::now();
                    }
                    VoiceCmd::Stop => break "stopped".to_string(),
                    VoiceCmd::Announce(text) => {
                        last_activity = Instant::now();
                        let _ = rt.control.send(RealtimeControl::InjectContext { text: announce_prompt(&text, true), respond: true });
                    }
                    VoiceCmd::HeartbeatAlert(text) => {
                        if speak_heartbeat {
                            let _ = rt.control.send(RealtimeControl::InjectContext {
                                text: format!("[Heartbeat alert — tell the person briefly] {text}"),
                                respond: true,
                            });
                        }
                    }
                }
            }
            _ = tick.tick() => {
                ui.level = level_window;
                level_window = 0.0;
                if capture_on {
                    ui.mic_warning = mic_warning(live_at.elapsed(), stats.mic_frames, stats.max_peak, &ui.mic);
                }
                if ui.phase == Phase::Speaking && !audio_io.is_playing() {
                    ui.phase = Phase::Listening;
                }
                // An announcement nobody answered: hang up once it's said.
                if announcing {
                    let said = stats.turns >= 1 && !audio_io.is_playing() && last_activity.elapsed() >= Duration::from_secs(5);
                    if said || live_at.elapsed() > Duration::from_secs(60) {
                        break "announced".to_string();
                    }
                }
                if let Some(at) = ending_at {
                    // Let the goodbye arrive and play out (it may come after
                    // the tool response).
                    let grace = at.elapsed() >= Duration::from_millis(1500) && !audio_io.is_playing();
                    if grace || at.elapsed() > Duration::from_secs(8) {
                        break "goodbye".to_string();
                    }
                }
                let busy = !brain_cancel.is_empty() || audio_io.is_playing() || held;
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
    if !heard_buf.trim().is_empty() {
        pending_lines.push((true, heard_buf));
    }
    if !spoke_buf.trim().is_empty() {
        pending_lines.push((false, spoke_buf));
    }
    flush_lines(ctx, &voice_session, &mut pending_lines, true);
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
}

impl Stats {
    fn summary(&self) -> String {
        format!(
            "mic frames {} (sent {}), peak {:.3}, model audio {:.1}s, heard/spoke fragments {}/{}, turns {}, tool calls {}",
            self.mic_frames,
            self.sent_frames,
            self.max_peak,
            self.model_audio_ms as f32 / 1000.0,
            self.heard_fragments,
            self.spoke_fragments,
            self.turns,
            self.tool_calls,
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
pub fn brain_started_response() -> Value {
    json!({
        "status": "working",
        "note": "The brain is working on this; it can take a minute. Say a few words to let them \
                 know you're thinking about it and will be right back (for example \"Let me \
                 think about that, one moment.\"). Don't guess the answer. The result arrives \
                 later on this same call.",
    })
}

/// Write buffered transcript lines into the Voice session, unless its turn
/// is streaming (appending mid-turn would interleave with the brain's
/// messages) — `force` writes regardless (session end).
fn flush_lines(ctx: BuiltinToolCtx, session_id: &str, lines: &mut Vec<(bool, String)>, force: bool) {
    if lines.is_empty() || (!force && crate::session_streaming(session_id)) {
        return;
    }
    for (heard, text) in lines.drain(..) {
        session::append_transcript(ctx.session_state, session_id, heard, &text);
    }
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
        let s = system_instruction(now, "User: 🎙 hi", "Hobbes: CI is red");
        assert!(s.contains(tools::ASK_BRAIN) && s.contains(tools::END_CONVERSATION));
        assert!(s.contains("CI is red") && s.contains("🎙 hi"));
        let bare = system_instruction(now, "", "");
        assert!(!bare.contains("Heartbeat notes"));
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
        // Not live but looking at the Voice tab: they typed it; stay quiet.
        assert!(!should_announce(&news, false, 0, true));
    }

    #[test]
    fn voice_requests_are_left_to_the_live_wait() {
        let msgs = vec![msg("User", &brain::framed_request("check CI")), msg("Hobbes", "CI is green.")];
        let news = turn_news(&msgs).unwrap();
        assert!(news.voice_request);
        // The live conversation's wait delivers it.
        assert!(!should_announce(&news, true, 1, false));
        // The conversation ended first: announce it, even in the Voice tab.
        assert!(should_announce(&news, false, 0, true));
        // Nothing to say: nothing announced.
        let empty = TurnNews { voice_request: false, reply: String::new(), needs_approval: false };
        assert!(!should_announce(&empty, true, 0, false));
    }

    #[test]
    fn announcements_start_with_excuse_me() {
        assert!(announce_prompt("CI is green.", true).contains("Excuse me"));
        assert!(announce_prompt("CI is green.", false).ends_with("CI is green."));
    }

    #[test]
    fn the_brain_handoff_is_announced() {
        let s = system_instruction(chrono::Local::now(), "", "");
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
