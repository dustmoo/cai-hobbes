//! Cross-platform audio over `cpal` (used where macOS VPIO isn't available,
//! i.e. Windows/Linux). No echo cancellation: headphones recommended.
//!
//! Threading: cpal streams aren't `Send` on every platform, so a dedicated
//! thread ("hobbes-voice-audio") builds and owns both streams and parks until
//! `stop`/drop. [`CpalIo`] only holds atomics, the playback ring and the
//! stop channel, so it is `Send` and `play`/`flush` never block on audio.
//!
//! - Capture: default (or chosen) input → channel 0 → 16 kHz mono i16, 20 ms
//!   frames to `mic_tx`.
//! - Playback: model PCM is resampled on the caller's thread to the output
//!   device rate and pushed onto a ring the output callback drains.

use super::dsp::{i16_to_f32, Framer, StreamResampler};
use super::{AudioDevice, AudioError, AudioIo};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use tokio::sync::mpsc;

const MIC_RATE: u32 = 16_000;
/// 20 ms at 16 kHz.
const MIC_FRAME_SAMPLES: usize = 320;

/// Input devices via cpal. cpal has no stable device id, so `uid` is the
/// device name.
pub fn list_input_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| d.name().ok())
        .map(|name| AudioDevice {
            is_default: Some(&name) == default_name.as_ref(),
            uid: name.clone(),
            name,
        })
        .collect()
}

struct Shared {
    mic_enabled: AtomicBool,
    /// Mono f32 samples at the output device rate, waiting to be played.
    ring: Mutex<VecDeque<f32>>,
    /// Output device rate; 0 until started.
    out_rate: AtomicU32,
}

pub struct CpalIo {
    shared: Arc<Shared>,
    input_uid: Option<String>,
    /// Dropping/sending on this ends the audio thread (and its streams).
    stop_tx: Option<std_mpsc::Sender<()>>,
}

impl CpalIo {
    /// `input_uid`: the input device name from [`list_input_devices`]
    /// (`None` = default; a missing device falls back to the default).
    pub fn new(input_uid: Option<&str>) -> Result<Self, AudioError> {
        Ok(Self {
            shared: Arc::new(Shared {
                mic_enabled: AtomicBool::new(true),
                ring: Mutex::new(VecDeque::new()),
                out_rate: AtomicU32::new(0),
            }),
            input_uid: input_uid.filter(|u| !u.is_empty()).map(str::to_owned),
            stop_tx: None,
        })
    }
}

