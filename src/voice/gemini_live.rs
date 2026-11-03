//! Gemini Live (Developer API, v1beta) over WebSocket.
//!
//! Wire building ([`setup_json`], [`control_json`]) and parsing
//! ([`parse_server_message`]) are pure functions; [`pump`] is thin glue that
//! also owns transparent session resumption (goAway / unexpected drop).
//! The API key rides in the URL, so the URL is never logged and errors are
//! scrubbed with [`scrub`].

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::realtime::{
    RealtimeControl, RealtimeEvent, RealtimeSession, RealtimeSetup, RealtimeVoice, Scheduling,
    INPUT_SAMPLE_RATE,
};
use crate::log_utils::{net_err, redact_url, truncate_for_log};

const WS_BASE: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000";
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFFS_MS: [u64; 3] = [500, 1000, 2000];
/// ~2 s of mic audio buffered during a reconnect.
const MAX_BUFFERED_AUDIO_SAMPLES: usize = 2 * INPUT_SAMPLE_RATE as usize;
const MAX_BUFFERED_CONTROLS: usize = 256;
const DEFAULT_OUTPUT_RATE: u32 = 24_000;

type Conn = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct GeminiLive {
    api_key: String,
}

impl GeminiLive {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

/// List model ids (no `models/` prefix) that support `bidiGenerateContent`.
pub async fn list_live_models(api_key: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut req = client.get(MODELS_URL).header("x-goog-api-key", api_key);
        if let Some(t) = &page {
            req = req.query(&[("pageToken", t.as_str())]);
        }
        let resp = req.send().await.map_err(net_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("model list failed: HTTP {status}"));
        }
        let body: Value = resp.json().await.map_err(net_err)?;
        out.extend(live_models_from_page(&body));
        match body.get("nextPageToken").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => page = Some(t.to_string()),
            _ => break,
        }
    }
    Ok(out)
}

fn live_models_from_page(body: &Value) -> Vec<String> {
    body.get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter(|m| {
                    m.get("supportedGenerationMethods")
                        .and_then(Value::as_array)
                        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some("bidiGenerateContent")))
                })
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Pure message building
// ---------------------------------------------------------------------------

/// Build the `setup` message. `resume_handle` = `None` on first connect.
#[cfg_attr(not(test), allow(dead_code))]
pub fn setup_json(setup: &RealtimeSetup, resume_handle: Option<&str>) -> Value {
    setup_json_with(setup, resume_handle, false)
}

/// As [`setup_json`]; `modalities_top_level` puts `responseModalities`
/// directly under `setup` (the quickstart placement) instead of
/// `generationConfig`.
fn setup_json_with(setup: &RealtimeSetup, resume_handle: Option<&str>, modalities_top_level: bool) -> Value {
    let mut s = Map::new();
    s.insert("model".into(), json!(format!("models/{}", setup.model)));

    let mut gencfg = Map::new();
    if !modalities_top_level {
        gencfg.insert("responseModalities".into(), json!(["AUDIO"]));
    }
    let mut speech = Map::new();
    if let Some(v) = &setup.voice_name {
        speech.insert(
            "voiceConfig".into(),
            json!({"prebuiltVoiceConfig": {"voiceName": v}}),
        );
    }
    if let Some(l) = &setup.language {
        speech.insert("languageCode".into(), json!(l));
    }
    if !speech.is_empty() {
        gencfg.insert("speechConfig".into(), Value::Object(speech));
    }
    if modalities_top_level {
        s.insert("responseModalities".into(), json!(["AUDIO"]));
    }
    if !gencfg.is_empty() {
        s.insert("generationConfig".into(), Value::Object(gencfg));
    }

    s.insert(
        "systemInstruction".into(),
        json!({"parts": [{"text": setup.system_instruction}]}),
    );

    if !setup.tools.is_empty() {
        let decls: Vec<Value> = setup
            .tools
            .iter()
            .map(|t| {
                let mut d = Map::new();
                d.insert("name".into(), json!(t.name));
                d.insert("description".into(), json!(t.description));
                d.insert("parameters".into(), t.parameters.clone());
                if t.non_blocking {
                    d.insert("behavior".into(), json!("NON_BLOCKING"));
                }
                Value::Object(d)
            })
            .collect();
        s.insert("tools".into(), json!([{"functionDeclarations": decls}]));
    }

    if setup.manual_activity {
        s.insert(
            "realtimeInputConfig".into(),
            json!({"automaticActivityDetection": {"disabled": true}}),
        );
    }
    s.insert("inputAudioTranscription".into(), json!({}));
    s.insert("outputAudioTranscription".into(), json!({}));
    s.insert(
        "sessionResumption".into(),
        match resume_handle {
            Some(h) => json!({"handle": h}),
            None => json!({}),
        },
    );
    s.insert("contextWindowCompression".into(), json!({"slidingWindow": {}}));
    json!({"setup": Value::Object(s)})
}

