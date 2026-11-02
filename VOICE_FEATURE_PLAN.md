# Voice — a realtime mouth on the harness

**Status:** Design specification (pre-spike)
**Target:** `cai-hobbes` (Rust · Dioxus 0.6 desktop · Tokio · SQLite · MCP host)
**Author:** drafted with Dustin Moore, 2026-11-02
**Gating:** `VOICE_PRO_ONLY` (mirrors `HEARTBEAT_PRO_ONLY`, `src/heartbeat/mod.rs:58`), off by default

---

## 1. What this is

Voice is not "speech in front of chat". Transcribe → normal turn → read the
answer aloud is a dictation loop, and it does not feel like a conversation.

Voice is a **conversational orchestration layer**: a realtime speech model you
talk *with*. It holds the floor, takes turns, can be interrupted mid-sentence,
and it acts through the Hobbes harness via tool calls. It is the mouth and ears.
The **brain** is the global connector (`Settings.active_connector_id`). Anything
that needs real thought, or heavy tools, is delegated to it.

Voice sits beside the Heartbeat: the Heartbeat notices, Voice speaks and
listens. Each has its own session, and each can see the other.

### Decisions so far (2026-11-02)

| Decision | Choice |
|---|---|
| Architecture | Realtime speech-to-speech model as the conversational surface; harness tools for action; global connector as the brain |
| First provider | **Gemini Live** |
| Other providers | Supported through the same trait; a profile without a realtime model shows "Voice needs a realtime model for the best experience" |
| Configuration | Voice is a **role on a connector profile**, with its own selection (`voice_connector_id`) independent of the brain |
| Activation | Hotkey, configurable. **Tap = start/end a conversation. Hold = push-to-talk** |
| Session | A dedicated **Voice** session (metal mouth icon), separate from Heartbeat, with read and message access in both directions |
| Local STT/TTS | Not on the main path; possible later as an offline fallback |
| CUDA | Not needed; realtime audio is cloud, local audio work is light |

---

## 2. Architecture

```
            ┌──────────── you: full-duplex audio, interrupt anytime ────────────┐
            ▼                                                                    │
   Native audio I/O (macOS Voice Processing IO: echo cancel, AGC, NS)            │
            ▼                                                                    │
   RealtimeVoice session (Gemini Live first) ── owns turn-taking + barge-in      │
            │ tool calls                                   ▲ speech              │
            ▼                                              │                     │
   Harness: dispatch_builtin_tool + scope_gates            │                     │
     • VOICE_ASK_BRAIN(question)  → async turn on the global connector           │
     • fleet / planner reads, approve/deny gates, timers, dispatch               │
     • VOICE_PEER_READ / message the Heartbeat session                           │
            ▲                                                                    │
   Heartbeat: perception → salience → while Voice is live, wake-now = speak ─────┘
```

### 2.1 Two tiers

- **Voice tier (realtime model).** Fast and shallow. It answers what a tool read
  can answer ("what's blocked?"), confirms, and asks clarifying questions. It
  never runs long chains of tools itself.
- **Brain tier (global connector).** Claude, Gemini, or whatever `active_connector_id`
  points at. It runs a normal StreamManager turn in the Voice session, with the
  full prompt builder, MCP tools, skills and trust rules. It is invoked through
  `VOICE_ASK_BRAIN`.

### 2.2 Delegation is asynchronous

