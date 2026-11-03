//! macOS audio: `AVAudioEngine` with Voice Processing IO (VPIO).
//!
//! Enabling voice processing on the engine's input node turns on Apple's
//! acoustic echo canceller (plus noise suppression and AGC). The canceller
//! needs to see what the speaker plays, so playback goes through the same
//! engine: an `AVAudioPlayerNode` feeding the main mixer.
//!
//! Threading: the engine and its nodes live on one dedicated thread
//! ("hobbes-voice-audio"), which owns every Objective-C object and receives
//! commands over a channel. [`VoiceProcessingIo`] itself only holds the
//! command sender and a few atomics, so it is `Send` without any `unsafe impl`,
//! and `play`/`flush`/`set_mic_enabled` never block. `start` and `stop` wait
//! for the audio thread to answer (engine start-up with VPIO can take a few
//! hundred milliseconds), so call `start` off the UI thread.
//!
//! Capture: a tap on input bus 0 takes channel 0, converts to i16, resamples
//! to 16 kHz with a streaming linear resampler and sends 20 ms frames.

use super::dsp::{i16_to_f32, Framer, StreamResampler};
use super::{AudioDevice, AudioError, AudioIo, MicPermission};
use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, Bool};
use objc2::{sel, AnyThread};
use objc2_avf_audio::{
    AVAudioApplication, AVAudioApplicationRecordPermission, AVAudioCommonFormat, AVAudioEngine,
    AVAudioFormat, AVAudioPCMBuffer, AVAudioPlayerNode, AVAudioPlayerNodeCompletionCallbackType,
    AVAudioTime, AVAudioVoiceProcessingOtherAudioDuckingConfiguration,
    AVAudioVoiceProcessingOtherAudioDuckingLevel,
};
use objc2_foundation::{NSError, NSObjectProtocol, NSString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Rate of the capture frames handed to `mic_tx`.
const MIC_RATE: u32 = 16_000;
/// 20 ms at 16 kHz.
const MIC_FRAME_SAMPLES: usize = 320;
/// Playback processing rate (the realtime models speak at 24 kHz).
const PLAYBACK_RATE: u32 = 24_000;
/// Frames per tap callback requested from the input node (a hint; the
/// engine may deliver a different size).
const TAP_BUFFER_FRAMES: u32 = 1024;

/// Model PCM (i16 at `sample_rate`) → f32 at [`PLAYBACK_RATE`].
fn to_playback_f32(pcm: &[i16], sample_rate: u32) -> Vec<f32> {
    if sample_rate == PLAYBACK_RATE || sample_rate == 0 {
        pcm.iter().copied().map(i16_to_f32).collect()
    } else {
        super::resample_linear(pcm, sample_rate, PLAYBACK_RATE)
            .into_iter()
            .map(i16_to_f32)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Shared state between the handle, the audio thread and the callbacks
// ---------------------------------------------------------------------------

struct Shared {
    mic_enabled: AtomicBool,
    /// Buffers handed to `play` and not yet played back.
    queued: AtomicUsize,
    /// Bumped by `flush`/`stop`; buffers and completion handlers from an
    /// older generation are dropped and don't touch `queued`.
    generation: AtomicU64,
    echo_cancelled: AtomicBool,
}

impl Shared {
    fn buffer_done(&self, epoch: u64) {
        if self.generation.load(Ordering::Acquire) != epoch {
            return;
        }
        let _ = self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |q| q.checked_sub(1));
    }

    fn invalidate_queue(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.queued.store(0, Ordering::Release);
    }
}

enum Cmd {
    Start {
        mic_tx: mpsc::UnboundedSender<Vec<i16>>,
        reply: std_mpsc::Sender<Result<(), AudioError>>,
    },
    Play {
        samples: Vec<f32>,
        epoch: u64,
    },
    Flush,
    Stop {
        reply: std_mpsc::Sender<()>,
    },
}

/// Mic + speaker over `AVAudioEngine` with voice processing.
pub struct VoiceProcessingIo {
    cmd_tx: Option<std_mpsc::Sender<Cmd>>,
    shared: Arc<Shared>,
    started: bool,
}

impl VoiceProcessingIo {
    /// Captures from the system default mic: voice processing ignores a
    /// device set on the node's IO unit (seen on macOS 27: the unit
    /// accepts the property and keeps capturing from the default), so a chosen mic
    /// goes through `cpal_io` instead (see `audio::open`).
    pub fn new() -> Result<Self, AudioError> {
        let shared = Arc::new(Shared {
            mic_enabled: AtomicBool::new(true),
            queued: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            echo_cancelled: AtomicBool::new(false),
        });
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();
        let thread_shared = shared.clone();
        // Detached: the thread ends when the command channel closes (on
        // drop). Not joined, so a wedged CoreAudio call can't hang the
        // dropper.
        std::thread::Builder::new()
            .name("hobbes-voice-audio".into())
            .spawn(move || audio_thread(cmd_rx, thread_shared))
            .map_err(|e| AudioError::Backend(format!("audio thread: {e}")))?;
        Ok(Self {
            cmd_tx: Some(cmd_tx),
            shared,
            started: false,
        })
    }

    fn send(&self, cmd: Cmd) -> bool {
        self.cmd_tx.as_ref().is_some_and(|tx| tx.send(cmd).is_ok())
    }
}

impl AudioIo for VoiceProcessingIo {
    fn start(&mut self, mic_tx: mpsc::UnboundedSender<Vec<i16>>) -> Result<(), AudioError> {
        if mic_permission() == MicPermission::Denied {
            return Err(AudioError::PermissionDenied);
        }
        let gone = || AudioError::Backend("audio thread is gone".into());
        let (reply, rx) = std_mpsc::channel();
        if !self.send(Cmd::Start { mic_tx, reply }) {
            return Err(gone());
        }
        let res = rx.recv().map_err(|_| gone())?;
        self.started = res.is_ok();
        res
    }

    fn play(&self, pcm: &[i16], sample_rate: u32) {
        if pcm.is_empty() || !self.started {
            return;
        }
        let samples = to_playback_f32(pcm, sample_rate);
        if samples.is_empty() {
            return;
        }
        let epoch = self.shared.generation.load(Ordering::Acquire);
        // Count before handing over, so `is_playing` is true as soon as
        // `play` returns.
        self.shared.queued.fetch_add(1, Ordering::AcqRel);
        if !self.send(Cmd::Play { samples, epoch }) {
            self.shared.buffer_done(epoch);
        }
    }

    fn flush(&self) {
        self.shared.invalidate_queue();
        self.send(Cmd::Flush);
    }

    fn is_playing(&self) -> bool {
        self.shared.queued.load(Ordering::Acquire) > 0
    }

    fn set_mic_enabled(&self, enabled: bool) {
        self.shared.mic_enabled.store(enabled, Ordering::Release);
    }

    fn echo_safe(&self) -> bool {
        self.shared.echo_cancelled.load(Ordering::Acquire)
    }

    fn stop(&mut self) {
        if !self.started {
            return;
        }
        self.started = false;
        self.shared.invalidate_queue();
        let (reply, rx) = std_mpsc::channel();
        if self.send(Cmd::Stop { reply }) {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
        }
    }
}

impl Drop for VoiceProcessingIo {
    fn drop(&mut self) {
        self.stop();
        // Closing the channel ends the audio thread.
        self.cmd_tx = None;
    }
}

// ---------------------------------------------------------------------------
// Audio thread
// ---------------------------------------------------------------------------

/// How often the audio thread checks that the engine is still running.
const WATCHDOG: Duration = Duration::from_millis(500);

/// Caps automatic engine rebuilds (after device/route changes stop the
/// engine) so a flapping device can't spin us.
pub(crate) struct RestartBudget {
    max: u32,
    window: Duration,
    window_start: Option<Instant>,
    used: u32,
    exhausted_warned: bool,
}

impl RestartBudget {
    pub(crate) fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            window_start: None,
            used: 0,
            exhausted_warned: false,
        }
    }

    /// Take one restart if the budget allows it at `now`.
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        match self.window_start {
            Some(t) if now.duration_since(t) < self.window => {}
            _ => {
                self.window_start = Some(now);
                self.used = 0;
                self.exhausted_warned = false;
            }
        }
        if self.used < self.max {
            self.used += 1;
            true
        } else {
            false
        }
    }
}

