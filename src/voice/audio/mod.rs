//! Native audio I/O for voice: one object owns the mic and the speaker so
//! the platform can cancel the speaker's echo out of the mic (macOS Voice
//! Processing IO). That is what lets the person talk over Hobbes on laptop
//! speakers without the model hearing itself.
//!
//! - macOS, voice processing: `AVAudioEngine` with Apple's echo canceller
//!   (`macos.rs`). It always captures from the system default mic (it
//!   ignores a chosen device) and puts a Bluetooth headset into call mode.
//! - Everywhere else, and on macOS when a specific mic is chosen or the
//!   output is headphones: `cpal` on exactly the chosen device, no echo
//!   cancellation (`cpal_io.rs`). See [`use_voice_processing`].

use tokio::sync::mpsc;

mod dsp;
#[cfg(target_os = "macos")]
mod macos;
mod cpal_io;

pub use crate::settings::VoiceEcho;

/// Why audio could not start.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioError {
    /// The person (or policy) denied microphone access.
    PermissionDenied,
    /// No input or output device.
    NoDevice,
    Backend(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::PermissionDenied => write!(f, "microphone access is denied"),
            AudioError::NoDevice => write!(f, "no audio device"),
            AudioError::Backend(e) => write!(f, "audio error: {e}"),
        }
    }
}

/// Microphone permission as the OS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicPermission {
    Granted,
    Denied,
    /// Never asked; the first capture (or [`request_mic_permission`]) prompts.
    Undetermined,
}

/// Mic + speaker. Created on a voice session's start and dropped at its end.
///
/// Threading: implementations run their own audio threads/callbacks and are
/// `Send`; every method is cheap and non-blocking.
pub trait AudioIo: Send {
    /// Start capture and playback. Mic audio arrives on `mic_tx` as mono
    /// 16-bit PCM at 16 kHz in frames of roughly 20–40 ms, already
    /// echo-cancelled where the platform supports it.
    fn start(&mut self, mic_tx: mpsc::UnboundedSender<Vec<i16>>) -> Result<(), AudioError>;
    /// Queue model speech (mono 16-bit PCM at `sample_rate`) for playback.
    fn play(&self, pcm: &[i16], sample_rate: u32);
    /// Drop all queued and playing speech at once (barge-in). Must take
    /// effect within ~20 ms.
    fn flush(&self);
    /// Whether speech is queued or playing.
    fn is_playing(&self) -> bool;
    /// Gate the mic (push-to-talk / mute) without tearing down the engine.
    /// While disabled, nothing is sent on `mic_tx`.
    fn set_mic_enabled(&self, enabled: bool);
    /// Open (true) or release (false) the mic device itself, not just the
    /// stream to the model: released, the system's mic indicator goes off.
    /// Playback is unaffected. Backends that can't release the mic without
    /// stopping playback (macOS voice processing shares one engine) only
    /// gate, as [`AudioIo::set_mic_enabled`].
    fn set_capture(&self, on: bool) {
        self.set_mic_enabled(on);
    }
    /// Whether the model can't hear itself: the backend cancels speaker
    /// echo (macOS voice processing), or the output is headphones. When
    /// false the controller mutes the mic while the voice speaks.
    fn echo_safe(&self) -> bool;
    /// Stop everything. Idempotent; also done on drop.
    fn stop(&mut self);
}

/// An audio input device the person can pick.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDevice {
    /// Stable identifier to store in settings (CoreAudio device UID on
    /// macOS; the device name elsewhere).
    pub uid: String,
    /// Human-readable name for the picker.
    pub name: String,
    /// Whether this is the system default input right now.
    pub is_default: bool,
}

/// Input devices (microphones) currently present.
pub fn list_input_devices() -> Vec<AudioDevice> {
    #[cfg(target_os = "macos")]
    {
        macos::list_input_devices()
    }
    #[cfg(not(target_os = "macos"))]
    {
        cpal_io::list_input_devices()
    }
}

/// Pick the device `uid` names from `devices`. `None` (use the system
/// default) when `uid` is `None`/empty or no longer present.
pub fn resolve_input_device<'a>(
    devices: &'a [AudioDevice],
    uid: Option<&str>,
) -> Option<&'a AudioDevice> {
    let uid = uid.filter(|u| !u.is_empty())?;
    devices.iter().find(|d| d.uid == uid)
}

