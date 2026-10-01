use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub mod keepalive;

use super::claude_models::ClaudeModel;
use super::config::{ClaudeCacheTtl, ClaudeConfig};
use super::convert::{LlmFormatConverter, StreamEvent};
use super::types::{ChatRole, ContentBlock, LlmPrompt};
use super::LlmConnector;
use crate::components::shared::{StreamMessage, ToolCall, UsageData};
use crate::mcp::manager::McpContext;

/// Anthropic Messages API endpoint.
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
/// Required `anthropic-version` header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The `cache_control` marker value for the configured TTL. Every marker in a
/// request must use the same TTL (a 1h entry may not follow a 5m one).
fn cache_control_marker(ttl: ClaudeCacheTtl) -> Value {
    match ttl {
        ClaudeCacheTtl::FiveMinutes => json!({ "type": "ephemeral" }),
        ClaudeCacheTtl::OneHour => json!({ "type": "ephemeral", "ttl": "1h" }),
    }
}

/// Split the system prompt into (stable, volatile) at the prompt builder's
/// volatile offset, clamped to the string length and a char boundary.
fn split_system(system: &str, offset: Option<usize>) -> (&str, &str) {
    let mut at = offset.unwrap_or(system.len()).min(system.len());
    while !system.is_char_boundary(at) {
        at -= 1;
    }
    system.split_at(at)
}

/// Put a marker on the last non-thinking content block of the message at or
/// before `from` (thinking blocks can't carry `cache_control`). Returns the
/// index of the message that received it.
fn mark_last_block(messages: &mut [Value], from: usize, marker: &Value) -> Option<usize> {
    for idx in (0..=from.min(messages.len().checked_sub(1)?)).rev() {
        let Some(blocks) = messages[idx]["content"].as_array_mut() else {
            continue;
        };
        if let Some(block) = blocks.iter_mut().rev().find(|b| b["type"] != "thinking") {
            block["cache_control"] = marker.clone();
            return Some(idx);
        }
    }
    None
}

/// Build the final `UsageData`. Anthropic's `input_tokens` excludes both cache
/// reads and cache writes, which bill at their own rates. `prompt_tokens` is
/// reported as the TOTAL prompt size (input + writes + reads) so it matches
/// the Gemini semantics the UI assumes (cached tokens are a subset of prompt).
fn claude_usage(
    model: &ClaudeModel,
    ttl: ClaudeCacheTtl,
    input: i32,
    completion: i32,
    cache_read: Option<i32>,
    cache_write: Option<i32>,
) -> UsageData {
    let read = cache_read.unwrap_or(0);
    let write = cache_write.unwrap_or(0);
    let cost = (input as f64 / 1_000_000.0) * model.input_price_per_mtok()
        + (read as f64 / 1_000_000.0) * model.cache_read_price_per_mtok()
        + (write as f64 / 1_000_000.0) * model.cache_write_price_per_mtok(ttl)
        + (completion as f64 / 1_000_000.0) * model.output_price_per_mtok();
    let prompt = input + read + write;
    UsageData {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_content_tokens: cache_read,
        cache_creation_tokens: cache_write,
        thoughts_tokens: None,
        cost: Some(cost),
    }
}

// ── Prompt-cache diagnostics ──
//
// A cache miss reports only token counts, not which part of the prompt moved.
// Every cached request is fingerprinted (tools, each system block, each
// message) and compared with the previous request of the same conversation,
// so the miss log can name the first component that changed.

/// Beta header value that opts into Anthropic's server-side cache diagnosis.
const CACHE_DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";
/// `cache_creation_input_tokens` above this logs the divergence at `info`.
const CACHE_WRITE_LOG_THRESHOLD: i32 = 4096;
/// Conversations remembered by the process-wide fingerprint store.
const FINGERPRINT_STORE_CAP: usize = 16;

/// What `build_request` appended after the cached history. It changes every
/// turn by design, so it is excluded from the fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolatileTail {
    None,
    /// A trailing `role: "system"` message.
    SystemMessage,
    /// A text block appended to the final user message.
    TextBlock,
    /// A new trailing user message (history ended on an assistant turn).
    UserMessage,
}

/// Role and block types of a message: log-safe, never content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageShape {
    role: String,
    blocks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestFingerprint {
    /// Conversation key: hash of the first (stable) system block plus the
    /// first message. The connector never sees the session id and
    /// `LlmPrompt` can't carry one, but a conversation's persona block and
    /// opening message are fixed for its lifetime and differ between tabs.
    key: u64,
    /// Hash of all system blocks; groups requests that share a stable prefix
    /// when the first message was front-trimmed away.
    system_key: u64,
    tools: u64,
    tools_len: usize,
    system: Vec<u64>,
    messages: Vec<u64>,
    shapes: Vec<MessageShape>,
}

fn hash_value<H: std::hash::Hasher>(v: &Value, h: &mut H) {
    use std::hash::Hash;
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => (1u8, *b).hash(h),
        Value::Number(n) => (2u8, n.to_string()).hash(h),
        Value::String(s) => (3u8, s).hash(h),
        Value::Array(a) => {
            (4u8, a.len()).hash(h);
            a.iter().for_each(|x| hash_value(x, h));
        }
        Value::Object(m) => {
            (5u8, m.len()).hash(h);
            for (k, x) in m {
                k.hash(h);
                hash_value(x, h);
            }
        }
    }
}

/// Hash a content block ignoring its top-level `cache_control`, which moves
/// between requests by design.
fn hash_block<H: std::hash::Hasher>(block: &Value, h: &mut H) {
    use std::hash::Hash;
    match block.as_object() {
        Some(m) => {
            for (k, x) in m.iter().filter(|(k, _)| *k != "cache_control") {
                k.hash(h);
                hash_value(x, h);
            }
        }
        None => hash_value(block, h),
    }
}

/// Fingerprint of one message, `cache_control` stripped. `drop_last_block`
/// leaves out the trailing volatile text block.
fn hash_message(m: &Value, drop_last_block: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    m["role"].as_str().hash(&mut h);
    match m["content"].as_array() {
        Some(blocks) => {
            let n = blocks.len() - usize::from(drop_last_block && !blocks.is_empty());
            blocks[..n].iter().for_each(|b| hash_block(b, &mut h));
        }
        None => hash_value(&m["content"], &mut h),
    }
    h.finish()
}

fn hash_one(v: &Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_value(v, &mut h);
    h.finish()
}

fn message_shape(m: &Value) -> MessageShape {
    let blocks = match m["content"].as_array() {
        Some(bs) => bs
            .iter()
            .map(|b| b["type"].as_str().unwrap_or("?").to_string())
            .collect(),
        None => vec!["text".to_string()],
    };
    MessageShape {
        role: m["role"].as_str().unwrap_or("?").to_string(),
        blocks,
    }
}

/// Fingerprint a built request. Call it BEFORE adding the `diagnostics`
/// field. Runs once per request, never per SSE chunk.
fn fingerprint_request(request: &Value, tail: VolatileTail) -> RequestFingerprint {
    use std::hash::{Hash, Hasher};
    let system: Vec<u64> = match request["system"].as_array() {
        Some(blocks) => blocks
            .iter()
            .map(|b| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                hash_block(b, &mut h);
                h.finish()
            })
            .collect(),
        // Caching off: a single plain string.
        None if request["system"].is_string() => vec![hash_one(&request["system"])],
        None => Vec::new(),
    };
    let (tools, tools_len) = match request["tools"].as_array() {
        Some(t) => (hash_one(&request["tools"]), t.len()),
        None => (0, 0),
    };
    let all: &[Value] = request["messages"].as_array().map_or(&[], |v| v.as_slice());
    let stable = match tail {
        VolatileTail::SystemMessage | VolatileTail::UserMessage => {
            &all[..all.len().saturating_sub(1)]
        }
        VolatileTail::None | VolatileTail::TextBlock => all,
    };
    let last = stable.len().saturating_sub(1);
    let messages: Vec<u64> = stable
        .iter()
        .enumerate()
        .map(|(i, m)| hash_message(m, tail == VolatileTail::TextBlock && i == last))
        .collect();
    let shapes = stable.iter().map(message_shape).collect();

    let mut kh = std::collections::hash_map::DefaultHasher::new();
    (system.first(), messages.first()).hash(&mut kh);
    let mut sh = std::collections::hash_map::DefaultHasher::new();
    system.hash(&mut sh);
    RequestFingerprint {
        key: kh.finish(),
        system_key: sh.finish(),
        tools,
        tools_len,
        system,
        messages,
        shapes,
    }
}

/// The first component that differs between two consecutive requests.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Divergence {
    Tools {
        prev_len: usize,
        new_len: usize,
    },
    System(usize),
    /// Same index, different hash.
    InPlaceEdit {
        index: usize,
        shape: MessageShape,
    },
    /// New messages equal the old ones shifted by `k`.
    FrontTrim(usize),
    /// New history is a strict prefix of the old one.
    Truncated {
        new_len: usize,
    },
    /// Everything shared is byte-identical (append-only, or a server-side /
    /// TTL cause the fingerprints can't see).
    None,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tools { prev_len, new_len } => {
                write!(f, "tools changed ({prev_len} -> {new_len} defs)")
            }
            Self::System(i) => write!(f, "system[{i}] changed"),
            Self::InPlaceEdit { index, shape } => write!(
                f,
                "messages[{index}] changed (role={}, blocks=[{}]) kind=in-place edit",
                shape.role,
                shape.blocks.join(",")
            ),
            Self::FrontTrim(k) => write!(f, "front-trim by {k} messages kind=front-trim"),
            Self::Truncated { new_len } => {
                write!(f, "history truncated to {new_len} messages")
            }
            Self::None => write!(f, "no component changed (TTL expiry or server-side)"),
        }
    }
}