struct Engine {
    engine: Retained<AVAudioEngine>,
    player: Retained<AVAudioPlayerNode>,
    play_format: Retained<AVAudioFormat>,
}

struct ThreadState {
    shared: Arc<Shared>,
    engine: Option<Engine>,
    /// Set while a session is started; lets the watchdog rebuild the engine.
    mic_tx: Option<mpsc::UnboundedSender<Vec<i16>>>,
    restarts: RestartBudget,
}

impl ThreadState {
    fn build(&mut self) -> Result<(), AudioError> {
        let Some(tx) = self.mic_tx.clone() else {
            return Ok(());
        };
        self.engine = Some(build_engine(tx, &self.shared)?);
        Ok(())
    }

    fn teardown(&mut self) {
        if let Some(e) = self.engine.take() {
            teardown(&e);
        }
    }

    /// A device or route change (AirPods connecting, a Bluetooth headset
    /// switching to its call profile, the default device changing) stops
    /// the engine. Rebuild it so capture and playback keep flowing.
    fn ensure_running(&mut self) {
        if self.mic_tx.is_none() {
            return;
        }
        // SAFETY: engine owned by this thread.
        let running = self
            .engine
            .as_ref()
            .is_some_and(|e| unsafe { e.engine.isRunning() });
        if running {
            return;
        }
        if !self.restarts.take(Instant::now()) {
            if !self.restarts.exhausted_warned {
                tracing::warn!("voice audio: engine keeps stopping; giving up on restarts for now");
                self.restarts.exhausted_warned = true;
            }
            return;
        }
        tracing::warn!("voice audio: engine stopped (device change?); rebuilding");
        self.teardown();
        // Buffers scheduled on the old player are gone.
        self.shared.invalidate_queue();
        if let Err(e) = self.build() {
            tracing::warn!("voice audio: rebuild failed: {e}");
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start { mic_tx, reply } => {
                self.teardown();
                self.mic_tx = Some(mic_tx);
                self.restarts = RestartBudget::new(3, Duration::from_secs(60));
                let res = self.build();
                if res.is_err() {
                    self.mic_tx = None;
                }
                let _ = reply.send(res);
            }
            Cmd::Play { samples, epoch } => {
                self.ensure_running();
                match self.engine.as_ref() {
                    Some(e) => schedule(e, &samples, epoch, &self.shared),
                    None => self.shared.buffer_done(epoch),
                }
            }
            Cmd::Flush => {
                if let Some(e) = self.engine.as_ref() {
                    // SAFETY: player belongs to this thread's engine; stop
                    // drops everything scheduled, play re-arms it for later
                    // buffers.
                    unsafe {
                        e.player.stop();
                        e.player.play();
                    }
                }
            }
            Cmd::Stop { reply } => {
                self.mic_tx = None;
                self.teardown();
                let _ = reply.send(());
            }
        }
    }
}

