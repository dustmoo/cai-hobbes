//! Voice diagnostics that check each pipe on its own:
//!
//! - [`mic_check`]: open the chosen mic for a few seconds and report what
//!   arrived (frames and level only; nothing is kept or sent anywhere).
//! - [`speech_round_trip`]: stream a synthesized spoken question into the
//!   realtime model and report what it heard and how it answered. No mic,
//!   no speaker — this isolates the provider protocol.
//!
//! Both back the Settings → Voice test buttons, and the round trip backs
//! the ignored live test in `gemini_live`.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::voice::audio::{self, VoiceEcho};
use crate::voice::realtime::{RealtimeControl, RealtimeEvent, RealtimeSetup, RealtimeVoice, INPUT_SAMPLE_RATE};

/// Peak absolute level of a 16-bit frame, 0.0–1.0.
pub fn peak(frame: &[i16]) -> f32 {
    frame.iter().map(|&s| (s as i32).unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0
}

/// Below this peak a mic is sending digital silence (muted, a Bluetooth
/// headset stuck switching profiles, or no permission), not a quiet room.
pub const SILENT_PEAK: f32 = 0.0005;

/// What a stretch of mic capture says about the mic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MicVerdict {
    /// Not a single frame arrived.
    NoAudio,
    /// Frames arrived but they are all (near) zero.
    Silent,
    /// Real audio, with this peak level.
    Ok(f32),
}

pub fn mic_verdict(frames: usize, max_peak: f32) -> MicVerdict {
    if frames == 0 {
        MicVerdict::NoAudio
    } else if max_peak < SILENT_PEAK {
        MicVerdict::Silent
    } else {
        MicVerdict::Ok(max_peak)
    }
}

/// The person-facing line for a verdict on `mic`.
pub fn verdict_text(v: MicVerdict, mic: &str) -> String {
    match v {
        MicVerdict::NoAudio => format!(
            "No audio is arriving from {mic}. Check that it's connected and that Hobbes has \
             microphone access (System Settings → Privacy & Security → Microphone)."
        ),
        MicVerdict::Silent => format!(
            "{mic} is sending silence. It may be muted, or in use by another app."
        ),
        MicVerdict::Ok(p) => format!("{mic} is working (peak level {:.0}%).", (p * 100.0).min(100.0)),
    }
}

#[derive(Clone, Debug)]
pub struct MicReport {
    pub input_name: String,
    pub voice_processing: bool,
    pub frames: usize,
    pub max_peak: f32,
}

impl MicReport {
    pub fn verdict(&self) -> MicVerdict {
        mic_verdict(self.frames, self.max_peak)
    }
}

/// Capture from the configured mic for `dur`, calling `on_level` (peak of
/// the last ~100 ms) as audio arrives.
pub async fn mic_check(
    input_uid: Option<String>,
    echo: VoiceEcho,
    dur: Duration,
    mut on_level: impl FnMut(f32),
) -> Result<MicReport, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();
    let opened = tokio::task::spawn_blocking(move || {
        let mut o = audio::open(input_uid.as_deref(), echo).map_err(|e| e.to_string())?;
        o.io.start(tx).map_err(|e| e.to_string())?;
        Ok::<_, String>(o)
    })
    .await
    .map_err(|e| format!("audio setup crashed: {e}"))??;
    let mut report = MicReport {
        input_name: opened.input_name.clone(),
        voice_processing: opened.voice_processing,
        frames: 0,
        max_peak: 0.0,
    };
    let end = Instant::now() + dur;
    let mut window = 0.0f32;
    let mut window_frames = 0;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(frame)) => {
                let p = peak(&frame);
                report.frames += 1;
                report.max_peak = report.max_peak.max(p);
                window = window.max(p);
                window_frames += 1;
                if window_frames >= 5 {
                    on_level(window);
                    window = 0.0;
                    window_frames = 0;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let mut io = opened.io;
    let _ = tokio::task::spawn_blocking(move || io.stop()).await;
    tracing::info!(
        "voice: mic check on '{}': {} frames, peak {:.3}",
        report.input_name,
        report.frames,
        report.max_peak
    );
    Ok(report)
}

/// A short spoken test question as 16 kHz mono PCM (macOS `say`).
pub fn synth_speech(text: &str) -> Result<Vec<i16>, String> {
    #[cfg(target_os = "macos")]
    {
        let path = std::env::temp_dir().join(format!("hobbes-voice-probe-{}.wav", uuid::Uuid::new_v4()));
        let out = std::process::Command::new("/usr/bin/say")
            .arg("-o")
            .arg(&path)
            .args(["--file-format=WAVE", "--data-format=LEI16@16000"])
            .arg(text)
            .output()
            .map_err(|e| format!("say: {e}"))?;
        let bytes = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        if !out.status.success() {
            return Err("say failed".into());
        }
        let bytes = bytes.map_err(|e| format!("read speech: {e}"))?;
        let (rate, pcm) = parse_wav_mono16(&bytes)?;
        Ok(audio::resample_linear(&pcm, rate, INPUT_SAMPLE_RATE))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err("speech synthesis for the test is macOS-only".into())
    }
}

/// `(sample_rate, samples)` from a 16-bit PCM WAV (first channel).
pub fn parse_wav_mono16(b: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut pos, mut fmt) = (12usize, None::<(u16, u16, u32, u16)>);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            fmt = Some((u16_at(0), u16_at(2), rate, u16_at(14)));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt.ok_or("WAV data before fmt")?;
            if format != 1 || bits != 16 || channels == 0 {
                return Err(format!("unsupported WAV (format {format}, {bits} bits)"));
            }
            let step = channels as usize * 2;
            let pcm = body.chunks_exact(step).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            return Ok((rate, pcm));
        }
        pos += 8 + len + (len & 1);
    }
    Err("WAV has no data".into())
}