/// Little-endian i16 PCM -> base64.
pub fn encode_pcm(pcm: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    B64.encode(bytes)
}

/// base64 little-endian i16 PCM -> samples. `None` on invalid base64.
pub fn decode_pcm(data: &str) -> Option<Vec<i16>> {
    let bytes = B64.decode(data).ok()?;
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// Wire message for a control, or `None` for `Close` (not a wire message).
pub fn control_json(c: &RealtimeControl) -> Option<Value> {
    Some(match c {
        RealtimeControl::Audio(pcm) => json!({"realtimeInput": {"audio": {
            "data": encode_pcm(pcm),
            "mimeType": format!("audio/pcm;rate={INPUT_SAMPLE_RATE}"),
        }}}),
        RealtimeControl::ActivityStart => json!({"realtimeInput": {"activityStart": {}}}),
        RealtimeControl::ActivityEnd => json!({"realtimeInput": {"activityEnd": {}}}),
        RealtimeControl::AudioStreamEnd => json!({"realtimeInput": {"audioStreamEnd": true}}),
        RealtimeControl::ToolResponse { id, name, response, scheduling, will_continue } => {
            let mut obj = match response {
                Value::Object(m) => m.clone(),
                other => {
                    let mut m = Map::new();
                    m.insert("result".into(), other.clone());
                    m
                }
            };
            if let Some(s) = scheduling {
                let s = match s {
                    Scheduling::Interrupt => "INTERRUPT",
                    Scheduling::WhenIdle => "WHEN_IDLE",
                    Scheduling::Silent => "SILENT",
                };
                obj.insert("scheduling".into(), json!(s));
            }
            let mut fr = json!({"id": id, "name": name, "response": Value::Object(obj)});
            // A FunctionResponse field (not inside `response`); only sent on
            // interim responses so final ones keep their proven shape.
            if *will_continue {
                fr["willContinue"] = json!(true);
            }
            json!({"toolResponse": {"functionResponses": [fr]}})
        }
        RealtimeControl::InjectContext { text, respond } => json!({"clientContent": {
            "turns": [{"role": "user", "parts": [{"text": text}]}],
            "turnComplete": respond,
        }}),
        RealtimeControl::Close => return None,
    })
}

// ---------------------------------------------------------------------------
// Pure parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
pub struct ParsedServerMessage {
    pub events: Vec<RealtimeEvent>,
    /// New resumption handle (only when resumable and non-empty).
    pub handle: Option<String>,
    pub go_away: bool,
    pub setup_complete: bool,
    /// Top-level server `error` payload, if any.
    pub error: Option<String>,
}

/// Text of a text or binary frame (both carry UTF-8 JSON).
pub fn frame_text(m: &Message) -> Option<String> {
    match m {
        Message::Text(t) => Some(t.clone()),
        Message::Binary(b) => String::from_utf8(b.clone()).ok(),
        _ => None,
    }
}

fn rate_from_mime(mime: &str) -> u32 {
    mime.split(';')
        .filter_map(|p| p.trim().strip_prefix("rate="))
        .filter_map(|r| r.trim().parse().ok())
        .next()
        .unwrap_or(DEFAULT_OUTPUT_RATE)
}

pub fn parse_server_message(text: &str) -> ParsedServerMessage {
    let mut out = ParsedServerMessage::default();
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            tracing::debug!("voice: gemini live: unparseable frame");
            return out;
        }
    };
    let Some(obj) = v.as_object() else { return out };

    if obj.contains_key("setupComplete") {
        out.setup_complete = true;
    }
    if let Some(e) = obj.get("error") {
        out.error = Some(
            e.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string()),
        );
    }

    if let Some(sc) = obj.get("serverContent") {
        let mut audio = Vec::new();
        if let Some(parts) = sc.pointer("/modelTurn/parts").and_then(Value::as_array) {
            for p in parts {
                let Some(inline) = p.get("inlineData") else { continue };
                let (Some(mime), Some(data)) = (
                    inline.get("mimeType").and_then(Value::as_str),
                    inline.get("data").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if !mime.starts_with("audio/") {
                    continue;
                }
                match decode_pcm(data) {
                    Some(pcm) => audio.push(RealtimeEvent::Audio { pcm, sample_rate: rate_from_mime(mime) }),
                    None => out.events.push(RealtimeEvent::Warning("undecodable audio chunk".into())),
                }
            }
        }
        out.events.extend(audio);
        if let Some(t) = sc.pointer("/inputTranscription/text").and_then(Value::as_str) {
            out.events.push(RealtimeEvent::InputTranscript(t.to_string()));
        }
        if let Some(t) = sc.pointer("/outputTranscription/text").and_then(Value::as_str) {
            out.events.push(RealtimeEvent::OutputTranscript(t.to_string()));
        }
        if sc.get("interrupted").and_then(Value::as_bool) == Some(true) {
            out.events.push(RealtimeEvent::Interrupted);
        }
        if sc.get("turnComplete").and_then(Value::as_bool) == Some(true) {
            out.events.push(RealtimeEvent::TurnComplete);
        }
    }

    if let Some(calls) = v.pointer("/toolCall/functionCalls").and_then(Value::as_array) {
        for c in calls {
            let (Some(id), Some(name)) = (
                c.get("id").and_then(Value::as_str),
                c.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            out.events.push(RealtimeEvent::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args: c.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    if let Some(ids) = v.pointer("/toolCallCancellation/ids").and_then(Value::as_array) {
        let ids: Vec<String> = ids.iter().filter_map(|i| i.as_str().map(str::to_string)).collect();
        out.events.push(RealtimeEvent::ToolCallCancelled(ids));
    }
    if let Some(u) = obj.get("sessionResumptionUpdate") {
        let resumable = u.get("resumable").and_then(Value::as_bool).unwrap_or(false);
        if let Some(h) = u.get("newHandle").and_then(Value::as_str) {
            if resumable && !h.is_empty() {
                out.handle = Some(h.to_string());
            }
        }
    }
    if obj.contains_key("goAway") {
        out.go_away = true;
    }
    out
}

fn message_kinds(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(",")))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Connection glue
// ---------------------------------------------------------------------------

/// Remove anything key-like from an error string before it is logged/returned.
fn scrub(key: &str, s: &str) -> String {
    let s = if key.is_empty() { s.to_string() } else { s.replace(key, "<key>") };
    truncate_for_log(&s, 300)
}

fn ws_url(key: &str) -> String {
    format!("{WS_BASE}?key={}", urlencoding::encode(key))
}

async fn send_json(ws: &mut Conn, v: &Value) -> Result<(), String> {
    ws.send(Message::Text(v.to_string())).await.map_err(|e| e.to_string())
}

/// Open the socket, send setup, wait for `setupComplete`.
async fn open(
    key: &str,
    setup: &RealtimeSetup,
    handle: Option<&str>,
    modalities_top_level: bool,
) -> Result<Conn, String> {
    let work = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(key).as_str())
            .await
            .map_err(|e| format!("connect failed: {e}"))?;
        send_json(&mut ws, &setup_json_with(setup, handle, modalities_top_level))
            .await
            .map_err(|e| format!("setup send failed: {e}"))?;
        loop {
            match ws.next().await {
                None => return Err("socket closed before setupComplete".to_string()),
                Some(Err(e)) => return Err(format!("socket error before setupComplete: {e}")),
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                    return Err(format!("server closed during setup: {reason}"));
                }
                Some(Ok(m)) => {
                    let Some(text) = frame_text(&m) else { continue };
                    let p = parse_server_message(&text);
                    if let Some(e) = p.error {
                        return Err(format!("server error during setup: {e}"));
                    }
                    if p.setup_complete {
                        return Ok(ws);
                    }
                }
            }
        }
    };
    match tokio::time::timeout(SETUP_TIMEOUT, work).await {
        Ok(r) => r.map_err(|e| scrub(key, &e)),
        Err(_) => Err("timed out waiting for setupComplete".to_string()),
    }
}

