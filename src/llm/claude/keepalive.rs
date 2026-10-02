//! Prompt-cache keep-alive for the Claude connector.
//!
//! Anthropic's ephemeral cache entry lives 5 minutes, and every read of it
//! refreshes the lifetime at no extra charge (the clock runs from the START of
//! the request that reads it). A tab left idle for longer than that pays a
//! full cache write (1.25x) on its next turn. Phase 3 made the prompt prefix
//! byte-stable, so a cheap read of that prefix keeps it warm.
//!
//! Economics (price multipliers on the model's base input price):
//! - cache read 0.1x, 5m write 1.25x, 1h write 2x;
//! - a ping costs ~0.1x * prefix + 0 output tokens; a miss costs ~1.25x * prefix;
//! - break-even is ~11 pings (~55 min of refreshes) per avoided miss, so
//!   pinging is bounded by an idle window and a daily spend cap.
//!
//! The ping is the last real request body with `stream: false` and
//! `max_tokens: 0` (documented cache pre-warm: no output, usage returned).
//! Everything that feeds the cache key (tools, system, messages, thinking,
//! tool_choice) is left byte-identical, and the real request's
//! `anthropic-beta` header is replayed verbatim: the body may carry beta
//! fields (`clear_at`, `thinking.block_binding`) that are only valid, and
//! only key the cache the same way, under the same betas.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;

use super::{ClaudeModel, VolatileTail, ANTHROPIC_API_URL, ANTHROPIC_VERSION};
use crate::llm::config::{ClaudeCacheTtl, ClaudeConfig};
use crate::usage_log::{CacheTelemetry, UsageKind, UsageLogEntry};

/// The 5-minute TTL; refreshes must land inside it.
const TTL_5M: Duration = Duration::from_secs(300);
/// Never schedule closer to the TTL than this, whatever the setting says.
const MIN_SAFETY_MARGIN: Duration = Duration::from_secs(15);
/// A ping that writes at least this many tokens (and more than it read) means
/// the entry had already expired: stop keeping that session alive.
const MISS_WRITE_TOKENS: i32 = 4096;
const PING_TIMEOUT: Duration = Duration::from_secs(60);

// ── Gating (pure) ──

/// Everything the "should we ping?" decision depends on.
#[derive(Debug, Clone)]
pub struct GateInput {
    pub enabled: bool,
    pub ttl: ClaudeCacheTtl,
    pub has_request: bool,
    pub in_flight: bool,
    pub dead: bool,
    pub quitting: bool,
    pub idle: Duration,
    pub max_idle: Duration,
    pub prefix_tokens: i32,
    pub min_prefix_tokens: u32,
    pub spent_today_usd: f64,
    pub daily_cap_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Disabled,
    NotFiveMinuteTtl,
    NoRequest,
    InFlight,
    Dead,
    Quitting,
    IdleTooLong,
    PrefixTooSmall,
    DailyCapReached,
}

pub fn gate(g: &GateInput) -> Result<(), Skip> {
    if !g.enabled {
        return Err(Skip::Disabled);
    }
    if g.ttl != ClaudeCacheTtl::FiveMinutes {
        return Err(Skip::NotFiveMinuteTtl);
    }
    if g.quitting {
        return Err(Skip::Quitting);
    }
    if !g.has_request {
        return Err(Skip::NoRequest);
    }
    if g.in_flight {
        return Err(Skip::InFlight);
    }
    if g.dead {
        return Err(Skip::Dead);
    }
    if g.idle >= g.max_idle {
        return Err(Skip::IdleTooLong);
    }
    if g.prefix_tokens < g.min_prefix_tokens as i32 {
        return Err(Skip::PrefixTooSmall);
    }
    if g.spent_today_usd >= g.daily_cap_usd {
        return Err(Skip::DailyCapReached);
    }
    Ok(())
}

/// Delay from a request's start to its refresh: the configured value clamped
/// so it always fires before the TTL, with margin for timer/clock jitter.
pub fn refresh_delay(configured_secs: u32) -> Duration {
    Duration::from_secs(configured_secs as u64).min(TTL_5M - MIN_SAFETY_MARGIN)
}

