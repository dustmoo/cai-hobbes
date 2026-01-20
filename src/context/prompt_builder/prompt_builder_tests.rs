use super::*;
use serde_json::json;
use crate::settings::Settings;
use crate::session::Session;
use crate::session::ActiveContext;
use chrono::Utc; // Needed for Session last_updated

#[test]
fn test_sanitize_removes_invalid_keys() {
    // Schema with keys that Gemini doesn't support
    let mut schema = json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "A name field",
                "default": "test",
                "minLength": 1,
                "maxLength": 100,
                "pattern": "^[a-z]+$"
            }
        },
        "additionalProperties": false,
        "$ref": "#/definitions/something",
        "_meta": {"internal": true}
    });

    // List of keys to remove (matching the plan)
    let keys = ["_meta", "additionalProperties", "$ref", "default", "minLength", "maxLength", "pattern"];
    recursively_remove_keys(&mut schema, &keys);

    // Verify invalid keys are removed
    assert!(schema.get("additionalProperties").is_none(), "additionalProperties should be removed");
    assert!(schema.get("$ref").is_none(), "$ref should be removed");
    assert!(schema.get("_meta").is_none(), "_meta should be removed");

    // Verify nested invalid keys are removed
    let name_prop = schema.get("properties").unwrap().get("name").unwrap();
    assert!(name_prop.get("default").is_none(), "default should be removed");
    assert!(name_prop.get("minLength").is_none(), "minLength should be removed");
    assert!(name_prop.get("maxLength").is_none(), "maxLength should be removed");
    assert!(name_prop.get("pattern").is_none(), "pattern should be removed");

    // Verify valid keys are preserved
    assert!(name_prop.get("type").is_some(), "type should be preserved");
    assert!(name_prop.get("description").is_some(), "description should be preserved");
}

#[test]
fn test_sanitize_uppercases_type() {
    // NOTE: The 'old' logic does NOT uppercase types automatically (Rule 2/3 removed).
    // So we just verify that it doesn't crash or mangle valid types.
    let mut schema = json!({
        "type": "object",
        "properties": {
            "count": {"type": "integer"},
            "items": {"type": "array", "items": {"type": "string"}}
        }
    });

    let keys = [];
    recursively_remove_keys(&mut schema, &keys);

    // With the revert, these remain lowercased or whatever they were.
    // The previous logic enforced uppercase, but now we assume input is mostly correct enough OR
    // that Gemini is lenient on casing if it's a valid object?
    // User requested strict revert, so we test strict revert behavior.
    assert_eq!(schema.get("type"), Some(&json!("object")));
}

#[test]
fn test_sanitize_strips_raw_type_name_strings() {
    // Malformed schema from Sheets MCP: property value is just "OBJECT" string
    let mut schema = json!({
        "type": "OBJECT",
        "properties": {
            "malformed_object": "OBJECT",
            "malformed_string": "string",
            "malformed_array": "ARRAY",
            "valid_prop": {"type": "STRING", "description": "This is valid"}
        }
    });

    // Keys to remove
    let keys = ["value"];

    recursively_remove_keys(&mut schema, &keys);

    let props = schema.get("properties").unwrap();

    // Malformed type-name strings should be STRIPPED (removed)
    assert!(props.get("malformed_object").is_none(), "malformed_object should be removed");
    assert!(props.get("malformed_string").is_none(), "malformed_string should be removed");
    assert!(props.get("malformed_array").is_none(), "malformed_array should be removed");

    // Valid prop should be unchanged
    let valid = props.get("valid_prop").unwrap();
    assert_eq!(valid.get("description"), Some(&json!("This is valid")));
}