fn audio_thread(rx: std_mpsc::Receiver<Cmd>, shared: Arc<Shared>) {
    let mut st = ThreadState {
        shared,
        engine: None,
        mic_tx: None,
        restarts: RestartBudget::new(3, Duration::from_secs(60)),
    };
    loop {
        match rx.recv_timeout(WATCHDOG) {
            Ok(cmd) => autoreleasepool(|_| st.handle(cmd)),
            Err(std_mpsc::RecvTimeoutError::Timeout) => autoreleasepool(|_| st.ensure_running()),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    st.mic_tx = None;
    autoreleasepool(|_| st.teardown());
}

fn ns_err(e: &NSError) -> AudioError {
    AudioError::Backend(e.localizedDescription().to_string())
}

fn build_engine(
    mic_tx: mpsc::UnboundedSender<Vec<i16>>,
    shared: &Arc<Shared>,
) -> Result<Engine, AudioError> {
    // SAFETY (whole block): plain AVFAudio calls on objects this thread
    // owns; the engine is stopped while we configure it, as VPIO requires.
    unsafe {
        let engine = AVAudioEngine::new();
        let input = engine.inputNode();

        // Build the output side BEFORE enabling VPIO. Creating the main
        // mixer after VPIO is on makes `start` fail with -10875
        // (kAudioUnitErr_FailedInitialization), seen on macOS 27.
        // Playback graph: player → main mixer, mono f32 at 24 kHz.
        let player = AVAudioPlayerNode::new();
        let play_format = AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            PLAYBACK_RATE as f64,
            1,
        )
        .ok_or_else(|| AudioError::Backend("playback format".into()))?;
        engine.attachNode(&player);
        let mixer = engine.mainMixerNode();
        engine.connect_to_format(&player, &mixer, Some(&play_format));

        let vpio = match input.setVoiceProcessingEnabled_error(true) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "voice audio: voice processing unavailable ({}); no echo cancellation",
                    e.localizedDescription()
                );
                false
            }
        };
        shared.echo_cancelled.store(vpio, Ordering::Release);

        if vpio {
            // Keep other apps' audio as loud as VPIO allows (macOS 14+).
            if input.respondsToSelector(sel!(setVoiceProcessingOtherAudioDuckingConfiguration:)) {
                input.setVoiceProcessingOtherAudioDuckingConfiguration(
                    AVAudioVoiceProcessingOtherAudioDuckingConfiguration {
                        enableAdvancedDucking: Bool::NO,
                        duckingLevel: AVAudioVoiceProcessingOtherAudioDuckingLevel::Min,
                    },
                );
            } else {
                tracing::warn!("voice audio: ducking control unavailable; other audio may duck");
            }
        }

        // Read the format after enabling VPIO: it often changes it.
        let in_format = input.outputFormatForBus(0);
        let in_rate = in_format.sampleRate();
        let in_channels = in_format.channelCount();
        if in_rate <= 0.0 || in_channels == 0 {
            return Err(AudioError::NoDevice);
        }
        let common = in_format.commonFormat();
        if common != AVAudioCommonFormat::PCMFormatFloat32
            && common != AVAudioCommonFormat::PCMFormatInt16
        {
            return Err(AudioError::Backend(format!(
                "unsupported input sample format {}",
                common.0
            )));
        }

        // Capture tap (the engine copies the block).
        let tap = make_tap(mic_tx, shared.clone(), in_rate.round() as u32);
        input.installTapOnBus_bufferSize_format_block(
            0,
            TAP_BUFFER_FRAMES,
            None,
            RcBlock::as_ptr(&tap),
        );

        engine.prepare();
        if let Err(e) = engine.startAndReturnError() {
            input.removeTapOnBus(0);
            return Err(if mic_permission() == MicPermission::Denied {
                AudioError::PermissionDenied
            } else {
                ns_err(&e)
            });
        }
        player.play();

        tracing::info!(
            "voice audio: VPIO {}, input {} Hz x{} → {} kHz, playback {} kHz",
            if vpio { "on" } else { "off" },
            in_rate as u32,
            in_channels,
            MIC_RATE / 1000,
            PLAYBACK_RATE / 1000,
        );

        Ok(Engine {
            engine,
            player,
            play_format,
        })
    }
}

