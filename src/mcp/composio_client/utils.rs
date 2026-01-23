use super::constants::MARKETPLACE_API_BASE;
use std::fs::File;
use std::io::Write;

/// Validate a Composio API key by making a lightweight API call
/// Returns Ok(()) if valid, Err with message if invalid
pub async fn validate_composio_api_key(api_key: &str) -> Result<(), String> {
    if api_key.trim().is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    let client = reqwest::Client::new();
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
        Err(e) => Err(format!("Network error: {}", e)),
    }
}

// Helper function to write content to a file for debugging
// Only writes files when DEBUG or TRACE log level is enabled
pub fn write_to_debug_file(filename: &str, content: &str) -> std::io::Result<()> {
    // Only write debug files if DEBUG or TRACE level is enabled
    if !tracing::enabled!(tracing::Level::DEBUG) {
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
    use rmcp::model::Tool;
    use serde_json::Value;
    use std::sync::Arc;

    // Prefer input_parameters or inputSchema if available, fall back to parameters
    let schema = if let Some(Value::Object(obj)) = &composio_tool.input_parameters {
        Arc::new(obj.clone())
    } else if let Some(Value::Object(obj)) = &composio_tool.input_schema {
        Arc::new(obj.clone())
    } else if let Some(Value::Object(obj)) = &composio_tool.parameters {
        Arc::new(obj.clone())
    } else {
        // rmcp::model::Tool expects a non-optional Arc, so we provide an empty map if schema is missing/invalid
        Arc::new(serde_json::Map::new())
    };

    // Create metadata with toolkit and version info
    let mut meta_map = serde_json::Map::new();
    if let Some(toolkit) = &composio_tool.toolkit {
        meta_map.insert(
            "toolkit_slug".to_string(),
            serde_json::Value::String(toolkit.slug.clone()),
        );
    }

    if let Some(version) = &composio_tool.version {
        meta_map.insert(
            "version".to_string(),
            serde_json::Value::String(version.clone()),
        );
    }

    // Create metadata with toolkit and version info
    let meta = if !meta_map.is_empty() {
        // Convert our map to a HashMap<String, String> for Meta
        let mut string_map = std::collections::HashMap::new();
        for (key, value) in meta_map {
            let string_value = match value {
                serde_json::Value::String(s) => s,
                _ => value.to_string(),
            };
            string_map.insert(key, string_value);
        }

        // Create a Meta object from our HashMap
        let mut meta_obj = rmcp::model::Meta::new();
        for (key, value) in string_map {
            meta_obj.insert(key, serde_json::Value::String(value));
        }
        Some(meta_obj)
    } else {
        None
    };

    Tool {
        name: composio_tool
            .slug
            .clone()
            .unwrap_or_else(|| composio_tool.name.clone())
            .into(), // Use slug if available, else name
        description: composio_tool.description.clone().map(|s| s.into()),
        input_schema: schema,
        title: Some(composio_tool.name.clone()), // Use display name as title
        output_schema: None,
        annotations: None,
        icons: None,
        meta,
    }
}