fn is_modalities_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("responsemodalities") || l.contains("response_modalities")
}

#[async_trait]
impl RealtimeVoice for GeminiLive {
    async fn connect(&self, setup: RealtimeSetup) -> Result<RealtimeSession, String> {
        tracing::debug!("voice: gemini live connecting to {}", redact_url(WS_BASE));
        let mut top_level = false;
        let ws = match open(&self.api_key, &setup, None, false).await {
            Ok(ws) => ws,
            Err(e) if is_modalities_error(&e) => {
                let ws = open(&self.api_key, &setup, None, true).await?;
                tracing::warn!("voice: gemini live: responseModalities accepted only at top level of setup (generationConfig placement was rejected)");
                top_level = true;
                ws
            }
            Err(e) => return Err(e),
        };
        tracing::info!("voice: gemini live connected (model {})", setup.model);

        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let _ = ev_tx.send(RealtimeEvent::Ready);
        let key = self.api_key.clone();
        tokio::spawn(async move {
            let reason = pump(ws, key, setup, top_level, ctl_rx, &ev_tx).await;
            tracing::info!("voice: gemini live closed: {}", truncate_for_log(&reason, 200));
            let _ = ev_tx.send(RealtimeEvent::Closed { reason });
        });
        Ok(RealtimeSession { control: ctl_tx, events: ev_rx })
    }
}

