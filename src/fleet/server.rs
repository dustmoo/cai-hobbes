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
//! # Gates
//!
//! `PermissionRequest` responses are HELD: the handler registers a oneshot
//! keyed by a generated request id and waits for the Fleet UI's
//! Approve/Deny. Per the verified hook contract the decision rides
//! `hookSpecificOutput.decision.behavior: "allow" | "deny"`, and a 2xx with
//! an **empty body** means "no decision" — the normal permission flow (the
//! terminal prompt) proceeds. There is no explicit escalate value for this
//! event; the empty body IS the passthrough. A timer resolves the gate as
//! passthrough [`hooks_config::GATE_TIMEOUT_MARGIN_SECS`] before the
//! configured hook timeout so the terminal prompt reliably appears — a gate
//! is never swallowed (even a hook-side timeout is documented as "no
//! decision, execution continues", so the failure mode is safe twice over).

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use super::events::{parse_event, FleetEvent};
use super::hooks_config::{GATE_TIMEOUT_MARGIN_SECS, PERMISSION_HOOK_TIMEOUT_SECS};
use super::{reduce, resolve_gate_in_state, store, sweep_stale, FleetShared, GateOutcome};

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
    /// How long a held gate waits for a UI decision before resolving as
    /// passthrough. Production: hook timeout minus the safety margin.
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
                            let mut state = shared.state.lock().expect("fleet state lock poisoned");
                            sweep_stale(&mut state, chrono::Utc::now())
                        };
                        if !changed.is_empty() {
                            store::persist_sessions(&changed);
                            shared.poke();
                        }
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
                    "message": "Denied from the Hobbes fleet panel."
                }
            }
        })
        .to_string(),
        // Empty 2xx body == no decision == normal permission flow (terminal
        // prompt). This is the passthrough per the hook contract.
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

    // Register the gate responder BEFORE the state becomes visible (the
    // poke), so a UI Approve racing the snapshot can never miss the oneshot.
    let held = if let FleetEvent::PermissionRequest { request_id, .. } = &event {
        Some(shared.register_gate(request_id))
    } else {
        None
    };

    // Append the raw event, reduce, persist — synchronous section.
    store::append_event(
        event.session_id(),
        event.event_name(),
        &String::from_utf8_lossy(&body),
    );
    let now = chrono::Utc::now();
    let auto_passthrough = {
        let mut state = shared.state.lock().expect("fleet state lock poisoned");
        let changed = reduce(&mut state, &event, now);
        store::persist_sessions(&changed);
        state
            .sessions
            .get(event.session_id())
            .map(|s| s.auto_passthrough)
            .unwrap_or(false)
    };
    shared.poke();

    // Everything except a held gate answers immediately with an empty 2xx.
    let FleetEvent::PermissionRequest {
        session_id,
        request_id,
        ..
    } = &event
    else {
        return Ok(respond(StatusCode::OK, ""));
    };

    let rx = held.expect("registered for every PermissionRequest");
    if auto_passthrough {
        // No UI hold: discard the responder and step aside immediately.
        let _ = shared.take_gate(request_id);
        let outcome = GateOutcome::Passthrough;
        finish_gate(&shared, session_id, request_id, outcome, "auto_passthrough");
        return Ok(respond(StatusCode::OK, &decision_body(outcome)));
    }

    // Hold the response for the UI, or resolve as passthrough shortly before
    // the hook's own timeout would fire.
    let outcome = tokio::select! {
        decision = rx => decision.unwrap_or(GateOutcome::Passthrough),
        _ = tokio::time::sleep(cfg.gate_hold) => {
            // Drop the now-dead responder so a late UI click is a no-op.
            let _ = shared.take_gate(request_id);
            GateOutcome::Passthrough
        }
    };
    finish_gate(&shared, session_id, request_id, outcome, outcome.as_str());
    Ok(respond(StatusCode::OK, &decision_body(outcome)))
}