/// Smallest `k >= 1` such that the old messages from `k` onward equal the new
/// messages over their whole overlap (at least two, or all of a shorter one).
fn front_trim_shift(prev: &[u64], new: &[u64]) -> Option<usize> {
    (1..prev.len()).find(|&k| {
        let m = (prev.len() - k).min(new.len());
        m >= new.len().min(2) && m > 0 && prev[k..k + m] == new[..m]
    })
}

fn find_divergence(prev: &RequestFingerprint, new: &RequestFingerprint) -> Divergence {
    if prev.tools != new.tools {
        return Divergence::Tools {
            prev_len: prev.tools_len,
            new_len: new.tools_len,
        };
    }
    let shared = prev.system.len().min(new.system.len());
    if let Some(i) = (0..shared).find(|&i| prev.system[i] != new.system[i]) {
        return Divergence::System(i);
    }
    if prev.system.len() != new.system.len() {
        return Divergence::System(shared);
    }
    let shared = prev.messages.len().min(new.messages.len());
    match (0..shared).find(|&i| prev.messages[i] != new.messages[i]) {
        Some(i) => match front_trim_shift(&prev.messages, &new.messages) {
            Some(k) => Divergence::FrontTrim(k),
            None => Divergence::InPlaceEdit {
                index: i,
                shape: new.shapes[i].clone(),
            },
        },
        None if new.messages.len() < prev.messages.len() => Divergence::Truncated {
            new_len: new.messages.len(),
        },
        None => Divergence::None,
    }
}

struct StoredRequest {
    fingerprint: RequestFingerprint,
    message_id: Option<String>,
    seq: u64,
    at: std::time::Instant,
}

/// The previous request matched to the current one.
#[derive(Debug, Clone)]
struct PreviousRequest {
    fingerprint: RequestFingerprint,
    message_id: Option<String>,
    age: std::time::Duration,
}

/// Request-time state carried to the point where usage arrives.
struct CacheDiagState {
    fingerprint: RequestFingerprint,
    previous: Option<PreviousRequest>,
}

fn fingerprint_store() -> &'static std::sync::Mutex<Vec<StoredRequest>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<Vec<StoredRequest>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Find the stored request this one continues: same conversation key; else
/// same first message (system block changed); else same system prefix whose
/// messages are this request's shifted by a front-trim.
fn lookup_previous(fp: &RequestFingerprint) -> Option<PreviousRequest> {
    let store = fingerprint_store().lock().ok()?;
    let found = store
        .iter()
        .find(|e| e.fingerprint.key == fp.key)
        .or_else(|| {
            store
                .iter()
                .filter(|e| {
                    !e.fingerprint.messages.is_empty()
                        && e.fingerprint.messages.first() == fp.messages.first()
                })
                .max_by_key(|e| e.seq)
        })
        .or_else(|| {
            store
                .iter()
                .filter(|e| {
                    e.fingerprint.system_key == fp.system_key
                        && front_trim_shift(&e.fingerprint.messages, &fp.messages).is_some()
                })
                .max_by_key(|e| e.seq)
        })?;
    Some(PreviousRequest {
        fingerprint: found.fingerprint.clone(),
        message_id: found.message_id.clone(),
        age: found.at.elapsed(),
    })
}