/// Await `fut` while buffering controls. `Err(())` = caller closed/dropped.
async fn drive<F: Future>(
    fut: F,
    rx: &mut mpsc::UnboundedReceiver<RealtimeControl>,
    buf: &mut ControlBuffer,
) -> Result<F::Output, ()> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return Ok(r),
            c = rx.recv() => match c {
                None | Some(RealtimeControl::Close) => return Err(()),
                Some(c) => buf.push(c),
            },
        }
    }
}

/// Bounded control buffer: audio beyond ~2 s is dropped.
#[derive(Default)]
struct ControlBuffer {
    items: VecDeque<RealtimeControl>,
    audio_samples: usize,
}

impl ControlBuffer {
    fn push(&mut self, c: RealtimeControl) {
        if let RealtimeControl::Audio(pcm) = &c {
            if self.audio_samples + pcm.len() > MAX_BUFFERED_AUDIO_SAMPLES {
                return;
            }
            self.audio_samples += pcm.len();
        }
        if self.items.len() < MAX_BUFFERED_CONTROLS {
            self.items.push_back(c);
        }
    }
}

enum Reconnect {
    Ok(Box<Conn>),
    Failed(String),
    CallerClosed,
}

async fn reconnect(
    key: &str,
    setup: &RealtimeSetup,
    handle: &str,
    top_level: bool,
    rx: &mut mpsc::UnboundedReceiver<RealtimeControl>,
    buf: &mut ControlBuffer,
) -> Reconnect {
    let mut last = String::new();
    for ms in BACKOFFS_MS {
        if drive(tokio::time::sleep(Duration::from_millis(ms)), rx, buf).await.is_err() {
            return Reconnect::CallerClosed;
        }
        match drive(open(key, setup, Some(handle), top_level), rx, buf).await {
            Err(()) => return Reconnect::CallerClosed,
            Ok(Ok(ws)) => return Reconnect::Ok(Box::new(ws)),
            Ok(Err(e)) => {
                tracing::debug!("voice: gemini live reconnect attempt failed: {e}");
                last = e;
            }
        }
    }
    Reconnect::Failed(last)
}

fn emit(tx: &mpsc::UnboundedSender<RealtimeEvent>, e: RealtimeEvent) -> bool {
    tx.send(e).is_ok()
}

