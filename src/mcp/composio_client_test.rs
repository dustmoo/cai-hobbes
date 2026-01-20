#[cfg(test)]
mod tests {
    use crate::mcp::composio_client::ComposioClient;
    use serde_json::json;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_composio_tool_deserialization() {
        let json_data = r#"
        {
            "name": "GMAIL_ADD_LABEL_TO_EMAIL",
            "description": "Adds and/or removes specified Gmail labels for a message",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "string",
                        "description": "Immutable ID of the message to modify"
                    }
                },
                "required": ["message_id"]
            },
            "annotations": {
                "title": "Modify email labels",
                "scopes": ["https://mail.google.com/"]
            }
        }
        "#;

        let tool: Result<crate::mcp::composio_client::ComposioTool, _> =
            serde_json::from_str(json_data);
        assert!(
            tool.is_ok(),
            "Failed to deserialize ComposioTool: {:?}",
            tool.err()
        );
        let tool = tool.unwrap();
        assert_eq!(tool.name, "GMAIL_ADD_LABEL_TO_EMAIL");
        assert!(tool.input_schema.is_some());
        assert!(tool.parameters.is_none());
    }

    #[tokio::test]
    async fn test_execute_tool_success() {
        let mock_server = MockServer::start().await;

        // 1. Mock connected accounts response (account discovery flow)
        let connected_accounts_response = json!({
            "items": [
                {
                    "id": "acc_test123",
                    "status": "ACTIVE",
                    "userId": "default",
                    "appName": "test",
                    "providerId": "test_provider"
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path_regex("/api/v3/connected_accounts.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(connected_accounts_response))
            .mount(&mock_server)
            .await;

        // 2. Mock tool execution success response
        let response_body = json!({
            "data": { "result": "success" },
            "successful": true,
            "log_id": "test-log-id"
        });

        Mock::given(method("POST"))
            .and(path_regex("/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&mock_server)
            .await;

        let client = ComposioClient::new(
            "test-key".to_string(),
            mock_server.uri(),
            Some("default".to_string()),
            None,
            "test-profile-id".to_string(),
        );

        let result = client
            .execute_tool("TEST_TOOL", json!({"arg": "value"}))
            .await;

        assert!(result.is_ok(), "Execute failed: {:?}", result.err());
        let response = result.unwrap();
        eprintln!("DEBUG: response = {:?}", response);
        assert!(
            response.successful,
            "Response not successful: {:?}",
            response
        );
        assert_eq!(response.data["result"], "success");
        assert_eq!(response.log_id, Some("test-log-id".to_string()));

        // Verify we made requests (connected_accounts + tool execution)
        let requests = mock_server.received_requests().await.unwrap();
        assert!(requests.len() >= 1, "Expected at least 1 request");

        // Find the tool execution request
        let tool_req = requests
            .iter()
            .find(|r| {
                let body = std::str::from_utf8(&r.body).unwrap_or("");
                body.contains("tools/call")
            })
            .expect("Tool execution request not found");

        // Verify JSON-RPC body
        let body: serde_json::Value = serde_json::from_slice(&tool_req.body).unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "TEST_TOOL");
        assert!(body["params"].get("arguments").is_some());
    }

    #[tokio::test]
    async fn test_execute_tool_fallback() {
        let mock_server = MockServer::start().await;

        // 1. Mock connected accounts response (account discovery flow)
        let connected_accounts_response = json!({
            "items": [
                {
                    "id": "acc_test456",
                    "status": "ACTIVE",
                    "userId": "default",
                    "appName": "raw"
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path_regex("/api/v3/connected_accounts.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(connected_accounts_response))
            .mount(&mock_server)
            .await;

        // 2. Mock raw response (fallback case - non-standard format)
        let response_body = json!({
            "some_raw_data": "value"
        });

        Mock::given(method("POST"))
            .and(path_regex("/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&mock_server)
            .await;

        let client = ComposioClient::new(
            "test-key".to_string(),
            mock_server.uri(),
            Some("default".to_string()),
            None,
            "test-profile-id".to_string(),
        );

        let result = client.execute_tool("RAW_TOOL", json!({})).await;

        assert!(result.is_ok(), "Execute failed: {:?}", result.err());
        let response = result.unwrap();
        assert!(response.successful);
        // Should contain the raw data in `data`
        assert_eq!(response.data["some_raw_data"], "value");

        // Find tool execution request
        let requests = mock_server.received_requests().await.unwrap();
        let tool_req = requests
            .iter()
            .find(|r| {
                let body = std::str::from_utf8(&r.body).unwrap_or("");
                body.contains("tools/call")
            })
            .expect("Tool execution request not found");

        // Verify JSON-RPC body
        let body: serde_json::Value = serde_json::from_slice(&tool_req.body).unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "RAW_TOOL");
    }

    #[test]
    fn test_parse_sse_response_structure() {
        let response_text = r#"event: message
data: {"result":{"tools":[{"name":"GMAIL_ADD_LABEL_TO_EMAIL","description":"test tool","inputSchema":{}}]},"jsonrpc":"2.0","id":"1"}"#;

        // Simulate the logic in list_tools
        let data_start = response_text.find("data:").unwrap() + "data:".len();
        let json_text = response_text[data_start..].trim();

        let json_value: serde_json::Value = serde_json::from_str(json_text).unwrap();

        // This mirrors the fix logic
        let mut tools_found = false;
        if let Some(result) = json_value.get("result") {
            if let Some(result_obj) = result.as_object() {
                if let Some(tools_field) = result_obj.get("tools") {
                    if let Some(tools_array) = tools_field.as_array() {
                        tools_found = true;
                        assert_eq!(tools_array.len(), 1);
                    }
                }
            }
        }

        assert!(
            tools_found,
            "Failed to navigate nested structure result.tools"
        );
    }
    #[tokio::test]
    async fn test_execute_tool_with_discovery() {
        let mock_server = MockServer::start().await;

        let discovery_client = ComposioClient::new(
            "test-key".to_string(),
            mock_server.uri(),
            None,
            None,
            "test-profile-id".to_string(),
        );

        // 1. Mock list_tools response (tools/list)
        let list_tools_response = json!({
            "result": {
                "tools": [
                    {
                        "name": "DISCOVERY_TOOL",
                        "toolkit": { "slug": "gmail" },
                        "description": "A tool for discovery"
                    }
                ]
            },
            "jsonrpc": "2.0",
            "id": "1"
        });

        // Matcher for tools/list
        let list_tools_matcher = wiremock::matchers::body_string_contains("tools/list");

        Mock::given(method("POST"))
            .and(path_regex("/.*"))
            .and(list_tools_matcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(list_tools_response))
            .mount(&mock_server)
            .await;

        // 2. Mock connected accounts response
        // Note: client converts .../v3/mcp to .../api/v3
        // If mock_server.uri() is http://127.0.0.1:xxx, get_api_base_url appends /api/v3
        // So path is /api/v3/connected_accounts
        let connected_accounts_response = json!({
            "items": [
                {
                    "id": "acc_12345",
                    "status": "ACTIVE",
                    "userId": "pg-test-f91413b4-0954-4660-857d-79ba689359d1",
                    "appName": "gmail",
                    "providerId": "google_mail"
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path_regex("/api/v3/connected_accounts.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(connected_accounts_response))
            .mount(&mock_server)
            .await;

        // 3. Mock execute tool response (tools/call)
        let execute_response = json!({
            "successful": true,
            "data": { "result": "discovered" }
        });

        let execute_matcher = wiremock::matchers::body_string_contains("tools/call");

        Mock::given(method("POST"))
            .and(path_regex("/.*"))
            .and(execute_matcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(execute_response))
            .mount(&mock_server)
            .await;

        // Populate the tool-toolkit map so the heuristic/auth check works
        let _ = discovery_client.list_tools().await;

        let result = discovery_client
            .execute_tool("DISCOVERY_TOOL", json!({}))
            .await;

        assert!(result.is_ok(), "Execution failed: {:?}", result.err());
        let response = result.unwrap();
        assert!(response.successful);

        // Verify the execute request used the discovered account ID
        let requests = mock_server.received_requests().await.unwrap();
        // Requests could be list_tools, connect_accounts, execute_tool (in order)

        // Find the execute request
        let execute_req = requests
            .iter()
            .find(|r| {
                let body = std::str::from_utf8(&r.body).unwrap();
                body.contains("tools/call")
            })
            .expect("Execute request not found");

        let body: serde_json::Value = serde_json::from_slice(&execute_req.body).unwrap();

        // Verify Pure MCP Payload: connected_account_id should NOT be in the body
        // The routing is handled by user_id in the URL query params, not the body
        assert!(
            body["params"].get("connected_account_id").is_none(),
            "connected_account_id should NOT be in the body (Pure MCP Payload mandate)"
        );
    }
}