/// A real turn start (a user message, not a tool-result continuation).
/// Trailing `role: "system"` messages (ledger entries, turn-scoped tails)
/// are context, not the turn's opener, so they are skipped.
pub(super) fn is_turn_start(request: &Value, tail: VolatileTail) -> bool {
    let Some(all) = request["messages"].as_array() else {
        return false;
    };
    let real = match tail {
        VolatileTail::SystemMessage | VolatileTail::UserMessage => {
            &all[..all.len().saturating_sub(1)]
        }
        VolatileTail::None | VolatileTail::TextBlock(_) => all,
    };
    let Some(last) = real.iter().rev().find(|m| m["role"] != "system") else {
        return false;
    };
    last["role"] == "user"
        && !last["content"]
            .as_array()
            .is_some_and(|bs| bs.iter().any(|b| b["type"] == "tool_result"))
}

// ── Ping request ──

/// The ping body: the real request body with `stream` off and a minimal
/// `max_tokens`. The cache-keyed fields are not touched, and the
/// `diagnostics` field is dropped (a ping has no previous message to compare
/// against; the header alone without the field is valid).
pub fn build_ping_body(real_body: &str, max_tokens: u32) -> Option<String> {
    let mut v: Value = serde_json::from_str(real_body).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("stream".into(), Value::Bool(false));
    obj.insert("max_tokens".into(), max_tokens.into());
    obj.remove("diagnostics");
    serde_json::to_string(&v).ok()
}

/// Token counts out of a non-streaming response: (input, read, write).
fn parse_usage(resp: &Value) -> (i32, i32, i32) {
    let u = &resp["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0) as i32;
    (
        n("input_tokens"),
        n("cache_read_input_tokens"),
        n("cache_creation_input_tokens"),
    )
}

// ── Per-session registry ──

/// What a ping needs that the request body doesn't carry.
#[derive(Clone)]
struct PingSettings {
    model: String,
    ttl: ClaudeCacheTtl,
    api_key: String,
    enabled: bool,
    max_idle: Duration,
    min_prefix_tokens: u32,
    daily_cap_usd: f64,
    refresh: Duration,
}

struct Entry {
    /// Bumped on every request start / cancel; a timer only acts if it still
    /// holds the current generation.
    generation: u64,
    in_flight: bool,
    dead: bool,
    settings: PingSettings,
    body: Option<String>,
    /// The `anthropic-beta` header the body was sent with.
    betas: Option<String>,
    prefix_tokens: i32,
    request_started: Instant,
    last_user_activity: Instant,
    cur_is_turn_start: bool,
    pings_in_gap: u32,
    bridged: bool,
    telemetry: CacheTelemetry,
    timer: Option<tokio::task::JoinHandle<()>>,
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static R: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static QUITTING: AtomicBool = AtomicBool::new(false);
/// Set once the API rejects `max_tokens: 0` for this key/model combination.
static ZERO_TOKENS_REJECTED: AtomicBool = AtomicBool::new(false);

/// Daily keep-alive spend (local day, all tabs).
struct Spend {
    day: chrono::NaiveDate,
    usd: f64,
}

fn spend() -> &'static Mutex<Spend> {
    static S: OnceLock<Mutex<Spend>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Spend {
            day: chrono::Local::now().date_naive(),
            usd: 0.0,
        })
    })
}

fn spent_today() -> f64 {
    let mut s = spend().lock().unwrap();
    let today = chrono::Local::now().date_naive();
    if s.day != today {
        *s = Spend { day: today, usd: 0.0 };
    }
    s.usd
}

fn add_spend(usd: f64) {
    spent_today();
    spend().lock().unwrap().usd += usd;
}

/// Seed today's spend from the persisted usage log at startup.
pub fn seed_spend(usd_today: f64) {
    spent_today();
    let mut s = spend().lock().unwrap();
    s.usd = s.usd.max(usd_today);
}

