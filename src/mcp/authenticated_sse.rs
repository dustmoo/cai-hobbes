// Authenticated SSE Client for Smithery OAuth
// This module provides a custom SseClient implementation that injects
// OAuth tokens into all requests for authenticated MCP connections.

use std::sync::Arc;
use futures::StreamExt;
use http::Uri;
use reqwest::header::ACCEPT;
use sse_stream::SseStream;
use tokio::sync::RwLock;
use thiserror::Error;


use rmcp::transport::{
    SseClientTransport,
    sse_client::{SseClient, SseClientConfig, SseTransportError},
};

// Header constants 
const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-ID";

#[derive(Debug, Error)]
pub enum AuthenticatedClientError {
    #[error("HTTP error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Authentication required: {0}")]
    AuthRequired(String),
}

/// A wrapper around reqwest::Client that always injects an OAuth token
#[derive(Clone)]
pub struct AuthenticatedSseClient {
    inner: reqwest::Client,
    auth_token: Arc<RwLock<Option<String>>>,
    use_post_for_sse: bool,
    auth_header: String,
    auth_prefix: String,
}

impl AuthenticatedSseClient {
    /// Create a new authenticated client with a token
    pub fn new(
        auth_token: Option<String>,
        use_post_for_sse: bool,
        auth_header: Option<String>,
        auth_prefix: Option<String>,
    ) -> Self {
        Self {
            inner: reqwest::Client::default(),
            auth_token: Arc::new(RwLock::new(auth_token)),
            use_post_for_sse,
            auth_header: auth_header.unwrap_or_else(|| "Authorization".to_string()),
            auth_prefix: auth_prefix.unwrap_or_else(|| "Bearer ".to_string()),
        }
    }
    

    


    /// Helper to check response status and handle 401
    async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, AuthenticatedClientError> {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Simple 401 handling - no OAuth metadata extraction needed
            // For Smithery servers, OAuth is handled by the CLI
            let body_bytes = response.bytes().await?;
            let body_str = String::from_utf8_lossy(&body_bytes);
            return Err(AuthenticatedClientError::AuthRequired(format!("401 Unauthorized: {}", body_str)));
        }
        
        response.error_for_status().map_err(AuthenticatedClientError::Reqwest)
    }
}

impl SseClient for AuthenticatedSseClient {
    type Error = AuthenticatedClientError;

    async fn post_message(
        &self,
        uri: Uri,
        message: rmcp::model::ClientJsonRpcMessage,
        _auth_token: Option<String>, // Ignored - we use our stored token
    ) -> Result<(), SseTransportError<Self::Error>> {
        let mut request_builder = self.inner.post(uri.to_string()).json(&message);
        
        // Inject our stored auth token
        if let Some(token) = self.auth_token.read().await.as_ref() {
            let auth_value = format!("{}{}", self.auth_prefix, token);
            request_builder = request_builder.header(&self.auth_header, auth_value);
        }
        
        let response = request_builder
            .send()
            .await
            .map_err(|e| SseTransportError::Client(AuthenticatedClientError::Reqwest(e)))?;
            
        Self::check_response(response)
            .await
            .map_err(SseTransportError::Client)
            .map(drop)
    }

    async fn get_stream(
        &self,
        uri: Uri,
        last_event_id: Option<String>,
        _auth_token: Option<String>, // Ignored - we use our stored token
    ) -> Result<
        rmcp::transport::common::client_side_sse::BoxedSseResponse,
        SseTransportError<Self::Error>,
    > {
        let uri_str = uri.to_string();
        let mut request_builder = if self.use_post_for_sse {
            self.inner.post(&uri_str)
        } else {
            self.inner.get(&uri_str)
        };
            
        // Some servers (e.g. Composio) require explicitly accepting both types to avoid 406 Not Acceptable
        request_builder = request_builder.header(ACCEPT, format!("application/json, {}", EVENT_STREAM_MIME_TYPE));
        
        // Composio requires Content-Type: application/json for POST requests, even empty ones
        if self.use_post_for_sse {
            request_builder = request_builder.header(reqwest::header::CONTENT_TYPE, "application/json");
            // Send a valid JSON-RPC 2.0 request as the body to initialize the stream
            // Composio expects this format and will return 400 with a ZodError if it's just "{}"
            let init_message = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "hobbes",
                        "version": "0.1.0"
                    }
                },
                "id": 1
            });
            request_builder = request_builder.json(&init_message);
        }
            
        // Inject our stored auth token
        if let Some(token) = self.auth_token.read().await.as_ref() {
            let auth_value = format!("{}{}", self.auth_prefix, token);
            request_builder = request_builder.header(&self.auth_header, auth_value);
        }
        
        // Debug logging for request
        tracing::info!("Starting SSE connection to {}", uri_str);
        tracing::info!("Using POST: {}", self.use_post_for_sse);
        if self.use_post_for_sse {
             tracing::info!("Sending JSON-RPC init body");
        }
        if let Some(token) = self.auth_token.read().await.as_ref() {
             tracing::info!("Auth header injected: {} (len: {})", self.auth_header, token.len());
        } else {
             tracing::warn!("No auth token found for SSE request!");
        }
        
        if let Some(last_event_id) = last_event_id {
            request_builder = request_builder.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        
        let response = request_builder.send().await.map_err(|e| SseTransportError::Client(AuthenticatedClientError::Reqwest(e)))?;
        let response = Self::check_response(response).await.map_err(SseTransportError::Client)?;
        
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or_default();

        // Special handling for Composio, which might return a JSON error instead of a stream
        if uri_str.contains("composio.dev") && content_type.starts_with("application/json") {
            let body_bytes = response.bytes().await.map_err(|e| {
                SseTransportError::Client(AuthenticatedClientError::Reqwest(e))
            })?;
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            tracing::error!("Expected SSE stream from Composio but got JSON: {}", body_str);
            return Err(SseTransportError::UnexpectedContentType(Some(format!(
                "Expected text/event-stream from Composio, got application/json: {}",
                body_str
            ))));
        }

        // Standard handling for all other SSE connections
        if !content_type.starts_with(EVENT_STREAM_MIME_TYPE) {
            return Err(SseTransportError::UnexpectedContentType(Some(
                content_type.to_string(),
            )));
        }
        
        let event_stream = SseStream::from_byte_stream(response.bytes_stream()).boxed();
        Ok(event_stream)
    }
}

/// Convenience function to create an SSE transport with authentication
pub async fn create_authenticated_transport(
    uri: &str,
    auth_token: Option<String>,
    use_post_for_sse: bool,
    auth_header: Option<String>,
    auth_prefix: Option<String>,
) -> Result<SseClientTransport<AuthenticatedSseClient>, SseTransportError<AuthenticatedClientError>> {
    let client =
        AuthenticatedSseClient::new(auth_token, use_post_for_sse, auth_header, auth_prefix);
    let config = SseClientConfig {
        sse_endpoint: uri.into(),
        ..Default::default()
    };
    SseClientTransport::start_with_client(client, config).await
}