/// Per-tap mutable state. Only the tap's thread touches it; the mutex is
/// uncontended.
struct TapState {
    resampler: StreamResampler,
    framer: Framer,
    scratch_f32: Vec<f32>,
    scratch_i16: Vec<i16>,
    was_enabled: bool,
}

type TapBlock = RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>)>;

fn make_tap(
    mic_tx: mpsc::UnboundedSender<Vec<i16>>,
    shared: Arc<Shared>,
    in_rate: u32,
) -> TapBlock {
    let state = Mutex::new(TapState {
        resampler: StreamResampler::new(in_rate, MIC_RATE),
        framer: Framer::new(MIC_FRAME_SAMPLES),
        scratch_f32: Vec::with_capacity(8192),
        scratch_i16: Vec::with_capacity(4096),
        was_enabled: true,
    });
    RcBlock::new(
        move |buf: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            let Ok(mut guard) = state.lock() else { return };
            let st = &mut *guard;
            if !shared.mic_enabled.load(Ordering::Acquire) {
                if st.was_enabled {
                    st.framer.clear();
                    st.was_enabled = false;
                }
                return;
            }
            st.was_enabled = true;
            st.scratch_f32.clear();
            // SAFETY: AVFAudio hands us a valid buffer for the duration of
            // the callback; we read `frameLength` samples of channel 0,
            // `stride` apart (stride covers interleaved layouts).
            unsafe {
                let buf = buf.as_ref();
                let len = buf.frameLength() as usize;
                if len == 0 {
                    return;
                }
                let stride = buf.stride().max(1);
                let fdata = buf.floatChannelData();
                if !fdata.is_null() {
                    let ch0 = (*fdata).as_ptr();
                    st.scratch_f32.extend((0..len).map(|i| *ch0.add(i * stride)));
                } else {
                    let idata = buf.int16ChannelData();
                    if idata.is_null() {
                        return;
                    }
                    let ch0 = (*idata).as_ptr();
                    st.scratch_f32
                        .extend((0..len).map(|i| i16_to_f32(*ch0.add(i * stride))));
                }
            }
            st.scratch_i16.clear();
            st.resampler.process(&st.scratch_f32, &mut st.scratch_i16);
            st.framer.push(&st.scratch_i16, |frame| {
                let _ = mic_tx.send(frame);
            });
        },
    )
}