/// What the default output device is, as far as echo goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Bluetooth audio or the headphone jack: the mic can't hear it.
    Headphones,
    /// Built-in or external speakers (or unknown: assume the worst).
    Speakers,
}

/// The default output device's kind.
pub fn default_output_kind() -> OutputKind {
    #[cfg(target_os = "macos")]
    {
        macos::default_output_kind()
    }
    #[cfg(not(target_os = "macos"))]
    {
        OutputKind::Speakers
    }
}

/// Whether to use Apple voice processing (macOS only). It only captures
/// from the system default mic, so a specifically chosen mic always wins
/// in `Auto`; and on headphones there is no echo to cancel.
pub fn use_voice_processing(mode: VoiceEcho, chosen_mic: bool, output: OutputKind) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    match mode {
        VoiceEcho::On => true,
        VoiceEcho::Off => false,
        VoiceEcho::Auto => !chosen_mic && output == OutputKind::Speakers,
    }
}

/// An opened (not yet started) audio backend and what it is using.
pub struct Opened {
    pub io: Box<dyn AudioIo>,
    /// The mic actually captured from, for the indicator and logs.
    pub input_name: String,
    pub voice_processing: bool,
}

/// Open audio for a conversation: capture from the mic `input_uid` names
/// (see [`list_input_devices`]; `None` or a device that is gone = system
/// default), play to the system default output.
pub fn open(input_uid: Option<&str>, echo: VoiceEcho) -> Result<Opened, AudioError> {
    let devices = list_input_devices();
    let chosen = resolve_input_device(&devices, input_uid).cloned();
    if input_uid.is_some_and(|u| !u.is_empty()) && chosen.is_none() {
        tracing::warn!("voice audio: the chosen mic isn't connected; using the system default");
    }
    let output = default_output_kind();
    let vp = use_voice_processing(echo, chosen.is_some(), output);
    let default_name = devices.iter().find(|d| d.is_default).map(|d| d.name.clone());
    if vp {
        return open_voice_processing(default_name, output);
    }
    // cpal finds devices by name.
    let name = chosen.as_ref().map(|d| d.name.clone());
    let io = cpal_io::CpalIo::new(name.as_deref(), output == OutputKind::Headphones)?;
    let input_name = name.or(default_name).unwrap_or_else(|| "the system default mic".into());
    tracing::info!("voice audio: mic '{input_name}', no echo cancellation; output {output:?}");
    Ok(Opened { io: Box::new(io), input_name, voice_processing: false })
}

#[cfg(target_os = "macos")]
fn open_voice_processing(default_name: Option<String>, output: OutputKind) -> Result<Opened, AudioError> {
    let io = macos::VoiceProcessingIo::new()?;
    let input_name = default_name.unwrap_or_else(|| "the system default mic".into());
    tracing::info!("voice audio: mic '{input_name}' (system default) with Apple voice processing; output {output:?}");
    Ok(Opened { io: Box::new(io), input_name, voice_processing: true })
}

#[cfg(not(target_os = "macos"))]
fn open_voice_processing(_: Option<String>, _: OutputKind) -> Result<Opened, AudioError> {
    Err(AudioError::Backend("voice processing is macOS-only".into()))
}

/// Current microphone permission.
pub fn mic_permission() -> MicPermission {
    #[cfg(target_os = "macos")]
    {
        macos::mic_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        MicPermission::Granted
    }
}

/// Ask for microphone access (shows the system prompt when undetermined).
/// Resolves with the outcome.
pub async fn request_mic_permission() -> MicPermission {
    #[cfg(target_os = "macos")]
    {
        macos::request_mic_permission().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        MicPermission::Granted
    }
}