#[derive(Clone, Debug, Default)]
pub struct RoundTrip {
    /// What the model transcribed from the speech.
    pub heard: String,
    /// What it said back (output transcript).
    pub replied: String,
    /// Seconds of reply audio received.
    pub reply_secs: f32,
    /// From the end of the speech to the first reply audio.
    pub first_audio_after: Option<Duration>,
}

/// Stream `speech` (16 kHz mono) into a fresh session at real-time pace,
/// then silence so the provider's turn detection closes the turn, and wait
/// for the spoken answer.
pub async fn speech_round_trip(
    provider: &dyn RealtimeVoice,
    setup: RealtimeSetup,
    speech: Vec<i16>,
) -> Result<RoundTrip, String> {
    const FRAME: usize = (INPUT_SAMPLE_RATE / 50) as usize; // 20 ms
    let mut s = provider.connect(setup).await?;
    let control = s.control.clone();
    let (spoken_tx, mut spoken_rx) = tokio::sync::oneshot::channel::<Instant>();
    let pacer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        for chunk in speech.chunks(FRAME) {
            tick.tick().await;
            if control.send(RealtimeControl::Audio(chunk.to_vec())).is_err() {
                return;
            }
        }
        let _ = spoken_tx.send(Instant::now());
        // Trailing silence: ends the turn for automatic activity detection.
        for _ in 0..100 {
            tick.tick().await;
            if control.send(RealtimeControl::Audio(vec![0; FRAME])).is_err() {
                return;
            }
        }
    });
    let mut out = RoundTrip::default();
    let mut spoke_at: Option<Instant> = None;
    let mut reply_samples = 0usize;
    let mut reply_rate = 24_000u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(if out.heard.is_empty() && reply_samples == 0 {
                "the model never responded to the speech (nothing heard, no reply)".to_string()
            } else {
                "the model's reply didn't finish within 30 s".to_string()
            });
        }
        tokio::select! {
            at = &mut spoken_rx, if spoke_at.is_none() => {
                spoke_at = Some(at.unwrap_or_else(|_| Instant::now()));
            }
            ev = tokio::time::timeout(left, s.events.recv()) => match ev {
                Err(_) => continue,
                Ok(None) => break Err("the connection ended".into()),
                Ok(Some(ev)) => match ev {
                    RealtimeEvent::InputTranscript(t) => out.heard.push_str(&t),
                    RealtimeEvent::OutputTranscript(t) => out.replied.push_str(&t),
                    RealtimeEvent::Audio { pcm, sample_rate } => {
                        if out.first_audio_after.is_none() {
                            out.first_audio_after = Some(spoke_at.map(|t| t.elapsed()).unwrap_or_default());
                        }
                        reply_samples += pcm.len();
                        reply_rate = sample_rate.max(1);
                    }
                    RealtimeEvent::TurnComplete if reply_samples > 0 => break Ok(()),
                    RealtimeEvent::ToolCall { id, name, .. } => {
                        let _ = s.control.send(RealtimeControl::ToolResponse { will_continue: false,
                            id, name, response: serde_json::json!({"result": "unavailable in the test"}), scheduling: None,
                        });
                    }
                    RealtimeEvent::Closed { reason } => break Err(format!("the connection closed: {reason}")),
                    _ => {}
                },
            },
        }
    };
    pacer.abort();
    let _ = s.control.send(RealtimeControl::Close);
    out.reply_secs = reply_samples as f32 / reply_rate as f32;
    out.heard = out.heard.trim().to_string();
    out.replied = out.replied.trim().to_string();
    tracing::info!(
        "voice: round trip: heard {} chars, reply {:.1}s audio / {} chars, first audio after {:?}",
        out.heard.chars().count(),
        out.reply_secs,
        out.replied.chars().count(),
        out.first_audio_after
    );
    result.map(|()| out)
}