/// Record a completed request, replacing the entry it continued.
fn store_request(
    fp: RequestFingerprint,
    previous: Option<&PreviousRequest>,
    message_id: Option<String>,
) {
    let Ok(mut store) = fingerprint_store().lock() else {
        return;
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(prev) = previous {
        store.retain(|e| e.fingerprint != prev.fingerprint);
    }
    store.retain(|e| e.fingerprint.key != fp.key);
    store.push(StoredRequest {
        fingerprint: fp,
        message_id,
        seq,
        at: std::time::Instant::now(),
    });
    while store.len() > FINGERPRINT_STORE_CAP {
        if let Some(oldest) = (0..store.len()).min_by_key(|&i| store[i].seq) {
            store.remove(oldest);
        }
    }
}

/// Depth-first search for a `diagnostics` key anywhere in a streamed event.
/// The response shape is unconfirmed, so it is located rather than assumed.
fn find_diagnostics(v: &Value) -> Option<&Value> {
    match v {
        Value::Object(m) => m
            .get("diagnostics")
            .filter(|d| !d.is_null())
            .or_else(|| m.values().find_map(find_diagnostics)),
        Value::Array(a) => a.iter().find_map(find_diagnostics),
        _ => None,
    }
}

/// What a streamed event line contributes to the cache diagnosis.
#[derive(Debug, Default, PartialEq)]
struct LineMeta {
    message_id: Option<String>,
    diagnostics: Option<Value>,
}

/// Pull `message.id` (from `message_start`) and any `diagnostics` value out
/// of an SSE line. Cheap substring gate first so ordinary delta lines are
/// never parsed twice.
fn scan_line_meta(line: &str) -> LineMeta {
    let mut meta = LineMeta::default();
    if !line.contains("\"message_start\"") && !line.contains("\"diagnostics\"") {
        return meta;
    }
    let Some(json_str) = line.trim().strip_prefix("data:") else {
        return meta;
    };
    let Ok(val) = serde_json::from_str::<Value>(json_str.trim()) else {
        return meta;
    };
    if val["type"] == "message_start" {
        meta.message_id = val["message"]["id"].as_str().map(str::to_string);
    }
    meta.diagnostics = find_diagnostics(&val).cloned();
    meta
}

/// Whether a response is a cache miss worth an `info` line.
fn is_notable_miss(created: i32, read: i32, has_previous: bool) -> bool {
    has_previous && (created > CACHE_WRITE_LOG_THRESHOLD || read == 0)
}

/// Format the divergence line. Hashes, indices, roles and block types only.
fn format_miss_line(
    state: &CacheDiagState,
    prev: &PreviousRequest,
    read: i32,
    created: i32,
) -> String {
    let divergence = find_divergence(&prev.fingerprint, &state.fingerprint);
    format!(
        "Claude cache miss: read={read} written={created} prev_messages={} new_messages={} since_prev={}s first divergence: {divergence}",
        prev.fingerprint.messages.len(),
        state.fingerprint.messages.len(),
        prev.age.as_secs(),
    )
}

/// Request-time debug line: one per request.
fn format_summary_line(fp: &RequestFingerprint, has_previous: bool) -> String {
    format!(
        "Claude cache fingerprint: key={:016x} tools={:016x} ({}) system_blocks={} messages={} last_message={:016x} baseline={}",
        fp.key,
        fp.tools,
        fp.tools_len,
        fp.system.len(),
        fp.messages.len(),
        fp.messages.last().copied().unwrap_or(0),
        has_previous,
    )
}

pub struct ClaudeConnector {
    config: ClaudeConfig,
}

impl ClaudeConnector {
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// Resolve the API key from config or the `ANTHROPIC_API_KEY` environment
    /// variable. Returns `Err` if neither source provides a key.
    fn resolve_api_key(&self) -> Result<String, ()> {
        if let Some(key) = self.config.api_key.clone() {
            if !key.is_empty() {
                return Ok(key);
            }
        }
        std::env::var("ANTHROPIC_API_KEY").map_err(|_| ())
    }

    /// The effective `max_tokens` for a request — the user override (if set),
    /// clamped to the model's output ceiling so the API never 400s.
    fn effective_max_tokens(&self) -> u32 {
        let model = ClaudeModel::from_slug(&self.config.model);
        let ceiling = model.max_output_tokens();
        self.config
            .max_tokens
            .unwrap_or(ceiling)
            .min(ceiling)
            .max(1)
    }

    /// `anthropic-beta` header value: every beta this request opts into,
    /// comma-joined, or `None` when there are none.
    fn anthropic_beta_header(&self) -> Option<String> {
        let mut betas: Vec<&str> = Vec::new();
        if self.config.cache_diagnostics {
            betas.push(CACHE_DIAGNOSIS_BETA);
        }
        (!betas.is_empty()).then(|| betas.join(","))
    }

    /// Add the `diagnostics` request field when the debug flag is on.
    fn apply_cache_diagnostics(&self, request: &mut Value, previous_id: Option<&str>) {
        if self.config.cache_diagnostics {
            request["diagnostics"] = json!({ "previous_message_id": previous_id });
        }
    }

    /// Build the request, fingerprint it (before the `diagnostics` field is
    /// added) and match it to the previous request of this conversation.
    fn prepare_request(&self, prompt: &LlmPrompt) -> (Value, Option<CacheDiagState>, bool) {
        let (mut request, tail) = self.build_request(prompt, true);
        let turn_start = keepalive::is_turn_start(&request, tail);
        if !self.config.prompt_caching_enabled {
            self.apply_cache_diagnostics(&mut request, None);
            return (request, None, turn_start);
        }
        let fingerprint = fingerprint_request(&request, tail);
        let previous = lookup_previous(&fingerprint);
        tracing::debug!("{}", format_summary_line(&fingerprint, previous.is_some()));
        self.apply_cache_diagnostics(
            &mut request,
            previous.as_ref().and_then(|p| p.message_id.as_deref()),
        );
        (
            request,
            Some(CacheDiagState {
                fingerprint,
                previous,
            }),
            turn_start,
        )
    }

    /// Called once per response, when usage is final: log the divergence on a
    /// notable miss, log any server diagnosis, and remember this request.
    fn finish_cache_diag(
        &self,
        state: Option<CacheDiagState>,
        created: Option<i32>,
        read: Option<i32>,
        message_id: Option<String>,
        diagnostics: Option<&Value>,
    ) {
        let Some(state) = state else {
            return;
        };
        let (created, read) = (created.unwrap_or(0), read.unwrap_or(0));
        let notable = state
            .previous
            .as_ref()
            .is_some_and(|_| is_notable_miss(created, read, true));
        if let (true, Some(prev)) = (notable, &state.previous) {
            tracing::info!("{}", format_miss_line(&state, prev, read, created));
        }
        if let Some(d) = diagnostics {
            if notable {
                tracing::info!("Claude cache diagnostics (server): {}", d);
            } else {
                tracing::debug!("Claude cache diagnostics (server): {}", d);
            }
        }
        store_request(state.fingerprint, state.previous.as_ref(), message_id);
    }

    /// Fallback resolution for a prefixed tool name → (server, tool) when it
    /// isn't found in the request-scoped tool map. Splits at the first
    /// underscore, mirroring the OpenAI-compat connector.
    fn resolve_tool_call(&self, prefixed_name: &str) -> (String, String) {
        if let Some(pos) = prefixed_name.find('_') {
            (
                prefixed_name[..pos].to_string(),
                prefixed_name[pos + 1..].to_string(),
            )
        } else {
            ("unknown".to_string(), prefixed_name.to_string())
        }
    }

    /// Produce a user-friendly error message from an Anthropic API error response.
    fn format_api_error(status: u16, body: &str) -> String {
        // Anthropic error envelope: {"type":"error","error":{"type":"...","message":"..."}}
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let err_type = parsed
            .as_ref()
            .and_then(|v| v["error"]["type"].as_str())
            .unwrap_or("");
        let err_msg = parsed
            .as_ref()
            .and_then(|v| v["error"]["message"].as_str())
            .unwrap_or(body);

        match status {
            401 | 403 => "⚠️ **Authentication Failed**\n\n\
                Anthropic rejected your API key.\n\n\
                **→ Go to Settings → AI Model** and check your Claude API Key."
                .to_string(),
            404 if err_type == "not_found_error" => format!(
                "⚠️ **Model Not Found**\n\nThe model was not found or your key lacks access.\n\n{}",
                err_msg
            ),
            429 => "⚠️ **Rate Limited**\n\n\
                You've hit Anthropic's rate limit. Wait a moment and try again."
                .to_string(),
            413 => "⚠️ **Prompt Too Large**\n\n\
                The request exceeds Anthropic's size limit. Reduce conversation \
                history or loaded tools."
                .to_string(),
            529 => "⚠️ **Anthropic Overloaded**\n\n\
                The API is temporarily overloaded. Please retry shortly."
                .to_string(),
            _ => format!("⚠️ **Claude API Error [{}]**\n\n{}", status, err_msg),
        }
    }
}

impl ClaudeConnector {
    /// Build the native request plus a note of which volatile tail (if any)
    /// was appended, so the cache fingerprints can exclude it.
    fn build_request(
        &self,
        prompt: &LlmPrompt,
        streaming: bool,
    ) -> (serde_json::Value, VolatileTail) {
        // Extended thinking is only sent when enabled AND the model supports it.
        // Gated here so thinking blocks in history are serialized consistently.
        // Always-on models (Fable 5.1, Opus 5.5) think regardless of the toggle
        // and reject a tool loop whose signed thinking blocks were dropped, so
        // for them the toggle cannot switch thinking off.
        let model = ClaudeModel::from_slug(&self.config.model);
        let thinking_enabled = model.thinking_always_on()
            || (self.config.extended_thinking && model.supports_thinking());

        // Build the Anthropic `messages` array. Claude only allows "user" and
        // "assistant" roles; tool results ride inside user-turn content blocks.
        let mut messages: Vec<serde_json::Value> = Vec::new();

        for msg in &prompt.messages {
            let mut content: Vec<serde_json::Value> = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    ContentBlock::Thinking { text, signature } => {
                        // Anthropic requires a thinking block to be replayed with
                        // its signature and to appear first in the assistant turn
                        // (history is already ordered thinking-first). Only include
                        // when thinking is enabled AND we have a non-empty signature
                        // — a thinking block without a valid signature returns a 400.
                        // An empty `thinking` text is still replayed: always-on
                        // models return signed blocks with no text, and dropping
                        // them from a tool loop is itself a 400.
                        // Otherwise omit (safe: we never end on an assistant turn, so
                        // the "must start with thinking" rule never applies here).
                        if thinking_enabled {
                            if let Some(sig) = signature {
                                if !sig.is_empty() {
                                    content.push(json!({
                                        "type": "thinking",
                                        "thinking": text,
                                        "signature": sig,
                                    }));
                                }
                            }
                        }
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        // `input` must be a JSON object.
                        let input = if arguments.is_object() {
                            arguments.clone()
                        } else if let Some(s) = arguments.as_str() {
                            serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
                        } else {
                            json!({})
                        };
                        content.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    ContentBlock::ToolResult {
                        call_id,
                        content: c,
                        ..
                    } => {
                        let result_str = if c.is_string() {
                            c.as_str().unwrap_or("").to_string()
                        } else {
                            serde_json::to_string(c).unwrap_or_else(|_| "{}".to_string())
                        };
                        content.push(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": result_str,
                        }));
                    }
                    ContentBlock::Image { mime_type, data } => {
                        content.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": mime_type,
                                "data": data,
                            }
                        }));
                    }
                }
            }

            if content.is_empty() {
                continue;
            }

            // Map neutral role → Anthropic role. Tool results and any System-role
            // messages are folded into the "user" turn.
            let role = match msg.role {
                ChatRole::Assistant => "assistant",
                ChatRole::User | ChatRole::System | ChatRole::Tool => "user",
            };

            messages.push(json!({ "role": role, "content": content }));
        }

        // Prompt caching: tools → system → messages is the render order, so
        // volatile per-turn content must live at the very end of `messages`
        // or it would invalidate everything after it.
        let mut system_blocks: Vec<serde_json::Value> = Vec::new();
        let mut tail = VolatileTail::None;
        if self.config.prompt_caching_enabled {
            let marker = cache_control_marker(self.config.cache_ttl);
            let (stable, volatile) = split_system(
                prompt.system.as_deref().unwrap_or(""),
                prompt.system_volatile_offset,
            );
            if !stable.trim().is_empty() {
                system_blocks.push(json!({
                    "type": "text",
                    "text": stable,
                    "cache_control": marker,
                }));
            }

            let last_is_user = messages.last().is_some_and(|m| m["role"] == "user");
            let volatile = volatile.trim();
            let history_end = messages.len().checked_sub(1);
            // Breakpoint on the last history block (before any volatile tail
            // is attached), so each request reads the prior conversation.
            let marked =
                history_end.and_then(|end| mark_last_block(&mut messages[..=end], end, &marker));
            if !volatile.is_empty() {
                if last_is_user && model.supports_mid_conversation_system() {
                    messages.push(json!({ "role": "system", "content": volatile }));
                    tail = VolatileTail::SystemMessage;
                } else if last_is_user {
                    if let Some(blocks) = messages
                        .last_mut()
                        .and_then(|m| m["content"].as_array_mut())
                    {
                        blocks.push(json!({ "type": "text", "text": volatile }));
                        tail = VolatileTail::TextBlock;
                    }
                } else {
                    // Assistant-last (or empty): the volatile text rides in a
                    // new user message after the cached history.
                    messages.push(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": volatile }],
                    }));
                    tail = VolatileTail::UserMessage;
                }
            }
            // A second breakpoint on the previous user turn keeps long tool
            // loops inside the API's 20-block lookback window.
            if let Some(m) = marked {
                if let Some(prev_user) = (0..m).rev().find(|&i| messages[i]["role"] == "user") {
                    mark_last_block(&mut messages[..=prev_user], prev_user, &marker);
                }
            }
        }

        let mut request = json!({
            "model": self.config.model,
            "max_tokens": self.effective_max_tokens(),
            "messages": messages,
            "stream": streaming,
        });

        // System prompt is a top-level field on Anthropic (not a message).
        if self.config.prompt_caching_enabled {
            if !system_blocks.is_empty() {
                request["system"] = json!(system_blocks);
            }
        } else if let Some(system) = &prompt.system {
            if !system.is_empty() {
                request["system"] = json!(system);
            }
        }

        // Adaptive thinking (GA on modern models; auto-enables interleaved
        // thinking with tools). budget_tokens is intentionally NOT used — it is
        // removed on current models and returns 400.
        if thinking_enabled {
            request["thinking"] = if model.thinking_always_on() {
                // These models default `display` to "omitted" (empty thinking
                // text), which would leave the thinking pane blank and hide
                // the narration they write between tool calls.
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                json!({ "type": "adaptive" })
            };
        }

        // Tools — sent with prompt_builder-consistent prefixed names so the
        // streamed `tool_use.name` resolves back via the same map.
        if !prompt.tools.is_empty() {
            let tools: Vec<serde_json::Value> = prompt
                .tools
                .iter()
                .map(|t| {
                    let prefixed =
                        crate::gemini::convert::get_prefixed_tool_name(&t.server_name, &t.name);
                    json!({
                        "name": prefixed,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            request["tools"] = json!(tools);
        }

        if self.config.model.is_empty() {
            tracing::error!("Claude request: model is EMPTY — this will 400. Check settings.");
        }

        tracing::debug!(
            "Claude request: model='{}', max_tokens={}, {} messages, {} tools",
            self.config.model,
            self.effective_max_tokens(),
            messages.len(),
            prompt.tools.len(),
        );

        (request, tail)
    }
}

/// Ties a keep-alive registration to one real request: `finish` re-arms the
/// refresh timer; dropping unfinished (error, cancel) just clears the busy flag.
struct KeepaliveGuard(Option<String>);

impl KeepaliveGuard {
    fn finish(&mut self, body: &str, input: i32, read: Option<i32>, created: Option<i32>) {
        if let Some(id) = self.0.take() {
            keepalive::finish_request(&id, body, input, read, created);
        }
    }
}

impl Drop for KeepaliveGuard {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            keepalive::abort_request(&id);
        }
    }
}