`VOICE_ASK_BRAIN` returns immediately with a handle ("on it"). The voice keeps
the conversation going. When the brain's turn completes, the result is injected
into the live realtime session as context and the voice speaks it at the next
natural gap. You can interrupt, redirect ("actually, just the trust-dispatch
one") or cancel (`cancel_stream`, `stream_manager.rs:2155`).

**Verify in the spike:** how each provider accepts a late tool result or
injected context without stalling audio. Gemini Live's support for non-blocking
function calls and client-content injection mid-session is the critical
assumption.

### 2.3 Where the brain's turns live

The Voice session is a real `Session`. It records:
- your utterances (the realtime model's input transcription) as user messages;
- the voice's spoken replies (output transcription) as assistant messages,
  marked as voice;
- each brain turn as a normal assistant turn with its tool calls.

The transcript is readable afterwards, and the brain sees the conversation so
far when it is asked something, because it is a normal turn in that session.

---

## 3. The Voice session and its link to the Heartbeat

- A dedicated session titled **"Voice"**, created on first use, like the
  Heartbeat's (`SESSION_TITLE` / `SESSION_EMOJI` in `src/heartbeat/runtime.rs:34-37`).
- **Icon: a polished-steel mouth.** A `MetalMouth` component next to `MetalHeart`
  (`src/components/metal_heart.rs`): same gradient and highlight treatment, a lips
  path. Drawn in the tab bar (`tab_bar.rs:293`) and the tab sidebar
  (`tab_sidebar.rs:295, :576`) in place of the session emoji, through a
  `use_voice_session_id()` memo mirroring `use_heartbeat_session_id()`.
- **Mutual access.** Generalise the dispatch relay (`src/todo/relay.rs`) from
  parent ↔ worker to a fixed **peer link** between Voice and Heartbeat, reusing
  its visible relay messages, journaling, rate limits and no-ping-pong hop guard.
  - Voice → Heartbeat: "watch this", "remind me when CI goes green", notes and
    commitments made in conversation.
  - Heartbeat → Voice: while a voice conversation is live, a salience `WakeNow`
    becomes a relay into Voice, which speaks it ("the trust-dispatch session is
    blocked on a permission"). When Voice is not live, nothing changes: the
    Heartbeat wakes its own session as today.
  - Read access: `HOBBES_PEER_READ` returns the other session's recent tail and,
    for the Heartbeat, its briefing and ledger tail (`heartbeat/briefing.rs`,
    `ledger.rs`). This is read-only and bounded.
- **Seeding.** A new voice conversation starts with the Heartbeat's current
  briefing, so it already knows the state of the world when you start talking.

---

## 4. Profiles: voice as a role

Profiles stay the unit of credentials and endpoints. Voice becomes a role a
profile can carry, chosen independently of the brain.

```rust
pub struct ProviderInstance {          // src/settings.rs:905
    pub id: String,
    pub name: String,
    pub config: ProviderInstanceConfig,
    #[serde(default = "default_roles")] // migration: existing profiles = chat only
    pub roles: ConnectorRoles,          // { chat: bool, voice: bool }
    #[serde(default)]
    pub voice: Option<VoiceProfile>,    // realtime model, voice name, language
}

// Settings
pub voice_connector_id: Option<String>, // independent of active_connector_id
```

- **Shared credentials.** A voice-enabled Gemini profile uses the same keychain
  key (`llm_api_key_<id>`) as chat. Nothing is duplicated.
- **Two pickers.** The chat-bar picker lists chat-role profiles (brain). The
  voice picker lists voice-role profiles. Switching one never touches the other.
- **Trying endpoints.** Add a profile (for example "OpenAI, voice only" or
  "Gemini via Vertex"), tick Voice, and switch the voice picker. A voice-only
  profile stays out of the brain picker.
- **Capability check.** `realtime_capable(instance)` per provider kind. Claude has
  no realtime API today, so a Claude profile cannot take the voice role (it can
  still be the brain). A non-capable selection shows: *"Voice needs a realtime
  model for the best experience. Gemini Live is supported today."*
- Voice profiles count toward `MAX_LLM_CONNECTORS` (10).

---

## 5. The provider trait

A new trait beside `LlmConnector` (`src/llm/mod.rs:204`), in `src/voice/realtime/`:

```rust
#[async_trait]
pub trait RealtimeVoice: Send + Sync {
    /// Open a session with system instructions, tool declarations and a seed briefing.
    async fn connect(&self, setup: RealtimeSetup) -> Result<RealtimeSession>;
}

pub struct RealtimeSession {
    pub audio_in:  mpsc::Sender<AudioFrame>,      // mic PCM (post echo-cancel)
    pub events:    mpsc::Receiver<RealtimeEvent>, // audio out, transcripts, tool calls,
                                                  // interrupted, turn complete, go-away
    pub control:   mpsc::Sender<RealtimeControl>, // tool results, injected context,
                                                  // interrupt, close
}
```

- **Gemini Live first:** WebSocket via `tokio-tungstenite`. 16 kHz PCM in,
  24 kHz out. Server-side voice activity detection and barge-in. Session
  resumption and context-window compression for conversations longer than the
  session cap. **Verify current session duration limits.**
- **OpenAI Realtime next:** same trait, for OpenAI-compatible profiles with a
  realtime endpoint.
- Tool calls from the realtime model go through `dispatch_builtin_tool`
  (`builtin_tools.rs:479`) and `scope_gates` (`builtin_tools.rs:271`), the same
  path as the Heartbeat session (P-015). No new call-site dispatch.

### 5.1 Voice tool surface

The realtime model gets a small, curated set; everything else goes through the brain.

| Tool | Purpose |
|---|---|
| `VOICE_ASK_BRAIN` | Async delegation to the global connector (§2.2) |
| `VOICE_CANCEL_BRAIN` | Cancel an in-flight brain turn |
| `HOBBES_FLEET_STATUS`, `HOBBES_TODO_LIST`, `HOBBES_PLAN_DAY` (read) | Quick answers without the brain |
| Gate approve / deny | "Approve that", same trust rules as clicking |
| `HOBBES_SET_TIMER` / `LIST` / `CANCEL` | Reminders made in conversation |
| `HOBBES_PEER_READ`, peer relay | Heartbeat access (§3) |

---

## 6. Audio I/O and activation

### 6.1 Native audio
- **macOS:** Voice Processing IO (`AVAudioEngine` with voice processing enabled)
  owns both mic and speaker, giving echo cancellation so you can talk over the
  voice on laptop speakers. Implemented as a small Swift static library behind a
  C ABI, or via `objc2-avf-audio` if that proves workable.
- **Windows (later):** `cpal` plus `webrtc-audio-processing`, or headphones-only.
- Playback flush on barge-in must be immediate (≤ 20 ms).

### 6.2 Permissions (blocker, fix first)
- Add `NSMicrophoneUsageDescription` to the Info.plist patches in
  `scripts/build_pro.sh` (around lines 110-131), `scripts/dev_launcher.sh:77` and
  the release path. **Without it macOS kills the app on first mic access.**
- `com.apple.security.device.audio-input` is already in `HobbesPro.entitlements`.
  Add it to `Hobbes.entitlements` too.
- A denied permission yields silent buffers. Detect it and deep-link to
  System Settings → Privacy & Security → Microphone.

### 6.3 Hotkey
- `global-hotkey` is already a dependency (`Cargo.toml:28`).
- **Default ⌥Space**, configurable in Settings → Voice.
  - **Tap** (released within ~300 ms): start the conversation, or end it if one is
    live. The mic stays open between turns; you can interrupt freely. It ends on
    a second tap or after an idle timeout (default 2 min of silence).
  - **Hold:** push-to-talk. The mic is open only while held; on release the
    utterance is committed, the voice answers, and the session closes after an
    idle timeout.
  - **Verify** that `global-hotkey` 0.5 reports press and release separately on
    macOS and Windows.
- A combination like ⌥Space needs no extra macOS permission. A bare modifier
  (Fn, or right ⌥ alone) needs Input Monitoring, so it is opt-in.

### 6.4 Visible state
- An always-visible indicator while the mic or the remote session is live, with
  states: listening · speaking · thinking (brain turn in flight) · paused.
- Audio is streamed to the voice provider only while the indicator is on.

---

## 7. Trust and privacy

- Every voice tool call goes through `scope_gates`, like the Heartbeat session.
- Destructive or outward-facing actions confirm by voice ("Send it?") and are
  logged in the Voice transcript.
- Heartbeat-triggered speech respects the Heartbeat's wake budget and a
  do-not-disturb toggle.
- Local-first ends at the first audio byte sent. Settings → Voice states which
  provider receives audio and links its data-retention terms.

---

## 8. Phases

1. **Spike (1–2 days).** `NSMicrophoneUsageDescription` fix; Gemini Live
   WebSocket from Rust with cpal I/O (headphones); one tool (`HOBBES_FLEET_STATUS`)
   and `VOICE_ASK_BRAIN`. Measure time to first audio, barge-in feel, session
   limits and async tool results.
2. **Foundation.** `ConnectorRoles` + `VoiceProfile` + `voice_connector_id` with
   migration; Settings → Voice; hotkey (tap + hold); the Voice session with
   `MetalMouth`; transcripts persisted; Pro gate.
3. **Conversation quality.** Voice Processing IO echo cancellation, so you can
   interrupt on speakers; idle timeouts; the visible-state indicator.
4. **Heartbeat link.** Generalised peer relay, `HOBBES_PEER_READ`, briefing
   seed, Heartbeat wake-now → speak while Voice is live.
5. **More providers.** OpenAI Realtime; voice picker polish.
6. **Later.** Windows audio; local STT/TTS offline fallback (Parakeet / Kokoro,
   from the 2026-11-02 research).

---

## 9. Open questions

- Gemini Live: non-blocking tool calls and mid-session context injection
  (decides how §2.2 is built); current session caps; pricing per minute with
  tool use.
- Does the brain's turn in the Voice session need a shorter, voice-shaped reply
  style, or does the voice tier always paraphrase what the brain returns?
- Should the voice speak brain results verbatim, summarise them, or only say
  "it's in the Voice tab" for long or code-heavy answers?
- Peer relay: does the Heartbeat's wake budget also cap relays *into* Voice?
