//! The localhost hook listener.
//!
//! A deliberately tiny hyper 1.x HTTP/1 server (hyper + hyper-util are
//! already in the dependency graph via reqwest — no new framework). Binds
//! `127.0.0.1` only; routes exactly `POST /fleet/{token}/{event}` and rejects
//! everything else. The per-install token is path-embedded, so a request
//! without it is a 403 before any parsing happens.
//!
//! Ingest per request: parse (async body read first), then — never holding a
//! lock across an await (P-010 spirit) — append the event row, reduce into
//! [`super::FleetState`], persist changed rows, and poke the UI drain loop.
//!
//! # Permission requests
//!
//! Per the verified hook contract a 2xx with an **empty body** means "no
//! decision", and an allow / deny rides
//! `hookSpecificOutput.decision.behavior`. Every request is recorded as the
//! session's pending gate, then answered one of two ways:
//!
//! - **Interactive sessions** (the default) are answered at once with the
//!   empty body: Claude Code shows its own permission prompt in the
//!   terminal, with the choices the Fleet can't offer (always allow, tell
//!   Claude what to do instead, AskUserQuestion options), and the Fleet
//!   takes the person there.
//! - **Headless runs Hobbes dispatched** (`claude -p`, stdin closed — see
//!   `todo::dispatch`; identified by the row's `dispatched_todo`,
//!   [`super::FleetSession::is_headless_dispatch`]) have no terminal to
//!   prompt in, so the response is HELD: the handler registers a oneshot
//!   keyed by the request id and waits for the Fleet / Planner Approve or
//!   Deny ([`FleetShared::resolve_gate`]). A timer gives up
//!   [`GATE_TIMEOUT_MARGIN_SECS`] before the configured hook timeout and
//!   answers with the empty body — for a headless run that means the tool
//!   is not permitted and the run carries on without it.
//!
//! (Hobbes's own chat tabs keep their in-app approvals; those never come
//! through here.)
//!
//! # Terminal hints
//!
//! `SessionStart` and `UserPromptSubmit` hooks forward the terminal's
//! environment as `X-Hobbes-*` headers (`hooks_config`). They are read
//! before the body is consumed, validated strictly
//! (`terminal_focus::hint_from_headers`; values are never logged), and folded
//! into the session's `terminal` for click-to-terminal.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use super::events::{parse_event, FleetEvent};
use super::hooks_config::{GATE_TIMEOUT_MARGIN_SECS, PERMISSION_HOOK_TIMEOUT_SECS};
use super::terminal_focus::{hint_from_headers, TerminalHint};
use super::{resolve_gate_in_state, store, sweep_stale, FleetShared, GateOutcome};

/// Default fixed port; on bind failure the listener falls back to an
/// ephemeral port (`:0`) and persists whatever it got in meta so hook
/// registration follows.
pub const DEFAULT_FLEET_PORT: u16 = 43917;

/// Largest accepted request body. Hook payloads are small; `tool_input` can
/// carry file contents, so leave slack without letting anything stream us a
/// gigabyte on loopback.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Cadence of the staleness sweep.
const SWEEP_INTERVAL_SECS: u64 = 60;

#[derive(Clone)]
pub struct FleetServerConfig {
    /// Port to try; 0 binds ephemeral.
    pub port: u16,
    pub token: String,
    /// How long a headless run's held gate waits for an Approve / Deny
    /// before resolving as passthrough. Production: hook timeout minus the
    /// safety margin.
    pub gate_hold: Duration,
}

impl FleetServerConfig {
    pub fn new(port: u16, token: String) -> Self {
        Self {
            port,
            token,
            gate_hold: Duration::from_secs(
                PERMISSION_HOOK_TIMEOUT_SECS.saturating_sub(GATE_TIMEOUT_MARGIN_SECS),
            ),
        }
    }
}

