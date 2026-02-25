// MCP to Gemini conversion logic.
// This module provides explicit, type-safe conversion from MCP tools to Gemini function declarations.

use crate::gemini::types::{GeminiFunctionDeclaration, GeminiSchema, SchemaType};
use crate::mcp::manager::{COMPOSIO_NATIVE_PREFIX, is_composio_native};
use serde_json::Value;
use std::collections::HashMap;

/// Gemini API rejects FunctionDeclaration schemas that exceed this nesting depth.
/// Empirically ~4 levels is the limit before INVALID_ARGUMENT errors occur.
const MAX_NESTING_DEPTH: usize = 4;

/// Errors that can occur during MCP-to-Gemini conversion.
#[derive(Debug)]
pub enum ConversionError {
    /// The input schema is missing or invalid.
    InvalidSchema(String),
    /// A required field is missing.
    MissingField(String),
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::InvalidSchema(msg) => write!(f, "Invalid schema: {}", msg),
            ConversionError::MissingField(field) => write!(f, "Missing required field: {}", field),
        }
    }
}

impl std::error::Error for ConversionError {}

/// Convert an MCP tool (rmcp::model::Tool) to a Gemini-compatible FunctionDeclaration.
///
/// This function handles all the edge cases and unsupported fields:
/// - Unsupported schema keys are ignored (not present in GeminiSchema)
/// - Properties with string values (like "OBJECT") are skipped
/// - The `required` array is filtered to only include existing properties
/// - Type names are normalized to uppercase
///
/// # Arguments
/// * `tool` - The MCP tool to convert
/// * `server_name` - The MCP server name (used for prefixing)
///
/// # Returns
/// A `Result` containing the `GeminiFunctionDeclaration` or a `ConversionError`.
/// Sanitize a function name to comply with Gemini API requirements:
/// - Must start with a letter or underscore
/// - Only alphanumeric, underscores, dots, colons, or dashes allowed
/// - Maximum 64 characters
pub fn sanitize_function_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '.' || c == ':' || c == '-' {
                c
            } else {
                '_' // Replace invalid characters with underscores
            }
        })
        .collect();

    // Ensure it starts with a letter or underscore
    if let Some(first_char) = sanitized.chars().next() {
        if !first_char.is_alphabetic() && first_char != '_' {
            sanitized = format!("_{}", sanitized);
        }
    }

    // Truncate to 64 characters
    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }

    // Remove consecutive underscores for cleaner names
    while sanitized.contains("__") {
        sanitized = sanitized.replace("__", "_");
    }

    sanitized
}

pub fn normalize_server_name(server_name: &str) -> &str {
    if is_composio_native(server_name) {
        COMPOSIO_NATIVE_PREFIX
    } else {
        server_name
    }
}

pub fn get_prefixed_tool_name(server_name: &str, tool_name: &str) -> String {
    let norm_server = normalize_server_name(server_name);
    sanitize_function_name(&format!("{}_{}", norm_server, tool_name))
}

pub fn mcp_tool_to_gemini(
    tool: &rmcp::model::Tool,
    server_name: &str,
) -> Result<GeminiFunctionDeclaration, ConversionError> {
    // Create prefixed name and sanitize it for Gemini API requirements
    let prefixed_name = get_prefixed_tool_name(server_name, &tool.name);

    // Convert the input_schema (Arc<Map<String, Value>>) to a serde_json::Value
    let schema_value = Value::Object((*tool.input_schema).clone());
    let parameters = convert_schema(&schema_value)?;

    Ok(GeminiFunctionDeclaration {
        name: prefixed_name,
        description: tool
            .description
            .as_ref()
            .map(|d| d.to_string())
            .filter(|s| !s.is_empty()),
        parameters: Some(parameters),
    })
}

