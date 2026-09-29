use rmcp::model::CallToolRequestParam;
use super::{McpClientType, McpManager};

impl McpManager {
    /// Start OAuth flow for a server that requires authentication
    /// This calls the generate_oauth_url tool and opens the browser
    #[allow(dead_code)]
    pub async fn start_oauth_flow(&self, server_name: &str) -> Result<String, String> {
        // First, check if the server has a generate_oauth_url tool
        let servers = self.servers.lock().await;

        if let Some(client) = servers.get(server_name) {
            // Check if server has OAuth tools
            let has_oauth_url_tool = client.tools.iter().any(|t| {
                let name = t.name.as_ref();
                name.contains("oauth_url")
                    || name.contains("generate_oauth")
                    || name.contains("auth_url")
            });

            if !has_oauth_url_tool {
                return Err(format!(
                    "Server '{}' does not have OAuth tools",
                    server_name
                ));
            }

            // Find the callback port
            let port = crate::mcp::oauth_flow::find_available_port()
                .ok_or("Could not find available port for OAuth callback")?;

            let redirect_uri = format!("http://localhost:{}/callback", port);

            // Call the generate_oauth_url tool
            let service = client.service.clone();
            drop(servers); // Release lock before async call

            // Try different tool name patterns
            let tool_names = [
                "generate_oauth_url",
                "GENERATE_OAUTH_URL",
                "generate_auth_url",
            ];
            let mut oauth_url = None;

            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(
                        serde_json::json!({
                            "redirect_uri": redirect_uri
                        })
                        .as_object()
                        .cloned()
                        .expect("static JSON is always an object"),
                    ),
                };

                if let McpClientType::Service(service_arc) = &service {
                    if let Ok(result) = service_arc.call_tool(request).await {
                        // Extract URL from result
                        if let Some(content) = result.content.first() {
                            if let Some(text) = content.raw.as_text() {
                                // The result might be a URL directly or JSON containing a URL
                                if text.text.starts_with("http") {
                                    oauth_url = Some(text.text.clone());
                                } else if let Ok(json) =
                                    serde_json::from_str::<serde_json::Value>(&text.text)
                                {
                                    if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
                                        oauth_url = Some(url.to_string());
                                    } else if let Some(url) =
                                        json.get("oauth_url").and_then(|v| v.as_str())
                                    {
                                        oauth_url = Some(url.to_string());
                                    }
                                }
                            }
                        }
                        if oauth_url.is_some() {
                            break;
                        }
                    }
                }
            }

            if let Some(url) = oauth_url {
                // Start callback server and open browser
                let _callback_rx = crate::mcp::oauth_flow::start_callback_server(port);
                crate::mcp::oauth_flow::open_browser(&url, None).await?;

                Ok(format!(
                    "OAuth flow started. Callback server on port {}",
                    port
                ))
            } else {
                Err("Failed to get OAuth URL from server".to_string())
            }
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }

    /// Complete OAuth flow by exchanging auth code for tokens
    #[allow(dead_code)]
    pub async fn complete_oauth_flow(
        &self,
        server_name: &str,
        auth_code: &str,
    ) -> Result<String, String> {
        let servers = self.servers.lock().await;

        if let Some(client) = servers.get(server_name) {
            let service = client.service.clone();
            drop(servers);

            // Try different tool name patterns
            let tool_names = ["exchange_auth_code", "EXCHANGE_AUTH_CODE", "exchange_code"];

            for tool_name in tool_names {
                let request = CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: Some(
                        serde_json::json!({
                            "code": auth_code
                        })
                        .as_object()
                        .cloned()
                        .expect("static JSON is always an object"),
                    ),
                };

                if let McpClientType::Service(service_arc) = &service {
                    if let Ok(result) = service_arc.call_tool(request).await {
                        // Check if exchange was successful
                        if result.is_error.unwrap_or(false) {
                            if let Some(content) = result.content.first() {
                                if let Some(text) = content.raw.as_text() {
                                    return Err(format!("Token exchange failed: {}", text.text));
                                }
                            }
                            return Err("Token exchange failed".to_string());
                        }

                        // Remove from auth_required_servers
                        self.auth_required_servers.lock().await.remove(server_name);

                        return Ok("OAuth completed successfully".to_string());
                    }
                }
            }

            Err("Failed to exchange auth code".to_string())
        } else {
            Err(format!("Server '{}' not found", server_name))
        }
    }
}
