// Type-safe Gemini API schema definitions.
// These structs match Gemini's Protobuf definitions for FunctionDeclaration and Schema.
// Unsupported fields are intentionally omitted to ensure Gemini compatibility.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Gemini's supported schema types.
/// See: <https://ai.google.dev/api/caching#Type>
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum SchemaType {
    Object,
    String,
    Number,
    Integer,
    Boolean,
    Array,
}

/// A Gemini-compatible JSON Schema.
/// Only includes fields supported by the Gemini API.
/// Unsupported fields (additionalProperties, $ref, default, etc.) are intentionally omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiSchema {
    #[serde(rename = "type")]
    pub schema_type: SchemaType,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<HashMap<String, GeminiSchema>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<GeminiSchema>>,

    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
}

impl Default for GeminiSchema {
    fn default() -> Self {
        Self {
            schema_type: SchemaType::Object,
            description: None,
            properties: None,
            required: None,
            items: None,
            enum_values: None,
        }
    }
}

/// A Gemini-compatible function declaration.
/// This is what gets sent to the Gemini API in the `tools.function_declarations` array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiFunctionDeclaration {
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<GeminiSchema>,
}

/// A Gemini tool container.
/// This wraps function declarations for the Gemini API request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct GeminiTool {
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_serialization() {
        let schema = GeminiSchema {
            schema_type: SchemaType::Object,
            description: Some("Test schema".to_string()),
            properties: Some({
                let mut map = HashMap::new();
                map.insert(
                    "name".to_string(),
                    GeminiSchema {
                        schema_type: SchemaType::String,
                        description: Some("User's name".to_string()),
                        ..Default::default()
                    },
                );
                map
            }),
            required: Some(vec!["name".to_string()]),
            items: None,
            enum_values: None,
        };

        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("\"type\":\"OBJECT\""));
        assert!(json.contains("\"name\""));
        assert!(!json.contains("additionalProperties")); // Should NOT be present
    }

    #[test]
    fn test_function_declaration_serialization() {
        let decl = GeminiFunctionDeclaration {
            name: "get_weather".to_string(),
            description: Some("Get the current weather".to_string()),
            parameters: Some(GeminiSchema {
                schema_type: SchemaType::Object,
                properties: Some({
                    let mut map = HashMap::new();
                    map.insert(
                        "location".to_string(),
                        GeminiSchema {
                            schema_type: SchemaType::String,
                            description: Some("City name".to_string()),
                            ..Default::default()
                        },
                    );
                    map
                }),
                required: Some(vec!["location".to_string()]),
                ..Default::default()
            }),
        };

        let json = serde_json::to_string(&decl).unwrap();
        assert!(json.contains("\"name\":\"get_weather\""));
        assert!(json.contains("\"parameters\""));
    }
}
