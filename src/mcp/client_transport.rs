//! Network MCP connections (Streamable HTTP).
//!
//! Hobbes connects to remote MCP servers over the Streamable HTTP transport
//! only. The legacy HTTP+SSE transport was removed from rmcp; a configured
//! URL ending in `/sse` is retried once at the sibling `/mcp` endpoint, which
//! is where most servers (Composio included) serve Streamable HTTP.
//!
//! Credentials are host-scoped (see SYSTEM_PATTERNS.md): the Composio API key
//! is sent as `x-api-key` only to `*.composio.dev`, and a bearer token only
//! when one was explicitly supplied for this server.

use std::collections::HashMap;
use std::time::Duration;

use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;

use crate::log_utils::{redact_url, truncate_for_log};

/// A running MCP client session (stdio or network).
pub type McpService = RunningService<RoleClient, ClientConfig>;

/// How long a server gets to answer `initialize`. Generous because some
/// stdio servers install themselves (npx/uvx) on first launch.
pub const MCP_INIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Upper bound on the error text carried back to the UI and logs.
const MAX_ERROR_CHARS: usize = 500;

/// The client handler and `initialize` parameters Hobbes uses for every MCP
/// server.
pub fn hobbes_client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("hobbes", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
}

/// Where and how to connect to a remote MCP server, with credentials already
/// scoped to its host by [`plan_remote_endpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub uri: String,
    /// Tried once if `uri` fails for a reason other than authorization: the
    /// `/mcp` sibling of a legacy `/sse` URL.
    pub fallback_uri: Option<String>,
    /// Sent as `Authorization: Bearer <token>`.
    pub bearer_token: Option<String>,
    /// Sent as `x-api-key` (Composio only).
    pub api_key: Option<String>,
}

/// True when `uri` parses and its host is `composio.dev` or a subdomain of it.
/// A substring match would accept `composio.dev.evil.example`.
pub fn is_composio_host(uri: &str) -> bool {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h == "composio.dev" || h.ends_with(".composio.dev"))
}

/// Decide the URI, fallback and credentials for a remote server.
///
/// * Composio hosts: any `transport` query pair is dropped (it selected the
///   legacy SSE transport), and the key is `explicit_token`, else
///   `composio_api_key`, sent as `x-api-key`. No bearer token.
/// * Any other host: `explicit_token` (if any) is the bearer token. The
///   Composio key is never attached — it must not leave `*.composio.dev`.
pub fn plan_remote_endpoint(
    uri: &str,
    explicit_token: Option<&str>,
    composio_api_key: Option<&str>,
) -> Result<RemoteEndpoint, String> {
    let mut url = url::Url::parse(uri.trim())
        .map_err(|e| format!("invalid MCP server URL ({}): {e}", redact_url(uri)))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported scheme '{other}' for a remote MCP server (expected http or https)"
            ))
        }
    }

    let composio = is_composio_host(url.as_str());
    let (bearer_token, api_key) = if composio {
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "transport")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
        let key = explicit_token
            .or(composio_api_key)
            .filter(|k| !k.trim().is_empty())
            .map(str::to_string);
        (None, key)
    } else {
        let token = explicit_token
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
        (token, None)
    };

    let fallback_uri = sse_to_mcp_fallback(&url);

    Ok(RemoteEndpoint {
        uri: url.to_string(),
        fallback_uri,
        bearer_token,
        api_key,
    })
}

/// If the last path segment is `sse` (with or without a trailing slash),
/// the same URL with that segment replaced by `mcp`; the query and any
/// trailing slash are kept.
fn sse_to_mcp_fallback(url: &url::Url) -> Option<String> {
    let path = url.path();
    let (body, trailing) = match path.strip_suffix('/') {
        Some(b) => (b, "/"),
        None => (path, ""),
    };
    let prefix = body.strip_suffix("sse")?;
    if !prefix.ends_with('/') {
        return None;
    }
    let mut fallback = url.clone();
    fallback.set_path(&format!("{prefix}mcp{trailing}"));
    Some(fallback.to_string())
}

/// Why a remote connection failed. `message` is safe to log and show.
#[derive(Debug)]
pub struct RemoteConnectError {
    pub message: String,
    /// The server answered with an authorization challenge (401/403 with
    /// `WWW-Authenticate`).
    pub auth_required: bool,
}

const LEGACY_SSE_HINT: &str = "This looks like a legacy HTTP+SSE endpoint, which Hobbes no longer supports; point the config at the server's Streamable HTTP endpoint (often …/mcp).";