/// Simplify compound schema types (oneOf, anyOf, allOf) that Gemini doesn't support.
/// Takes the first element from these arrays and merges its properties into the result.
fn simplify_compound_types(obj: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut result = obj.clone();

    // Handle oneOf/anyOf/allOf by taking the first element
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(arr_val) = result.remove(key) {
            if let Some(arr) = arr_val.as_array() {
                if let Some(first_item) = arr.first() {
                    // If the first item is an object, merge its properties
                    if let Some(first_obj) = first_item.as_object() {
                        for (k, v) in first_obj {
                            // Don't overwrite existing keys
                            if !result.contains_key(k) {
                                result.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
            // Don't check other compound types once we found one
            break;
        }
    }

    // If items is an array, replace with the first element
    if let Some(items_val) = result.get("items").cloned() {
        if let Some(arr) = items_val.as_array() {
            if let Some(first) = arr.first() {
                result.insert("items".to_string(), first.clone());
            } else {
                // Empty array - provide a default object schema
                result.insert("items".to_string(), serde_json::json!({ "type": "STRING" }));
            }
        }
    }

    result
}

/// Convert a serde_json::Value representing a JSON Schema to a GeminiSchema.
/// This recursively processes the schema, ignoring unsupported fields.
fn convert_schema(value: &Value) -> Result<GeminiSchema, ConversionError> {
    convert_schema_inner(value, 0)
}

/// Inner recursive converter with depth tracking.
/// At depths exceeding MAX_NESTING_DEPTH, schemas are flattened to STRING
/// to prevent Gemini INVALID_ARGUMENT errors on deeply nested tool definitions.
fn convert_schema_inner(value: &Value, depth: usize) -> Result<GeminiSchema, ConversionError> {
    let obj = value
        .as_object()
        .ok_or_else(|| ConversionError::InvalidSchema("Schema must be an object".to_string()))?;

    // Depth guard: flatten to STRING if we've exceeded Gemini's nesting limit
    if depth > MAX_NESTING_DEPTH {
        let desc = obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some("(Schema flattened due to nesting depth limit)".to_string()));
        return Ok(GeminiSchema {
            schema_type: SchemaType::String,
            description: desc,
            properties: None,
            required: None,
            items: None,
            enum_values: None,
        });
    }

    // Pre-process: Handle oneOf/anyOf/allOf by using the first element
    // Gemini doesn't support these compound types
    let working_obj = simplify_compound_types(obj);
    let obj = &working_obj;

    // Determine the type
    let schema_type = if let Some(type_val) = obj.get("type") {
        parse_schema_type(type_val)?
    } else if obj.contains_key("properties") || obj.contains_key("required") {
        // Infer OBJECT if properties or required exists
        SchemaType::Object
    } else {
        // Default to STRING if no type is specified
        SchemaType::String
    };

    // Convert properties (filtering out invalid entries)
    let properties = if let Some(props_val) = obj.get("properties") {
        if let Some(props_obj) = props_val.as_object() {
            let mut converted_props = HashMap::new();
            for (key, val) in props_obj {
                // Skip properties that are raw strings (like "OBJECT" instead of {"type": "OBJECT"})
                if val.is_string() {
                    tracing::warn!(
                        "Skipping property '{}' with invalid string value '{}' during Gemini conversion",
                        key,
                        val.as_str().unwrap_or("")
                    );
                    continue;
                }

                // Recursively convert the property schema
                match convert_schema_inner(val, depth + 1) {
                    Ok(schema) => {
                        converted_props.insert(key.clone(), schema);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Skipping property '{}' due to conversion error: {}",
                            key,
                            e
                        );
                    }
                }
            }
            if converted_props.is_empty() {
                None
            } else {
                Some(converted_props)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Filter required to only include properties that exist
    // BUT: if any required properties were dropped, return an error (tool is incompatible)
    let required = if let Some(req_val) = obj.get("required") {
        if let Some(req_arr) = req_val.as_array() {
            let original_required: Vec<String> = req_arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect();

            let valid_props: std::collections::HashSet<&String> = properties
                .as_ref()
                .map(|p| p.keys().collect())
                .unwrap_or_default();

            // Check for dropped required properties
            let dropped: Vec<&String> = original_required
                .iter()
                .filter(|r| !valid_props.contains(r))
                .collect();

            if !dropped.is_empty() {
                return Err(ConversionError::MissingField(format!(
                    "Required properties dropped during conversion: {:?}",
                    dropped
                )));
            }

            if original_required.is_empty() {
                None
            } else {
                Some(original_required)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Convert items (for arrays)
    let items = if let Some(items_val) = obj.get("items") {
        // Handle null items or failed conversion
        if items_val.is_null() {
            // Null items - provide default for ARRAY types
            if schema_type == SchemaType::Array {
                Some(Box::new(GeminiSchema::default()))
            } else {
                None
            }
        } else {
            match convert_schema_inner(items_val, depth + 1) {
                Ok(schema) => Some(Box::new(schema)),
                Err(_) => {
                    // If items conversion fails, provide default for ARRAY types
                    if schema_type == SchemaType::Array {
                        Some(Box::new(GeminiSchema::default()))
                    } else {
                        None
                    }
                }
            }
        }
    } else {
        // Gemini requires items for ARRAY types
        if schema_type == SchemaType::Array {
            Some(Box::new(GeminiSchema::default()))
        } else {
            None
        }
    };

    // Convert enum values (must all be strings)
    let enum_values = if let Some(enum_val) = obj.get("enum") {
        if let Some(enum_arr) = enum_val.as_array() {
            let string_values: Vec<String> = enum_arr
                .iter()
                .map(|v| {
                    if let Some(s) = v.as_str() {
                        s.to_string()
                    } else {
                        // Convert non-strings to their string representation
                        v.to_string().replace('"', "")
                    }
                })
                .collect();
            if string_values.is_empty() {
                None
            } else {
                Some(string_values)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Get description
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Gemini rejects properties/required on non-OBJECT types,
    // and enum on non-STRING types.
    // Strip them to prevent INVALID_ARGUMENT errors from raw MCP tool schemas.
    let (final_properties, final_required) = if schema_type != SchemaType::Object {
        (None, None)
    } else {
        (properties, required)
    };

    let final_enum = if schema_type != SchemaType::String {
        None
    } else {
        enum_values
    };

    Ok(GeminiSchema {
        schema_type,
        description,
        properties: final_properties,
        required: final_required,
        items,
        enum_values: final_enum,
    })
}

/// Parse a JSON value into a SchemaType.
fn parse_schema_type(value: &Value) -> Result<SchemaType, ConversionError> {
    let type_str = if let Some(s) = value.as_str() {
        s.to_uppercase()
    } else if let Some(arr) = value.as_array() {
        // Handle type arrays like ["string", "null"] - take the first non-null type
        arr.iter()
            .filter_map(|v| v.as_str())
            .find(|s| !s.eq_ignore_ascii_case("null"))
            .map(|s| s.to_uppercase())
            .unwrap_or_else(|| "STRING".to_string())
    } else {
        return Err(ConversionError::InvalidSchema(
            "Type must be a string or array".to_string(),
        ));
    };

    match type_str.as_str() {
        "OBJECT" => Ok(SchemaType::Object),
        "STRING" => Ok(SchemaType::String),
        "NUMBER" => Ok(SchemaType::Number),
        "INTEGER" => Ok(SchemaType::Integer),
        "BOOLEAN" => Ok(SchemaType::Boolean),
        "ARRAY" => Ok(SchemaType::Array),
        _ => {
            tracing::warn!("Unknown type '{}', defaulting to STRING", type_str);
            Ok(SchemaType::String)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_convert_simple_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "User's name"
                },
                "age": {
                    "type": "integer"
                }
            },
            "required": ["name"]
        });

        let result = convert_schema(&schema).unwrap();
        assert_eq!(result.schema_type, SchemaType::Object);
        assert!(result.properties.is_some());
        assert_eq!(result.required, Some(vec!["name".to_string()]));
    }

    #[test]
    fn test_convert_schema_with_invalid_properties() {
        // Schema with a required property that has an invalid value (string instead of object)
        // This should now return an error since we can't include the required property
        let schema = json!({
            "type": "object",
            "properties": {
                "valid_prop": {
                    "type": "string"
                },
                "invalid_prop": "OBJECT"  // This is invalid - should cause conversion to fail
            },
            "required": ["valid_prop", "invalid_prop"]
        });

        // Should fail because required property "invalid_prop" would be dropped
        let result = convert_schema(&schema);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("Required properties dropped"));
    }

    #[test]
    fn test_convert_schema_with_array() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "string"
            }
        });

        let result = convert_schema(&schema).unwrap();
        assert_eq!(result.schema_type, SchemaType::Array);
        assert!(result.items.is_some());
        assert_eq!(result.items.unwrap().schema_type, SchemaType::String);
    }

    #[test]
    fn test_convert_schema_with_enum() {
        let schema = json!({
            "type": "string",
            "enum": ["red", "green", "blue"]
        });

        let result = convert_schema(&schema).unwrap();
        assert_eq!(result.schema_type, SchemaType::String);
        assert_eq!(
            result.enum_values,
            Some(vec![
                "red".to_string(),
                "green".to_string(),
                "blue".to_string()
            ])
        );
    }

    #[test]
    fn test_unsupported_fields_are_ignored() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,  // Unsupported
            "$ref": "#/definitions/foo",     // Unsupported
            "default": "test",               // Unsupported
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,          // Unsupported
                    "maxLength": 100         // Unsupported
                }
            }
        });

        let result = convert_schema(&schema).unwrap();

        // Should succeed and only include supported fields
        assert_eq!(result.schema_type, SchemaType::Object);
        assert!(result.properties.is_some());

        // Verify the serialized output doesn't contain unsupported fields
        let json_str = serde_json::to_string(&result).unwrap();
        assert!(!json_str.contains("additionalProperties"));
        assert!(!json_str.contains("$ref"));
        assert!(!json_str.contains("default"));
        assert!(!json_str.contains("minLength"));
        assert!(!json_str.contains("maxLength"));
    }

    #[test]
    fn test_max_nesting_depth_flattens() {
        // Build a schema 6 levels deep — should flatten at depth > MAX_NESTING_DEPTH (4)
        let schema = json!({
            "type": "object",
            "properties": {
                "level1": {                           // depth 1
                    "type": "object",
                    "properties": {
                        "level2": {                   // depth 2
                            "type": "object",
                            "properties": {
                                "level3": {           // depth 3
                                    "type": "object",
                                    "properties": {
                                        "level4": {   // depth 4
                                            "type": "object",
                                            "properties": {
                                                "level5": {   // depth 5 — exceeds MAX_NESTING_DEPTH
                                                    "type": "object",
                                                    "description": "This is deep",
                                                    "properties": {
                                                        "value": { "type": "string" }
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
        });

        let result = convert_schema(&schema).unwrap();

        // Navigate to depth 5 and verify it was flattened to STRING
        let l1 = result.properties.as_ref().unwrap().get("level1").unwrap();
        let l2 = l1.properties.as_ref().unwrap().get("level2").unwrap();
        let l3 = l2.properties.as_ref().unwrap().get("level3").unwrap();
        let l4 = l3.properties.as_ref().unwrap().get("level4").unwrap();
        let l5 = l4.properties.as_ref().unwrap().get("level5").unwrap();

        // Level 5 should be flattened to STRING (depth > 4)
        assert_eq!(l5.schema_type, SchemaType::String);
        assert!(l5.properties.is_none(), "Flattened schema should have no properties");
        assert!(l5.description.is_some(), "Flattened schema should preserve description");
        assert_eq!(l5.description.as_ref().unwrap(), "This is deep");
    }

    #[test]
    fn test_properties_stripped_from_non_object_types() {
        // Some raw MCP tools attach properties to STRING or ARRAY types
        // Gemini rejects this combination
        let schema = json!({
            "type": "string",
            "properties": {
                "foo": { "type": "string" }
            },
            "required": ["foo"]
        });

        let result = convert_schema(&schema).unwrap();
        assert_eq!(result.schema_type, SchemaType::String);
        assert!(result.properties.is_none(), "properties should be stripped from non-OBJECT");
        assert!(result.required.is_none(), "required should be stripped from non-OBJECT");
    }

    #[test]
    fn test_title_field_ignored() {
        // Raw MCP tools may include 'title' which GeminiSchema doesn't support
        let schema = json!({
            "type": "object",
            "title": "My Tool Parameters",
            "properties": {
                "name": {
                    "type": "string",
                    "title": "User Name"
                }
            }
        });

        let result = convert_schema(&schema).unwrap();
        let json_str = serde_json::to_string(&result).unwrap();
        assert!(!json_str.contains("title"), "title field should not appear in serialized output");
        assert!(!json_str.contains("My Tool Parameters"));
        assert!(!json_str.contains("User Name"));
    }

    #[test]
    fn test_enum_stripped_from_non_string_types() {
        // Exact pattern from ClickUp MCP: max_depth is INTEGER with enum values
        // Gemini error: "properties[max_depth].enum: only allowed for STRING type"
        let schema = json!({
            "type": "object",
            "properties": {
                "max_depth": {
                    "type": "integer",
                    "enum": [1, 2, 3, 4, 5],
                    "description": "Maximum depth level"
                },
                "status": {
                    "type": "string",
                    "enum": ["open", "closed"]
                }
            }
        });

        let result = convert_schema(&schema).unwrap();
        let props = result.properties.as_ref().unwrap();

        // INTEGER with enum — enum should be stripped
        let max_depth = props.get("max_depth").unwrap();
        assert_eq!(max_depth.schema_type, SchemaType::Integer);
        assert!(max_depth.enum_values.is_none(), "enum should be stripped from INTEGER type");
        assert_eq!(max_depth.description.as_ref().unwrap(), "Maximum depth level");

        // STRING with enum — enum should be preserved
        let status = props.get("status").unwrap();
        assert_eq!(status.schema_type, SchemaType::String);
        assert_eq!(
            status.enum_values,
            Some(vec!["open".to_string(), "closed".to_string()])
        );
    }
}