/// A running listener. Dropping the handle does not stop it — call
/// [`FleetServer::shutdown`].
pub struct FleetServer {
    pub port: u16,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl FleetServer {
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Bind and serve. Tries `cfg.port`, falls back to ephemeral; returns the
/// actually-bound port on the handle.
pub async fn start(shared: Arc<FleetShared>, cfg: FleetServerConfig) -> Result<FleetServer, String> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "fleet: port {} unavailable ({}), falling back to an ephemeral port",
                cfg.port,
                e
            );
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|e| format!("fleet listener bind failed: {e}"))?
        }
    };
    let port = listener
        .local_addr()
        .map_err(|e| format!("fleet listener addr: {e}"))?
        .port();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Staleness sweep: periodic, stops with the server.
    {
        let shared = shared.clone();
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)) => {
                        let changed = {
                            let now = chrono::Utc::now();
                            let mut state = shared.state.lock().expect("fleet state lock poisoned");
                            let mut changed = sweep_stale(&mut state, now);
                            changed.extend(crate::fleet::sweep_retired(&mut state, now));
                            // Time passing (not just events) can end an
                            // engaged period — this tick is what closes
                            // coverage after a silent walk-away.
                            store::save_coverage(&crate::fleet::sync_coverage(&mut state, now));
                            changed
                        };
                        if !changed.is_empty() {
                            store::persist_sessions(&changed);
                            shared.poke();
                        }

                        // Dispatched runs that finished: hand their result
                        // back to the chat that launched them. Ended rows
                        // have left the live map, so today's store rows come
                        // along — a run that finished before the last local
                        // midnight is stale news and deliberately drops.
                        let now = chrono::Utc::now();
                        let ended = store::sessions_active_on(chrono::Local::now().date_naive());
                        let sweep = {
                            let mut state = shared.state.lock().expect("fleet state lock poisoned");
                            crate::fleet::sweep_dispatch_continuations(
                                &mut state,
                                &ended,
                                now,
                                crate::fleet::dispatch_auto_continue(),
                            )
                        };
                        if !sweep.is_empty() {
                            store::persist_sessions(&sweep.live_rows);
                            for id in &sweep.ended_ids {
                                store::merge_session(id, |row| row.continuation_sent = true);
                            }
                            shared.push_continuations(sweep.continuations);
                            shared.poke();
                        }
                        // Truth-on-disk reconciliation: catches events lost
                        // while Hobbes wasn't running (file IO off the lock).
                        let shared_for_reconcile = shared.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::fleet::reconcile::reconcile_all(&shared_for_reconcile);
                        });
                    }
                }
            }
        });
    }

    // Accept loop.
    {
        let shared = shared.clone();
        let cfg = cfg.clone();
        let mut rx = shutdown_rx;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!("fleet: accept failed: {e}");
                                continue;
                            }
                        };
                        let shared = shared.clone();
                        let cfg = cfg.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req| {
                                handle(shared.clone(), cfg.clone(), req)
                            });
                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service)
                                .await
                            {
                                tracing::debug!("fleet: connection error: {e}");
                            }
                        });
                    }
                }
            }
            tracing::info!("fleet: listener on port {} stopped", port);
        });
    }

    tracing::info!("fleet: listening on 127.0.0.1:{}", port);
    Ok(FleetServer {
        port,
        shutdown: shutdown_tx,
    })
}

fn respond(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    builder
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("static response builds")
}

/// The verified PermissionRequest decision shape.
fn decision_body(outcome: GateOutcome) -> String {
    match outcome {
        GateOutcome::Allow => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" }
            }
        })
        .to_string(),
        GateOutcome::Deny => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "deny",
                    "message": "Denied from Hobbes."
                }
            }
        })
        .to_string(),
        // Empty 2xx body == no decision.
        GateOutcome::Passthrough => String::new(),
    }
}