#[async_trait]
impl LlmConnector for ClaudeConnector {
    async fn generate_content_stream(
        &self,
        prompt_data: LlmPrompt,
        tx: mpsc::UnboundedSender<StreamMessage>,
        _mcp_context: Option<McpContext>,
        session_id: Option<String>,
    ) {
        let api_key = match self.resolve_api_key() {
            Ok(k) => k,
            Err(()) => {
                let _ = tx.send(StreamMessage::Error {
                    message: "⚠️ **API Key Not Configured**\n\nPlease set your Claude API key in \
                        Settings → AI Model to use Claude."
                        .to_string(),
                });
                return;
            }
        };

        let (native_request, mut cache_state, turn_start) = self.prepare_request(&prompt_data);
        let beta_header = self.anthropic_beta_header();
        // Keep-alive bookkeeping needs the session; every exit path below
        // either `finish`es or `abort`s the request via this guard.
        let mut ka_guard = KeepaliveGuard(None);

        // Reverse-map prefixed tool names → (server_name, tool_name), matching the
        // exact format prompt_builder uses for history. Anthropic echoes the tool
        // name we sent in the `tool_use` block, so this resolves it back.
        let mut tool_name_map: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        for tool_def in &prompt_data.tools {
            let prefixed = crate::gemini::convert::get_prefixed_tool_name(
                &tool_def.server_name,
                &tool_def.name,
            );
            tool_name_map.insert(
                prefixed,
                (tool_def.server_name.clone(), tool_def.name.clone()),
            );
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build reqwest client");

        let mut body_str = serde_json::to_string(&native_request)
            .expect("Failed to serialize Claude request body");

        if let (Some(id), true) = (session_id.as_deref(), cache_state.is_some()) {
            keepalive::begin_request(keepalive::RequestStart {
                session_id: id,
                config: &self.config,
                api_key: &api_key,
                turn_start,
            });
            ka_guard.0 = Some(id.to_string());
        }

        let model_for_cost = ClaudeModel::from_slug(&self.config.model);

        // Pending tool calls: Anthropic emits a `tool_use` content_block_start
        // (carries id+name), then a run of `input_json_delta` fragments for that
        // block, then content_block_stop. We accumulate the partial JSON and
        // flush complete calls at the end (same shape as the OpenAI-compat path).
        struct PendingToolCall {
            name: String,
            server_name: String,
            arguments: String,
        }

        let flush_tool_calls =
            |pending: &mut Vec<PendingToolCall>, tx: &mpsc::UnboundedSender<StreamMessage>| {
                for tc in pending.drain(..) {
                    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                    let tool_call = ToolCall::new(tc.server_name, tc.name, args, None, None);
                    tracing::debug!(
                        "Claude: Sending tool call '{}' on server '{}'",
                        tool_call.tool_name,
                        tool_call.server_name
                    );
                    let _ = tx.send(StreamMessage::ToolCall(Box::new(tool_call)));
                }
            };

        let cache_ttl = self.config.cache_ttl;
        let emit_usage = |prompt: i32,
                          completion: i32,
                          cached: Option<i32>,
                          created: Option<i32>,
                          tx: &mpsc::UnboundedSender<StreamMessage>| {
            if prompt == 0 && completion == 0 && cached.is_none() && created.is_none() {
                return;
            }
            let _ = tx.send(StreamMessage::Usage(claude_usage(
                &model_for_cost,
                cache_ttl,
                prompt,
                completion,
                cached,
                created,
            )));
        };

        // Retry transient failures (network errors, 429, 529, 5xx) with backoff —
        // but only while NO content has been streamed to the UI yet, so a
        // mid-stream failure never duplicates output. Honors `retry-after`.
        const MAX_RETRIES: u32 = 2;

        // Belt-and-braces guard (retries at most once, kept separate from the
        // transient-failure budget above): a mid-chat provider switch can
        // leave a `signature` minted by a different provider on a `thinking`
        // block in history. Anthropic rejects it with a 400 naming the
        // signature — drop every `thinking` block and retry exactly once.
        // This never fires after stream data has been emitted, since it only
        // triggers on the pre-stream status check below.
        let mut signature_retry_used = false;

        for attempt in 0..=MAX_RETRIES {
            let backoff = std::time::Duration::from_millis(500u64 * (1u64 << attempt));

            let mut http_request = client
                .post(ANTHROPIC_API_URL)
                .header("content-type", "application/json")
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(betas) = &beta_header {
                http_request = http_request.header("anthropic-beta", betas);
            }
            let response = match http_request.body(body_str.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        tracing::warn!(
                            "Claude: network error (attempt {}), retrying: {}",
                            attempt + 1,
                            e
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    let _ = tx.send(StreamMessage::Error {
                        message: format!("Network error: {}", e),
                    });
                    return;
                }
            };

            if !response.status().is_success() {
                let code = response.status().as_u16();

                // 400s are never part of the transient-failure retry budget
                // below, so it's safe to read the body eagerly here and check
                // for the foreign-signature shape before falling through to
                // the generic error report.
                if code == 400 && !signature_retry_used {
                    let body = response.text().await.unwrap_or_default();
                    if attempt < MAX_RETRIES
                        && crate::llm::is_claude_thought_signature_error(code, &body)
                    {
                        signature_retry_used = true;
                        tracing::warn!(
                            provider = "claude",
                            model = %self.config.model,
                            "Claude rejected a foreign thinking-block signature (likely left over from a mid-chat provider switch); stripping thinking blocks and retrying once"
                        );
                        let mut stripped_messages = prompt_data.messages.clone();
                        crate::llm::strip_thought_signatures(&mut stripped_messages);
                        let stripped_prompt = LlmPrompt {
                            system: prompt_data.system.clone(),
                            system_volatile_offset: prompt_data.system_volatile_offset,
                            messages: stripped_messages,
                            tools: prompt_data.tools.clone(),
                        };
                        let (stripped_native, stripped_state, _) =
                            self.prepare_request(&stripped_prompt);
                        cache_state = stripped_state;
                        body_str = serde_json::to_string(&stripped_native)
                            .expect("Failed to serialize Claude request body");
                        continue;
                    }
                    let _ = tx.send(StreamMessage::Error {
                        message: Self::format_api_error(code, &body),
                    });
                    return;
                }

                let retryable = code == 429 || code == 529 || response.status().is_server_error();
                if retryable && attempt < MAX_RETRIES {
                    let delay = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(std::time::Duration::from_secs)
                        .unwrap_or(backoff);
                    tracing::warn!(
                        "Claude: HTTP {} (attempt {}), retrying in {:?}",
                        code,
                        attempt + 1,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(StreamMessage::Error {
                    message: Self::format_api_error(code, &body),
                });
                return;
            }

            // ── Per-attempt accumulators (reset on each retry) ──
            let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();
            // Usage is split across message_start (input) and message_delta
            // (output, cumulative); last non-zero value wins, emit once at end.
            let mut usage_prompt: i32 = 0;
            let mut usage_completion: i32 = 0;
            let mut usage_cached: Option<i32> = None;
            let mut usage_created: Option<i32> = None;
            // Request-id / diagnostics for the cache diagnosis, from the SSE
            // lines that carry them.
            let mut response_message_id: Option<String> = None;
            let mut response_diagnostics: Option<Value> = None;
            // Only Text/Thinking are streamed to the UI mid-flight; tool calls and
            // usage are buffered and emitted at the end. So this gates retry safely.
            let mut has_sent_data = false;

            let mut stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();

            // Gemini-style buffered-line SSE reading: accumulate raw bytes, drain
            // complete lines on '\n'. Anthropic emits one SSE `data:` JSON object
            // per line, so a complete line is always a complete event payload.
            let stream_failed = loop {
                let item = match stream.next().await {
                    Some(it) => it,
                    None => break false, // stream ended cleanly
                };
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        // Retry only if nothing was streamed yet (avoids dup output).
                        if !has_sent_data && attempt < MAX_RETRIES {
                            tracing::warn!(
                                "Claude: stream error (attempt {}, no data yet), retrying: {}",
                                attempt + 1,
                                e
                            );
                            break true;
                        }
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Stream error: {}", e),
                        });
                        return;
                    }
                };

                buffer.extend_from_slice(&bytes);
                while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
                    let line = String::from_utf8_lossy(&line_bytes);

                    let meta = scan_line_meta(&line);
                    if meta.message_id.is_some() {
                        response_message_id = meta.message_id;
                    }
                    if meta.diagnostics.is_some() {
                        response_diagnostics = meta.diagnostics;
                    }

                    for event in self.parse_stream_chunk(&line) {
                        match event {
                            StreamEvent::Text { content } => {
                                if !content.is_empty() {
                                    has_sent_data = true;
                                    let _ = tx.send(StreamMessage::Text {
                                        content,
                                        thought_signature: None,
                                        thought_summary: None,
                                    });
                                }
                            }
                            StreamEvent::Thinking { text, signature } => {
                                // Stream thinking text for display AND carry the
                                // signature (arrives via signature_delta at the end
                                // of the thinking block) so stream_manager persists
                                // it for the next-turn round-trip.
                                let summary = if text.is_empty() { None } else { Some(text) };
                                if summary.is_some() || signature.is_some() {
                                    has_sent_data = true;
                                    let _ = tx.send(StreamMessage::Text {
                                        content: String::new(),
                                        thought_signature: signature,
                                        thought_summary: summary,
                                    });
                                }
                            }
                            StreamEvent::ToolCall {
                                name, arguments, ..
                            } => {
                                if !name.is_empty() {
                                    // New tool_use block — resolve server/tool.
                                    let (server, tool) = tool_name_map
                                        .get(&name)
                                        .cloned()
                                        .unwrap_or_else(|| self.resolve_tool_call(&name));
                                    pending_tool_calls.push(PendingToolCall {
                                        name: tool,
                                        server_name: server,
                                        arguments: arguments.as_str().unwrap_or("").to_string(),
                                    });
                                } else if let Some(last) = pending_tool_calls.last_mut() {
                                    // input_json_delta fragment for the current block.
                                    last.arguments.push_str(arguments.as_str().unwrap_or(""));
                                }
                            }
                            StreamEvent::Usage(u) => {
                                if u.prompt_tokens > 0 {
                                    usage_prompt = u.prompt_tokens;
                                }
                                if u.completion_tokens > 0 {
                                    usage_completion = u.completion_tokens;
                                }
                                if u.cached_content_tokens.is_some() {
                                    usage_cached = u.cached_content_tokens;
                                }
                                if u.cache_creation_tokens.is_some() {
                                    usage_created = u.cache_creation_tokens;
                                }
                            }
                            StreamEvent::Done => {
                                flush_tool_calls(&mut pending_tool_calls, &tx);
                                ka_guard.finish(&body_str, usage_prompt, usage_cached, usage_created);
                                self.finish_cache_diag(
                                    cache_state.take(),
                                    usage_created,
                                    usage_cached,
                                    response_message_id.take(),
                                    response_diagnostics.as_ref(),
                                );
                                emit_usage(
                                    usage_prompt,
                                    usage_completion,
                                    usage_cached,
                                    usage_created,
                                    &tx,
                                );
                                return;
                            }
                            StreamEvent::Error { message } => {
                                let _ = tx.send(StreamMessage::Error { message });
                                return;
                            }
                        }
                    }
                }
            };