async fn pump(
    mut ws: Conn,
    key: String,
    setup: RealtimeSetup,
    top_level: bool,
    mut rx: mpsc::UnboundedReceiver<RealtimeControl>,
    tx: &mpsc::UnboundedSender<RealtimeEvent>,
) -> String {
    let mut handle: Option<String> = None;
    let mut buf = ControlBuffer::default();
    loop {
        let next: Option<&'static str> = tokio::select! {
            c = rx.recv() => match c {
                None | Some(RealtimeControl::Close) => {
                    let _ = ws.send(Message::Close(None)).await;
                    return "closed".to_string();
                }
                Some(c) => match control_json(&c) {
                    Some(v) => match send_json(&mut ws, &v).await {
                        Ok(()) => None,
                        Err(_) => Some("send failed"),
                    },
                    None => None,
                },
            },
            m = ws.next() => match m {
                None => Some("socket closed"),
                Some(Err(e)) => Some(if matches!(e, tokio_tungstenite::tungstenite::Error::ConnectionClosed) { "socket closed" } else { "socket error" }),
                Some(Ok(Message::Close(frame))) => {
                    let r = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                    tracing::debug!("voice: gemini live close frame: {}", truncate_for_log(&r, 200));
                    Some("server closed")
                }
                Some(Ok(m)) => match frame_text(&m) {
                    None => None,
                    Some(text) => {
                        tracing::debug!("voice: gemini live message: {}", message_kinds(&text));
                        let p = parse_server_message(&text);
                        if let Some(h) = p.handle {
                            handle = Some(h);
                        }
                        if let Some(e) = p.error {
                            let _ = emit(tx, RealtimeEvent::Warning(scrub(&key, &e)));
                        }
                        for e in p.events {
                            if !emit(tx, e) {
                                return "event receiver dropped".to_string();
                            }
                        }
                        if p.go_away { Some("goAway") } else { None }
                    }
                },
            },
        };

        let Some(why) = next else { continue };
        let Some(h) = handle.clone() else {
            return format!("{why} (no resumption handle)");
        };
        tracing::debug!("voice: gemini live reconnecting ({why})");
        match reconnect(&key, &setup, &h, top_level, &mut rx, &mut buf).await {
            Reconnect::CallerClosed => return "closed".to_string(),
            Reconnect::Failed(e) => return format!("reconnect failed after {why}: {e}"),
            Reconnect::Ok(new_ws) => {
                ws = *new_ws;
                tracing::info!("voice: gemini live reconnected");
                let pending: Vec<RealtimeControl> = buf.items.drain(..).collect();
                buf.audio_samples = 0;
                for c in pending {
                    if let Some(v) = control_json(&c) {
                        if send_json(&mut ws, &v).await.is_err() {
                            break; // next loop iteration will see the dead socket
                        }
                    }
                }
                if !emit(tx, RealtimeEvent::Reconnected) {
                    return "event receiver dropped".to_string();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::realtime::ToolDecl;

    fn base() -> RealtimeSetup {
        RealtimeSetup {
            model: "gemini-live-x".into(),
            system_instruction: "be brief".into(),
            ..Default::default()
        }
    }

    fn tool(name: &str, nb: bool) -> ToolDecl {
        ToolDecl {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            non_blocking: nb,
        }
    }

    #[test]
    fn setup_minimal() {
        let v = setup_json(&base(), None);
        let s = &v["setup"];
        assert_eq!(s["model"], "models/gemini-live-x");
        assert_eq!(s["generationConfig"]["responseModalities"], json!(["AUDIO"]));
        assert!(s["generationConfig"].get("speechConfig").is_none());
        assert_eq!(s["systemInstruction"]["parts"][0]["text"], "be brief");
        assert!(s.get("tools").is_none());
        assert!(s.get("realtimeInputConfig").is_none());
        assert_eq!(s["sessionResumption"], json!({}));
        assert_eq!(s["contextWindowCompression"], json!({"slidingWindow": {}}));
        assert_eq!(s["inputAudioTranscription"], json!({}));
        assert_eq!(s["outputAudioTranscription"], json!({}));
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[test]
    fn setup_tools_behavior() {
        let mut st = base();
        st.tools = vec![tool("a", true), tool("b", false)];
        let v = setup_json(&st, None);
        let d = &v["setup"]["tools"][0]["functionDeclarations"];
        assert_eq!(d[0]["behavior"], "NON_BLOCKING");
        assert!(d[1].get("behavior").is_none());
        assert_eq!(d[1]["parameters"], json!({"type": "object"}));
    }

    #[test]
    fn setup_manual_voice_language_resume() {
        let mut st = base();
        st.manual_activity = true;
        st.voice_name = Some("Puck".into());
        st.language = Some("en-US".into());
        let v = setup_json(&st, Some("h1"));
        let s = &v["setup"];
        assert_eq!(s["realtimeInputConfig"]["automaticActivityDetection"]["disabled"], true);
        let sp = &s["generationConfig"]["speechConfig"];
        assert_eq!(sp["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"], "Puck");
        assert_eq!(sp["languageCode"], "en-US");
        assert_eq!(s["sessionResumption"], json!({"handle": "h1"}));
    }

    #[test]
    fn setup_language_only_and_top_level_modalities() {
        let mut st = base();
        st.language = Some("fr-FR".into());
        let v = setup_json_with(&st, None, true);
        let s = &v["setup"];
        assert_eq!(s["responseModalities"], json!(["AUDIO"]));
        assert!(s["generationConfig"].get("responseModalities").is_none());
        assert!(s["generationConfig"]["speechConfig"].get("voiceConfig").is_none());
        assert_eq!(s["generationConfig"]["speechConfig"]["languageCode"], "fr-FR");
        // No speech parts and top-level: no empty generationConfig.
        let v = setup_json_with(&base(), None, true);
        assert!(v["setup"].get("generationConfig").is_none());
    }

    #[test]
    fn audio_round_trip() {
        let pcm = vec![0i16, 1, -1, i16::MAX, i16::MIN, 12345];
        assert_eq!(decode_pcm(&encode_pcm(&pcm)).unwrap(), pcm);
        assert!(decode_pcm("!!!").is_none());
        let v = control_json(&RealtimeControl::Audio(vec![1, 2])).unwrap();
        let a = &v["realtimeInput"]["audio"];
        assert_eq!(a["mimeType"], "audio/pcm;rate=16000");
        assert_eq!(B64.decode(a["data"].as_str().unwrap()).unwrap(), vec![1, 0, 2, 0]);
    }

    #[test]
    fn activity_controls() {
        assert_eq!(control_json(&RealtimeControl::ActivityStart).unwrap(), json!({"realtimeInput": {"activityStart": {}}}));
        assert_eq!(control_json(&RealtimeControl::ActivityEnd).unwrap(), json!({"realtimeInput": {"activityEnd": {}}}));
        assert_eq!(control_json(&RealtimeControl::AudioStreamEnd).unwrap(), json!({"realtimeInput": {"audioStreamEnd": true}}));
        assert!(control_json(&RealtimeControl::Close).is_none());
    }

    #[test]
    fn interim_tool_responses_continue_at_the_function_response_level() {
        let v = control_json(&RealtimeControl::ToolResponse {
            id: "c1".into(),
            name: "VOICE_ASK_BRAIN".into(),
            response: json!({"status": "working"}),
            scheduling: Some(Scheduling::WhenIdle),
            will_continue: true,
        })
        .unwrap();
        let fr = &v["toolResponse"]["functionResponses"][0];
        assert_eq!(fr["willContinue"], true);
        assert_eq!(fr["response"]["status"], "working");
        assert_eq!(fr["response"]["scheduling"], "WHEN_IDLE");
        // Final responses keep their shape (no willContinue at all).
        let v = control_json(&RealtimeControl::ToolResponse {
            id: "c1".into(),
            name: "VOICE_ASK_BRAIN".into(),
            response: json!({"result": "done"}),
            scheduling: Some(Scheduling::WhenIdle),
            will_continue: false,
        })
        .unwrap();
        assert!(v["toolResponse"]["functionResponses"][0].get("willContinue").is_none());
    }

    #[test]
    fn tool_response_scheduling_and_wrap() {
        let v = control_json(&RealtimeControl::ToolResponse { will_continue: false,
            id: "i".into(),
            name: "n".into(),
            response: json!({"ok": true}),
            scheduling: Some(Scheduling::WhenIdle),
        })
        .unwrap();
        let r = &v["toolResponse"]["functionResponses"][0];
        assert_eq!(r["id"], "i");
        assert_eq!(r["name"], "n");
        assert_eq!(r["response"], json!({"ok": true, "scheduling": "WHEN_IDLE"}));

        let v = control_json(&RealtimeControl::ToolResponse { will_continue: false,
            id: "i".into(),
            name: "n".into(),
            response: json!("done"),
            scheduling: None,
        })
        .unwrap();
        assert_eq!(v["toolResponse"]["functionResponses"][0]["response"], json!({"result": "done"}));

        for (s, w) in [(Scheduling::Interrupt, "INTERRUPT"), (Scheduling::Silent, "SILENT")] {
            let v = control_json(&RealtimeControl::ToolResponse { will_continue: false,
                id: "i".into(),
                name: "n".into(),
                response: json!(5),
                scheduling: Some(s),
            })
            .unwrap();
            let r = &v["toolResponse"]["functionResponses"][0]["response"];
            assert_eq!(r["scheduling"], w);
            assert_eq!(r["result"], 5);
        }
    }

    #[test]
    fn inject_context() {
        let v = control_json(&RealtimeControl::InjectContext { text: "hi".into(), respond: true }).unwrap();
        assert_eq!(v["clientContent"]["turns"][0]["role"], "user");
        assert_eq!(v["clientContent"]["turns"][0]["parts"][0]["text"], "hi");
        assert_eq!(v["clientContent"]["turnComplete"], true);
        let v = control_json(&RealtimeControl::InjectContext { text: "hi".into(), respond: false }).unwrap();
        assert_eq!(v["clientContent"]["turnComplete"], false);
    }

    #[test]
    fn parse_multi_part_server_content() {
        let a = encode_pcm(&[1, 2, 3]);
        let b = encode_pcm(&[4]);
        let msg = json!({"serverContent": {
            "modelTurn": {"parts": [
                {"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": a}},
                {"inlineData": {"mimeType": "audio/pcm", "data": b}},
            ]},
            "turnComplete": true,
            "interrupted": true,
            "outputTranscription": {"text": "out"},
            "inputTranscription": {"text": "in"},
        }, "usageMetadata": {"totalTokenCount": 3}})
        .to_string();
        let p = parse_server_message(&msg);
        assert_eq!(
            p.events,
            vec![
                RealtimeEvent::Audio { pcm: vec![1, 2, 3], sample_rate: 24000 },
                RealtimeEvent::Audio { pcm: vec![4], sample_rate: 24000 },
                RealtimeEvent::InputTranscript("in".into()),
                RealtimeEvent::OutputTranscript("out".into()),
                RealtimeEvent::Interrupted,
                RealtimeEvent::TurnComplete,
            ]
        );
        let m = json!({"serverContent": {"modelTurn": {"parts": [{"inlineData": {"mimeType": "audio/pcm;rate=16000", "data": a}}]}}}).to_string();
        assert!(matches!(parse_server_message(&m).events[0], RealtimeEvent::Audio { sample_rate: 16000, .. }));
    }

    #[test]
    fn parse_tool_call_and_cancel() {
        let p = parse_server_message(
            &json!({"toolCall": {"functionCalls": [
                {"id": "1", "name": "a", "args": {"x": 1}},
                {"id": "2", "name": "b"},
            ]}})
            .to_string(),
        );
        assert_eq!(
            p.events,
            vec![
                RealtimeEvent::ToolCall { id: "1".into(), name: "a".into(), args: json!({"x": 1}) },
                RealtimeEvent::ToolCall { id: "2".into(), name: "b".into(), args: json!({}) },
            ]
        );
        let p = parse_server_message(&json!({"toolCallCancellation": {"ids": ["1", "2"]}}).to_string());
        assert_eq!(p.events, vec![RealtimeEvent::ToolCallCancelled(vec!["1".into(), "2".into()])]);
    }

    #[test]
    fn parse_resumption_goaway_setup_unknown() {
        let p = parse_server_message(&json!({"sessionResumptionUpdate": {"newHandle": "H", "resumable": true}}).to_string());
        assert_eq!(p.handle.as_deref(), Some("H"));
        let p = parse_server_message(&json!({"sessionResumptionUpdate": {"newHandle": "H", "resumable": false}}).to_string());
        assert!(p.handle.is_none());
        let p = parse_server_message(&json!({"sessionResumptionUpdate": {"newHandle": "", "resumable": true}}).to_string());
        assert!(p.handle.is_none());
        let p = parse_server_message(&json!({"goAway": {"timeLeft": "50s"}}).to_string());
        assert!(p.go_away && p.events.is_empty());
        let p = parse_server_message("{\"setupComplete\":{}}");
        assert!(p.setup_complete);
        let p = parse_server_message("{\"mystery\":1}");
        assert_eq!(p, ParsedServerMessage::default());
        assert_eq!(parse_server_message("not json"), ParsedServerMessage::default());
    }

    #[test]
    fn text_and_binary_frames() {
        let s = "{\"setupComplete\":{}}";
        assert_eq!(frame_text(&Message::Text(s.into())).as_deref(), Some(s));
        assert_eq!(frame_text(&Message::Binary(s.as_bytes().to_vec())).as_deref(), Some(s));
        assert!(frame_text(&Message::Binary(vec![0xff, 0xfe])).is_none());
        assert!(frame_text(&Message::Ping(vec![])).is_none());
    }

    #[test]
    fn models_filter() {
        let body = json!({"models": [
            {"name": "models/a", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/b-live", "supportedGenerationMethods": ["bidiGenerateContent"]},
        ]});
        assert_eq!(live_models_from_page(&body), vec!["b-live".to_string()]);
    }

    #[test]
    fn buffer_bounds_audio_and_scrub_hides_key() {
        let mut b = ControlBuffer::default();
        for _ in 0..10 {
            b.push(RealtimeControl::Audio(vec![0; 8000]));
        }
        b.push(RealtimeControl::ActivityEnd);
        assert_eq!(b.items.len(), 5);
        assert_eq!(scrub("SECRET", "bad url ?key=SECRET"), "bad url ?key=<key>");
        assert!(is_modalities_error("Unknown name \"responseModalities\" at setup"));
    }

    /// Live protocol check against the real API (no mic / speaker):
    /// `GEMINI_API_KEY=… cargo test --release gemini_live_smoke -- --ignored --nocapture`
    /// Optional `GEMINI_LIVE_MODEL` (default gemini-3.8-live).
    #[tokio::test]
    #[ignore]
    async fn gemini_live_smoke() {
        let key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY");
        let models = list_live_models(&key).await.expect("models.list");
        println!("live models: {models:?}");
        let model = std::env::var("GEMINI_LIVE_MODEL").unwrap_or_else(|_| "gemini-3.8-live".into());
        let setup = RealtimeSetup {
            model,
            system_instruction: "You are a test. When asked for the weather, call get_weather; otherwise answer in one short sentence.".into(),
            voice_name: Some("Kore".into()),
            language: None,
            tools: vec![ToolDecl {
                name: "get_weather".into(),
                description: "Weather for a city.".into(),
                parameters: serde_json::json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
                non_blocking: true,
            }],
            manual_activity: false,
        };
        let mut s = GeminiLive::new(key).connect(setup).await.expect("connect");
        let deadline = std::time::Duration::from_secs(30);
        // 1. Spoken reply to injected text.
        s.control.send(RealtimeControl::InjectContext { text: "Say hello in five words.".into(), respond: true }).unwrap();
        let (mut audio_samples, mut transcript) = (0usize, String::new());
        loop {
            match tokio::time::timeout(deadline, s.events.recv()).await.expect("timed out").expect("closed") {
                RealtimeEvent::Audio { pcm, sample_rate } => { audio_samples += pcm.len(); assert!(sample_rate >= 16_000); }
                RealtimeEvent::OutputTranscript(t) => transcript.push_str(&t),
                RealtimeEvent::TurnComplete => break,
                RealtimeEvent::Closed { reason } => panic!("closed: {reason}"),
                other => println!("event: {other:?}"),
            }
        }
        println!("hello: {audio_samples} samples, transcript {transcript:?}");
        assert!(audio_samples > 8_000, "expected speech audio");
        // 2. Async tool call → late result with WHEN_IDLE.
        s.control.send(RealtimeControl::InjectContext { text: "What's the weather in Paris?".into(), respond: true }).unwrap();
        let mut answered = false;
        let mut spoke_after = 0usize;
        loop {
            match tokio::time::timeout(deadline, s.events.recv()).await.expect("timed out").expect("closed") {
                RealtimeEvent::ToolCall { id, name, args } => {
                    println!("tool call {name} {args}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    s.control.send(RealtimeControl::ToolResponse { will_continue: false,
                        id, name, response: serde_json::json!({"result": "Sunny, 21 degrees"}),
                        scheduling: Some(Scheduling::WhenIdle),
                    }).unwrap();
                    answered = true;
                }
                RealtimeEvent::Audio { pcm, .. } if answered => spoke_after += pcm.len(),
                RealtimeEvent::OutputTranscript(t) => print!("{t}"),
                RealtimeEvent::TurnComplete if answered && spoke_after > 0 => break,
                RealtimeEvent::Closed { reason } => panic!("closed: {reason}"),
                _ => {}
            }
        }
        println!("\ntool result spoken: {spoke_after} samples");
        s.control.send(RealtimeControl::Close).unwrap();
    }

    /// The brain handoff: interim `willContinue` response, then the result.
    /// `GEMINI_API_KEY=… cargo test --release gemini_live_brain_handoff -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn gemini_live_brain_handoff() {
        let key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY");
        let model = std::env::var("GEMINI_LIVE_MODEL").unwrap_or_else(|_| "gemini-3.8-live".into());
        let setup = RealtimeSetup { model, ..Default::default() };
        let h = crate::voice::probe::brain_handoff_round_trip(&GeminiLive::new(key), setup).await.expect("handoff");
        println!("{h:?}");
        assert!(!(h.before_call.is_empty() && h.holding.is_empty()), "silent while the tool ran");
        assert!(!h.result.is_empty());
    }

    /// The mic path's protocol: real (synthesized) speech streamed as
    /// `realtimeInput.audio` must be transcribed and answered aloud.
    /// `GEMINI_API_KEY=… cargo test --release gemini_live_speech_round_trip -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn gemini_live_speech_round_trip() {
        let key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY");
        let model = std::env::var("GEMINI_LIVE_MODEL").unwrap_or_else(|_| "gemini-3.8-live".into());
        let speech = crate::voice::probe::synth_speech(crate::voice::probe::TEST_QUESTION).expect("say");
        let setup = RealtimeSetup {
            model,
            system_instruction: "Answer in one short sentence.".into(),
            ..Default::default()
        };
        let r = crate::voice::probe::speech_round_trip(&GeminiLive::new(key), setup, speech)
            .await
            .expect("round trip");
        println!("heard {:?}\nreplied {:?} ({:.1} s audio, first after {:?})", r.heard, r.replied, r.reply_secs, r.first_audio_after);
        assert!(r.heard.to_lowercase().contains("two"), "transcript: {:?}", r.heard);
        assert!(r.reply_secs > 0.3);
    }
}