async fn handle(
    shared: Arc<FleetShared>,
    cfg: FleetServerConfig,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // Route: POST /fleet/{token}/{event}
    if req.method() != hyper::Method::POST {
        return Ok(respond(StatusCode::METHOD_NOT_ALLOWED, ""));
    }
    let path = req.uri().path().to_string();
    let mut parts = path.trim_start_matches('/').split('/');
    let (ns, token) = (parts.next(), parts.next());
    if ns != Some("fleet") {
        return Ok(respond(StatusCode::NOT_FOUND, ""));
    }
    if token != Some(cfg.token.as_str()) {
        return Ok(respond(StatusCode::FORBIDDEN, ""));
    }

    // Terminal hints ride the headers, so read them before the body is
    // consumed. Only the hint events send them; a rejected value is logged
    // by header name only.
    let hint: Option<TerminalHint> = {
        let event = parts.next().unwrap_or("");
        if super::hooks_config::TERMINAL_HINT_EVENTS.contains(&event) {
            let headers = req.headers();
            let (hint, rejected) = hint_from_headers(|name| headers.get(name).map(|v| v.as_bytes()));
            for name in rejected {
                tracing::debug!("fleet: rejected terminal hint header {name}");
            }
            Some(hint)
        } else {
            None
        }
    };

    // Body — everything async happens before any lock is taken.
    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::warn!("fleet: body read failed: {e}");
            return Ok(respond(StatusCode::BAD_REQUEST, ""));
        }
    };
    if body.len() > MAX_BODY_BYTES {
        return Ok(respond(StatusCode::PAYLOAD_TOO_LARGE, ""));
    }

    let event = match parse_event(&body) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!("fleet: rejected hook payload: {e}");
            return Ok(respond(StatusCode::BAD_REQUEST, ""));
        }
    };

    // The start time that picks the right `claude` process later: a fresh
    // process (startup/resume/fork), not `/clear` or a compaction inside an
    // already-running one.
    let hint = hint.map(|mut h| {
        if let FleetEvent::SessionStart { source, .. } = &event {
            if !matches!(source.as_str(), "clear" | "compact") {
                h.started_at = Some(chrono::Utc::now());
            }
        }
        h
    });

    // Append the raw event, reduce, persist — synchronous section.
    // ToolActivity is a high-volume heartbeat: it updates state but is not
    // worth a `fleet_events` row per tool call.
    if !matches!(event, FleetEvent::ToolActivity { .. }) {
        store::append_event(
            event.session_id(),
            event.event_name(),
            &String::from_utf8_lossy(&body),
        );
    }
    let now = chrono::Utc::now();
    let transcript_path = crate::fleet::events::transcript_path_from_body(&body);
    let prompt_id = crate::fleet::events::prompt_id_from_body(&body);
    let held = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        // Revival: an event for a session that retired (or resumed after
        // SessionEnd) rejoins with its stored row, keeping banked time.
        if !state.sessions.contains_key(event.session_id()) {
            if let Some(mut row) = store::load_session(event.session_id()) {
                row.ended_at = None;
                row.pending_gate = None;
                state.sessions.insert(row.id.clone(), row);
            }
        }
        let mut changed =
            crate::fleet::reduce_with_prompt(&mut state, &event, prompt_id.as_deref(), now);
        crate::fleet::note_transcript_and_dirty(
            &mut state,
            &mut changed,
            &event,
            transcript_path.as_deref(),
            now,
        );
        if let Some(hint) = &hint {
            crate::fleet::apply_terminal_hint(&mut state, &mut changed, event.session_id(), hint);
        }
        store::persist_sessions(&changed);
        store::save_coverage(&crate::fleet::sync_coverage(&mut state, now));
        // A headless dispatched run's permission request is held for the
        // in-app Approve / Deny. Register the responder BEFORE the state
        // becomes visible (the poke), so a UI click racing the snapshot can
        // never miss it.
        match &event {
            FleetEvent::PermissionRequest { request_id, .. }
                if state
                    .sessions
                    .get(event.session_id())
                    .is_some_and(|s| s.is_headless_dispatch()) =>
            {
                Some((request_id.clone(), shared.register_gate(request_id)))
            }
            _ => None,
        }
    };
    shared.poke();

    // Title refresh at ingest: the card should match the Claude Code session
    // title (terminal tab name) without waiting for the brief debounce — a
    // small file read, off the response path.
    // UserPromptSubmit carries the tab title directly — instant, no file
    // read; the transcript path below covers the other events.
    if matches!(event, FleetEvent::PromptSubmit { .. }) {
        if let Some(title) = crate::fleet::events::session_title_from_body(&body) {
            shared.set_session_title(event.session_id(), &title);
        }
    }
    if matches!(
        event,
        FleetEvent::SessionStart { .. } | FleetEvent::Stop { .. }
    ) {
        if let Some(path) = transcript_path.clone() {
            let shared_for_title = shared.clone();
            let sid = event.session_id().to_string();
            tokio::spawn(async move {
                let title = tokio::task::spawn_blocking(move || {
                    crate::fleet::transcript::read_tail(
                        &path,
                        crate::fleet::transcript::TITLE_TAIL_BYTES,
                    )
                    .ok()
                    .and_then(|tail| crate::fleet::transcript::latest_ai_title(&tail))
                })
                .await
                .ok()
                .flatten();
                if let Some(t) = title {
                    shared_for_title.set_session_title(&sid, &t);
                }
            });
        }
    }

    // Everything except a headless run's held gate answers immediately with
    // an empty 2xx — for an interactive permission request that is "no
    // decision", and Claude Code shows its own prompt in the terminal.
    let Some((request_id, rx)) = held else {
        return Ok(respond(StatusCode::OK, ""));
    };

    // Hold for the UI, or give up shortly before the hook's own timeout.
    let outcome = tokio::select! {
        decision = rx => decision.unwrap_or(GateOutcome::Passthrough),
        _ = tokio::time::sleep(cfg.gate_hold) => {
            // Drop the now-dead responder so a late UI click is a no-op.
            let _ = shared.take_gate(&request_id);
            GateOutcome::Passthrough
        }
    };
    finish_gate(&shared, event.session_id(), &request_id, outcome);
    Ok(respond(StatusCode::OK, &decision_body(outcome)))
}