/// Connect and initialize an MCP session over Streamable HTTP.
///
/// Tries `endpoint.uri`; if that fails for a reason other than authorization
/// or a timeout and a `fallback_uri` exists, tries it once.
pub async fn connect_remote(
    endpoint: &RemoteEndpoint,
    init_timeout: Duration,
) -> Result<McpService, RemoteConnectError> {
    let first = match connect_once(endpoint, &endpoint.uri, init_timeout).await {
        Ok(service) => return Ok(service),
        Err(failure) => failure,
    };

    let fallback = match &endpoint.fallback_uri {
        Some(f) if !first.error.auth_required && !first.timed_out => f,
        _ => return Err(first.error),
    };

    match connect_once(endpoint, fallback, init_timeout).await {
        Ok(service) => {
            tracing::warn!(
                "MCP server at {} is not served at its configured …/sse URL but answered at …/mcp; update the server config to the /mcp endpoint",
                redact_url(&endpoint.uri)
            );
            Ok(service)
        }
        // The /mcp endpoint exists and wants credentials: that is the
        // actionable error.
        Err(second) if second.error.auth_required => Err(second.error),
        Err(_) => Err(RemoteConnectError {
            message: format!("{} {}", first.error.message, LEGACY_SSE_HINT),
            auth_required: false,
        }),
    }
}

struct AttemptFailure {
    error: RemoteConnectError,
    timed_out: bool,
}

