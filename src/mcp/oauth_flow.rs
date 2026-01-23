// OAuth Flow Handler for MCP Servers
// This module handles OAuth flows for MCPs that require external authorization (e.g., Google Calendar)
// Follows Smithery SDK patterns for OAuth 2.1 with PKCE

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use tokio::sync::mpsc;

// ============================================================================
// PKCE (Proof Key for Code Exchange) Implementation
// ============================================================================

/// Generate a cryptographically random code verifier for PKCE
/// Returns a 43-128 character URL-safe string
pub fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a code challenge from a code verifier using SHA256
/// This is sent to the authorization server
pub fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    URL_SAFE_NO_PAD.encode(hash)
}

// ============================================================================
// OAuth Types
// ============================================================================

/// OAuth 2.0 tokens returned from token endpoint
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// OAuth client metadata for dynamic registration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientMetadata {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub response_types: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl Default for OAuthClientMetadata {
    fn default() -> Self {
        Self {
            client_name: "Hobbes MCP Client".to_string(),
            redirect_uris: vec!["http://localhost:30432/callback".to_string()],
            grant_types: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            response_types: vec!["code".to_string()],
            scope: Some("mcp:tools".to_string()),
        }
    }
}

/// OAuth server metadata (discovered from well-known endpoint)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

/// OAuth client information (after registration)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientInfo {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// OAuth protected resource metadata (for discovery)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Option<Vec<String>>,
}

// ============================================================================
// Token Exchange
// ============================================================================

/// Exchange an authorization code for tokens
pub async fn exchange_code_for_tokens(
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<OAuthTokens, String> {
    let client = reqwest::Client::new();

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
        ("client_id", client_id),
    ];

    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }

    let response = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to exchange code: {}", e))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed: {}", error_text));
    }

    response
        .json::<OAuthTokens>()
        .await
        .map_err(|e| format!("Failed to parse tokens: {}", e))
}

/// Refresh an access token using a refresh token
#[allow(dead_code)]
pub async fn refresh_access_token(
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<OAuthTokens, String> {
    let client = reqwest::Client::new();

    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];

    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }

    let response = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to refresh token: {}", e))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed: {}", error_text));
    }

    response
        .json::<OAuthTokens>()
        .await
        .map_err(|e| format!("Failed to parse refreshed tokens: {}", e))
}

// ============================================================================
// Metadata Discovery
// ============================================================================