/// Where ping entries go; the app drains it into its `UsageLog` signal
/// (a `Signal` can't be written from a background task).
fn sink() -> &'static OnceLock<mpsc::UnboundedSender<UsageLogEntry>> {
    static SINK: OnceLock<mpsc::UnboundedSender<UsageLogEntry>> = OnceLock::new();
    &SINK
}

pub fn install_sink(tx: mpsc::UnboundedSender<UsageLogEntry>) {
    let _ = sink().set(tx);
}

/// App is closing: no more pings.
pub fn shutdown() {
    QUITTING.store(true, Ordering::SeqCst);
    cancel_all();
}

fn bump(e: &mut Entry) {
    e.generation += 1;
    if let Some(t) = e.timer.take() {
        t.abort();
    }
}

/// Tab closed, session deleted, or its tool set / profile changed.
pub fn cancel_session(session_id: &str) {
    if let Ok(mut r) = registry().lock() {
        if let Some(mut e) = r.remove(session_id) {
            bump(&mut e);
        }
    }
}

fn cancel_all() {
    if let Ok(mut r) = registry().lock() {
        r.drain().for_each(|(_, mut e)| bump(&mut e));
    }
}

// ── Hooks called by the connector ──

pub struct RequestStart<'a> {
    pub session_id: &'a str,
    pub config: &'a ClaudeConfig,
    pub api_key: &'a str,
    pub turn_start: bool,
}

fn settings_from(config: &ClaudeConfig, api_key: &str) -> PingSettings {
    PingSettings {
        model: config.model.clone(),
        ttl: config.cache_ttl,
        api_key: api_key.to_string(),
        enabled: config.keepalive_enabled && config.prompt_caching_enabled,
        max_idle: Duration::from_secs(config.keepalive_max_idle_minutes as u64 * 60),
        min_prefix_tokens: config.keepalive_min_prefix_tokens,
        daily_cap_usd: config.keepalive_daily_cap_usd,
        refresh: refresh_delay(config.keepalive_refresh_secs),
    }
}

fn price_telemetry(model: &str, ttl: ClaudeCacheTtl) -> CacheTelemetry {
    let m = ClaudeModel::from_slug(model);
    CacheTelemetry {
        input_price_per_mtok: Some(m.input_price_per_mtok()),
        cache_read_price_per_mtok: Some(m.cache_read_price_per_mtok()),
        ttl: Some(
            match ttl {
                ClaudeCacheTtl::FiveMinutes => "5m",
                ClaudeCacheTtl::OneHour => "1h",
            }
            .to_string(),
        ),
        ..Default::default()
    }
}

/// A real request is about to go out: cancel any pending refresh and mark
/// the session busy.
pub fn begin_request(start: RequestStart) {
    let now = Instant::now();
    let Ok(mut r) = registry().lock() else {
        return;
    };
    let settings = settings_from(start.config, start.api_key);
    let e = r.entry(start.session_id.to_string()).or_insert_with(|| Entry {
        generation: 0,
        in_flight: false,
        dead: false,
        settings: settings.clone(),
        body: None,
        betas: None,
        prefix_tokens: 0,
        request_started: now,
        last_user_activity: now,
        cur_is_turn_start: false,
        pings_in_gap: 0,
        bridged: false,
        telemetry: CacheTelemetry::default(),
        timer: None,
    });
    bump(e);
    let had_previous = e.body.is_some();
    let gap = now.duration_since(e.request_started).as_secs();
    e.settings = settings;
    e.in_flight = true;
    e.dead = false;
    e.cur_is_turn_start = start.turn_start;
    if start.turn_start {
        e.last_user_activity = now;
        e.telemetry = price_telemetry(&e.settings.model, e.settings.ttl);
        e.telemetry.idle_gap_secs = had_previous.then_some(gap);
        e.telemetry.keepalive_bridged = Some(e.bridged);
        e.telemetry.keepalive_pings_in_gap = Some(e.pings_in_gap);
        e.pings_in_gap = 0;
        e.bridged = false;
    }
    e.request_started = now;
}