/// Post-decision bookkeeping for a held gate: state transition,
/// persistence, decision log, UI poke. Synchronous — called after all
/// awaits are done.
fn finish_gate(shared: &FleetShared, session_id: &str, request_id: &str, outcome: GateOutcome) {
    let now = chrono::Utc::now();
    let changed = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        resolve_gate_in_state(&mut state, request_id, outcome, now)
    };
    if let Some(row) = changed {
        store::persist_sessions(std::slice::from_ref(&row));
    }
    store::append_event(
        session_id,
        "GateDecision",
        &serde_json::json!({ "request_id": request_id, "outcome": outcome.as_str() }).to_string(),
    );
    shared.poke();
}

// ── Identity (port + token in session_store meta) ───────────────────────────

/// The port + per-install token hooks should point at, creating and
/// persisting them on first use. Callable whether or not the listener is
/// running (Connect needs it either way).
pub fn ensure_identity() -> (u16, String) {
    let port = crate::session_store::meta_get(super::META_FLEET_PORT)
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FLEET_PORT);
    let token = match crate::session_store::meta_get(super::META_FLEET_TOKEN) {
        Some(t) if !t.is_empty() => t,
        _ => {
            let t = uuid::Uuid::new_v4().simple().to_string();
            if let Err(e) = crate::session_store::meta_set(super::META_FLEET_TOKEN, &t) {
                tracing::error!("fleet: failed to persist token: {e}");
            }
            t
        }
    };
    (port, token)
}

