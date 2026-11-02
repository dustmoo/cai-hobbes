//! Voice: a realtime speech layer that talks with the person and acts
//! through the Hobbes harness. See `VOICE_FEATURE_PLAN.md`.
//!
//! - [`realtime`]: the provider-neutral session contract.
//! - [`gemini_live`]: Gemini Live over WebSocket (the first provider).
//! - [`audio`]: mic + speaker with platform echo cancellation.
//! - [`controller`]: the live conversation (hotkey, barge-in, tools).
//! - [`brain`]: `VOICE_ASK_BRAIN` — a turn on the global connector.
//! - [`session`]: the dedicated Voice chat session and peer reads.
//! - [`tools`]: the voice tier's curated tools.
//! - [`hotkey`]: the global tap / hold hotkey.

pub mod audio;
pub mod brain;
pub mod controller;
pub mod gemini_live;
pub mod hotkey;
pub mod realtime;
pub mod session;
pub mod tools;

use crate::settings::Settings;

/// Single Pro-gating switch for voice (mirrors `HEARTBEAT_PRO_ONLY`).
pub const VOICE_PRO_ONLY: bool = true;

/// Whether voice can be offered at all in this build/licence.
pub fn voice_available() -> bool {
    !VOICE_PRO_ONLY || crate::entitlement::pro_active()
}

/// Whether voice is available and switched on.
pub fn voice_active(settings: &Settings) -> bool {
    voice_available() && settings.voice.enabled
}