/// The request completed: record its usage, remember its body and beta
/// header, and re-arm.
pub fn finish_request(
    session_id: &str,
    body: &str,
    betas: Option<&str>,
    input: i32,
    read: Option<i32>,
    created: Option<i32>,
) {
    let (read, created) = (read.unwrap_or(0), created.unwrap_or(0));
    let Ok(mut r) = registry().lock() else {
        return;
    };
    let Some(e) = r.get_mut(session_id) else {
        return;
    };
    e.in_flight = false;
    if e.cur_is_turn_start {
        e.telemetry.first_input_tokens = Some(input);
        e.telemetry.first_cache_read = Some(read);
        e.telemetry.first_cache_write = Some(created);
    } else {
        let t = &mut e.telemetry;
        t.within_turn_requests = Some(t.within_turn_requests.unwrap_or(0) + 1);
        t.within_turn_cache_read = Some(t.within_turn_cache_read.unwrap_or(0) + read as i64);
        t.within_turn_cache_write = Some(t.within_turn_cache_write.unwrap_or(0) + created as i64);
    }
    e.body = Some(body.to_string());
    e.betas = betas.map(str::to_string);
    e.prefix_tokens = read + created;
    let from = e.request_started;
    arm(session_id, e, from);
}

/// The request failed or was cancelled: just clear the busy flag.
pub fn abort_request(session_id: &str) {
    if let Ok(mut r) = registry().lock() {
        if let Some(e) = r.get_mut(session_id) {
            e.in_flight = false;
        }
    }
}

/// Cache telemetry for the turn that just finished, for its usage-log entry.
pub fn take_turn_telemetry(session_id: &str) -> Option<CacheTelemetry> {
    let mut r = registry().lock().ok()?;
    let e = r.get_mut(session_id)?;
    let t = std::mem::take(&mut e.telemetry);
    (t.ttl.is_some()).then_some(t)
}

// ── Timer + ping ──

