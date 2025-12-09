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
}

impl AuthenticatedSseClient {
    /// Create a new authenticated client with a token
    pub fn new(auth_token: Option<String>) -> Self {
        Self {
            inner: reqwest::Client::default(),
            auth_token: Arc::new(RwLock::new(auth_token)),
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
            request_builder = request_builder.bearer_auth(token);
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
        let mut request_builder = self.inner
            .get(uri.to_string())
            .header(ACCEPT, EVENT_STREAM_MIME_TYPE);
            
        // Inject our stored auth token
        if let Some(token) = self.auth_token.read().await.as_ref() {
            request_builder = request_builder.bearer_auth(token);
        }
        
        if let Some(last_event_id) = last_event_id {
            request_builder = request_builder.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        
        let response = request_builder.send().await.map_err(|e| SseTransportError::Client(AuthenticatedClientError::Reqwest(e)))?;
        let response = Self::check_response(response).await.map_err(SseTransportError::Client)?;
        
        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(ct) => {
                if !ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) {
                    return Err(SseTransportError::UnexpectedContentType(Some(
                        String::from_utf8_lossy(ct.as_bytes()).to_string(),
                    )));
                }
            }
            None => {
                return Err(SseTransportError::UnexpectedContentType(None));
            }
        }
        
        let event_stream = SseStream::from_byte_stream(response.bytes_stream()).boxed();
        Ok(event_stream)
    }
}

/// Convenience function to create an SSE transport with authentication
pub async fn create_authenticated_transport(
    uri: &str,
    auth_token: Option<String>,
) -> Result<SseClientTransport<AuthenticatedSseClient>, SseTransportError<AuthenticatedClientError>> {
    let client = AuthenticatedSseClient::new(auth_token);
    let config = SseClientConfig {
        sse_endpoint: uri.into(),
        ..Default::default()
    };
    SseClientTransport::start_with_client(client, config).await
}