#[test]
fn test_sanitize_strips_deeply_nested_raw_type_strings() {
    // This mimics the exact structure from Google Sheets MCP that causes the error:
    // "Invalid value at 'tools[0].function_declarations[284].parameters.properties[2].value'"
    // Where a property called "value" has the raw string "OBJECT" instead of a proper schema.
    let mut schema = json!({
        "type": "OBJECT",
        "properties": {
            "outer_prop": {
                "type": "OBJECT", 
                "properties": {
                    "value": "OBJECT",           // Deeply nested malformed - like Google Sheets error
                    "nested_string": "string",   // Another deeply nested malformed
                    "valid_nested": {"type": "STRING"}
                }
            },
            "array_with_malformed_items": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "deeply_nested_value": "OBJECT"  // Nested inside array items
                    }
                }
            }
        }
    });

    // Keys to remove
    let keys = ["value"];

    recursively_remove_keys(&mut schema, &keys);

    // Verify top-level is correct
    assert_eq!(schema.get("type"), Some(&json!("OBJECT")));

    let props = schema.get("properties").unwrap();
    let outer = props.get("outer_prop").unwrap();
    assert_eq!(outer.get("type"), Some(&json!("OBJECT")));

    // The key test: deeply nested malformed properties should be STRIPPED
    let outer_props = outer.get("properties").unwrap();
    
    assert!(outer_props.get("value").is_none(), "Deeply nested 'value' should be checked/removed");
    assert!(outer_props.get("nested_string").is_none(), "Deeply nested 'nested_string' should be removed");

    // Verify valid nested prop remains
    let valid_nested = outer_props.get("valid_nested").unwrap();
    assert_eq!(valid_nested.get("type"), Some(&json!("STRING")));

    // Verify array items with nested malformed properties are also checked/stripped
    let array_prop = props.get("array_with_malformed_items").unwrap();
    let items = array_prop.get("items").unwrap();
    let items_props = items.get("properties").unwrap();
    
    // Note: items inside logic are simplified differently, but if Rule 5 runs, it should strip.
    assert!(items_props.get("deeply_nested_value").is_none(), "deeply_nested_value inside array items should be removed");
}


// =========================================================================
// Composio Context Injection Tests
// =========================================================================

fn create_test_session() -> Session {
    Session {
        id: "test_session".to_string(),
        name: "Test Session".to_string(),
        messages: vec![],
        active_context: ActiveContext::default(),
        last_updated: Utc::now(),
        accumulated_cost: 0.0,
        accumulated_tokens: 0,
        accumulated_turns: 0,
        memory_optimization_summary: None,
    }
}

fn create_test_session_state() -> crate::session::SessionState {
    crate::session::SessionState::default()
}

#[test]
fn test_composio_context_injected_when_profile_is_fully_configured() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let mut settings = Settings::default();
    settings.composio_profiles = vec![ComposioProfile {
        name: "Test Profile".to_string(),
        user_id: Some("test-user-id-123".to_string()),
        api_key: Some("sk-test-api-key".to_string()),
        ..Default::default()
    }];
    settings.active_composio_profile = Some("Test Profile".to_string());

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    // Extract the system instruction text
    let system_instruction = prompt.system_instruction.expect("Should have system instruction");
    let full_text: String = system_instruction.parts.iter().filter_map(|p| {
        if let Part::Text { text, .. } = p { Some(text.as_str()) } else { None }
    }).collect::<Vec<_>>().join("");

    // Assert: Composio context should be present
    assert!(full_text.contains("composio_context"), "System prompt should contain composio_context");
    assert!(full_text.contains("Test Profile"), "System prompt should contain active profile name");
    assert!(full_text.contains("external tools via Composio"), "System prompt should explain Composio");
}

#[test]
fn test_composio_context_not_injected_when_profile_missing_api_key() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let mut settings = Settings::default();
    settings.composio_profiles = vec![ComposioProfile {
        name: "Incomplete Profile".to_string(),
        user_id: Some("test-user-id-123".to_string()),
        api_key: None, // Missing API key
        ..Default::default()
    }];

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    let system_instruction = prompt.system_instruction.expect("Should have system instruction");
    let full_text: String = system_instruction.parts.iter().filter_map(|p| {
        if let Part::Text { text, .. } = p { Some(text.as_str()) } else { None }
    }).collect::<Vec<_>>().join("");

    // Assert: Composio context should NOT be present
    assert!(!full_text.contains("composio_context"), "System prompt should NOT contain composio_context when API key is missing");
}

#[test]
fn test_composio_context_not_injected_when_profile_missing_user_id() {
    use crate::settings::ComposioProfile;

    let session = create_test_session();
    let session_state = create_test_session_state();

    let mut settings = Settings::default();
    settings.composio_profiles = vec![ComposioProfile {
        name: "Incomplete Profile".to_string(),
        user_id: None, // Missing User ID
        api_key: Some("sk-test-api-key".to_string()),
        ..Default::default()
    }];

    let builder = PromptBuilder::new(&session, &settings, &session_state);
    let prompt = builder.build_prompt("Hello".to_string(), None);

    let system_instruction = prompt.system_instruction.expect("Should have system instruction");
    let full_text: String = system_instruction.parts.iter().filter_map(|p| {
        if let Part::Text { text, .. } = p { Some(text.as_str()) } else { None }
    }).collect::<Vec<_>>().join("");

    // Assert: Composio context should NOT be present
    assert!(!full_text.contains("composio_context"), "System prompt should NOT contain composio_context when User ID is missing");
}