/// The sentence a voice sample reads.
pub fn sample_line(voice: &str) -> String {
    format!("Hi, I'm {voice}. This is how I'll sound when I bring you news from your day.")
}

/// Voice samples already fetched this run, by voice name: replaying one
/// costs nothing.
type SampleCache = std::collections::HashMap<String, (Vec<i16>, u32)>;
static SAMPLES: std::sync::Mutex<Option<SampleCache>> = std::sync::Mutex::new(None);

/// A short clip of `voice` reading [`sample_line`]: from this run's cache,
/// else one realtime connection with that voice, asked to read the line.
/// Mono PCM and its sample rate. No mic, no speaker.
pub async fn voice_sample(
    provider: &dyn RealtimeVoice,
    mut setup: RealtimeSetup,
    voice: &str,
) -> Result<(Vec<i16>, u32), String> {
    if let Some(hit) = SAMPLES.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.get(voice).cloned())) {
        return Ok(hit);
    }
    setup.voice_name = Some(voice.to_string());
    setup.system_instruction =
        "You are a voice sample. When given a sentence, read it aloud exactly as written, in a natural, friendly tone. Say nothing else.".into();
    setup.tools.clear();
    let mut s = provider.connect(setup).await?;
    let _ = s.control.send(RealtimeControl::InjectContext {
        text: format!("Read this aloud exactly: \"{}\"", sample_line(voice)),
        respond: true,
    });
    let mut pcm: Vec<i16> = Vec::new();
    let mut rate = 24_000u32;
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err("the voice didn't finish the sample within 20 s".to_string());
        }
        match tokio::time::timeout(left, s.events.recv()).await {
            Err(_) => continue,
            Ok(None) => break Err("the connection ended".into()),
            Ok(Some(ev)) => match ev {
                RealtimeEvent::Audio { pcm: chunk, sample_rate } => {
                    pcm.extend_from_slice(&chunk);
                    rate = sample_rate.max(1);
                }
                RealtimeEvent::TurnComplete if !pcm.is_empty() => break Ok(()),
                RealtimeEvent::Closed { reason } => break Err(format!("the connection closed: {reason}")),
                _ => {}
            },
        }
    };
    let _ = s.control.send(RealtimeControl::Close);
    result?;
    if let Ok(mut m) = SAMPLES.lock() {
        m.get_or_insert_with(Default::default).insert(voice.to_string(), (pcm.clone(), rate));
    }
    Ok((pcm, rate))
}

#[derive(Clone, Debug, Default)]
pub struct Handoff {
    /// Speech before the slow tool was called.
    pub before_call: String,
    /// Speech after the interim "working" response (the holding phrase).
    pub holding: String,
    /// Speech after the final result.
    pub result: String,
}