/// Schedule the next refresh `refresh` after `from` (the TTL counts from the
/// start of the request that last touched the entry).
fn arm(session_id: &str, e: &mut Entry, from: Instant) {
    if !e.settings.enabled || e.settings.ttl != ClaudeCacheTtl::FiveMinutes {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    bump(e);
    let (id, token) = (session_id.to_string(), e.generation);
    let delay = e.settings.refresh.saturating_sub(from.elapsed());
    e.timer = Some(rt.spawn(async move {
        tokio::time::sleep(delay).await;
        run_ping(id, token).await;
    }));
}

/// Decide, snapshot under the lock, then ping outside it.
async fn run_ping(id: String, token: u64) {
    let (body, betas, settings, prefix, idle) = {
        let Ok(r) = registry().lock() else {
            return;
        };
        let Some(e) = r.get(&id).filter(|e| e.generation == token) else {
            return;
        };
        let idle = e.last_user_activity.elapsed();
        let verdict = gate(&GateInput {
            enabled: e.settings.enabled,
            ttl: e.settings.ttl,
            has_request: e.body.is_some(),
            in_flight: e.in_flight,
            dead: e.dead,
            quitting: QUITTING.load(Ordering::SeqCst),
            idle,
            max_idle: e.settings.max_idle,
            prefix_tokens: e.prefix_tokens,
            min_prefix_tokens: e.settings.min_prefix_tokens,
            spent_today_usd: spent_today(),
            daily_cap_usd: e.settings.daily_cap_usd,
        });
        if let Err(skip) = verdict {
            tracing::debug!(session = %id, ?skip, "cache keep-alive: not pinging");
            return;
        }
        (
            e.body.clone().unwrap_or_default(),
            e.betas.clone(),
            e.settings.clone(),
            e.prefix_tokens,
            idle,
        )
    };

    let started = Instant::now();
    let Some(result) = send_ping(&body, betas.as_deref(), &settings).await else {
        tracing::info!(session = %id, "cache keep-alive: ping failed; stopping for this tab");
        return;
    };
    let (input, read, created) = result;
    let m = ClaudeModel::from_slug(&settings.model);
    let cost = (input as f64 * m.input_price_per_mtok()
        + read as f64 * m.cache_read_price_per_mtok()
        + created as f64 * m.cache_write_price_per_mtok(settings.ttl))
        / 1_000_000.0;
    add_spend(cost);
    let missed = created >= MISS_WRITE_TOKENS && created > read;
    tracing::info!(
        session = %id, prefix, read, created, cost, idle_secs = idle.as_secs(),
        "cache keep-alive ping{}", if missed { " MISSED (entry had expired); stopping" } else { "" }
    );
    if let Some(tx) = sink().get() {
        let _ = tx.send(UsageLogEntry {
            timestamp: chrono::Utc::now(),
            session_id: id.clone(),
            model: settings.model.clone(),
            prompt_tokens: input + read + created,
            completion_tokens: 0,
            total_tokens: input + read + created,
            thoughts_tokens: None,
            cached_content_tokens: Some(read),
            cache_creation_tokens: Some(created),
            cost: Some(cost),
            kind: UsageKind::Keepalive,
            cache: Some(CacheTelemetry {
                prefix_tokens: Some(prefix),
                idle_secs: Some(idle.as_secs()),
                ..price_telemetry(&settings.model, settings.ttl)
            }),
        });
    }

    let Ok(mut r) = registry().lock() else {
        return;
    };
    let Some(e) = r.get_mut(&id).filter(|e| e.generation == token) else {
        return; // a real request started meanwhile and owns the timer
    };
    if missed {
        e.dead = true;
        return;
    }
    e.pings_in_gap += 1;
    e.bridged = true;
    e.prefix_tokens = read + created;
    arm(&id, e, started);
}

/// The ping's HTTP request: same endpoint, version and beta header as the
/// real request it replays.
fn ping_request(
    client: &reqwest::Client,
    body: String,
    api_key: &str,
    betas: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut req = client
        .post(ANTHROPIC_API_URL)
        .header("content-type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION);
    if let Some(b) = betas {
        req = req.header("anthropic-beta", b);
    }
    req.body(body)
}

/// POST the ping; falls back to `max_tokens: 1` if the API rejects 0.
/// Returns (input, read, write) tokens, or None on any failure.
async fn send_ping(real_body: &str, betas: Option<&str>, s: &PingSettings) -> Option<(i32, i32, i32)> {
    let client = reqwest::Client::builder().timeout(PING_TIMEOUT).build().ok()?;
    let attempts: &[u32] = if ZERO_TOKENS_REJECTED.load(Ordering::SeqCst) {
        &[1]
    } else {
        &[0, 1]
    };
    for &max_tokens in attempts {
        let body = build_ping_body(real_body, max_tokens)?;
        let resp = ping_request(&client, body, &s.api_key, betas).send().await.ok()?;
        let status = resp.status();
        let text = resp.text().await.ok()?;
        if status.is_success() {
            return Some(parse_usage(&serde_json::from_str(&text).ok()?));
        }
        if status.as_u16() == 400 && max_tokens == 0 {
            tracing::warn!(
                "cache keep-alive: max_tokens 0 rejected, falling back to 1: {}",
                crate::log_utils::truncate_for_log(&text, crate::log_utils::LOG_BODY_MAX_CHARS)
            );
            ZERO_TOKENS_REJECTED.store(true, Ordering::SeqCst);
            continue;
        }
        tracing::warn!("cache keep-alive: HTTP {status}: {}", crate::log_utils::truncate_for_log(&text, crate::log_utils::LOG_BODY_MAX_CHARS));
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_gate() -> GateInput {
        GateInput {
            enabled: true,
            ttl: ClaudeCacheTtl::FiveMinutes,
            has_request: true,
            in_flight: false,
            dead: false,
            quitting: false,
            idle: Duration::from_secs(60),
            max_idle: Duration::from_secs(1800),
            prefix_tokens: 20_000,
            min_prefix_tokens: 8_000,
            spent_today_usd: 0.1,
            daily_cap_usd: 1.0,
        }
    }

    #[test]
    fn gate_passes_when_all_conditions_hold() {
        assert_eq!(gate(&ok_gate()), Ok(()));
    }

    #[test]
    fn gate_blocks_each_condition() {
        let cases: Vec<(Box<dyn Fn(&mut GateInput)>, Skip)> = vec![
            (Box::new(|g| g.enabled = false), Skip::Disabled),
            (Box::new(|g| g.ttl = ClaudeCacheTtl::OneHour), Skip::NotFiveMinuteTtl),
            (Box::new(|g| g.has_request = false), Skip::NoRequest),
            (Box::new(|g| g.in_flight = true), Skip::InFlight),
            (Box::new(|g| g.dead = true), Skip::Dead),
            (Box::new(|g| g.quitting = true), Skip::Quitting),
            (Box::new(|g| g.idle = Duration::from_secs(1800)), Skip::IdleTooLong),
            (Box::new(|g| g.prefix_tokens = 7_999), Skip::PrefixTooSmall),
            (Box::new(|g| g.spent_today_usd = 1.0), Skip::DailyCapReached),
        ];
        for (mutate, want) in cases {
            let mut g = ok_gate();
            mutate(&mut g);
            assert_eq!(gate(&g), Err(want));
        }
    }

    #[test]
    fn refresh_delay_always_fires_before_the_ttl() {
        assert_eq!(refresh_delay(255), Duration::from_secs(255));
        assert_eq!(refresh_delay(10_000), Duration::from_secs(285));
    }

    #[test]
    fn turn_start_ignores_tool_result_continuations() {
        let user = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert!(is_turn_start(&user, VolatileTail::None));
        let cont = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "1", "name": "x", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1", "content": "ok"}]},
        ]});
        assert!(!is_turn_start(&cont, VolatileTail::None));
        let with_tail = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "system", "content": "volatile"},
        ]});
        assert!(is_turn_start(&with_tail, VolatileTail::SystemMessage));
    }

    #[test]
    fn ping_body_changes_only_stream_max_tokens_and_diagnostics() {
        let real = json!({
            "model": "m", "max_tokens": 32000, "stream": true,
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}],
            "thinking": {"type": "adaptive"},
            "tools": [{"name": "t"}],
            "tool_choice": {"type": "auto"},
            "diagnostics": {"previous_message_id": null},
        });
        let ping: Value = serde_json::from_str(&build_ping_body(&real.to_string(), 0).unwrap()).unwrap();
        assert_eq!(ping["stream"], false);
        assert_eq!(ping["max_tokens"], 0);
        assert!(ping.get("diagnostics").is_none());
        for k in ["model", "system", "messages", "thinking", "tools", "tool_choice"] {
            assert_eq!(ping[k], real[k], "{k} must be untouched");
        }
    }

    #[test]
    fn ping_prefix_fingerprint_matches_the_real_request() {
        use crate::llm::claude::{fingerprint_request, ClaudeConnector};
        use crate::llm::types::{ChatRole, ContentBlock, ChatMessage, LlmPrompt};
        let c = ClaudeConnector::new(ClaudeConfig {
            model: "claude-sonnet-4-6".into(),
            extended_thinking: true,
            ..ClaudeConfig::default()
        });
        let prompt = LlmPrompt {
            system: Some("stable\nvolatile".into()),
            system_volatile_offset: Some(6),
            turn_scoped: Vec::new(),
            history_rewritten: false,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::Text { text: "hello".into() }],
            }],
            tools: vec![],
        };
        let (real, tail) = c.build_request(&prompt, true);
        let body = serde_json::to_string(&real).unwrap();
        let ping: Value = serde_json::from_str(&build_ping_body(&body, 0).unwrap()).unwrap();
        assert_eq!(
            format!("{:?}", fingerprint_request(&real, tail)),
            format!("{:?}", fingerprint_request(&ping, tail)),
        );
        for k in ["system", "messages", "thinking"] {
            assert_eq!(real[k], ping[k]);
        }
    }

    #[test]
    fn turn_start_skips_trailing_system_messages() {
        let opener = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "text", "text": "L1"}]},
            {"role": "system", "content": "T1", "clear_at": "next_user_message"},
        ]});
        assert!(is_turn_start(&opener, VolatileTail::None));
        let cont = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "1", "name": "x", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1", "content": "ok"}]},
            {"role": "system", "content": "T2", "clear_at": "next_user_message"},
        ]});
        assert!(!is_turn_start(&cont, VolatileTail::None));
    }

    #[test]
    fn ping_replays_beta_fields_and_the_beta_header() {
        let real = json!({
            "model": "claude-opus-5-5", "max_tokens": 32000, "stream": true,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "system", "content": "T1", "clear_at": "next_user_message"},
            ],
            "thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
        });
        let ping: Value = serde_json::from_str(&build_ping_body(&real.to_string(), 0).unwrap()).unwrap();
        assert_eq!(ping["messages"], real["messages"]);
        assert_eq!(ping["thinking"], real["thinking"]);

        let betas = "mid-conversation-system-clear-at-2026-08-21,thinking-binding-controls-2026-08-01";
        let client = reqwest::Client::new();
        let req = ping_request(&client, "{}".into(), "k", Some(betas)).build().unwrap();
        assert_eq!(req.headers()["anthropic-beta"], betas);
        assert_eq!(req.headers()["anthropic-version"], ANTHROPIC_VERSION);
        let req = ping_request(&client, "{}".into(), "k", None).build().unwrap();
        assert!(req.headers().get("anthropic-beta").is_none());

        // The registry keeps the header the real request went out with.
        let id = "ka-test-betas";
        begin_request(start(id));
        finish_request(id, "{}", Some(betas), 1, Some(10_000), Some(0));
        assert_eq!(
            registry().lock().unwrap().get(id).unwrap().betas.as_deref(),
            Some(betas)
        );
        cancel_session(id);
    }

    #[test]
    fn ping_header_matches_what_the_connector_sends() {
        use crate::llm::claude::ClaudeConnector;
        use crate::llm::types::{ChatMessage, ChatRole, ContentBlock, LlmPrompt};
        let c = ClaudeConnector::new(ClaudeConfig {
            model: "claude-opus-5-5".into(),
            ..ClaudeConfig::default()
        });
        let text = |role, t: &str| ChatMessage {
            role,
            content: vec![ContentBlock::Text { text: t.into() }],
        };
        let prompt = LlmPrompt {
            system: Some("stable".into()),
            system_volatile_offset: None,
            turn_scoped: vec![1],
            history_rewritten: true,
            messages: vec![text(ChatRole::User, "hello"), text(ChatRole::System, "T1")],
            tools: vec![],
        };
        let (real, _) = c.build_request(&prompt, true);
        let header = c.anthropic_beta_header(&real);
        let body = serde_json::to_string(&real).unwrap();
        let ping: Value = serde_json::from_str(&build_ping_body(&body, 0).unwrap()).unwrap();
        // Same body-derived betas as the real request, so replaying the
        // stored header is consistent with the replayed body.
        assert_eq!(c.anthropic_beta_header(&ping), header);
    }

    fn start(id: &str) -> RequestStart<'_> {
        // Leaked: tests only, keeps the borrow trivial.
        let cfg: &'static ClaudeConfig = Box::leak(Box::new(ClaudeConfig::default()));
        RequestStart { session_id: id, config: cfg, api_key: "k", turn_start: true }
    }

    #[tokio::test]
    async fn new_request_and_cancel_invalidate_pending_timers() {
        let id = "ka-test-cancel";
        begin_request(start(id));
        finish_request(id, "{}", None, 1, Some(10_000), Some(0));
        let g1 = registry().lock().unwrap().get(id).unwrap().generation;
        assert!(registry().lock().unwrap().get(id).unwrap().timer.is_some());
        begin_request(start(id));
        let e_gen = registry().lock().unwrap().get(id).unwrap().generation;
        assert!(e_gen > g1, "a new request must supersede the armed timer");
        assert!(registry().lock().unwrap().get(id).unwrap().timer.is_none());
        cancel_session(id);
        assert!(registry().lock().unwrap().get(id).is_none());
    }
}