impl AudioIo for CpalIo {
    fn start(&mut self, mic_tx: mpsc::UnboundedSender<Vec<i16>>) -> Result<(), AudioError> {
        self.stop();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), AudioError>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let shared = self.shared.clone();
        let input_uid = self.input_uid.clone();
        std::thread::Builder::new()
            .name("hobbes-voice-audio".into())
            .spawn(move || {
                match build_streams(&shared, input_uid.as_deref(), mic_tx) {
                    Ok(streams) => {
                        let _ = ready_tx.send(Ok(()));
                        // Park until stop (message or sender dropped).
                        let _ = stop_rx.recv();
                        drop(streams);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| AudioError::Backend(format!("audio thread: {e}")))?;
        let res = ready_rx
            .recv()
            .unwrap_or_else(|_| Err(AudioError::Backend("audio thread is gone".into())));
        if res.is_ok() {
            self.stop_tx = Some(stop_tx);
        }
        res
    }

    fn play(&self, pcm: &[i16], sample_rate: u32) {
        let out_rate = self.shared.out_rate.load(Ordering::Acquire);
        if pcm.is_empty() || out_rate == 0 || self.stop_tx.is_none() {
            return;
        }
        let resampled;
        let src = if sample_rate == out_rate || sample_rate == 0 {
            pcm
        } else {
            resampled = super::resample_linear(pcm, sample_rate, out_rate);
            &resampled
        };
        if let Ok(mut ring) = self.shared.ring.lock() {
            ring.extend(src.iter().copied().map(i16_to_f32));
        }
    }

    fn flush(&self) {
        if let Ok(mut ring) = self.shared.ring.lock() {
            ring.clear();
        }
    }

    fn is_playing(&self) -> bool {
        self.shared.ring.lock().map(|r| !r.is_empty()).unwrap_or(false)
    }

    fn set_mic_enabled(&self, enabled: bool) {
        self.shared.mic_enabled.store(enabled, Ordering::Release);
    }

    fn echo_cancelled(&self) -> bool {
        false
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.flush();
        self.shared.out_rate.store(0, Ordering::Release);
    }
}

impl Drop for CpalIo {
    fn drop(&mut self) {
        self.stop();
    }
}

fn backend<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> AudioError + '_ {
    move |e| AudioError::Backend(format!("{what}: {e}"))
}

fn pick_input(host: &cpal::Host, uid: Option<&str>) -> Option<cpal::Device> {
    if let Some(uid) = uid {
        let found = host
            .input_devices()
            .ok()
            .and_then(|mut it| it.find(|d| d.name().map(|n| n == uid).unwrap_or(false)));
        match found {
            Some(d) => return Some(d),
            None => tracing::warn!("voice audio: chosen input device is gone; using the default"),
        }
    }
    host.default_input_device()
}

fn build_streams(
    shared: &Arc<Shared>,
    input_uid: Option<&str>,
    mic_tx: mpsc::UnboundedSender<Vec<i16>>,
) -> Result<(cpal::Stream, cpal::Stream), AudioError> {
    let host = cpal::default_host();
    let input = pick_input(&host, input_uid).ok_or(AudioError::NoDevice)?;
    let output = host.default_output_device().ok_or(AudioError::NoDevice)?;

    let in_cfg = input
        .default_input_config()
        .map_err(backend("input config"))?;
    let out_cfg = output
        .default_output_config()
        .map_err(backend("output config"))?;
    let in_format = in_cfg.sample_format();
    let out_format = out_cfg.sample_format();
    let in_cfg: StreamConfig = in_cfg.into();
    let out_cfg: StreamConfig = out_cfg.into();

    let in_stream = match in_format {
        SampleFormat::F32 => input_stream::<f32>(&input, &in_cfg, shared, mic_tx),
        SampleFormat::I16 => input_stream::<i16>(&input, &in_cfg, shared, mic_tx),
        SampleFormat::U16 => input_stream::<u16>(&input, &in_cfg, shared, mic_tx),
        SampleFormat::I32 => input_stream::<i32>(&input, &in_cfg, shared, mic_tx),
        other => Err(AudioError::Backend(format!(
            "unsupported input sample format {other:?}"
        ))),
    }?;
    let out_stream = match out_format {
        SampleFormat::F32 => output_stream::<f32>(&output, &out_cfg, shared),
        SampleFormat::I16 => output_stream::<i16>(&output, &out_cfg, shared),
        SampleFormat::U16 => output_stream::<u16>(&output, &out_cfg, shared),
        SampleFormat::I32 => output_stream::<i32>(&output, &out_cfg, shared),
        other => Err(AudioError::Backend(format!(
            "unsupported output sample format {other:?}"
        ))),
    }?;

    let denied = |e: cpal::PlayStreamError| AudioError::Backend(format!("start stream: {e}"));
    in_stream.play().map_err(denied)?;
    out_stream.play().map_err(denied)?;
    shared
        .out_rate
        .store(out_cfg.sample_rate.0, Ordering::Release);

    tracing::info!(
        "voice audio: cpal (no echo cancellation), input {} Hz x{} → {} kHz, output {} Hz x{}",
        in_cfg.sample_rate.0,
        in_cfg.channels,
        MIC_RATE / 1000,
        out_cfg.sample_rate.0,
        out_cfg.channels,
    );
    Ok((in_stream, out_stream))
}

fn map_build_err(e: cpal::BuildStreamError) -> AudioError {
    match e {
        cpal::BuildStreamError::DeviceNotAvailable => AudioError::NoDevice,
        other => AudioError::Backend(format!("build stream: {other}")),
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    cfg: &StreamConfig,
    shared: &Arc<Shared>,
    mic_tx: mpsc::UnboundedSender<Vec<i16>>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = (cfg.channels as usize).max(1);
    let shared = shared.clone();
    let mut resampler = StreamResampler::new(cfg.sample_rate.0, MIC_RATE);
    let mut framer = Framer::new(MIC_FRAME_SAMPLES);
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let mut out: Vec<i16> = Vec::with_capacity(4096);
    let mut was_enabled = true;
    device
        .build_input_stream(
            cfg,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if !shared.mic_enabled.load(Ordering::Acquire) {
                    if was_enabled {
                        framer.clear();
                        was_enabled = false;
                    }
                    return;
                }
                was_enabled = true;
                mono.clear();
                mono.extend(
                    data.chunks(channels)
                        .map(|frame| frame[0].to_sample::<f32>()),
                );
                out.clear();
                resampler.process(&mono, &mut out);
                framer.push(&out, |frame| {
                    let _ = mic_tx.send(frame);
                });
            },
            |e| tracing::warn!("voice audio: input stream error: {e}"),
            None,
        )
        .map_err(map_build_err)
}

fn output_stream<T>(
    device: &cpal::Device,
    cfg: &StreamConfig,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = (cfg.channels as usize).max(1);
    let shared = shared.clone();
    device
        .build_output_stream(
            cfg,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                // try_lock: never block the realtime callback; on contention
                // play silence for this period.
                let mut ring = shared.ring.try_lock().ok();
                for frame in data.chunks_mut(channels) {
                    let s = ring.as_mut().and_then(|r| r.pop_front()).unwrap_or(0.0);
                    let v = T::from_sample(s);
                    for out in frame.iter_mut() {
                        *out = v;
                    }
                }
            },
            |e| tracing::warn!("voice audio: output stream error: {e}"),
            None,
        )
        .map_err(map_build_err)
}