/// The brain handoff as a conversation runs it: a question that needs a
/// slow (non-blocking) tool, an interim "working" response with
/// `willContinue`, then the result. Checks the voice fills the wait and
/// still speaks the result.
pub async fn brain_handoff_round_trip(provider: &dyn RealtimeVoice, mut setup: RealtimeSetup) -> Result<Handoff, String> {
    use crate::voice::realtime::{Scheduling, ToolDecl};
    const TOOL: &str = "look_up";
    setup.system_instruction = format!(
        "This is a sound check of a slow tool. For any question, call {TOOL}. As you call it, say a \
         short holding phrase out loud. When the result comes back, say it in one sentence."
    );
    setup.tools = vec![ToolDecl {
        name: TOOL.into(),
        description: "Look something up. Slow: takes a while.".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}),
        non_blocking: true,
    }];
    let mut s = provider.connect(setup).await?;
    s.control
        .send(RealtimeControl::InjectContext { text: "Did last night's build pass?".into(), respond: true })
        .map_err(|_| "the connection ended".to_string())?;
    #[derive(PartialEq)]
    enum Stage {
        BeforeCall,
        Holding,
        Result,
    }
    let mut stage = Stage::BeforeCall;
    let mut out = Handoff::default();
    let mut call: Option<(String, String)> = None;
    let mut result_audio = 0usize;
    let mut final_due: Option<tokio::time::Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(45);
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(match stage {
                Stage::BeforeCall => "the model never called the slow tool".to_string(),
                Stage::Holding => "no reply after the interim response".to_string(),
                Stage::Result => "the result was never spoken".to_string(),
            });
        }
        let due = final_due.unwrap_or_else(|| tokio::time::Instant::now() + left);
        tokio::select! {
            _ = tokio::time::sleep_until(due), if final_due.is_some() => {
                final_due = None;
                if let Some((id, name)) = call.clone() {
                    stage = Stage::Result;
                    let _ = s.control.send(RealtimeControl::ToolResponse {
                        id, name,
                        response: serde_json::json!({"result": "Yes, last night's build passed."}),
                        scheduling: Some(Scheduling::WhenIdle),
                        will_continue: false,
                    });
                }
            }
            ev = tokio::time::timeout(left, s.events.recv()) => match ev {
                Err(_) => continue,
                Ok(None) => break Err("the connection ended".into()),
                Ok(Some(ev)) => match ev {
                    RealtimeEvent::ToolCall { id, name, .. } if call.is_none() => {
                        call = Some((id.clone(), name.clone()));
                        stage = Stage::Holding;
                        let _ = s.control.send(RealtimeControl::ToolResponse {
                            id, name,
                            response: crate::voice::controller::brain_started_response(),
                            scheduling: Some(Scheduling::WhenIdle),
                            will_continue: true,
                        });
                        // The "brain" answers a few seconds later.
                        final_due = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                    }
                    RealtimeEvent::OutputTranscript(t) => match stage {
                        Stage::BeforeCall => out.before_call.push_str(&t),
                        Stage::Holding => out.holding.push_str(&t),
                        Stage::Result => out.result.push_str(&t),
                    },
                    RealtimeEvent::Audio { pcm, .. } if stage == Stage::Result => result_audio += pcm.len(),
                    RealtimeEvent::TurnComplete if stage == Stage::Result && result_audio > 0 => break Ok(()),
                    RealtimeEvent::Closed { reason } => break Err(format!("the connection closed: {reason}")),
                    _ => {}
                },
            },
        }
    };
    let _ = s.control.send(RealtimeControl::Close);
    out.before_call = out.before_call.trim().to_string();
    out.holding = out.holding.trim().to_string();
    out.result = out.result.trim().to_string();
    tracing::info!(
        "voice: handoff check: before call {} chars, holding {} chars, result {} chars",
        out.before_call.chars().count(),
        out.holding.chars().count(),
        out.result.chars().count()
    );
    result.map(|()| out)
}

/// The test question used by Settings and the live test.
pub const TEST_QUESTION: &str = "Hi Hobbes, this is a sound check. What is two plus two?";

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        b.extend_from_slice(&(2 * channels).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        // An odd-sized extra chunk before data (padded), as `say` may write.
        b.extend_from_slice(b"FLLR");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    #[test]
    fn the_sample_line_names_the_voice() {
        assert!(sample_line("Sulafat").starts_with("Hi, I'm Sulafat."));
    }

    #[test]
    fn wav_parsing_takes_the_first_channel() {
        let (rate, pcm) = parse_wav_mono16(&wav(16_000, 1, &[1, -2, 3])).unwrap();
        assert_eq!((rate, pcm), (16_000, vec![1, -2, 3]));
        let (_, pcm) = parse_wav_mono16(&wav(22_050, 2, &[5, 9, -5, 9])).unwrap();
        assert_eq!(pcm, vec![5, -5]);
        assert!(parse_wav_mono16(b"nope").is_err());
    }

    #[test]
    fn verdicts_tell_missing_silent_and_working_apart() {
        assert_eq!(mic_verdict(0, 0.0), MicVerdict::NoAudio);
        assert_eq!(mic_verdict(150, 0.0), MicVerdict::Silent);
        assert_eq!(mic_verdict(150, 0.2), MicVerdict::Ok(0.2));
        assert!(verdict_text(MicVerdict::NoAudio, "Logitech BRIO").starts_with("No audio is arriving from Logitech BRIO"));
        assert_eq!(peak(&[0, -16384, 100]), 0.5);
        assert_eq!(peak(&[i16::MIN]), 1.0);
    }

    /// Real speech from `say` is loud enough to count as audio, and sits at
    /// the rate providers expect. Skips where `say` isn't available.
    #[cfg(target_os = "macos")]
    #[test]
    fn synthesized_speech_is_real_audio() {
        let Ok(pcm) = synth_speech(TEST_QUESTION) else {
            eprintln!("say unavailable; skipping");
            return;
        };
        let secs = pcm.len() as f32 / INPUT_SAMPLE_RATE as f32;
        assert!(secs > 1.0 && secs < 10.0, "{secs} s");
        assert!(matches!(mic_verdict(pcm.len() / 320, peak(&pcm)), MicVerdict::Ok(_)));
    }
}
