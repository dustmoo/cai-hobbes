// Smithery OAuth MCP Client
// A complete OAuth 2.1 client for connecting to Smithery-hosted MCP servers
// Follows the official Smithery SDK patterns for authentication
//
// This module is designed to be clean and modular for potential contribution to Smithery.

use std::sync::Arc;
use tokio::sync::RwLock;
use crate::mcp::oauth_flow::{
    OAuthTokens, OAuthServerMetadata, OAuthClientInfo, OAuthClientMetadata,
    generate_code_verifier, generate_code_challenge,
    discover_oauth_metadata, exchange_code_for_tokens, refresh_access_token,
    build_authorization_url, find_available_port, start_callback_server,
};

// ============================================================================
// Smithery OAuth Client
// ============================================================================

/// State machine for OAuth flow
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthState {
    /// Initial state - not connected
    Disconnected,
    /// OAuth metadata discovered, ready to authorize
    AwaitingAuthorization {
        auth_url: String,
        code_verifier: String,
    },
    /// Authorization code received, ready to exchange for tokens
    AwaitingTokenExchange {
        code: String,
        code_verifier: String,
    },
    /// Connected with valid tokens
    Connected,
    /// Error state
    Error(String),
}

/// Error types for Smithery OAuth
#[derive(Debug, thiserror::Error)]
pub enum SmitheryOAuthError {
    #[error("Authentication required")]
    AuthRequired(String),
    #[error("OAuth discovery failed: {0}")]
    DiscoveryFailed(String),
    #[error("Token exchange failed: {0}")]
    TokenExchangeFailed(String),
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
}

/// Configuration for the OAuth client
#[derive(Debug, Clone)]
pub struct SmitheryOAuthConfig {
    /// Smithery server URL (e.g., "https://server.smithery.ai/googlecalendar/mcp")
    pub server_url: String,
    /// Client metadata for OAuth registration
    pub client_metadata: OAuthClientMetadata,
    /// Callback port for local OAuth redirect
    pub callback_port: u16,
}

impl SmitheryOAuthConfig {
    pub fn new(server_url: &str) -> Self {
        let port = find_available_port().unwrap_or(30432);
        Self {
            server_url: server_url.to_string(),
            client_metadata: OAuthClientMetadata {
                redirect_uris: vec![format!("http://localhost:{}/callback", port)],
                ..Default::default()
            },
            callback_port: port,
        }
    }
    
    pub fn callback_url(&self) -> String {
        format!("http://localhost:{}/callback", self.callback_port)
    }
}

/// Smithery OAuth Client
/// 
/// Handles the complete OAuth 2.1 flow with PKCE for Smithery-hosted MCP servers.
/// 
/// # Example
/// ```rust
/// let config = SmitheryOAuthConfig::new("https://server.smithery.ai/googlecalendar/mcp");
/// let mut client = SmitheryOAuthClient::new(config);
/// 
/// // Attempt to connect
/// match client.connect().await {
///     Ok(()) => println!("Connected without auth!"),
///     Err(SmitheryOAuthError::AuthRequired(auth_url)) => {
///         // Open browser for user to authorize
///         open::that(&auth_url)?;
///         
///         // Wait for callback and complete auth
///         let code = wait_for_callback().await?;
///         client.finish_auth(&code).await?;
///     }
///     Err(e) => return Err(e),
/// }
/// 
/// // Now connected - use the access token
/// let token = client.access_token().await;
/// ```
pub struct SmitheryOAuthClient {
    config: SmitheryOAuthConfig,
    state: Arc<RwLock<OAuthState>>,
    tokens: Arc<RwLock<Option<OAuthTokens>>>,
    client_info: Arc<RwLock<Option<OAuthClientInfo>>>,
    server_metadata: Arc<RwLock<Option<OAuthServerMetadata>>>,
    http_client: reqwest::Client,
}