            if stream_failed {
                tokio::time::sleep(backoff).await;
                continue;
            }

            // Stream ended without an explicit message_stop — flush what we have.
            flush_tool_calls(&mut pending_tool_calls, &tx);
            ka_guard.finish(&body_str, usage_prompt, usage_cached, usage_created);
            self.finish_cache_diag(
                cache_state.take(),
                usage_created,
                usage_cached,
                response_message_id.take(),
                response_diagnostics.as_ref(),
            );
            emit_usage(
                usage_prompt,
                usage_completion,
                usage_cached,
                usage_created,
                &tx,
            );
            return;
        }
    }

    async fn summarize_conversation(
        &self,
        previous_summary: String,
        recent_messages: String,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let api_key = self.resolve_api_key().map_err(|()| {
            Box::new(std::io::Error::other(
                "No Claude API key configured for summarization.",
            )) as Box<dyn std::error::Error + Send + Sync>
        })?;

        let summary_model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| self.config.model.clone());

        tracing::debug!(model = %summary_model, "LLM: Summarizing (Claude)");

        let system_prompt = r#"You are an AI assistant that refines a conversation summary.
You will be given a previous summary (which may be empty) and the most recent messages in a conversation.
Your primary task is to integrate the new information from the recent messages into the previous summary, updating and extending it.
Preserve existing information while incorporating new facts, entities, or user preferences.

You MUST respond with ONLY valid JSON (no prose, no markdown fences) containing exactly these fields:
- "summary": A concise, updated summary of the entire conversation so far.
- "sentiment": A brief string describing the user's current mood.
- "current_task": A one-sentence description of the specific task the user is CURRENTLY working on. Empty string if none.
- "entities": An object with:
  - "user_name": The user's name if mentioned.
  - "project_name": The active project or codebase name.
  - "key_topics": Main topics discussed (array of short strings).
  - "key_decisions": Important decisions made (array of short strings).
  - "active_profile": The active Composio profile name if mentioned.
  - "blockers": Current blockers or open issues (array of short strings).
- "tab_title": A 2-4 word topic for this chat's browser-tab-style label (e.g. "Fix login bug"). Empty string if unclear.
- "tab_emoji": A single emoji that specifically captures this chat's topic — reach for the most precise, least-generic match to the actual subject matter (a tool, domain, object, or action mentioned), not a default like 💬, 🤖, 📝, ✅, or 🚀. Vary your choice across different chats rather than reusing a small favorite set. Examples of specific picks: "🐛" for a bug hunt, "🧮" for a math problem, "🗺️" for trip planning, "🔐" for auth work, "🍞" for a baking recipe."#;

        let user_message = format!(
            "Previous Summary:\n---\n{}\n---\n\nRecent Messages:\n---\n{}\n---",
            previous_summary, recent_messages
        );

        let request_body = json!({
            "model": summary_model,
            "max_tokens": 2048,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": user_message }
            ],
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let response = client
            .post(ANTHROPIC_API_URL)
            .header("content-type", "application/json")
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(serde_json::to_string(&request_body).unwrap_or_default())
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!("Claude summarization error [{}]: {}", status, body);
            return Err(Box::new(std::io::Error::other(format!(
                "Summarization API request failed with status {}: {}",
                status, body
            ))) as Box<dyn std::error::Error + Send + Sync>);
        }

        let response_json: Value = response
            .json()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Anthropic response: { "content": [ { "type": "text", "text": "..." }, ... ] }
        let text = response_json["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or_default();

        if text.is_empty() {
            tracing::error!("Claude: Summarization response had no text content.");
            return Ok(serde_json::Value::Null);
        }

        if let Some(json_value) = super::extract_json_object(text) {
            return Ok(json_value);
        }
        tracing::warn!("Claude: Failed to parse summary as JSON. Using raw text.");
        Ok(json!({ "summary": text, "entities": {}, "sentiment": "neutral" }))
    }

    async fn select_tools_for_toolkit(
        &self,
        request: &crate::mcp::tool_selection::ToolSelectionRequest,
    ) -> Result<crate::mcp::tool_selection::ToolSelectionResponse, String> {
        use crate::mcp::tool_selection::{build_selection_prompt, parse_selection_response};

        let api_key = self
            .resolve_api_key()
            .map_err(|()| "No Claude API key configured for tool selection.".to_string())?;

        let model = self
            .config
            .summary_model
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| self.config.model.clone());

        tracing::info!(
            model = %model,
            toolkit = %request.toolkit_name,
            tool_count = %request.available_tools.len(),
            "LLM: Selecting tools for toolkit (Claude)"
        );

        let request_body = json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [
                { "role": "user", "content": build_selection_prompt(request) }
            ],
        });

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;

        let response = client
            .post(ANTHROPIC_API_URL)
            .header("content-type", "application/json")
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(serde_json::to_string(&request_body).unwrap_or_default())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Tool selection API request failed with status {}: {}",
                status, body
            ));
        }

        let response_json: Value = response.json().await.map_err(|e| e.to_string())?;
        let text = response_json["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or_default();

        if text.is_empty() {
            return Err("No response from Claude for tool selection".to_string());
        }

        parse_selection_response(text)
    }
}

impl LlmFormatConverter for ClaudeConnector {
    fn to_native_request(&self, prompt: &LlmPrompt, streaming: bool) -> serde_json::Value {
        self.build_request(prompt, streaming).0
    }