/// Start the production listener from persisted identity, updating the meta
/// port if the bind fell back to an ephemeral one (so a later Connect
/// registers matching URLs).
pub async fn start_from_meta(shared: Arc<FleetShared>) -> Result<FleetServer, String> {
    let (port, token) = ensure_identity();
    let server = start(shared, FleetServerConfig::new(port, token)).await?;
    if server.port != port {
        if let Err(e) =
            crate::session_store::meta_set(super::META_FLEET_PORT, &server.port.to_string())
        {
            tracing::error!("fleet: failed to persist fallback port: {e}");
        }
    } else if crate::session_store::meta_get(super::META_FLEET_PORT).is_none() {
        let _ = crate::session_store::meta_set(super::META_FLEET_PORT, &port.to_string());
    }
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{AttentionKind, FleetStatus};

    /// Spin up a listener on an ephemeral port.
    async fn test_server() -> (Arc<FleetShared>, FleetServer, String) {
        test_server_with_hold(5000).await
    }

    /// A listener whose held gates (headless dispatched runs) give up after
    /// `hold_ms`.
    async fn test_server_with_hold(hold_ms: u64) -> (Arc<FleetShared>, FleetServer, String) {
        let shared = Arc::new(FleetShared::new());
        let mut cfg = FleetServerConfig::new(0, "testtok".into());
        cfg.gate_hold = Duration::from_millis(hold_ms);
        let server = start(shared.clone(), cfg).await.unwrap();
        let base = format!("http://127.0.0.1:{}/fleet/testtok", server.port);
        (shared, server, base)
    }

    fn start_body(id: &str) -> String {
        serde_json::json!({
            "session_id": id,
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "SessionStart",
            "source": "startup"
        })
        .to_string()
    }

    fn permission_body(id: &str) -> String {
        serde_json::json!({
            "session_id": id,
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "rm -rf node_modules" }
        })
        .to_string()
    }

    #[tokio::test]
    async fn ingests_events_and_rejects_bad_tokens() {
        let (shared, server, base) = test_server().await;
        let client = reqwest::Client::new();

        // Wrong token → 403, no state.
        let bad = client
            .post(format!(
                "http://127.0.0.1:{}/fleet/WRONG/SessionStart",
                server.port
            ))
            .body(start_body("s1"))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 403);
        assert!(shared.snapshot().sessions.is_empty());

        // Wrong path → 404; GET → 405.
        assert_eq!(
            client
                .post(format!("http://127.0.0.1:{}/nope/x/y", server.port))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            client
                .get(format!("{base}/SessionStart"))
                .send()
                .await
                .unwrap()
                .status(),
            405
        );

        // Garbage body → 400.
        assert_eq!(
            client
                .post(format!("{base}/SessionStart"))
                .body("not json")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        // Good event → 200 empty body, state updated.
        let ok = client
            .post(format!("{base}/SessionStart"))
            .body(start_body("s1"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert!(ok.bytes().await.unwrap().is_empty());
        let snap = shared.snapshot();
        assert_eq!(snap.sessions["s1"].status, FleetStatus::Working);
        assert_eq!(snap.sessions["s1"].name, "hobbes");

        server.shutdown();
    }

    /// Seed a row the way `precreate_dispatched_session` does for a headless
    /// HOBBES_DISPATCH run.
    fn seed_headless(shared: &FleetShared, id: &str) {
        let mut row = crate::fleet::FleetSession::new(id, "/Users/x/dev/hobbes", chrono::Utc::now());
        row.dispatched_todo = Some("todo-1".into());
        assert!(row.is_headless_dispatch());
        shared.state.lock().unwrap().sessions.insert(id.into(), row);
    }

    /// Answer the session's gate from "the UI" as soon as it shows up.
    fn ui_answers(shared: &Arc<FleetShared>, id: &'static str, allow: bool) -> tokio::task::JoinHandle<()> {
        let shared = shared.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                let gate = shared.snapshot().sessions.get(id).and_then(|s| s.pending_gate.clone());
                if let Some(gate) = gate {
                    assert!(shared.gate_is_held(&gate.request_id));
                    assert!(shared.resolve_gate(&gate.request_id, allow));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("gate never appeared in state");
        })
    }

    #[tokio::test]
    async fn headless_dispatched_gate_is_held_and_approved_from_the_ui() {
        let (shared, server, base) = test_server().await;
        seed_headless(&shared, "h1");
        let ui = ui_answers(&shared, "h1", true);
        let resp = reqwest::Client::new()
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("h1"))
            .send()
            .await
            .unwrap();
        ui.await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "verified contract: decision.behavior, not permissionDecision"
        );
        assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        let snap = shared.snapshot();
        assert_eq!(snap.sessions["h1"].status, FleetStatus::Working);
        assert!(snap.sessions["h1"].pending_gate.is_none());
        server.shutdown();
    }

    #[tokio::test]
    async fn headless_dispatched_gate_denied_from_the_ui() {
        let (shared, server, base) = test_server().await;
        seed_headless(&shared, "h1");
        let ui = ui_answers(&shared, "h1", false);
        let resp = reqwest::Client::new()
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("h1"))
            .send()
            .await
            .unwrap();
        ui.await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(body["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .unwrap()
            .contains("Hobbes"));
        assert!(shared.snapshot().sessions["h1"].pending_gate.is_none());
        server.shutdown();
    }

    #[tokio::test]
    async fn unanswered_headless_gate_times_out_to_an_empty_passthrough() {
        // 100ms stands in for the production 110s (120s hook timeout minus
        // the 10s margin).
        let (shared, server, base) = test_server_with_hold(100).await;
        seed_headless(&shared, "h1");
        let started = std::time::Instant::now();
        let resp = reqwest::Client::new()
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("h1"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100), "it was held");
        assert_eq!(resp.status(), 200);
        assert!(resp.bytes().await.unwrap().is_empty(), "passthrough is an EMPTY 2xx");
        let snap = shared.snapshot();
        assert!(snap.sessions["h1"].pending_gate.is_none());
        assert_eq!(snap.sessions["h1"].status, FleetStatus::Working, "the run carries on");
        // A late UI click on the dead gate is a no-op.
        assert!(!shared.resolve_gate("whatever", true));
        server.shutdown();
    }

    #[test]
    fn gate_timing_margin_matches_the_hook_contract() {
        let cfg = FleetServerConfig::new(0, "t".into());
        assert_eq!(
            cfg.gate_hold,
            Duration::from_secs(PERMISSION_HOOK_TIMEOUT_SECS - GATE_TIMEOUT_MARGIN_SECS)
        );
        assert!(cfg.gate_hold < Duration::from_secs(PERMISSION_HOOK_TIMEOUT_SECS));
    }

    #[tokio::test]
    async fn interactive_permission_requests_are_never_held() {
        let (shared, server, base) = test_server().await;
        let client = reqwest::Client::new();

        // Answered at once (well inside the client timeout) with the empty
        // "no decision" body, so Claude Code shows its own terminal prompt.
        let resp = client
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("s1"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .expect("a permission request must not wait on Hobbes");
        assert_eq!(resp.status(), 200);
        assert!(
            resp.bytes().await.unwrap().is_empty(),
            "passthrough is a 2xx with an EMPTY body — the terminal prompt takes over"
        );

        // The session needs the user, and the request stays on the row for
        // display until the session moves on.
        let snap = shared.snapshot();
        assert_eq!(
            snap.sessions["s1"].status,
            FleetStatus::NeedsAttention(AttentionKind::Gate)
        );
        let gate = snap.sessions["s1"].pending_gate.as_ref().expect("kept for display");
        assert!(!shared.gate_is_held(&gate.request_id), "no responder for an interactive session");
        assert!(!shared.resolve_gate(&gate.request_id, true));
        assert_eq!(gate.tool_name, "Bash");
        assert!(gate.input_summary.contains("rm -rf node_modules"));

        // Answering in the terminal lets the tool run: the gate clears.
        client
            .post(format!("{base}/PostToolUse"))
            .body(
                serde_json::json!({
                    "session_id": "s1",
                    "cwd": "/Users/x/dev/hobbes",
                    "hook_event_name": "PostToolUse",
                    "tool_name": "Bash"
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        let snap = shared.snapshot();
        assert_eq!(snap.sessions["s1"].status, FleetStatus::Working);
        assert!(snap.sessions["s1"].pending_gate.is_none());

        server.shutdown();
    }

    #[tokio::test]
    async fn terminal_hint_headers_are_validated_and_merged() {
        let (shared, server, base) = test_server().await;
        let client = reqwest::Client::new();
        let uuid = "68F0CAC3-3C5D-482C-AC4A-BE7854943045";

        let r = client
            .post(format!("{base}/SessionStart"))
            .header("X-Hobbes-Term", "iTerm.app")
            .header("X-Hobbes-App", "com.googlecode.iterm2")
            .header("X-Hobbes-Iterm", format!("w1t1p0:{uuid}"))
            .header("X-Hobbes-Tmux-Pane", "not-a-pane")
            .header("X-Hobbes-Wt", "")
            .body(start_body("s1"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let hint = shared.snapshot().sessions["s1"].terminal.clone().expect("hint stored");
        assert_eq!(hint.term_program.as_deref(), Some("iTerm.app"));
        assert_eq!(hint.bundle_id.as_deref(), Some("com.googlecode.iterm2"));
        assert_eq!(hint.iterm_session, Some(format!("w1t1p0:{uuid}")));
        assert_eq!(hint.tmux_pane, None, "invalid values are dropped");
        assert_eq!(hint.wt_session, None, "empty values are None");
        assert!(hint.started_at.is_some(), "a startup records the process start");

        // A later prompt with a new tab id replaces the old one; values it
        // doesn't send are kept.
        let r = client
            .post(format!("{base}/UserPromptSubmit"))
            .header("X-Hobbes-Iterm", format!("w1t3p0:{uuid}"))
            .body(
                serde_json::json!({
                    "session_id": "s1",
                    "cwd": "/Users/x/dev/hobbes",
                    "hook_event_name": "UserPromptSubmit",
                    "prompt": "hi"
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let hint = shared.snapshot().sessions["s1"].terminal.clone().unwrap();
        assert_eq!(hint.iterm_session, Some(format!("w1t3p0:{uuid}")));
        assert_eq!(hint.bundle_id.as_deref(), Some("com.googlecode.iterm2"));

        // Headers on other events are ignored.
        let r = client
            .post(format!("{base}/Stop"))
            .header("X-Hobbes-App", "com.apple.Terminal")
            .body(
                serde_json::json!({
                    "session_id": "s1",
                    "cwd": "/Users/x/dev/hobbes",
                    "hook_event_name": "Stop"
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            shared.snapshot().sessions["s1"].terminal.as_ref().unwrap().bundle_id.as_deref(),
            Some("com.googlecode.iterm2")
        );

        server.shutdown();
    }

    #[tokio::test]
    async fn stop_refreshes_the_session_title_at_ingest() {
        // The card name must match the Claude Code session title right after
        // a turn ends — no brief/LLM debounce involved.
        let dir = std::env::temp_dir().join(format!("hobbes_title_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("s-title.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Benchmark tool CSV support\"}\n",
        )
        .unwrap();

        let (shared, server, base) = test_server().await;
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "session_id": "s-title",
            "cwd": "/Users/x/dev/puget",
            "hook_event_name": "Stop",
            "transcript_path": transcript.to_string_lossy(),
        })
        .to_string();
        let r = client.post(format!("{base}/Stop")).body(body).send().await.unwrap();
        assert_eq!(r.status(), 200);

        // The refresh task is fire-and-forget; poll briefly.
        let mut title = None;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            title = shared
                .snapshot()
                .sessions
                .get("s-title")
                .and_then(|s| s.session_title.clone());
            if title.is_some() {
                break;
            }
        }
        assert_eq!(title.as_deref(), Some("Benchmark tool CSV support"));
        assert_eq!(
            shared.snapshot().sessions["s-title"].display_name(),
            "Benchmark tool CSV support"
        );

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn full_lifecycle_reaches_idle_and_removal() {
        let (shared, server, base) = test_server().await;
        let client = reqwest::Client::new();

        for (event, body) in [
            ("SessionStart", start_body("s1")),
            (
                "Stop",
                serde_json::json!({
                    "session_id": "s1",
                    "cwd": "/Users/x/dev/hobbes",
                    "hook_event_name": "Stop",
                    "transcript_path": "/Users/x/.claude/projects/p/s1.jsonl",
                    "last_assistant_message": "done"
                })
                .to_string(),
            ),
        ] {
            let r = client
                .post(format!("{base}/{event}"))
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
        }
        {
            let snapshot = shared.snapshot();
            let s = &snapshot.sessions["s1"];
            assert_eq!(s.status, FleetStatus::Idle);
            // The Stop recorded the transcript and queued a brief.
            assert_eq!(
                s.transcript_path.as_deref(),
                Some("/Users/x/.claude/projects/p/s1.jsonl")
            );
            assert!(s.brief_dirty_at.is_some());
        }

        let r = client
            .post(format!("{base}/SessionEnd"))
            .body(
                serde_json::json!({
                    "session_id": "s1",
                    "hook_event_name": "SessionEnd",
                    "reason": "logout"
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            shared.snapshot().sessions.is_empty(),
            "SessionEnd removes the session from the live map"
        );

        server.shutdown();
    }
}