async fn connect_once(
    endpoint: &RemoteEndpoint,
    uri: &str,
    init_timeout: Duration,
) -> Result<McpService, AttemptFailure> {
    let fail = |message: String, auth_required: bool, timed_out: bool| AttemptFailure {
        error: RemoteConnectError {
            message,
            auth_required,
        },
        timed_out,
    };

    let mut headers = HashMap::new();
    if let Some(key) = &endpoint.api_key {
        let mut value = http::HeaderValue::from_str(key)
            .map_err(|_| fail("API key contains invalid header characters".into(), false, false))?;
        value.set_sensitive(true);
        headers.insert(http::HeaderName::from_static("x-api-key"), value);
    }

    let mut config = StreamableHttpClientTransportConfig::with_uri(uri).custom_headers(headers);
    if let Some(token) = &endpoint.bearer_token {
        // rmcp adds the `Bearer ` prefix itself.
        config = config.auth_header(token.clone());
    }
    // Lazy: nothing is sent until `serve` runs the initialize handshake.
    let transport = StreamableHttpClientTransport::from_config(config);

    match tokio::time::timeout(init_timeout, hobbes_client_config().serve(transport)).await {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(e)) => Err(fail(
            truncate_for_log(&format!("Failed to connect: {e}"), MAX_ERROR_CHARS),
            e.is_authorization_required(),
            false,
        )),
        Err(_) => Err(fail(
            "Timed out waiting for MCP server to initialize".to_string(),
            false,
            true,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{any, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const T: Duration = Duration::from_secs(10);

    // ── plan_remote_endpoint ────────────────────────────────────────────

    #[test]
    fn composio_strips_transport_and_uses_api_key() {
        let ep = plan_remote_endpoint(
            "https://backend.composio.dev/v3/mcp/abc?transport=sse&user_id=u",
            None,
            Some("ck"),
        )
        .unwrap();
        assert!(!ep.uri.contains("transport"), "{}", ep.uri);
        assert!(ep.uri.contains("user_id=u"), "{}", ep.uri);
        assert_eq!(ep.api_key.as_deref(), Some("ck"));
        assert_eq!(ep.bearer_token, None);
        assert_eq!(ep.fallback_uri, None);
    }

    #[test]
    fn composio_explicit_token_wins_and_lone_transport_query_is_dropped() {
        let ep = plan_remote_endpoint(
            "https://mcp.composio.dev/x/sse?transport=sse",
            Some("explicit"),
            Some("env"),
        )
        .unwrap();
        assert_eq!(ep.uri, "https://mcp.composio.dev/x/sse");
        assert_eq!(ep.api_key.as_deref(), Some("explicit"));
        assert_eq!(ep.fallback_uri.as_deref(), Some("https://mcp.composio.dev/x/mcp"));
    }

    #[test]
    fn composio_key_never_goes_to_other_hosts() {
        let ep = plan_remote_endpoint("https://example.com/mcp", None, Some("ck")).unwrap();
        assert_eq!(ep.api_key, None);
        assert_eq!(ep.bearer_token, None);
    }

    #[test]
    fn lookalike_host_is_not_composio() {
        assert!(!is_composio_host("https://composio.dev.evil.example/mcp"));
        assert!(!is_composio_host("https://notcomposio.dev/mcp"));
        assert!(is_composio_host("https://composio.dev/mcp"));
        assert!(is_composio_host("https://Backend.Composio.dev/mcp"));
        let ep = plan_remote_endpoint("https://composio.dev.evil.example/mcp", None, Some("ck"))
            .unwrap();
        assert_eq!(ep.api_key, None);
    }

    #[test]
    fn explicit_token_is_bearer_for_other_hosts() {
        let ep = plan_remote_endpoint("https://server.smithery.ai/foo/mcp", Some("tok"), Some("ck"))
            .unwrap();
        assert_eq!(ep.bearer_token.as_deref(), Some("tok"));
        assert_eq!(ep.api_key, None);
    }

    #[test]
    fn sse_paths_get_an_mcp_fallback() {
        let ep = plan_remote_endpoint("https://h.example/a/sse?x=1", None, None).unwrap();
        assert_eq!(ep.fallback_uri.as_deref(), Some("https://h.example/a/mcp?x=1"));
        let ep = plan_remote_endpoint("https://h.example/sse/", None, None).unwrap();
        assert_eq!(ep.fallback_uri.as_deref(), Some("https://h.example/mcp/"));
        let ep = plan_remote_endpoint("https://h.example/mcp", None, None).unwrap();
        assert_eq!(ep.fallback_uri, None);
        let ep = plan_remote_endpoint("https://h.example/notsse", None, None).unwrap();
        assert_eq!(ep.fallback_uri, None);
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let err = plan_remote_endpoint("ftp://h.example/mcp", None, None).unwrap_err();
        assert!(err.contains("unsupported scheme"), "{err}");
        assert!(plan_remote_endpoint("not a url", None, None).is_err());
    }

    // ── connect_remote against a mock Streamable HTTP server ────────────

    /// Minimal Streamable HTTP MCP server: JSON responses, no SSE stream.
    struct McpMock;

    impl Respond for McpMock {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            match req.method.as_str() {
                "GET" => return ResponseTemplate::new(405),
                "DELETE" => return ResponseTemplate::new(200),
                _ => {}
            }
            let body: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(_) => return ResponseTemplate::new(400),
            };
            let Some(id) = body.get("id").cloned() else {
                // Notifications (e.g. notifications/initialized).
                return ResponseTemplate::new(202);
            };
            let result = match body["method"].as_str() {
                Some("initialize") => serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock", "version": "0.0.1" }
                }),
                Some("tools/list") => serde_json::json!({
                    "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }]
                }),
                _ => serde_json::json!({}),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        }
    }

    fn endpoint(uri: String) -> RemoteEndpoint {
        RemoteEndpoint {
            uri,
            fallback_uri: None,
            bearer_token: None,
            api_key: None,
        }
    }

    async fn tool_names(service: &McpService) -> Vec<String> {
        service
            .list_tools(None)
            .await
            .expect("tools/list")
            .tools
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    async fn initialize_request(server: &MockServer) -> Request {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body)
                    .is_ok_and(|v| v["method"] == "initialize")
            })
            .expect("an initialize request")
    }

    #[tokio::test]
    async fn connects_and_lists_tools() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(McpMock).mount(&server).await;

        let service = connect_remote(&endpoint(format!("{}/mcp", server.uri())), T)
            .await
            .expect("connect");
        assert_eq!(tool_names(&service).await, vec!["echo".to_string()]);
        let _ = service.cancel().await;
    }

    #[tokio::test]
    async fn bearer_token_is_sent_without_api_key() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(McpMock).mount(&server).await;

        let mut ep = endpoint(format!("{}/mcp", server.uri()));
        ep.bearer_token = Some("t".into());
        let service = connect_remote(&ep, T).await.expect("connect");

        let init = initialize_request(&server).await;
        assert_eq!(
            init.headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer t")
        );
        assert!(init.headers.get("x-api-key").is_none());
        let _ = service.cancel().await;
    }

    #[tokio::test]
    async fn api_key_is_sent_as_x_api_key_without_bearer() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(McpMock).mount(&server).await;

        let mut ep = endpoint(format!("{}/mcp", server.uri()));
        ep.api_key = Some("k".into());
        let service = connect_remote(&ep, T).await.expect("connect");

        let init = initialize_request(&server).await;
        assert_eq!(
            init.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("k")
        );
        assert!(init.headers.get("authorization").is_none());
        let _ = service.cancel().await;
    }

    #[tokio::test]
    async fn legacy_sse_url_falls_back_to_mcp() {
        let server = MockServer::start().await;
        Mock::given(path("/sse"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        Mock::given(path("/mcp")).respond_with(McpMock).mount(&server).await;

        let ep = plan_remote_endpoint(&format!("{}/sse", server.uri()), None, None).unwrap();
        assert!(ep.fallback_uri.is_some());
        let service = connect_remote(&ep, T).await.expect("connect via fallback");
        assert_eq!(tool_names(&service).await, vec!["echo".to_string()]);
        let _ = service.cancel().await;
    }

    #[tokio::test]
    async fn legacy_sse_url_without_mcp_sibling_explains_why() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;

        let ep = plan_remote_endpoint(&format!("{}/sse", server.uri()), None, None).unwrap();
        let err = connect_remote(&ep, T).await.err().expect("must fail");
        assert!(!err.auth_required);
        assert!(err.message.contains("legacy HTTP+SSE"), "{}", err.message);
    }

    #[tokio::test]
    async fn unauthorized_with_challenge_is_auth_required() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", "Bearer realm=\"x\""),
            )
            .mount(&server)
            .await;

        let err = connect_remote(&endpoint(format!("{}/mcp", server.uri())), T)
            .await
            .err()
            .expect("must fail");
        assert!(err.auth_required, "{}", err.message);
    }
}