/// Linear resample of mono 16-bit PCM (good enough for speech).
pub fn resample_linear(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let a = input[idx] as f64;
        let b = *input.get(idx + 1).unwrap_or(&input[idx]) as f64;
        out.push((a + (b - a) * frac).round() as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_duration() {
        let input: Vec<i16> = (0..480).map(|i| (i * 10) as i16).collect(); // 10 ms @ 48k
        let out = resample_linear(&input, 48_000, 16_000);
        assert_eq!(out.len(), 160);
        assert_eq!(out[0], 0);
        assert_eq!(resample_linear(&input, 16_000, 16_000), input);
    }

    #[test]
    fn resolve_input_device_falls_back_to_default() {
        let devs = vec![
            AudioDevice { uid: "a".into(), name: "Headset".into(), is_default: true },
            AudioDevice { uid: "b".into(), name: "Webcam".into(), is_default: false },
        ];
        assert_eq!(resolve_input_device(&devs, Some("b")).map(|d| d.name.as_str()), Some("Webcam"));
        assert_eq!(resolve_input_device(&devs, Some("gone")), None);
        assert_eq!(resolve_input_device(&devs, Some("")), None);
        assert_eq!(resolve_input_device(&devs, None), None);
    }

    /// Every CoreAudio mic must be findable by cpal under the same name
    /// (cpal opens devices by name). Query only; opens nothing.
    /// `cargo test --release cpal_sees_the_same_mics -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn cpal_sees_the_same_mics() {
        let cpal_names: Vec<String> = cpal_io::list_input_devices().into_iter().map(|d| d.name).collect();
        println!("cpal: {cpal_names:?}");
        for d in list_input_devices() {
            assert!(cpal_names.contains(&d.name), "cpal can't see {:?}", d.name);
        }
    }

    /// Opens a mic for 3 s and prints frames + peak (nothing is kept).
    /// `HOBBES_MIC_UID=<uid> cargo test --release mic_check_prints -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn mic_check_prints() {
        let uid = std::env::var("HOBBES_MIC_UID").ok();
        let r = crate::voice::probe::mic_check(uid, VoiceEcho::Auto, std::time::Duration::from_secs(3), |_| {})
            .await
            .expect("mic check");
        println!("{r:?}\n{}", crate::voice::probe::verdict_text(r.verdict(), &r.input_name));
    }

    /// Push-to-talk releases the mic device between holds, and stopping
    /// releases it for good — checked with the HAL's "running somewhere"
    /// flag (the mic indicator), not just our own gate.
    /// `HOBBES_MIC_UID=<uid> cargo test --release capture_toggle_releases_the_mic -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn capture_toggle_releases_the_mic() {
        let uid = std::env::var("HOBBES_MIC_UID").expect("set HOBBES_MIC_UID");
        // This process only: other apps may be using the same mic.
        let running = || macos::this_process_running_input().expect("HAL process query");
        let settle = || std::thread::sleep(std::time::Duration::from_millis(400));
        println!("device in use by anyone: {:?}", macos::device_running_somewhere(&uid));
        assert!(!running(), "this process already has input running");
        let mut o = open(Some(&uid), VoiceEcho::Off).expect("open");
        assert!(!o.voice_processing);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        o.io.start(tx).expect("start");
        settle();
        println!("started: running={}", running());
        assert!(running());
        o.io.set_capture(false);
        settle();
        while rx.try_recv().is_ok() {}
        settle();
        println!("capture off: running={} frames since={}", running(), rx.try_recv().is_ok());
        assert!(!running(), "mic still open after capture off");
        o.io.set_capture(true);
        settle();
        println!("capture on: running={}", running());
        assert!(running());
        assert!(rx.try_recv().is_ok(), "no frames after capture on");
        o.io.stop();
        settle();
        println!("stopped: running={}", running());
        assert!(!running(), "mic still open after stop");
    }

    #[test]
    fn a_chosen_mic_wins_over_voice_processing_in_auto() {
        let on_mac = cfg!(target_os = "macos");
        // No mic chosen, laptop speakers: echo cancellation helps.
        assert_eq!(use_voice_processing(VoiceEcho::Auto, false, OutputKind::Speakers), on_mac);
        // A chosen mic (e.g. the webcam): honour it; VPIO would ignore it.
        assert!(!use_voice_processing(VoiceEcho::Auto, true, OutputKind::Speakers));
        // Headphones: nothing to cancel, and VPIO would flip a Bluetooth
        // headset into call mode.
        assert!(!use_voice_processing(VoiceEcho::Auto, false, OutputKind::Headphones));
        assert_eq!(use_voice_processing(VoiceEcho::On, true, OutputKind::Headphones), on_mac);
        assert!(!use_voice_processing(VoiceEcho::Off, false, OutputKind::Speakers));
    }
}