/// Post-decision bookkeeping: state transition, persistence, decision log,
/// UI poke. Synchronous — called after all awaits are done.
fn finish_gate(
    shared: &FleetShared,
    session_id: &str,
    request_id: &str,
    outcome: GateOutcome,
    logged_as: &str,
) {
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
        &serde_json::json!({ "request_id": request_id, "outcome": logged_as }).to_string(),
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

    /// Spin up a listener on an ephemeral port with a short gate hold.
    async fn test_server(hold_ms: u64) -> (Arc<FleetShared>, FleetServer, String) {
        let shared = Arc::new(FleetShared::new());
        let cfg = FleetServerConfig {
            port: 0,
            token: "testtok".into(),
            gate_hold: Duration::from_millis(hold_ms),
        };
        let server = start(shared.clone(), cfg).await.unwrap();
        let base = format!("http://127.0.0.1:{}/fleet/testtok", server.port);
        (shared, server, base)
    }

    fn start_body(id: &str) -> String {
        serde_json::json!({
            "session_id": id,
            "cwd": "/Users/x/dev/hobbes",
            "hook_event_name": "SessionStart",
            "reason": "startup"
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
        let (shared, server, base) = test_server(1000).await;
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

    #[tokio::test]
    async fn held_gate_resolves_allow_from_the_ui() {
        let (shared, server, base) = test_server(5000).await;
        let client = reqwest::Client::new();

        // Approve from "the UI" as soon as the gate shows up in state.
        let ui = {
            let shared = shared.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    let gate = shared
                        .snapshot()
                        .sessions
                        .get("s1")
                        .and_then(|s| s.pending_gate.clone());
                    if let Some(gate) = gate {
                        shared.resolve_gate(&gate.request_id, true);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("gate never appeared in state");
            })
        };

        let resp = client
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("s1"))
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

        // Allow resumes the session.
        let snap = shared.snapshot();
        assert_eq!(snap.sessions["s1"].status, FleetStatus::Working);
        assert!(snap.sessions["s1"].pending_gate.is_none());

        server.shutdown();
    }

    #[tokio::test]
    async fn held_gate_resolves_deny_with_a_message() {
        let (shared, server, base) = test_server(5000).await;
        let client = reqwest::Client::new();

        let ui = {
            let shared = shared.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    let gate = shared
                        .snapshot()
                        .sessions
                        .get("s1")
                        .and_then(|s| s.pending_gate.clone());
                    if let Some(gate) = gate {
                        shared.resolve_gate(&gate.request_id, false);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("gate never appeared in state");
            })
        };

        let resp = client
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("s1"))
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

        server.shutdown();
    }

    #[tokio::test]
    async fn unanswered_gate_times_out_to_an_empty_passthrough() {
        // 100ms hold stands in for the production 110s (120s hook timeout
        // minus the 10s margin).
        let (shared, server, base) = test_server(100).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("s1"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            resp.bytes().await.unwrap().is_empty(),
            "passthrough is a 2xx with an EMPTY body — the terminal prompt takes over"
        );

        // Session still needs the user (in the terminal now), gate cleared.
        let snap = shared.snapshot();
        assert_eq!(
            snap.sessions["s1"].status,
            FleetStatus::NeedsAttention(AttentionKind::Gate)
        );
        assert!(snap.sessions["s1"].pending_gate.is_none());
        // A late UI click on the dead gate is a no-op (responder was dropped).
        shared.resolve_gate("whatever", true);

        server.shutdown();
    }

    #[tokio::test]
    async fn auto_passthrough_answers_immediately_without_a_hold() {
        // Long hold: if auto-passthrough failed to short-circuit, the request
        // below would take 5s and trip the client timeout.
        let (shared, server, base) = test_server(5000).await;
        let client = reqwest::Client::new();

        // Seed the session, then flip its auto-passthrough.
        client
            .post(format!("{base}/SessionStart"))
            .body(start_body("s1"))
            .send()
            .await
            .unwrap();
        shared.set_auto_passthrough("s1", true);

        let resp = client
            .post(format!("{base}/PermissionRequest"))
            .body(permission_body("s1"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .expect("must answer well before the hold window");
        assert_eq!(resp.status(), 200);
        assert!(resp.bytes().await.unwrap().is_empty());

        server.shutdown();
    }

    #[tokio::test]
    async fn full_lifecycle_reaches_idle_and_removal() {
        let (shared, server, base) = test_server(1000).await;
        let client = reqwest::Client::new();

        for (event, body) in [
            ("SessionStart", start_body("s1")),
            (
                "Stop",
                serde_json::json!({
                    "session_id": "s1",
                    "cwd": "/Users/x/dev/hobbes",
                    "hook_event_name": "Stop",
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
        assert_eq!(shared.snapshot().sessions["s1"].status, FleetStatus::Idle);

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

    #[tokio::test]
    async fn gate_timing_margin_matches_the_hook_contract() {
        // The production hold must leave the documented margin before the
        // registered hook timeout, or the terminal prompt could be delayed.
        let cfg = FleetServerConfig::new(0, "t".into());
        assert_eq!(
            cfg.gate_hold,
            Duration::from_secs(PERMISSION_HOOK_TIMEOUT_SECS - GATE_TIMEOUT_MARGIN_SECS)
        );
        assert!(cfg.gate_hold < Duration::from_secs(PERMISSION_HOOK_TIMEOUT_SECS));
    }
}