impl SmitheryOAuthClient {
    /// Create a new OAuth client with the given configuration
    pub fn new(config: SmitheryOAuthConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(OAuthState::Disconnected)),
            tokens: Arc::new(RwLock::new(None)),
            client_info: Arc::new(RwLock::new(None)),
            server_metadata: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
        }
    }
    
    /// Attempt to connect to the server
    /// Returns Ok(()) if no auth needed, or Err(AuthRequired(url)) if authorization needed
    pub async fn connect(&self) -> Result<(), SmitheryOAuthError> {
        // Try to make a test request to the server
        let response = self.http_client
            .get(&self.config.server_url)
            .send()
            .await
            .map_err(|e| SmitheryOAuthError::ConnectionFailed(e.to_string()))?;
        
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Need to authenticate
            return self.initiate_auth().await;
        }
        
        if response.status().is_success() {
            *self.state.write().await = OAuthState::Connected;
            return Ok(());
        }
        
        Err(SmitheryOAuthError::ConnectionFailed(format!(
            "Server returned status: {}",
            response.status()
        )))
    }
    
    /// Initiate the OAuth authorization flow
    async fn initiate_auth(&self) -> Result<(), SmitheryOAuthError> {
        // Get API key from env if available (to help with discovery)
        let api_key = std::env::var("SMITHERY_API_KEY").ok();
        
        // Discover OAuth metadata
        let metadata = discover_oauth_metadata(&self.config.server_url, api_key.as_deref())
            .await
            .map_err(SmitheryOAuthError::DiscoveryFailed)?;
        
        *self.server_metadata.write().await = Some(metadata.clone());
        
        // Generate PKCE codes
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        
        // For now, use a simple client_id (in production, you'd register dynamically)
        let client_id = "hobbes-mcp-client";
        *self.client_info.write().await = Some(OAuthClientInfo {
            client_id: client_id.to_string(),
            client_secret: None,
        });
        
        // Build authorization URL
        let auth_url = build_authorization_url(
            &metadata.authorization_endpoint,
            client_id,
            &self.config.callback_url(),
            &code_challenge,
            self.config.client_metadata.scope.as_deref(),
            None,
        );
        
        *self.state.write().await = OAuthState::AwaitingAuthorization {
            auth_url: auth_url.clone(),
            code_verifier,
        };
        
        Err(SmitheryOAuthError::AuthRequired(auth_url))
    }
    
    /// Complete the OAuth flow with the authorization code from callback
    pub async fn finish_auth(&self, code: &str) -> Result<(), SmitheryOAuthError> {
        let state = self.state.read().await.clone();
        
        let code_verifier = match state {
            OAuthState::AwaitingAuthorization { code_verifier, .. } => code_verifier,
            _ => return Err(SmitheryOAuthError::InvalidState(
                "Not awaiting authorization".to_string()
            )),
        };
        
        let metadata = self.server_metadata.read().await.clone()
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No server metadata".to_string()))?;
        
        let client_info = self.client_info.read().await.clone()
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No client info".to_string()))?;
        
        // Exchange code for tokens
        let tokens = exchange_code_for_tokens(
            &metadata.token_endpoint,
            code,
            &self.config.callback_url(),
            &code_verifier,
            &client_info.client_id,
            client_info.client_secret.as_deref(),
        )
        .await
        .map_err(SmitheryOAuthError::TokenExchangeFailed)?;
        
        *self.tokens.write().await = Some(tokens);
        *self.state.write().await = OAuthState::Connected;
        
        Ok(())
    }
    
    /// Get the current access token (if any)
    pub async fn access_token(&self) -> Option<String> {
        self.tokens.read().await.as_ref().map(|t| t.access_token.clone())
    }
    
    /// Refresh the access token using the refresh token
    pub async fn refresh(&self) -> Result<(), SmitheryOAuthError> {
        let tokens = self.tokens.read().await.clone()
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No tokens to refresh".to_string()))?;
        
        let refresh_token = tokens.refresh_token
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No refresh token".to_string()))?;
        
        let metadata = self.server_metadata.read().await.clone()
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No server metadata".to_string()))?;
        
        let client_info = self.client_info.read().await.clone()
            .ok_or_else(|| SmitheryOAuthError::InvalidState("No client info".to_string()))?;
        
        let new_tokens = refresh_access_token(
            &metadata.token_endpoint,
            &refresh_token,
            &client_info.client_id,
            client_info.client_secret.as_deref(),
        )
        .await
        .map_err(SmitheryOAuthError::TokenExchangeFailed)?;
        
        *self.tokens.write().await = Some(new_tokens);
        
        Ok(())
    }
    
    /// Get the current state
    pub async fn state(&self) -> OAuthState {
        self.state.read().await.clone()
    }
    
    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        matches!(*self.state.read().await, OAuthState::Connected)
    }
    
    /// Start the callback server and return the receiver for auth codes
    pub fn start_callback_server(&self) -> tokio::sync::mpsc::UnboundedReceiver<crate::mcp::oauth_flow::OAuthResult> {
        start_callback_server(self.config.callback_port)
    }
    
    /// Get the server URL
    pub fn server_url(&self) -> &str {
        &self.config.server_url
    }
    
    /// Get the callback URL
    pub fn callback_url(&self) -> String {
        self.config.callback_url()
    }
}

// ============================================================================
// Convenience Functions
// ============================================================================

/// Create a Smithery OAuth client for a specific server
pub fn create_smithery_client(server_name: &str) -> SmitheryOAuthClient {
    let server_url = format!("https://server.smithery.ai/{}/mcp", server_name);
    let config = SmitheryOAuthConfig::new(&server_url);
    SmitheryOAuthClient::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_config_default() {
        let config = SmitheryOAuthConfig::new("https://server.smithery.ai/test/mcp");
        assert!(config.callback_url().contains("localhost"));
        assert!(config.callback_url().contains("/callback"));
    }
    
    #[test]
    fn test_pkce_generation() {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        
        // Verifier should be 43 characters (32 bytes base64 encoded)
        assert_eq!(verifier.len(), 43);
        // Challenge should be 43 characters (SHA256 hash base64 encoded)
        assert_eq!(challenge.len(), 43);
        // They should be different
        assert_ne!(verifier, challenge);
    }
}