fn schedule(e: &Engine, samples: &[f32], epoch: u64, shared: &Arc<Shared>) {
    if shared.generation.load(Ordering::Acquire) != epoch {
        return; // flushed before it got here
    }
    // SAFETY: objects owned by this thread; the buffer is fully written
    // before it is scheduled and the player retains it.
    unsafe {
        let Some(buffer) = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
            AVAudioPCMBuffer::alloc(),
            &e.play_format,
            samples.len() as u32,
        ) else {
            shared.buffer_done(epoch);
            return;
        };
        let data = buffer.floatChannelData();
        if data.is_null() {
            shared.buffer_done(epoch);
            return;
        }
        std::ptr::copy_nonoverlapping(samples.as_ptr(), (*data).as_ptr(), samples.len());
        buffer.setFrameLength(samples.len() as u32);

        let done_shared = shared.clone();
        let done = RcBlock::new(move |_t: AVAudioPlayerNodeCompletionCallbackType| {
            done_shared.buffer_done(epoch);
        });
        e.player.scheduleBuffer_completionCallbackType_completionHandler(
            &buffer,
            AVAudioPlayerNodeCompletionCallbackType::DataPlayedBack,
            RcBlock::as_ptr(&done),
        );
        if !e.player.isPlaying() {
            e.player.play();
        }
    }
}

fn teardown(e: &Engine) {
    // SAFETY: objects owned by this thread.
    unsafe {
        e.engine.inputNode().removeTapOnBus(0);
        e.player.stop();
        e.engine.stop();
    }
}

// ---------------------------------------------------------------------------
// Devices (CoreAudio HAL)
// ---------------------------------------------------------------------------

fn prop_addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// Byte size of a property's value, `None` if absent.
fn prop_size(obj: AudioObjectID, addr: AudioObjectPropertyAddress) -> Option<u32> {
    let mut size = 0u32;
    // SAFETY: valid address and out pointer; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            obj,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    (status == 0).then_some(size)
}

/// A property holding one plain-old-data value or an array of them.
fn prop_pod_vec<T: Copy + Default>(obj: AudioObjectID, addr: AudioObjectPropertyAddress) -> Vec<T> {
    let Some(size) = prop_size(obj, addr) else {
        return Vec::new();
    };
    let n = size as usize / std::mem::size_of::<T>();
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![T::default(); n];
    let mut io_size = (n * std::mem::size_of::<T>()) as u32;
    // SAFETY: `out` has room for `io_size` bytes of `T`s.
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut io_size),
            NonNull::new_unchecked(out.as_mut_ptr().cast()),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    out.truncate(io_size as usize / std::mem::size_of::<T>());
    out
}

