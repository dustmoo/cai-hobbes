//! Native audio I/O for voice: one object owns the mic and the speaker so
//! the platform can cancel the speaker's echo out of the mic (macOS Voice
//! Processing IO). That is what lets the person talk over Hobbes on laptop
//! speakers without the model hearing itself.
//!
//! - macOS: `AVAudioEngine` with voice processing enabled (`macos.rs`).
//! - Elsewhere: `cpal`, without echo cancellation (`cpal_io.rs`); headphones
//!   recommended.

use tokio::sync::mpsc;

mod dsp;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod cpal_io;

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
    /// Whether this backend cancels speaker echo (macOS VPIO: true).
    fn echo_cancelled(&self) -> bool;
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

/// The platform's best audio backend, capturing from the input device
/// `input_uid` names (see [`list_input_devices`]). `None`, or a device that
/// is gone (logged at WARN), uses the system default input. Output always
/// goes to the system default.
pub fn open(input_uid: Option<&str>) -> Result<Box<dyn AudioIo>, AudioError> {
    #[cfg(target_os = "macos")]
    {
        macos::VoiceProcessingIo::new(input_uid).map(|io| Box::new(io) as Box<dyn AudioIo>)
    }
    #[cfg(not(target_os = "macos"))]
    {
        cpal_io::CpalIo::new(input_uid).map(|io| Box::new(io) as Box<dyn AudioIo>)
    }
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
}
