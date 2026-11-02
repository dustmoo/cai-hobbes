//! The provider-neutral realtime voice contract.
//!
//! A [`RealtimeVoice`] opens a full-duplex speech session with a realtime
//! model (Gemini Live first). The session is three channels: mic audio in,
//! events out (audio, transcripts, tool calls, interruptions), and control
//! in (tool results, injected context, activity markers, close).
//!
//! Providers own their own reconnect/resumption: a session survives a
//! server `goAway` transparently and reports it as [`RealtimeEvent::Reconnected`].
//! The controller (`voice::controller`) never sees provider wire formats.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

/// Mic sample rate every provider receives (mono, 16-bit).
pub const INPUT_SAMPLE_RATE: u32 = 16_000;

/// A function the realtime model may call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments (`{"type":"object",...}`).
    pub parameters: Value,
    /// Long-running: the model keeps talking while it runs, and the result
    /// arrives later with a [`Scheduling`] hint. Providers without async
    /// tools treat this as a normal (blocking) call.
    pub non_blocking: bool,
}

/// How the model should handle a late tool result (async tools only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Interrupt / Silent: part of the contract, unused by the controller yet
pub enum Scheduling {
    /// Stop what it is saying and deal with the result now.
    Interrupt,
    /// Mention it at the next natural pause.
    WhenIdle,
    /// Absorb it without speaking.
    Silent,
}

/// Everything a provider needs to open a session.
#[derive(Clone, Debug, Default)]
pub struct RealtimeSetup {
    /// Provider model id, e.g. `gemini-3.8-live` (no `models/` prefix).
    pub model: String,
    pub system_instruction: String,
    /// Provider voice name (Gemini: "Kore", "Puck", ...). `None` = provider default.
    pub voice_name: Option<String>,
    /// BCP-47 language hint for speech output/transcription. `None` = auto.
    pub language: Option<String>,
    pub tools: Vec<ToolDecl>,
    /// Push-to-talk: the provider's own voice-activity detection is off and
    /// the controller marks speech with `ActivityStart` / `ActivityEnd`.
    pub manual_activity: bool,
}

/// Provider → controller.
#[derive(Clone, Debug, PartialEq)]
pub enum RealtimeEvent {
    /// Setup accepted; audio may flow.
    Ready,
    /// Model speech: mono 16-bit PCM at `sample_rate` (Gemini: 24 kHz).
    Audio { pcm: Vec<i16>, sample_rate: u32 },
    /// Transcript fragment of what the person said (may be partial; append).
    InputTranscript(String),
    /// Transcript fragment of what the model said (append).
    OutputTranscript(String),
    /// The person barged in: flush playback immediately.
    Interrupted,
    /// The model finished its turn.
    TurnComplete,
    ToolCall { id: String, name: String, args: Value },
    /// The model abandoned these in-flight calls (usually after a barge-in).
    ToolCallCancelled(Vec<String>),
    /// The provider reconnected (resumption) without losing the conversation.
    Reconnected,
    /// Non-fatal provider error worth logging / surfacing.
    Warning(String),
    /// The session is over and cannot be resumed. Final event.
    Closed { reason: String },
}

/// Controller → provider.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // ActivityStart / End: manual-activity providers
pub enum RealtimeControl {
    /// Mic audio: mono 16-bit PCM at [`INPUT_SAMPLE_RATE`].
    Audio(Vec<i16>),
    /// Push-to-talk pressed (manual activity only).
    ActivityStart,
    /// Push-to-talk released (manual activity only).
    ActivityEnd,
    /// The mic paused for a while (automatic activity only).
    AudioStreamEnd,
    ToolResponse {
        id: String,
        name: String,
        response: Value,
        /// Only meaningful for `non_blocking` tools.
        scheduling: Option<Scheduling>,
    },
    /// Add text to the conversation mid-session (a Heartbeat alert, a brain
    /// result that arrived outside a tool call). `respond` = the model should
    /// speak to it now; otherwise it is only absorbed as context.
    InjectContext { text: String, respond: bool },
    Close,
}

/// An open session.
pub struct RealtimeSession {
    pub control: mpsc::UnboundedSender<RealtimeControl>,
    pub events: mpsc::UnboundedReceiver<RealtimeEvent>,
}

#[async_trait]
pub trait RealtimeVoice: Send + Sync {
    /// Connect and complete setup (returns after `Ready` is queued, or an error).
    async fn connect(&self, setup: RealtimeSetup) -> Result<RealtimeSession, String>;
}