/// A CFString-valued property (UID, name).
fn prop_string(obj: AudioObjectID, addr: AudioObjectPropertyAddress) -> Option<String> {
    let mut cf: *mut NSString = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut NSString>() as u32;
    // SAFETY: the property is a CFStringRef, which we receive into a
    // pointer-sized slot. CFString is toll-free bridged to NSString, and the
    // HAL returns it +1 retained, which `Retained::from_raw` takes over.
    unsafe {
        let status = AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut cf).cast(),
        );
        if status != 0 {
            return None;
        }
        Retained::from_raw(cf).map(|s| s.to_string())
    }
}

/// Input-capable devices with their HAL ids.
fn input_devices_with_ids() -> Vec<(AudioObjectID, AudioDevice)> {
    let default_id = prop_pod_vec::<AudioObjectID>(
        SYSTEM_OBJECT,
        prop_addr(kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal),
    )
    .first()
    .copied();
    let ids = prop_pod_vec::<AudioObjectID>(
        SYSTEM_OBJECT,
        prop_addr(kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal),
    );
    ids.into_iter()
        .filter(|&id| {
            prop_size(id, prop_addr(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput))
                .is_some_and(|s| s > 0)
        })
        .filter_map(|id| {
            let uid = prop_string(
                id,
                prop_addr(kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal),
            )?;
            let name = prop_string(
                id,
                prop_addr(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal),
            )
            .unwrap_or_else(|| uid.clone());
            Some((
                id,
                AudioDevice {
                    uid,
                    name,
                    is_default: Some(id) == default_id,
                },
            ))
        })
        .collect()
}

pub fn list_input_devices() -> Vec<AudioDevice> {
    input_devices_with_ids().into_iter().map(|(_, d)| d).collect()
}

/// The default output device's kind: Bluetooth, or the built-in jack with
/// headphones plugged in, counts as headphones.
pub fn default_output_kind() -> super::OutputKind {
    use objc2_core_audio::{
        kAudioDevicePropertyDataSource, kAudioDevicePropertyTransportType,
        kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
        kAudioDeviceTransportTypeBuiltIn, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeOutput,
    };
    /// 'hdpn': the built-in output's headphone data source.
    const HEADPHONES_SOURCE: u32 = u32::from_be_bytes(*b"hdpn");
    let Some(id) = prop_pod_vec::<AudioObjectID>(
        SYSTEM_OBJECT,
        prop_addr(kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal),
    )
    .first()
    .copied() else {
        return super::OutputKind::Speakers;
    };
    let transport = prop_pod_vec::<u32>(id, prop_addr(kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal))
        .first()
        .copied()
        .unwrap_or(0);
    let source = || {
        prop_pod_vec::<u32>(id, prop_addr(kAudioDevicePropertyDataSource, kAudioObjectPropertyScopeOutput))
            .first()
            .copied()
    };
    output_kind(transport, kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE, kAudioDeviceTransportTypeBuiltIn, source, HEADPHONES_SOURCE)
}

fn output_kind(
    transport: u32,
    bt: u32,
    ble: u32,
    builtin: u32,
    source: impl FnOnce() -> Option<u32>,
    headphones_source: u32,
) -> super::OutputKind {
    if transport == bt || transport == ble || (transport == builtin && source() == Some(headphones_source)) {
        super::OutputKind::Headphones
    } else {
        super::OutputKind::Speakers
    }
}

// ---------------------------------------------------------------------------
// Permission
// ---------------------------------------------------------------------------

/// `AVAudioApplication` is macOS 14+.
fn has_audio_application() -> bool {
    AnyClass::get(c"AVAudioApplication").is_some()
}

pub fn mic_permission() -> MicPermission {
    if !has_audio_application() {
        return MicPermission::Undetermined;
    }
    // SAFETY: the class exists (checked above); recordPermission has no
    // preconditions.
    let p = unsafe { AVAudioApplication::sharedInstance().recordPermission() };
    map_permission(p)
}

fn map_permission(p: AVAudioApplicationRecordPermission) -> MicPermission {
    if p == AVAudioApplicationRecordPermission::Granted {
        MicPermission::Granted
    } else if p == AVAudioApplicationRecordPermission::Denied {
        MicPermission::Denied
    } else {
        MicPermission::Undetermined
    }
}

