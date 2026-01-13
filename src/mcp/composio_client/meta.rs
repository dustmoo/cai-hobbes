use super::models::*;

/// Get meta-tools for Tool Router pattern
/// These allow the AI to search for and execute tools on-demand
pub fn get_meta_tools() -> Vec<ComposioTool> {
    vec![
        // Meta-tool 1: Discover available apps/toolkits (e.g., "Gmail", "GitHub")
        ComposioTool {
            name: "COMPOSIO_DISCOVER_APPS".to_string(),
            description: Some(
                "Discover available Composio applications and toolkits. \
                Use this to find which apps match your needs (e.g., query 'email' to find 'Gmail'). \
                Returns app names, descriptions, and tool counts. \
                After finding the right app, use COMPOSIO_GET_APP_TOOLS to list its specific tools.".to_string()
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language query to search for apps (e.g., 'email', 'crm', 'calendar', 'github')"
                    }
                },
                "required": []
            })),
            toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
            app: None,
            slug: Some("COMPOSIO_DISCOVER_APPS".to_string()),
            input_parameters: None,
            input_schema: None,
            output_parameters: None,
            tags: None,
            version: None,
            available_versions: None,
            is_deprecated: None,
            is_no_auth: Some(true),
        },
        // Meta-tool 2: Get specific tools for a chosen app
        ComposioTool {
            name: "COMPOSIO_GET_APP_TOOLS".to_string(),
            description: Some(
                "List all available tools for a specific Composio app/toolkit. \
                Use this AFTER discovering the app name with COMPOSIO_DISCOVER_APPS. \
                Returns tool names, descriptions, and parameter schemas for the selected app.".to_string()
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app_name": {
                        "type": "string",
                        "description": "The name or slug of the app to list tools for (e.g., 'Gmail', 'GitHub', 'Google Sheets')"
                    }
                },
                "required": ["app_name"]
            })),
            toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
            app: None,
            slug: Some("COMPOSIO_GET_APP_TOOLS".to_string()),
            input_parameters: None,
            input_schema: None,
            output_parameters: None,
            tags: None,
            version: None,
            available_versions: None,
            is_deprecated: None,
            is_no_auth: Some(true),
        },
        // Meta-tool 3: Execute a specific tool
        ComposioTool {
            name: "COMPOSIO_EXECUTE_TOOL".to_string(),
            description: Some(
                "Execute a Composio tool by name. Use COMPOSIO_GET_APP_TOOLS first to find the correct \
                tool name and required parameters. Pass the exact tool name and arguments as JSON.".to_string()
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The exact name of the tool to execute (e.g., 'GMAIL_SEND_EMAIL', 'GITHUB_CREATE_ISSUE')"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "The arguments to pass to the tool, matching the tool's parameter schema"
                    }
                },
                "required": ["tool_name", "arguments"]
            })),
            toolkit: Some(ComposioToolkit { slug: "composio".to_string() }),
            app: None,
            slug: Some("COMPOSIO_EXECUTE_TOOL".to_string()),
            input_parameters: None,
            input_schema: None,
            output_parameters: None,
            tags: None,
            version: None,
            available_versions: None,
            is_deprecated: None,
            is_no_auth: Some(true),
        }
    ]
}