/// Discover OAuth server metadata from well-known endpoint
pub async fn discover_oauth_metadata(
    server_url: &str,
    api_token: Option<&str>,
) -> Result<OAuthServerMetadata, String> {
    let client = reqwest::Client::new();
    let trimmed_server_url = server_url.trim_end_matches('/');

    // Step 1: Discover Authorization Server via Protected Resource Metadata (RFC 9207 / MCP Spec)
    // Try the specific resource location first
    let pr_url = format!(
        "{}/.well-known/oauth-protected-resource",
        trimmed_server_url
    );

    // Determine potential authorization server URL
    let mut authorization_server_url = trimmed_server_url.to_string();

    tracing::debug!(
        "Attempting to discover protected resource metadata at: {}",
        pr_url
    );

    // Prepare request with optional auth token
    let mut request = client.get(&pr_url);
    if let Some(token) = api_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let pr_response = request.send().await;

    let mut found_metadata = false;

    match pr_response {
        Ok(resp) => {
            if resp.status().is_success() {
                if let Ok(meta) = resp.json::<OAuthProtectedResourceMetadata>().await {
                    if let Some(servers) = meta.authorization_servers {
                        if let Some(first) = servers.first() {
                            tracing::debug!(
                                "Discovered authorization server via resource metadata: {}",
                                first
                            );
                            authorization_server_url = first.trim_end_matches('/').to_string();
                            found_metadata = true;
                        }
                    }
                }
            } else if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                // Check WWW-Authenticate header for resource_metadata (RFC 9207)
                // Header format: Bearer ..., resource_metadata="https://..."
                if let Some(auth_val) = resp.headers().get("www-authenticate") {
                    if let Ok(auth_str) = auth_val.to_str() {
                        tracing::debug!("Received 401 with WWW-Authenticate: {}", auth_str);
                        if let Some(idx) = auth_str.find("resource_metadata=\"") {
                            let start = idx + "resource_metadata=\"".len();
                            if let Some(end) = auth_str[start..].find('"') {
                                let metadata_url = &auth_str[start..start + end];
                                tracing::debug!(
                                    "Found resource metadata URL from header: {}",
                                    metadata_url
                                );

                                // Fetch from the discovered metadata URL
                                if let Ok(meta_resp) = client.get(metadata_url).send().await {
                                    if meta_resp.status().is_success() {
                                        if let Ok(meta) =
                                            meta_resp.json::<OAuthProtectedResourceMetadata>().await
                                        {
                                            if let Some(servers) = meta.authorization_servers {
                                                if let Some(first) = servers.first() {
                                                    tracing::debug!("Discovered authorization server via header metadata: {}", first);
                                                    authorization_server_url =
                                                        first.trim_end_matches('/').to_string();
                                                    found_metadata = true;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to fetch protected resource metadata: {}", e);
        }
    }

    if !found_metadata {
        // Fallback: Try the ROOT of the server (origin) - strictly for Smithery-style hosts
        if let Ok(parsed) = url::Url::parse(server_url) {
            let origin_url = parsed.origin().ascii_serialization();
            let pr_root_url = format!(
                "{}/.well-known/oauth-protected-resource",
                origin_url.trim_end_matches('/')
            );

            if pr_root_url != pr_url {
                // Don't retry if it's same url
                tracing::debug!(
                    "Attempting to discover protected resource metadata at root: {}",
                    pr_root_url
                );
                if let Ok(resp) = client.get(&pr_root_url).send().await {
                    if resp.status().is_success() {
                        if let Ok(meta) = resp.json::<OAuthProtectedResourceMetadata>().await {
                            if let Some(servers) = meta.authorization_servers {
                                if let Some(first) = servers.first() {
                                    tracing::debug!(
                                        "Discovered authorization server at root: {}",
                                        first
                                    );
                                    authorization_server_url =
                                        first.trim_end_matches('/').to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Step 2: Fetch Authorization Server Metadata
    tracing::debug!(
        "Fetching OAuth authorization server metadata from: {}",
        authorization_server_url
    );

    // Try standard OAuth 2.0 well-known endpoint
    let well_known_url = format!(
        "{}/.well-known/oauth-authorization-server",
        authorization_server_url
    );

    let response = client.get(&well_known_url).send().await.map_err(|e| {
        format!(
            "Failed to discover OAuth metadata from {}: {}",
            well_known_url, e
        )
    })?;

    if response.status().is_success() {
        return response
            .json::<OAuthServerMetadata>()
            .await
            .map_err(|e| format!("Failed to parse OAuth metadata: {}", e));
    }

    // Try OpenID Connect discovery
    let oidc_url = format!(
        "{}/.well-known/openid-configuration",
        authorization_server_url
    );

    let response = client
        .get(&oidc_url)
        .send()
        .await
        .map_err(|e| format!("Failed to discover OIDC metadata from {}: {}", oidc_url, e))?;

    if response.status().is_success() {
        return response
            .json::<OAuthServerMetadata>()
            .await
            .map_err(|e| format!("Failed to parse OIDC metadata: {}", e));
    }

    // Fallback: If discovery fails, assume standard Smithery/OAuth paths based on the issuer
    tracing::warn!(
        "Metadata discovery failed. Falling back to constructed endpoints for: {}",
        authorization_server_url
    );
    Ok(OAuthServerMetadata {
        authorization_endpoint: format!("{}/oauth/authorize", authorization_server_url),
        token_endpoint: format!("{}/oauth/token", authorization_server_url),
        registration_endpoint: None,
        revocation_endpoint: None,
    })
}

/// Build authorization URL with PKCE
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    scope: Option<&str>,
    state: Option<&str>,
) -> String {
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(code_challenge),
    );

    if let Some(scope) = scope {
        url.push_str(&format!("&scope={}", urlencoding::encode(scope)));
    }

    if let Some(state) = state {
        url.push_str(&format!("&state={}", urlencoding::encode(state)));
    }

    url
}

/// Result of an OAuth flow
#[derive(Debug, Clone)]
pub struct OAuthResult {
    pub success: bool,
    pub auth_code: Option<String>,
    pub error: Option<String>,
    /// Capture all query parameters for generic handling (e.g. connectedAccountId)
    pub params: std::collections::HashMap<String, String>,
}

/// Find an available port for the callback server
pub fn find_available_port() -> Option<u16> {
    // Try ports in range 30000-40000
    for port in 30000..40000 {
        if TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// Start a local HTTP server to capture OAuth callback
/// Returns a channel receiver that will receive the auth code when callback is received
pub fn start_callback_server(port: u16) -> mpsc::UnboundedReceiver<OAuthResult> {
    let (tx, rx) = mpsc::unbounded_channel();

    std::thread::spawn(move || {
        let listener = match TcpListener::bind(format!("127.0.0.1:{}", port)) {
            Ok(l) => l,
            Err(e) => {
                let _ = tx.send(OAuthResult {
                    success: false,
                    auth_code: None,
                    error: Some(format!("Failed to bind callback server: {}", e)),
                    params: std::collections::HashMap::new(),
                });
                return;
            }
        };

        // Set timeout for the server
        listener.set_nonblocking(false).ok();

        tracing::debug!("OAuth callback server started on port {}", port);

        // Accept only one connection
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 4096];
            if let Ok(n) = stream.read(&mut buffer) {
                let request = String::from_utf8_lossy(&buffer[..n]);
                tracing::debug!("Received callback request");

                let params = extract_query_params(&request);

                // Parse the request to extract the auth code
                if let Some(code) = params.get("code") {
                    // Send success response
                    let response = build_success_response();
                    let _ = stream.write_all(response.as_bytes());

                    let _ = tx.send(OAuthResult {
                        success: true,
                        auth_code: Some(code.clone()),
                        error: None,
                        params,
                    });
                } else if let Some(error) = params
                    .get("error")
                    .or_else(|| params.get("error_description"))
                {
                    // Send error response
                    let response = build_error_response(error);
                    let _ = stream.write_all(response.as_bytes());

                    let _ = tx.send(OAuthResult {
                        success: false,
                        auth_code: None,
                        error: Some(error.clone()),
                        params,
                    });
                } else if params.contains_key("connectedAccountId")
                    || params.contains_key("connected_account_id")
                    || params
                        .get("status")
                        .map(|s| s == "success")
                        .unwrap_or(false)
                {
                    // Specific handling for Composio success which might not have 'code'
                    // Composio can return: connectedAccountId (camelCase), connected_account_id (snake_case), or status=success
                    let response = build_success_response();
                    let _ = stream.write_all(response.as_bytes());

                    let _ = tx.send(OAuthResult {
                        success: true,
                        auth_code: None,
                        error: None,
                        params,
                    });
                } else {
                    // Unknown request
                    let response = build_error_response("Invalid callback request");
                    let _ = stream.write_all(response.as_bytes());

                    let _ = tx.send(OAuthResult {
                        success: false,
                        auth_code: None,
                        error: Some(
                            "No auth code or known success param found in callback".to_string(),
                        ),
                        params,
                    });
                }
            }
        }

        tracing::debug!("OAuth callback server shutting down");
    });

    rx
}

/// Extract all query parameters from the request
fn extract_query_params(request: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();

    if let Some(first_line) = request.lines().next() {
        if let Some(path) = first_line.split_whitespace().nth(1) {
            if let Some(query_start) = path.find('?') {
                let query = &path[query_start + 1..];
                for param in query.split('&') {
                    if let Some((key, value)) = param.split_once('=') {
                        let decoded_key = urlencoding::decode(key).unwrap_or_default().into_owned();
                        let decoded_value =
                            urlencoding::decode(value).unwrap_or_default().into_owned();
                        params.insert(decoded_key, decoded_value);
                    }
                }
            }
        }
    }

    params
}

/// Build HTML response for successful OAuth
fn build_success_response() -> String {
    let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Authorization Successful</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #1a1a2e; color: white; }
        .container { text-align: center; padding: 40px; }
        .success { color: #4ade80; font-size: 48px; margin-bottom: 20px; }
        h1 { margin: 0 0 16px 0; }
        p { color: #9ca3af; }
    </style>
</head>
<body>
    <div class="container">
        <div class="success">✓</div>
        <h1>Authorization Successful!</h1>
        <p>You can close this window and return to Hobbes.</p>
    </div>
    <script>setTimeout(() => window.close(), 3000);</script>
</body>
</html>"#;

    // Calculate length specifically for UTF-8 bytes to ensure correct Content-Length
    let html_bytes = html.as_bytes();

    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html_bytes.len(),
        html
    )
}

/// Build HTML response for failed OAuth
fn build_error_response(error: &str) -> String {
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Authorization Failed</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #1a1a2e; color: white; }}
        .container {{ text-align: center; padding: 40px; }}
        .error {{ color: #ef4444; font-size: 48px; margin-bottom: 20px; }}
        h1 {{ margin: 0 0 16px 0; }}
        p {{ color: #9ca3af; }}
        .error-msg {{ color: #f87171; font-family: monospace; background: rgba(239,68,68,0.1); padding: 12px; border-radius: 8px; margin-top: 16px; }}
    </style>
</head>
<body>
    <div class="container">
        <div class="error">✗</div>
        <h1>Authorization Failed</h1>
        <p>Please try again from Hobbes.</p>
        <div class="error-msg">{}</div>
    </div>
</body>
</html>"#,
        error
    );

    let html_bytes = html.as_bytes();

    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html_bytes.len(),
        html
    )
}

/// Open a URL in the default browser
pub fn open_browser(url: &str) -> Result<(), String> {
    open::that(url).map_err(|e| format!("Failed to open browser: {}", e))
}