pub async fn request_mic_permission() -> MicPermission {
    if !has_audio_application() {
        return MicPermission::Undetermined;
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |granted: Bool| {
        if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
            let _ = tx.send(granted.as_bool());
        }
    });
    // SAFETY: the callee copies the block, which may run on any thread; it
    // only touches a Mutex-guarded oneshot sender.
    unsafe { AVAudioApplication::requestRecordPermissionWithCompletionHandler(&block) };
    drop(block);
    match rx.await {
        Ok(true) => MicPermission::Granted,
        Ok(false) => MicPermission::Denied,
        Err(_) => mic_permission(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::audio::dsp::f32_to_i16;

    #[test]
    fn playback_conversion_resamples_to_24k() {
        let pcm = vec![16384i16; 1600]; // 100 ms @ 16 kHz
        let out = to_playback_f32(&pcm, 16_000);
        assert_eq!(out.len(), 2400);
        assert!((out[10] - 0.5).abs() < 1e-6);
        assert_eq!(to_playback_f32(&pcm[..10], 24_000).len(), 10);
    }

    #[test]
    fn flush_invalidates_queue_and_stale_completions() {
        let s = Shared {
            mic_enabled: AtomicBool::new(true),
            queued: AtomicUsize::new(2),
            generation: AtomicU64::new(0),
            echo_cancelled: AtomicBool::new(false),
        };
        s.invalidate_queue();
        assert_eq!(s.queued.load(Ordering::Acquire), 0);
        s.queued.store(1, Ordering::Release);
        s.buffer_done(0); // stale generation: ignored
        assert_eq!(s.queued.load(Ordering::Acquire), 1);
        s.buffer_done(1);
        s.buffer_done(1); // never underflows
        assert_eq!(s.queued.load(Ordering::Acquire), 0);
    }

    #[test]
    fn restart_budget_caps_per_window() {
        let mut b = RestartBudget::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(b.take(t0));
        assert!(b.take(t0 + Duration::from_secs(1)));
        assert!(!b.take(t0 + Duration::from_secs(2)));
        assert!(b.take(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn bluetooth_and_headphone_jack_count_as_headphones() {
        use crate::voice::audio::OutputKind::*;
        let k = |t: u32, src: Option<u32>| output_kind(t, 1, 2, 3, || src, 9);
        assert_eq!(k(1, None), Headphones);
        assert_eq!(k(2, None), Headphones);
        assert_eq!(k(3, Some(9)), Headphones);
        assert_eq!(k(3, Some(4)), Speakers);
        assert_eq!(k(7, Some(9)), Speakers); // USB etc.: assume speakers
    }

    /// Lists input devices (CoreAudio query only; opens nothing).
    /// `cargo test --release list_input_devices_prints -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn list_input_devices_prints() {
        for d in list_input_devices() {
            println!("{d:?}");
        }
        println!("default output: {:?}", default_output_kind());
    }

    /// Manual smoke test: plays a 1 s 440 Hz tone and captures 3 s.
    /// `cargo test --release vpio_loopback_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn vpio_loopback_smoke() {
        println!("mic permission: {:?}", mic_permission());
        let mut io = VoiceProcessingIo::new().expect("new");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = std::time::Instant::now();
        io.start(tx).expect("start");
        println!(
            "started in {:?}, echo_cancelled={}",
            t0.elapsed(),
            io.echo_safe()
        );
        let tone: Vec<i16> = (0..PLAYBACK_RATE)
            .map(|i| {
                let t = i as f32 / PLAYBACK_RATE as f32;
                f32_to_i16(0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin())
            })
            .collect();
        for chunk in tone.chunks(2400) {
            io.play(chunk, PLAYBACK_RATE);
        }
        println!("is_playing after queue: {}", io.is_playing());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        println!("is_playing after 1.5 s: {}", io.is_playing());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        io.stop();
        let (mut frames, mut sum_sq, mut n) = (0usize, 0f64, 0usize);
        while let Ok(f) = rx.try_recv() {
            frames += 1;
            sum_sq += f.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
            n += f.len();
        }
        let rms = if n > 0 { (sum_sq / n as f64).sqrt() } else { 0.0 };
        println!(
            "captured {frames} frames ({n} samples, {:.2} s), RMS {rms:.1}",
            n as f64 / MIC_RATE as f64
        );
        io.stop(); // idempotent
    }
}