    fn parse_stream_chunk(&self, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for line in chunk.lines() {
            let line = line.trim();
            // We only care about `data:` lines; `event:` and blank lines are ignored.
            let json_str = match line.strip_prefix("data:") {
                Some(s) => s.trim(),
                None => continue,
            };
            let val: Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match val["type"].as_str() {
                Some("message_start") => {
                    // Initial usage: input tokens (+ cache reads). output is ~1 here.
                    let usage = &val["message"]["usage"];
                    let input = usage["input_tokens"].as_i64().unwrap_or(0) as i32;
                    let cached = usage["cache_read_input_tokens"].as_i64().map(|v| v as i32);
                    let created = usage["cache_creation_input_tokens"]
                        .as_i64()
                        .map(|v| v as i32);
                    if input > 0 || cached.is_some() || created.is_some() {
                        events.push(StreamEvent::Usage(UsageData {
                            prompt_tokens: input,
                            completion_tokens: 0,
                            total_tokens: input,
                            cached_content_tokens: cached,
                            cache_creation_tokens: created,
                            thoughts_tokens: None,
                            cost: None,
                        }));
                    }
                }
                Some("content_block_start") => {
                    let block = &val["content_block"];
                    if block["type"] == "tool_use" {
                        // Declaration: carries id + name; input arrives via deltas.
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        if !name.is_empty() {
                            events.push(StreamEvent::ToolCall {
                                id,
                                name,
                                server_name: None,
                                arguments: json!(""),
                                signature: None,
                            });
                        }
                    }
                }
                Some("content_block_delta") => {
                    let delta = &val["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                events.push(StreamEvent::Text {
                                    content: text.to_string(),
                                });
                            }
                        }
                        Some("input_json_delta") => {
                            // Argument fragment for the current tool_use block.
                            let frag = delta["partial_json"].as_str().unwrap_or("");
                            events.push(StreamEvent::ToolCall {
                                id: String::new(),
                                name: String::new(), // empty = continuation
                                server_name: None,
                                arguments: json!(frag),
                                signature: None,
                            });
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta["thinking"].as_str() {
                                events.push(StreamEvent::Thinking {
                                    text: text.to_string(),
                                    signature: None,
                                });
                            }
                        }
                        Some("signature_delta") => {
                            // Arrives once at the end of a thinking block; carries
                            // the signature needed to replay the block next turn.
                            if let Some(sig) = delta["signature"].as_str() {
                                events.push(StreamEvent::Thinking {
                                    text: String::new(),
                                    signature: Some(sig.to_string()),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    let output = val["usage"]["output_tokens"].as_i64().unwrap_or(0) as i32;
                    let created = val["usage"]["cache_creation_input_tokens"]
                        .as_i64()
                        .filter(|&v| v > 0)
                        .map(|v| v as i32);
                    if output > 0 || created.is_some() {
                        events.push(StreamEvent::Usage(UsageData {
                            prompt_tokens: 0,
                            completion_tokens: output,
                            total_tokens: output,
                            cached_content_tokens: None,
                            cache_creation_tokens: created,
                            thoughts_tokens: None,
                            cost: None,
                        }));
                    }
                }
                Some("message_stop") => events.push(StreamEvent::Done),
                Some("error") => {
                    let msg = val["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown Claude error");
                    events.push(StreamEvent::Error {
                        message: format!("Claude API error: {}", msg),
                    });
                }
                // "ping", "content_block_stop", etc. — nothing to emit.
                _ => {}
            }
        }
        events
    }

    fn convert_mcp_tool(
        &self,
        tool: &rmcp::model::Tool,
        server_name: &str,
    ) -> Result<crate::llm::ToolDefinition, String> {
        Ok(crate::llm::ToolDefinition::from_mcp(tool, server_name))
    }

    // sanitize_tool_name: uses trait default (alphanumeric + underscore)

    fn original_tool_name(&self, sanitized: &str) -> Option<String> {
        Some(sanitized.to_string())
    }

    fn max_tools(&self) -> usize {
        // Anthropic supports a large tool list; cap conservatively.
        128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ChatMessage, ToolDefinition};

    fn connector() -> ClaudeConnector {
        ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            max_tokens: Some(4096),
            prompt_caching_enabled: false,
            ..Default::default()
        })
    }

    #[test]
    fn request_puts_system_at_top_level_not_in_messages() {
        let prompt = LlmPrompt {
            system: Some("You are Hobbes.".to_string()),
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }],
            tools: vec![],
        };
        let req = connector().to_native_request(&prompt, true);
        assert_eq!(req["system"], "You are Hobbes.");
        assert_eq!(req["model"], "claude-opus-4-8");
        assert_eq!(req["max_tokens"], 4096);
        assert_eq!(req["stream"], true);
        // System must NOT appear as a message.
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn max_tokens_clamped_to_model_ceiling() {
        let c = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-1".to_string(), // 32K ceiling
            max_tokens: Some(999_999),
            ..Default::default()
        });
        assert_eq!(c.effective_max_tokens(), 32_000);
    }

    #[test]
    fn tool_result_maps_to_user_turn_block() {
        let prompt = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_1".to_string(),
                    name: "search".to_string(),
                    content: json!("results here"),
                }],
            }],
            tools: vec![],
        };
        let req = connector().to_native_request(&prompt, true);
        let msg = &req["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"][0]["type"], "tool_result");
        assert_eq!(msg["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(msg["content"][0]["content"], "results here");
    }

    #[test]
    fn tools_use_prefixed_names_and_input_schema() {
        let prompt = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "list_files".to_string(),
                server_name: "fs".to_string(),
                description: "List files".to_string(),
                parameters: json!({ "type": "object", "properties": {} }),
            }],
        };
        let req = connector().to_native_request(&prompt, true);
        let tool = &req["tools"][0];
        assert_eq!(
            tool["name"],
            crate::gemini::convert::get_prefixed_tool_name("fs", "list_files")
        );
        assert_eq!(tool["input_schema"]["type"], "object");
    }

    #[test]
    fn parses_text_delta() {
        let chunk = "event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n";
        let events = connector().parse_stream_chunk(chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Text { content } => assert_eq!(content, "Hello"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn parses_tool_use_start_and_input_delta() {
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"fs_list_files\",\"input\":{}}}\n";
        let delta = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\".\\\"}\"}}\n";

        let start_events = connector().parse_stream_chunk(start);
        match &start_events[0] {
            StreamEvent::ToolCall { name, .. } => assert_eq!(name, "fs_list_files"),
            other => panic!("expected ToolCall decl, got {:?}", other),
        }

        let delta_events = connector().parse_stream_chunk(delta);
        match &delta_events[0] {
            StreamEvent::ToolCall {
                name, arguments, ..
            } => {
                assert!(name.is_empty(), "continuation has empty name");
                assert_eq!(arguments.as_str().unwrap(), "{\"path\":\".\"}");
            }
            other => panic!("expected ToolCall fragment, got {:?}", other),
        }
    }

    #[test]
    fn parses_usage_and_message_stop() {
        let start = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n";
        let delta = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":17}}\n";
        let stop = "data: {\"type\":\"message_stop\"}\n";
        let c = connector();

        let su = c.parse_stream_chunk(start);
        match &su[0] {
            StreamEvent::Usage(u) => assert_eq!(u.prompt_tokens, 42),
            other => panic!("expected Usage, got {:?}", other),
        }
        let du = c.parse_stream_chunk(delta);
        match &du[0] {
            StreamEvent::Usage(u) => assert_eq!(u.completion_tokens, 17),
            other => panic!("expected Usage, got {:?}", other),
        }
        assert!(matches!(c.parse_stream_chunk(stop)[0], StreamEvent::Done));
    }

    /// Opus 5.5 thinks on every request: the toggle can't drop its signed
    /// thinking blocks (a tool loop missing them is a 400), empty-text blocks
    /// are replayed, and `display: "summarized"` is requested so the
    /// thinking text isn't omitted.
    #[test]
    fn always_on_model_replays_thinking_even_with_toggle_off() {
        let connector = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-5-5".to_string(),
            max_tokens: Some(4096),
            extended_thinking: false,
            ..Default::default()
        });
        let prompt = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: String::new(),
                        signature: Some("sig-empty".into()),
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                    },
                ],
            }],
            tools: vec![],
        };
        let req = connector.to_native_request(&prompt, true);
        assert_eq!(req["thinking"]["type"], "adaptive");
        assert_eq!(req["thinking"]["display"], "summarized");
        let blocks = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "sig-empty");
        assert_eq!(blocks[1]["type"], "text");

        // Other adaptive models keep the plain request shape.
        let other = thinking_connector(true).to_native_request(&prompt, true);
        assert!(other["thinking"].get("display").is_none());
    }

    fn thinking_connector(enabled: bool) -> ClaudeConnector {
        ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            max_tokens: Some(4096),
            extended_thinking: enabled,
            ..Default::default()
        })
    }

    #[test]
    fn thinking_param_sent_only_when_enabled() {
        let prompt = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
        };
        let off = thinking_connector(false).to_native_request(&prompt, true);
        assert!(off.get("thinking").is_none());

        let on = thinking_connector(true).to_native_request(&prompt, true);
        assert_eq!(on["thinking"]["type"], "adaptive");
    }

    #[test]
    fn thinking_block_serialized_only_with_signature() {
        let with_sig = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: Some("sig-abc".into()),
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                    },
                ],
            }],
            tools: vec![],
        };
        let req = thinking_connector(true).to_native_request(&with_sig, true);
        let blocks = req["messages"][0]["content"].as_array().unwrap();
        // thinking block first, then text
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "reasoning");
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[1]["type"], "text");

        // Without a signature, the thinking block is omitted (would 400).
        let no_sig = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                    },
                ],
            }],
            tools: vec![],
        };
        let req2 = thinking_connector(true).to_native_request(&no_sig, true);
        let blocks2 = req2["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks2.len(), 1);
        assert_eq!(blocks2[0]["type"], "text");

        // Even with a signature, thinking is omitted when the feature is off.
        let req3 = thinking_connector(false).to_native_request(&with_sig, true);
        let blocks3 = req3["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks3.len(), 1);
        assert_eq!(blocks3[0]["type"], "text");
    }

    #[test]
    fn parses_signature_delta() {
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n";
        let events = connector().parse_stream_chunk(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Thinking { text, signature } => {
                assert!(text.is_empty());
                assert_eq!(signature.as_deref(), Some("sig-xyz"));
            }
            other => panic!("expected Thinking signature, got {:?}", other),
        }
    }

    // ── Foreign thought-signature guard ──────────────────────────────────

    #[test]
    fn detects_claude_thought_signature_error() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0.signature: Invalid signature for thinking block."}}"#;
        assert!(crate::llm::is_claude_thought_signature_error(400, body));

        // Case-insensitive.
        let body_upper = r#"{"type":"error","error":{"type":"INVALID_REQUEST_ERROR","message":"Thinking block SIGNATURE is invalid."}}"#;
        assert!(crate::llm::is_claude_thought_signature_error(
            400, body_upper
        ));
    }

    #[test]
    fn does_not_detect_unrelated_claude_400() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: model requires max_tokens"}}"#;
        assert!(!crate::llm::is_claude_thought_signature_error(400, body));
    }

    #[test]
    fn does_not_detect_claude_429_even_with_signature_wording() {
        // A rate-limit body that happens to mention "signature" (e.g. inside an
        // echoed request) must not be treated as the thought-signature case.
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"thinking block signature check delayed"}}"#;
        assert!(!crate::llm::is_claude_thought_signature_error(429, body));
    }

    #[test]
    fn strip_thought_signatures_clears_thinking_and_leaves_others_untouched() {
        let mut messages = vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        signature: Some("gemini-minted-sig".into()),
                    },
                    ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "search".into(),
                        arguments: json!({"q": "x"}),
                        signature: Some("gemini-minted-sig-2".into()),
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                    },
                ],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    call_id: "call_1".into(),
                    name: "search".into(),
                    content: json!({"ok": true}),
                }],
            },
        ];

        crate::llm::strip_thought_signatures(&mut messages);

        match &messages[0].content[0] {
            ContentBlock::Thinking { signature, text } => {
                assert!(signature.is_none());
                assert_eq!(text, "reasoning"); // text untouched
            }
            other => panic!("expected Thinking, got {:?}", other),
        }
        match &messages[0].content[1] {
            ContentBlock::ToolCall {
                signature, name, ..
            } => {
                assert!(signature.is_none());
                assert_eq!(name, "search"); // other fields untouched
            }
            other => panic!("expected ToolCall, got {:?}", other),
        }
        assert_eq!(
            messages[0].content[2],
            ContentBlock::Text {
                text: "answer".into()
            }
        );
        // ToolResult (no signature field at all) is untouched.
        assert_eq!(
            messages[1].content[0],
            ContentBlock::ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                content: json!({"ok": true}),
            }
        );
    }

    #[test]
    fn stripped_thinking_block_serializes_without_signature_key() {
        // A thinking block carrying a foreign signature, after stripping, must
        // serialize to no `thinking` block at all (matches the existing
        // "no signature => omitted" behavior asserted by
        // `thinking_block_serialized_only_with_signature`), and the tool call
        // must serialize with no `signature`-carrying field.
        let mut messages = vec![ChatMessage {
            role: ChatRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "reasoning".into(),
                    signature: Some("foreign-sig".into()),
                },
                ContentBlock::ToolCall {
                    id: "call_1".into(),
                    name: "search".into(),
                    arguments: json!({"q": "x"}),
                    signature: Some("foreign-sig-2".into()),
                },
            ],
        }];
        crate::llm::strip_thought_signatures(&mut messages);

        let prompt = LlmPrompt {
            system: None,
            system_volatile_offset: None,
            messages,
            tools: vec![],
        };
        let req = thinking_connector(true).to_native_request(&prompt, true);
        let blocks = req["messages"][0]["content"].as_array().unwrap();

        assert!(
            blocks.iter().all(|b| b["type"] != "thinking"),
            "stripped thinking block must not be serialized: {:?}",
            blocks
        );
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(
            !serialized.contains("\"signature\""),
            "no signature key should remain in the request: {}",
            serialized
        );
        // The tool_use block itself is still present (Claude has no
        // signature field on tool_use, so nothing to strip there beyond the
        // neutral ContentBlock::ToolCall.signature we already cleared).
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
    }

    // ── Prompt caching ──

    fn cache_connector(model: &str, enabled: bool, ttl: ClaudeCacheTtl) -> ClaudeConnector {
        ClaudeConnector::new(ClaudeConfig {
            model: model.to_string(),
            max_tokens: Some(4096),
            prompt_caching_enabled: enabled,
            cache_ttl: ttl,
            ..Default::default()
        })
    }

    fn text_msg(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn cache_prompt(system: &str, offset: Option<usize>, messages: Vec<ChatMessage>) -> LlmPrompt {
        LlmPrompt {
            system: Some(system.to_string()),
            system_volatile_offset: offset,
            messages,
            tools: vec![],
        }
    }

    fn count_markers(v: &Value) -> usize {
        match v {
            Value::Object(m) => {
                usize::from(m.contains_key("cache_control"))
                    + m.values().map(count_markers).sum::<usize>()
            }
            Value::Array(a) => a.iter().map(count_markers).sum(),
            _ => 0,
        }
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            text_msg(ChatRole::User, "first"),
            text_msg(ChatRole::Assistant, "reply"),
            text_msg(ChatRole::User, "second"),
        ]
    }

    #[test]
    fn caching_off_keeps_plain_string_system_and_no_markers() {
        let p = cache_prompt("STABLE|VOLATILE", Some(6), history());
        let req = cache_connector("claude-opus-5-5", false, ClaudeCacheTtl::OneHour)
            .to_native_request(&p, true);
        assert_eq!(req["system"], "STABLE|VOLATILE");
        assert_eq!(count_markers(&req), 0);
        assert_eq!(req["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn caching_on_without_offset_caches_whole_system_and_last_block() {
        let p = cache_prompt("all stable", None, history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        let sys = req["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], "all stable");
        assert_eq!(sys[0]["cache_control"], json!({ "type": "ephemeral" }));
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["content"][0]["cache_control"]["type"], "ephemeral");
        // Previous user turn guards the lookback window.
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(msgs[1]["content"][0].get("cache_control").is_none());
        assert!(count_markers(&req) <= 4);
    }

    #[test]
    fn volatile_goes_to_trailing_system_message_on_supporting_models() {
        let p = cache_prompt("STABLE\nVOLATILE", Some(7), history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        assert_eq!(req["system"].as_array().unwrap()[0]["text"], "STABLE\n");
        assert!(!req["system"].to_string().contains("VOLATILE"));
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3]["role"], "system");
        assert_eq!(msgs[3]["content"], "VOLATILE");
        // Marker sits on the last history block, never on the volatile tail.
        assert_eq!(msgs[2]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(msgs[3].get("cache_control").is_none());
    }

    #[test]
    fn volatile_becomes_trailing_text_block_on_unsupported_models() {
        for model in ["claude-sonnet-5", "claude-opus-4-6", "claude-mystery-9"] {
            let p = cache_prompt("STABLEVOLATILE", Some(6), history());
            let req = cache_connector(model, true, ClaudeCacheTtl::FiveMinutes)
                .to_native_request(&p, true);
            assert!(!req["system"].to_string().contains("VOLATILE"), "{model}");
            let msgs = req["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 3, "{model}");
            let blocks = msgs[2]["content"].as_array().unwrap();
            assert_eq!(blocks.len(), 2, "{model}");
            assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
            assert_eq!(blocks[1]["text"], "VOLATILE");
            assert!(blocks[1].get("cache_control").is_none());
        }
    }

    #[test]
    fn assistant_last_gets_a_trailing_user_message_for_volatile() {
        let msgs = vec![
            text_msg(ChatRole::User, "q"),
            text_msg(ChatRole::Assistant, "partial"),
        ];
        let p = cache_prompt("STABLEVOLATILE", Some(6), msgs);
        // Even on a mid-system-capable model: last message isn't user-role.
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["text"], "VOLATILE");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(msgs[2]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn tool_result_tail_gets_volatile_after_the_result_block() {
        let msgs = vec![
            text_msg(ChatRole::User, "q"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "x".into(),
                    arguments: json!({}),
                    signature: None,
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    call_id: "t1".into(),
                    name: "x".into(),
                    content: json!("ok"),
                }],
            },
        ];
        let p = cache_prompt("SV", Some(1), msgs);
        let req = cache_connector("claude-sonnet-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        let blocks = req["messages"][2]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["text"], "V");
    }

    #[test]
    fn offset_past_end_or_mid_codepoint_does_not_panic() {
        let past = cache_prompt("abc", Some(999), history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&past, true);
        assert_eq!(req["system"][0]["text"], "abc");
        assert_eq!(req["messages"].as_array().unwrap().len(), 3);

        // "é" is 2 bytes; offset 2 is inside the second char of "aé" + "b".
        let mid = cache_prompt("aéb", Some(2), history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&mid, true);
        assert_eq!(req["system"][0]["text"], "a");
        assert_eq!(req["messages"][3]["content"], "éb");
    }

    #[test]
    fn empty_stable_part_skips_the_system_block() {
        let p = cache_prompt("VOLATILE ONLY", Some(0), history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        assert!(req.get("system").is_none());
        assert_eq!(req["messages"][3]["content"], "VOLATILE ONLY");
    }

    #[test]
    fn markers_skip_thinking_blocks_and_stay_within_four() {
        let msgs = vec![
            text_msg(ChatRole::User, "q"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentBlock::Text { text: "a".into() },
                    ContentBlock::Thinking {
                        text: "hm".into(),
                        signature: Some("sig".into()),
                    },
                ],
            },
        ];
        let p = cache_prompt("S", None, msgs);
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::FiveMinutes)
            .to_native_request(&p, true);
        let blocks = req["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "thinking");
        assert!(blocks[1].get("cache_control").is_none());
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert!(count_markers(&req) <= 4);
    }

    #[test]
    fn one_hour_ttl_propagates_to_every_marker() {
        let p = cache_prompt("STABLEVOLATILE", Some(6), history());
        let req = cache_connector("claude-opus-5-5", true, ClaudeCacheTtl::OneHour)
            .to_native_request(&p, true);
        let n = count_markers(&req);
        assert!((2..=4).contains(&n));
        assert_eq!(req.to_string().matches("\"ttl\":\"1h\"").count(), n);
    }

    #[test]
    fn mid_conversation_system_support_matrix() {
        for ok in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-sonnet-5-5",
        ] {
            assert!(
                ClaudeModel::from_slug(ok).supports_mid_conversation_system(),
                "{ok}"
            );
        }
        for no in [
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-haiku-4-5",
            "nonsense",
        ] {
            assert!(
                !ClaudeModel::from_slug(no).supports_mid_conversation_system(),
                "{no}"
            );
        }
    }

    #[test]
    fn parses_cache_creation_tokens_from_message_start() {
        let c = connector();
        let chunk = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":500,"cache_read_input_tokens":200,"output_tokens":1}}}"#;
        let events = c.parse_stream_chunk(chunk);
        match &events[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.cache_creation_tokens, Some(500));
                assert_eq!(u.cached_content_tokens, Some(200));
            }
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn cost_includes_cache_writes_and_reports_total_prompt() {
        // Opus 5.5: $4 in, $20 out, $0.20 read.
        let m = ClaudeModel::Opus5_5;
        let u = claude_usage(
            &m,
            ClaudeCacheTtl::FiveMinutes,
            1_000_000,
            0,
            Some(1_000_000),
            Some(1_000_000),
        );
        // 4.0 + 0.2 + 5.0 (1.25x)
        assert!((u.cost.unwrap() - 9.2).abs() < 1e-9);
        assert_eq!(u.prompt_tokens, 3_000_000);
        assert_eq!(u.cached_content_tokens, Some(1_000_000));
        assert_eq!(u.cache_creation_tokens, Some(1_000_000));
        let h = claude_usage(&m, ClaudeCacheTtl::OneHour, 0, 0, None, Some(1_000_000));
        assert!((h.cost.unwrap() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn old_claude_config_json_defaults_to_caching_on_five_minutes() {
        let cfg: ClaudeConfig =
            serde_json::from_str(r#"{"model":"claude-opus-5-5","summary_model":null}"#).unwrap();
        assert!(cfg.prompt_caching_enabled);
        assert_eq!(cfg.cache_ttl, ClaudeCacheTtl::FiveMinutes);
    }

    // ── Cache diagnostics ──

    fn fp_of(c: &ClaudeConnector, p: &LlmPrompt) -> RequestFingerprint {
        let (req, tail) = c.build_request(p, true);
        fingerprint_request(&req, tail)
    }

    fn grown(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                let role = if i % 2 == 0 {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                };
                text_msg(role, &format!("message {i}"))
            })
            .collect()
    }

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: "d".to_string(),
            parameters: json!({ "type": "object" }),
            server_name: "srv".to_string(),
        }
    }

    #[test]
    fn fingerprints_ignore_cache_control_and_volatile_tail() {
        let c = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        let sys_a = "STABLE-PART VOLATILE-ONE";
        let sys_b = "STABLE-PART VOLATILE-TWO-DIFFERENT";
        let off = Some("STABLE-PART".len());
        let a = fp_of(&c, &cache_prompt(sys_a, off, grown(3)));
        let b = fp_of(&c, &cache_prompt(sys_b, off, grown(3)));
        assert_eq!(a, b);

        // Markers moving: strip them from a built request and the hashes match.
        let (req, tail) = c.build_request(&cache_prompt(sys_a, off, grown(3)), true);
        let mut bare = req.clone();
        for m in bare["messages"].as_array_mut().unwrap() {
            if let Some(blocks) = m["content"].as_array_mut() {
                for b in blocks {
                    b.as_object_mut().unwrap().remove("cache_control");
                }
            }
        }
        assert_eq!(
            fingerprint_request(&req, tail).messages,
            fingerprint_request(&bare, tail).messages
        );

        // Text-block tail (model without mid-conversation system) and a
        // trailing user message (assistant-last) are excluded too.
        let old = cache_connector("claude-haiku-4-5", true, ClaudeCacheTtl::FiveMinutes);
        assert_eq!(
            fp_of(&old, &cache_prompt(sys_a, off, grown(3))),
            fp_of(&old, &cache_prompt(sys_b, off, grown(3)))
        );
        assert_eq!(
            fp_of(&old, &cache_prompt(sys_a, off, grown(4))).messages,
            fp_of(&old, &cache_prompt(sys_b, off, grown(4))).messages
        );
        // ...but a real content change is seen.
        let mut edited = grown(3);
        edited[1] = text_msg(ChatRole::Assistant, "changed");
        assert_ne!(a, fp_of(&c, &cache_prompt(sys_a, off, edited)));
    }

    #[test]
    fn divergence_finder_names_the_first_changed_component() {
        let c = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        let prompt = |sys: &str, msgs: Vec<ChatMessage>, tools: &[&str]| LlmPrompt {
            tools: tools.iter().map(|t| tool_def(t)).collect(),
            ..cache_prompt(sys, None, msgs)
        };
        let base = fp_of(&c, &prompt("sys", grown(7), &["a"]));

        // Appending is not a divergence.
        let appended = fp_of(&c, &prompt("sys", grown(9), &["a"]));
        assert_eq!(find_divergence(&base, &appended), Divergence::None);

        let tools = fp_of(&c, &prompt("sys", grown(7), &["a", "b"]));
        assert_eq!(
            find_divergence(&base, &tools),
            Divergence::Tools {
                prev_len: 1,
                new_len: 2
            }
        );

        let system = fp_of(&c, &prompt("sys2", grown(7), &["a"]));
        assert_eq!(find_divergence(&base, &system), Divergence::System(0));

        let mut edited = grown(7);
        edited[4] = text_msg(ChatRole::User, "edited");
        let edit = fp_of(&c, &prompt("sys", edited, &["a"]));
        match find_divergence(&base, &edit) {
            Divergence::InPlaceEdit { index, shape } => {
                assert_eq!(index, 4);
                assert_eq!(shape.role, "user");
                assert_eq!(shape.blocks, vec!["text"]);
            }
            other => panic!("expected in-place edit, got {other:?}"),
        }

        // Front-trim by 2: new history is the old one shifted, plus a turn.
        let trimmed: Vec<ChatMessage> = grown(10).split_off(2);
        let trim = fp_of(&c, &prompt("sys", trimmed, &["a"]));
        assert_eq!(find_divergence(&base, &trim), Divergence::FrontTrim(2));
        assert!(Divergence::FrontTrim(2).to_string().contains("front-trim"));

        let short = fp_of(&c, &prompt("sys", grown(3), &["a"]));
        assert_eq!(
            find_divergence(&base, &short),
            Divergence::Truncated { new_len: 3 }
        );
    }

    #[test]
    fn conversation_key_is_stable_while_the_conversation_grows() {
        let c = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        let a = fp_of(&c, &cache_prompt("persona", None, grown(3)));
        let b = fp_of(&c, &cache_prompt("persona", None, grown(9)));
        assert_eq!(a.key, b.key);
        let other_tab = fp_of(&c, &cache_prompt("persona", None, vec![text_msg(ChatRole::User, "hi")]));
        assert_ne!(a.key, other_tab.key);
    }

    #[test]
    fn store_matches_growth_and_front_trim_and_caps_entries() {
        let c = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        let sys = "store-test-persona";
        let first = fp_of(&c, &cache_prompt(sys, None, grown(5)));
        assert!(lookup_previous(&first).is_none());
        store_request(first.clone(), None, Some("msg_1".into()));

        let next = fp_of(&c, &cache_prompt(sys, None, grown(7)));
        let prev = lookup_previous(&next).expect("same key");
        assert_eq!(prev.message_id.as_deref(), Some("msg_1"));
        store_request(next.clone(), Some(&prev), Some("msg_2".into()));

        let trimmed = fp_of(&c, &cache_prompt(sys, None, grown(9).split_off(2)));
        assert_ne!(trimmed.key, next.key);
        let prev = lookup_previous(&trimmed).expect("front-trim match");
        assert_eq!(prev.message_id.as_deref(), Some("msg_2"));

        for i in 0..(FINGERPRINT_STORE_CAP * 2) {
            let mut fp = first.clone();
            fp.key = 1_000 + i as u64;
            store_request(fp, None, None);
        }
        assert!(fingerprint_store().lock().unwrap().len() <= FINGERPRINT_STORE_CAP);
    }

    #[test]
    fn miss_line_names_divergence_without_content() {
        let c = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        let prev_fp = fp_of(&c, &cache_prompt("s", None, grown(7)));
        let mut edited = grown(7);
        edited[3] = text_msg(ChatRole::Assistant, "SECRET-CONTENT");
        let state = CacheDiagState {
            fingerprint: fp_of(&c, &cache_prompt("s", None, edited)),
            previous: None,
        };
        let prev = PreviousRequest {
            fingerprint: prev_fp,
            message_id: None,
            age: std::time::Duration::from_secs(12),
        };
        let line = format_miss_line(&state, &prev, 1200, 67000);
        assert!(line.contains("read=1200 written=67000"));
        assert!(line.contains("prev_messages=7 new_messages=7"));
        assert!(line.contains("messages[3] changed (role=assistant, blocks=[text]) kind=in-place edit"));
        assert!(!line.contains("SECRET"));
        assert!(is_notable_miss(5000, 100, true));
        assert!(is_notable_miss(0, 0, true));
        assert!(!is_notable_miss(100, 9000, true));
        assert!(!is_notable_miss(9000, 0, false));
    }

    #[test]
    fn cache_diagnostics_adds_field_and_header_only_when_on() {
        let on = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            cache_diagnostics: true,
            ..Default::default()
        });
        let off = cache_connector("claude-opus-4-8", true, ClaudeCacheTtl::FiveMinutes);
        assert_eq!(on.anthropic_beta_header().as_deref(), Some("cache-diagnosis-2026-04-07"));
        assert_eq!(off.anthropic_beta_header(), None);

        let (mut req, _) = on.build_request(&cache_prompt("s", None, grown(1)), true);
        on.apply_cache_diagnostics(&mut req, Some("msg_abc"));
        assert_eq!(req["diagnostics"], json!({ "previous_message_id": "msg_abc" }));
        on.apply_cache_diagnostics(&mut req, None);
        assert_eq!(req["diagnostics"], json!({ "previous_message_id": null }));

        let (mut req, _) = off.build_request(&cache_prompt("s", None, grown(1)), true);
        off.apply_cache_diagnostics(&mut req, Some("msg_abc"));
        assert!(req.get("diagnostics").is_none());
    }

    #[test]
    fn prepare_request_fingerprints_before_diagnostics_field() {
        let on = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-4-8".to_string(),
            cache_diagnostics: true,
            ..Default::default()
        });
        let (req, state, _) = on.prepare_request(&cache_prompt("prepare-test", None, grown(3)));
        assert!(req.get("diagnostics").is_some());
        assert!(state.is_some());
    }

    #[test]
    fn scan_line_meta_finds_message_id_and_diagnostics_defensively() {
        let start = r#"data: {"type":"message_start","message":{"id":"msg_01","usage":{"input_tokens":5}}}"#;
        let m = scan_line_meta(start);
        assert_eq!(m.message_id.as_deref(), Some("msg_01"));
        assert_eq!(m.diagnostics, None);

        let nested = r#"data: {"type":"message_start","message":{"id":"msg_02","diagnostics":{"cache_miss":{"component":"tools"}}}}"#;
        assert_eq!(
            scan_line_meta(nested).diagnostics,
            Some(json!({ "cache_miss": { "component": "tools" } }))
        );
        let delta = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"diagnostics":{"x":1}}"#;
        assert_eq!(scan_line_meta(delta).diagnostics, Some(json!({ "x": 1 })));

        let text = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"diagnostics"}}"#;
        assert_eq!(scan_line_meta(text), LineMeta::default());
    }

    #[test]
    fn old_claude_config_json_defaults_cache_diagnostics_off() {
        let cfg: ClaudeConfig =
            serde_json::from_str(r#"{"model":"claude-opus-5-5","summary_model":null}"#).unwrap();
        assert!(!cfg.cache_diagnostics);
    }
}
