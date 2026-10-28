use super::constants::MARKETPLACE_API_BASE;
use std::fs::File;
use std::io::Write;

/// Validate a Composio API key by making a lightweight API call
/// Returns Ok(()) if valid, Err with message if invalid
pub async fn validate_composio_api_key(api_key: &str) -> Result<(), String> {
    if api_key.trim().is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    // The key goes as x-api-key; keep it from following a redirect off-origin.
    let client = reqwest::Client::builder()
        .redirect(crate::mcp::redirect_policy::same_origin_redirect_policy())
        .build()
        .map_err(|e| format!("Network error: {}", crate::log_utils::net_err(e)))?;
    let url = format!("{}/toolkits?limit=1", MARKETPLACE_API_BASE);

    match client
        .get(&url)
        .header("x-api-key", api_key)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                Err("Invalid API key".to_string())
            } else {
                let error_text = response.text().await.unwrap_or_default();
                Err(format!("API error ({}): {}", status, error_text))
            }
        }
        Err(e) => Err(format!("Network error: {}", crate::log_utils::net_err(e))),
    }
}

// Helper function to write content to a file for debugging.
// Only writes with an explicit HOBBES_DEBUG_DUMPS=1 — these payloads include
// auth configs and connected-account data, so DEBUG logging alone must not
// switch them on.
pub fn write_to_debug_file(filename: &str, content: &str) -> std::io::Result<()> {
    if !crate::log_utils::debug_dumps_enabled() {
        return Ok(());
    }

    // Use system temp directory for logs to avoid triggering hot-reload watchers
    let debug_dir = std::env::temp_dir().join("hobbes_debug_logs");
    if !debug_dir.exists() {
        std::fs::create_dir_all(&debug_dir)?;
    }

    let file_path = debug_dir.join(filename);
    let mut file = File::create(&file_path)?;
    file.write_all(content.as_bytes())?;

    // Log the absolute path for clarity
    tracing::debug!("Wrote debug file to: {}", file_path.display());

    Ok(())
}

/// Adapter function to convert a ComposioTool to a standard rmcp::model::Tool
pub fn composio_to_rmcp_tool(composio_tool: &super::models::ComposioTool) -> rmcp::model::Tool {
    use rmcp::model::{JsonObject, MetaObject, Tool};
    use serde_json::Value;

    // Prefer input_parameters or inputSchema if available, fall back to parameters
    let schema: JsonObject = if let Some(Value::Object(obj)) = &composio_tool.input_parameters {
        obj.clone()
    } else if let Some(Value::Object(obj)) = &composio_tool.input_schema {
        obj.clone()
    } else if let Some(Value::Object(obj)) = &composio_tool.parameters {
        obj.clone()
    } else {
        // rmcp::model::Tool requires a schema, so provide an empty map if missing/invalid
        JsonObject::new()
    };

    // Metadata with toolkit and version info
    let mut meta = MetaObject::new();
    if let Some(toolkit) = &composio_tool.toolkit {
        meta.insert(
            "toolkit_slug".to_string(),
            Value::String(toolkit.slug.clone()),
        );
    }
    if let Some(version) = &composio_tool.version {
        meta.insert("version".to_string(), Value::String(version.clone()));
    }

    // Use slug if available, else name; the display name becomes the title
    let name = composio_tool
        .slug
        .clone()
        .unwrap_or_else(|| composio_tool.name.clone());
    let tool = Tool::new_with_raw(name, composio_tool.description.clone().map(Into::into), schema)
        .with_title(composio_tool.name.clone());

    if meta.is_empty() {
        tool
    } else {
        tool.with_meta(meta)
    }
}

/// Convert a snake_case string to camelCase.
/// Example: "workspace_id" -> "workspaceId", "connected_account_id" -> "connectedAccountId"
pub fn snake_to_camel(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;

    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_to_camel_converts_known_argument_names() {
        // The cases that actually flow through Composio argument remapping.
        assert_eq!(snake_to_camel("workspace_id"), "workspaceId");
        assert_eq!(snake_to_camel("connected_account_id"), "connectedAccountId");
    }

    #[test]
    fn snake_to_camel_leaves_single_words_untouched() {
        assert_eq!(snake_to_camel("gmail"), "gmail");
        assert_eq!(snake_to_camel(""), "");
    }

    #[test]
    fn snake_to_camel_handles_edge_underscores() {
        // A trailing underscore has no following char to capitalize → dropped.
        assert_eq!(snake_to_camel("foo_"), "foo");
        // A leading underscore capitalizes the first real char.
        assert_eq!(snake_to_camel("_foo"), "Foo");
        // Consecutive underscores collapse (each just arms capitalize_next).
        assert_eq!(snake_to_camel("a__b"), "aB");
    }

    fn composio_tool(v: serde_json::Value) -> super::super::models::ComposioTool {
        serde_json::from_value(v).expect("ComposioTool")
    }

    #[test]
    fn composio_tool_uses_slug_as_name_and_display_name_as_title() {
        let tool = composio_to_rmcp_tool(&composio_tool(serde_json::json!({
            "name": "Send email",
            "slug": "GMAIL_SEND_EMAIL",
            "description": "Sends an email"
        })));
        assert_eq!(tool.name, "GMAIL_SEND_EMAIL");
        assert_eq!(tool.title.as_deref(), Some("Send email"));
        assert_eq!(tool.description.as_deref(), Some("Sends an email"));
        assert!(tool.input_schema.is_empty());
        assert!(tool.meta.is_none(), "no toolkit/version → no _meta");

        let unslugged = composio_to_rmcp_tool(&composio_tool(serde_json::json!({ "name": "Plain" })));
        assert_eq!(unslugged.name, "Plain");
        assert_eq!(unslugged.description, None);
    }

    #[test]
    fn composio_tool_schema_precedence() {
        let schema = |key: &str| serde_json::json!({ "type": "object", "x-from": key });
        let from = |v: serde_json::Value| {
            composio_to_rmcp_tool(&composio_tool(v)).input_schema["x-from"].clone()
        };
        assert_eq!(
            from(serde_json::json!({
                "name": "t",
                "input_parameters": schema("input_parameters"),
                "inputSchema": schema("inputSchema"),
                "parameters": schema("parameters")
            })),
            "input_parameters"
        );
        assert_eq!(
            from(serde_json::json!({
                "name": "t",
                "inputSchema": schema("inputSchema"),
                "parameters": schema("parameters")
            })),
            "inputSchema"
        );
        assert_eq!(
            from(serde_json::json!({
                "name": "t",
                // A non-object schema is skipped, not used.
                "input_parameters": "bogus",
                "parameters": schema("parameters")
            })),
            "parameters"
        );
    }

    #[test]
    fn composio_tool_meta_serializes_toolkit_and_version() {
        let tool = composio_to_rmcp_tool(&composio_tool(serde_json::json!({
            "name": "Send email",
            "slug": "GMAIL_SEND_EMAIL",
            "toolkit": { "slug": "gmail" },
            "version": "20250101_00"
        })));
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["_meta"]["toolkit_slug"], "gmail");
        assert_eq!(json["_meta"]["version"], "20250101_00");
        assert_eq!(json["title"], "Send email");
        assert_eq!(json["name"], "GMAIL_SEND_EMAIL");
    }

    #[test]
    fn snake_to_camel_is_idempotent_on_camel_input() {
        // Already-camelCase strings contain no underscores, so they round-trip.
        assert_eq!(snake_to_camel("alreadyCamel"), "alreadyCamel");
    }
}
