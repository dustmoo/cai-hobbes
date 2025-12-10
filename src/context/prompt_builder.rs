
use crate::components::llm::{Content, FunctionCallPart, FunctionResponsePart, InlineDataPart, Part, SystemInstruction};
use crate::session::{Session, Tool};
use crate::settings::Settings;
use chrono::Local;
use serde_json::{self, json};
use crate::components::chat::Message;
use crate::components::shared::MessageContent;

impl From<Message> for Content {
    fn from(msg: Message) -> Self {
        let role = if msg.author == "User" { "user" } else { "model" }.to_string();
        match msg.content {
            MessageContent::Text { content: text, .. } => {
                let mut parts = vec![Part::Text { text, thought: None }];
                for attachment in msg.attachments {
                    parts.push(Part::InlineData {
                        inline_data: InlineDataPart {
                            mime_type: attachment.mime_type,
                            data: attachment.data,
                        },
                    });
                }
                Content { role, parts }
            }
            MessageContent::ToolCall(_) => {
                // Tool calls are handled separately in the tool_call_history loop.
                // Return empty parts so this message is filtered out from the main history.
                Content { role, parts: vec![] }
            },
            MessageContent::PermissionRequest(_) => {
                // Permission requests are UI-only and should not be in the prompt history.
                Content { role, parts: vec![] }
            }
            MessageContent::Error { .. } => {
                // Error messages are UI-only and should not be in the prompt history.
                Content { role, parts: vec![] }
            }
        }
    }
}

/// A structured container for all components of an LLM prompt.
#[derive(Debug)]
pub struct LlmPrompt {
    pub system_instruction: Option<SystemInstruction>,
    pub contents: Vec<Content>,
    pub tools: Option<Vec<Tool>>,
}

/// Builds a structured `LlmPrompt` object for the LLM.
pub struct PromptBuilder<'a> {
    session: &'a Session,
    settings: &'a Settings,
    session_state: &'a crate::session::SessionState,
}

impl<'a> PromptBuilder<'a> {
    pub fn new(session: &'a Session, settings: &'a Settings, session_state: &'a crate::session::SessionState) -> Self {
        Self { session, settings, session_state }
    }

    /// Builds the structured `LlmPrompt` with system instructions, tools, and conversation history.
    pub fn build_prompt(
        &self,
        user_message: String,
        _last_agent_message: Option<String>,
    ) -> LlmPrompt {
        // 1. Extract and format tools from the session context.
        let tools = self.session.active_context.mcp_tools.as_ref().map(|mcp_context| {
            let mut function_declarations = Vec::new();
            for server in &mcp_context.servers {
                for tool in &server.tools {
                    if let Ok(mut tool_value) = serde_json::to_value(tool) {
                        if let Some(obj) = tool_value.as_object_mut() {
                            // 1. Remove invalid keys from the top-level of the function declaration itself.
                            obj.remove("annotations");
                            obj.remove("outputSchema");
                            obj.remove("_meta");

                            // 2. Rename inputSchema to parameters and sanitize it.
                            if let Some(mut schema) = obj.remove("inputSchema") {
                                // 3. Sanitize the entire schema recursively.
                                recursively_sanitize_schema(&mut schema);

                                // 4. Enforce top-level structural requirements after sanitization.
                                if let Some(schema_obj) = schema.as_object_mut() {
                                    schema_obj.remove("$schema");

                                    // Gemini requires `type: "object"` if `properties` are present.
                                    if schema_obj.contains_key("properties") {
                                        schema_obj.insert("type".to_string(), json!("OBJECT"));
                                    }
                                }
                                obj.insert("parameters".to_string(), schema);
                            }
                        }
                        // Finally, remove the 'title' field from the top-level tool declaration, as it's not supported by Gemini.
                        if let Some(obj) = tool_value.as_object_mut() {
                            obj.remove("title");
                            
                            // Prefix the tool name with the server name to avoid collisions
                            if let Some(name_val) = obj.get("name").and_then(|v| v.as_str()) {
                                let new_name = format!("{}_{}", server.name, name_val);
                                obj.insert("name".to_string(), json!(new_name));
                            }
                        }
                        function_declarations.push(tool_value);
                    }
                }
            }
            vec![Tool { function_declarations }]
        });

        // 2. Build the system instruction from the remaining context.
        let mut active_context = self.session.active_context.clone();
        let mut persona = self.settings.persona.clone();
        if let Some(instruction) = &self.settings.force_tool_use_instruction {
            persona = format!("{}\n\nCRITICAL INSTRUCTION: {}", persona, instruction);
        }



        // Check if the last message is an empty placeholder from Hobbes (continuation scenario)
        let last_message = self.session.messages.last();
        let is_continuation_placeholder = last_message.map_or(false, |m| {
            m.author == "Hobbes" && matches!(m.content, MessageContent::Text { ref content, .. } if content.is_empty())
        });

        if user_message.is_empty() {
            // Check if the last message (or the one before placeholder) was a tool call
            let message_to_check = if is_continuation_placeholder {
                if self.session.messages.len() >= 2 {
                    self.session.messages.get(self.session.messages.len() - 2)
                } else {
                    None
                }
            } else {
                last_message
            };

            let last_message_was_tool = message_to_check.map_or(false, |m| {
                matches!(m.content, MessageContent::ToolCall(_))
            });

            if last_message_was_tool {
                let tool_completion_instruction = "\n\nTOOL COMPLETION INSTRUCTION: The tool execution has completed. Use the tool output above to answer the user's request. Do not ask the user for the tool output again.";
                persona.push_str(tool_completion_instruction);
            } else {
                let continuation_instruction = "\n\nCONTINUATION INSTRUCTION: You were the last one to speak. The user has not replied. Continue the conversation based on the existing context. Do not repeat yourself. Provide new information or ask a clarifying question.";
                persona.push_str(continuation_instruction);
            }
        }

        if self.session_state.tool_call_history.iter().any(|r| matches!(r.result.status, crate::components::shared::ToolCallStatus::Error)) {
            let recovery_instruction = "\n\nCRITICAL RECOVERY INSTRUCTION: A previous tool call failed. Analyze the error message and attempt a different tool call to accomplish the user's goal. Do not repeat the failed tool call.";
            persona.push_str(recovery_instruction);
        }
        
        active_context.system_persona = Some(persona);
        active_context.mcp_tools = None; // Exclude tools from the instruction text.

        let mut system_context_map = serde_json::Map::new();
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&active_context) {
            system_context_map = map;
        }

        // Determine the user's name, prioritizing settings over conversation summary.
        let final_user_name = self.settings.user_name.as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let name_from_summary = &active_context.conversation_summary.entities.user_name;
                if !name_from_summary.trim().is_empty() {
                    Some(name_from_summary.as_str())
                } else {
                    None
                }
            });

        if let Some(name) = final_user_name {
            // If we have a name, add it to the context and ensure the instruction is removed.
            system_context_map.insert("user_name".to_string(), json!(name));
            system_context_map.remove("user_instruction");
        } else {
            // If no name is found, add the instruction to ask for it and guide the user to settings.
            system_context_map.insert(
                "user_instruction".to_string(),
                json!("Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called. Direct them to set this in the 'Application Behavior' section of the settings."),
            );
        }

        system_context_map.insert(
            "current_time".to_string(),
            json!({
                "iso_8601": Local::now().to_rfc3339(),
                "timezone": "Local"
            }),
        );

        let instruction_text = serde_json::to_string(&system_context_map).unwrap_or_default();
        let system_instruction = if !instruction_text.is_empty() && instruction_text != "{}" {
            Some(SystemInstruction {
                parts: vec![Part::Text { text: instruction_text, thought: None }],
            })
        } else {
            None
        };

        // 3. Construct the conversational contents.
        let mut contents = Vec::new();
        let history_len = self.settings.chat_history_length;
        let messages = &self.session.messages;
        let mut first_message_id = None;
        let mut last_thought_signature: Option<String> = None;

        // 1. Add the first user message to preserve the original intent.
        if let Some(first_message) = messages.iter().find(|m| m.author == "User") {
            if let MessageContent::Text { .. } = &first_message.content {
                 contents.push(first_message.clone().into());
                 first_message_id = Some(first_message.id);
            }
        }

        // 2. Add the last `history_len` messages.
        let start_index = messages.len().saturating_sub(history_len);
        
        for message in messages.iter().skip(start_index) {
            // Avoid duplicating the first message if it's within the recent window
            if Some(message.id) != first_message_id {
                // Skip the empty placeholder message if it's the last one
                if is_continuation_placeholder && message.id == last_message.unwrap().id {
                    continue;
                }

                match &message.content {
                    MessageContent::Text { thought_signature, .. } => {
                        if let Some(sig) = thought_signature {
                            last_thought_signature = Some(sig.clone());
                        }
                        let content: Content = message.clone().into();
                        if !content.parts.is_empty() {
                            contents.push(content);
                        }
                        
                        // Handle comments as separate user messages
                        if !message.comments.is_empty() {
                            let mut comment_text = String::from("[User comments on the above message:");
                            for comment in &message.comments {
                                comment_text.push_str(&format!("\n- On \"{}\": {}", comment.text_selection, comment.comment));
                            }
                            comment_text.push_str("]");
                            
                            contents.push(Content {
                                role: "user".to_string(),
                                parts: vec![Part::Text { text: comment_text, thought: None }],
                            });
                        }
                    }
                    MessageContent::ToolCall(tc) => {
                        // Update thought signature if present, or use the last one
                        let current_thought_signature = tc.thought_signature.clone().or(last_thought_signature.clone());
                        if let Some(sig) = &tc.thought_signature {
                            last_thought_signature = Some(sig.clone());
                        }

                        // 1. Add the model's function call
                        let args_value: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                        
                        // Log the thought_signature field for debugging
                        if let Some(ref thought_sig) = current_thought_signature {
                            tracing::info!("Reconstructing function call '{}' with thought_signature: '{}'", 
                                tc.tool_name, 
                                if thought_sig.len() > 50 { &thought_sig[..50] } else { thought_sig }
                            );
                        } else {
                            tracing::warn!("Reconstructing function call '{}' WITHOUT thought_signature field - THIS WILL CAUSE API ERROR", tc.tool_name);
                        }
                        
                        contents.push(Content {
                            role: "model".to_string(),
                            parts: vec![Part::FunctionCall {
                                function_call: FunctionCallPart {
                                    name: format!("{}_{}", tc.server_name, tc.tool_name),
                                    args: args_value,
                                },
                                thought_signature: current_thought_signature.clone(),
                            }],
                        });

                        // 2. Add the user's function response
                        // Truncation logic: If this is a historical tool call (not the most recent message),
                        // and it exceeds the max length, truncate it to save context.
                        // We check against last_message_id (ignoring placeholder) to determine if it's historical.
                        let _is_historical = Some(message.id) != first_message_id && // Safety check, though first msg is usually User
                                          Some(message.id) != last_message.map(|m| m.id) &&
                                          (!is_continuation_placeholder ||
                                           Some(message.id) != self.session.messages.get(self.session.messages.len().saturating_sub(2)).map(|m| m.id));

                        // Better historical check:
                        // The last "meaningful" message is either the last message, or the one before the placeholder.
                        let last_meaningful_id = if is_continuation_placeholder {
                            if self.session.messages.len() >= 2 {
                                self.session.messages.get(self.session.messages.len() - 2).map(|m| m.id)
                            } else {
                                None
                            }
                        } else {
                            last_message.map(|m| m.id)
                        };

                        let is_active_tool_call = Some(message.id) == last_meaningful_id;
                        
                        let mut result_string = tc.response.clone();
                        let max_len = if is_active_tool_call {
                            self.settings.max_active_tool_output_length
                        } else {
                            self.settings.max_tool_output_length
                        };

                        if result_string.len() > max_len {
                            let original_len = result_string.len();
                            result_string.truncate(max_len);
                            result_string.push_str(&format!("... [Output truncated from {} chars - excessive size]", original_len));
                        }

                        let result_value: serde_json::Value = serde_json::from_str(&result_string)
                            .unwrap_or_else(|_| json!(result_string));
                        
                        contents.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part::FunctionResponse {
                                function_response: FunctionResponsePart {
                                    name: format!("{}_{}", tc.server_name, tc.tool_name),
                                    response: json!({ "result": result_value }),
                                },
                            }],
                        });

                        // Handle comments as separate user messages
                        if !message.comments.is_empty() {
                            let mut comment_text = String::from("[User comments on the above message:");
                            for comment in &message.comments {
                                comment_text.push_str(&format!("\n- On \"{}\": {}", comment.text_selection, comment.comment));
                            }
                            comment_text.push_str("]");
                            
                            contents.push(Content {
                                role: "user".to_string(),
                                parts: vec![Part::Text { text: comment_text, thought: None }],
                            });
                        }
                    }
                    MessageContent::PermissionRequest(_) => {
                        // Skip permission requests in the prompt
                    }
                    MessageContent::Error { .. } => {
                        // Skip error messages in the prompt
                    }
                }
            }
        }

        // 5. Add the current user message, only if it's not empty.
        if !user_message.is_empty() {
            contents.push(Content {
                role: "user".to_string(),
                parts: vec![Part::Text { text: user_message, thought: None }],
            });
        }

        // 6. Assemble and return the final LlmPrompt object.
        LlmPrompt {
            system_instruction,
            contents,
            tools,
        }
    }
}

/// A comprehensive, recursive sanitizer for Gemini tool schemas.
fn recursively_sanitize_schema(value: &mut serde_json::Value) {
    // Pass 1: Simplify complex structures first.
    simplify_schema(value);
    // Pass 2: Fix types and remove invalid keys from the simplified structure.
    fix_and_remove_invalid_fields(value);
}

/// Pass 1: Recursively simplifies complex schema structures like `oneOf` and `items` arrays.
fn simplify_schema(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        // Simplify `oneOf`, `anyOf`, `allOf` by taking the first element.
        for key in ["oneOf", "anyOf", "allOf"].iter() {
            if let Some(arr_val) = map.remove(*key) {
                if let Some(arr) = arr_val.as_array() {
                    if let Some(first_item) = arr.get(0) {
                        // If the first item is an object, merge its properties into the current map.
                        if let Some(obj) = first_item.as_object() {
                            for (k, v) in obj {
                                map.insert(k.clone(), v.clone());
                            }
                        }
                        break;
                    }
                }
            }
        }

        // If `items` is an array, replace it with its first element.
        if let Some(items_val) = map.get_mut("items") {
            if let Some(arr) = items_val.as_array() {
                if let Some(first_item) = arr.get(0) {
                    *items_val = first_item.clone();
                } else {
                    // If the array is empty, replace it with an empty object to satisfy the API.
                    *items_val = json!({});
                }
            }
        }

        // Recurse into all child values.
        for (_, val) in map.iter_mut() {
            simplify_schema(val);
        }
    } else if let serde_json::Value::Array(arr) = value {
        for val in arr.iter_mut() {
            simplify_schema(val);
        }
    }
}

/// Pass 2: Recursively fixes enums, enforces object types, and removes invalid keys.
fn fix_and_remove_invalid_fields(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        // Rule 1: Convert numeric enums to strings.
        if let Some(enum_val) = map.get_mut("enum") {
            if let serde_json::Value::Array(arr) = enum_val {
                if arr.iter().any(|v| v.is_number() || v.is_null()) {
                    let new_arr: Vec<serde_json::Value> = arr
                        .iter()
                        .map(|v| serde_json::Value::String(v.to_string().replace('\"', "")))
                        .collect();
                    *enum_val = serde_json::Value::Array(new_arr);
                    // If we converted a numeric enum, we must also change the type to STRING.
                    map.insert("type".to_string(), json!("STRING"));
                }
            }
        }

        // Rule 2: Enforce `type: "object"` if `properties` or `required` exist.
        if map.contains_key("properties") || map.contains_key("required") {
            map.insert("type".to_string(), json!("OBJECT"));
        }

        // Rule 3: Convert type values to uppercase for Gemini compatibility.
        if let Some(type_val) = map.get_mut("type") {
            if let serde_json::Value::Array(arr) = type_val {
                // If type is an array (e.g., ["string", "number"]), take the first type.
                if let Some(first) = arr.get(0) {
                    if let Some(s) = first.as_str() {
                        *type_val = serde_json::Value::String(s.to_string());
                    }
                }
            }
            if let serde_json::Value::String(s) = type_val {
                *type_val = serde_json::Value::String(s.to_uppercase());
            }
        }

        // Rule 4: Remove globally invalid keys.
        let keys_to_remove = [
            "exclusiveMaximum",
            "exclusiveMinimum",
            "ge",
            "le",
            "additionalProperties",
            "$ref",
            "_meta",
            // "type" // DO NOT REMOVE: This is required for nested objects and arrays.
        ];
        for key in &keys_to_remove {
            map.remove(*key);
        }

        // Recurse into all child values after processing the current level.
        for (_, val) in map.iter_mut() {
            fix_and_remove_invalid_fields(val);
        }
    } else if let serde_json::Value::Array(arr) = value {
        for val in arr.iter_mut() {
            fix_and_remove_invalid_fields(val);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::manager::{McpContext, McpServerContext};
    use crate::session::{ActiveContext, ConversationSummary, ConversationSummaryEntities, Session};
    use crate::settings::Settings;
    use chrono::Utc;
    use rmcp::model::Tool;
    use serde_json::json;
    use uuid::Uuid;
    use crate::components::chat::Message;
    use crate::components::shared::MessageContent;

    fn create_mock_session_with_tools() -> Session {
        let tool1: Tool = serde_json::from_value(json!({
            "name": "get_weather",
            "description": "Get the current weather",
            "annotations": { "source": "test" },
            "outputSchema": { "type": "string" },
            "inputSchema": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                // "type": "object", // Intentionally missing to test enforcement
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "$ref": "#/definitions/location"
                    },
                    "options": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "unit": { "type": "string" },
                            "priority": {
                                "type": "number",
                                "enum": [1, 2, 3, 4, null]
                            }
                        }
                    },
                    "tags": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "name": { "type": "string" } }
                        }
                    },
                    "deep_items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "level1": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": { "name": { "type": "string" } }
                                    }
                                }
                            }
                        }
                    },
                    "complex_items": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": { "name": { "type": "string" } }
                                }
                            ]
                        }
                    },
                    "array_items": {
                        "type": "array",
                        "items": [
                            {
                                "type": "object",
                                "properties": { "id": { "type": "string" } }
                            }
                        ]
                    }
                },
                "required": ["location"],
                "definitions": {
                    "location": {
                        "type": "string",
                        "description": "The city and state"
                    }
                }
            }
        }))
        .unwrap();

        let tool2: Tool = serde_json::from_value(json!({
            "name": "complex_tool",
            "description": "A tool with a more complex schema",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "string_array": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "object_array": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "key": { "type": "string" } },
                            "required": ["key"]
                        }
                    },
                    "string_enum": {
                        "type": "string",
                        "enum": ["A", "B", "C"]
                    },
                    "mixed_enum": {
                        "type": "string",
                        "enum": ["A", 1, null, 3.14]
                    },
                    "empty_items_array": {
                        "type": "array",
                        "items": []
                    }
                }
            }
        }))
        .unwrap();

        let server = McpServerContext {
            name: "weather_server".to_string(),
            description: "Provides weather information".to_string(),
            tools: vec![tool1, tool2],
        };

        let mcp_context = McpContext {
            servers: vec![server],
        };

        let active_context = ActiveContext {
            mcp_tools: Some(mcp_context),
            conversation_summary: ConversationSummary {
                summary: "".to_string(),
                sentiment: "neutral".to_string(),
                entities: ConversationSummaryEntities {
                    user_name: "TestUser".to_string(),
                    ..Default::default()
                },
            },
            ..Default::default()
        };

        Session {
            id: "test_session".to_string(),
            name: "Test Session".to_string(),
            messages: vec![],
            active_context,
            last_updated: Utc::now(),
        }
    }

    #[test]
    fn test_build_prompt_renames_schema_and_removes_keys() {
        let session = create_mock_session_with_tools();
        let settings = Settings::default();
        let session_state = crate::session::SessionState::default();
        let builder = PromptBuilder::new(&session, &settings, &session_state);

        let prompt = builder.build_prompt("What's the weather?".to_string(), None);

        let tools = prompt.tools.expect("Should have tools");
        let tool_declarations = &tools[0].function_declarations;
        assert_eq!(tool_declarations.len(), 2);

        let tool_json = &tool_declarations[0];

        // 1. Verify "inputSchema" was renamed to "parameters"
        assert!(tool_json.get("parameters").is_some());
        assert!(tool_json.get("inputSchema").is_none());

        // 2. Verify invalid top-level tool keys were removed
        assert!(tool_json.get("annotations").is_none());
        assert!(tool_json.get("outputSchema").is_none());

        // 3. Verify unsupported keys were removed from the parameters schema
        let parameters = tool_json.get("parameters").unwrap();
        // Top-level schema keys
        assert!(parameters.get("$schema").is_none());
        assert_eq!(parameters.get("type"), Some(&json!("OBJECT"))); // Type is enforced
        assert!(parameters.get("additionalProperties").is_none()); // Recursive removal

        let properties = parameters.get("properties").unwrap();
        let location = properties.get("location").unwrap();
        assert!(location.get("$ref").is_none()); // Recursive removal of $ref

        let options = properties.get("options").unwrap();
        assert!(options.get("additionalProperties").is_none()); // Nested removal

        let priority = options.get("properties").unwrap().get("priority").unwrap();
        let priority_enum = priority.get("enum").unwrap().as_array().unwrap();
        assert_eq!(priority_enum[0], "1");
        assert_eq!(priority_enum[4], "null"); // Handles null correctly

        let tags = properties.get("tags").unwrap();
        let tag_items = tags.get("items").unwrap();
        assert_eq!(tag_items.get("type"), Some(&json!("OBJECT"))); // Type is NOT removed from items with properties
        assert!(tag_items.get("properties").is_some());

        let deep_items = properties.get("deep_items").unwrap();
        let deep_items_level1 = deep_items.get("items").unwrap().get("properties").unwrap().get("level1").unwrap();
        let deep_items_level2_items = deep_items_level1.get("items").unwrap();
        assert_eq!(deep_items_level2_items.get("type"), Some(&json!("OBJECT"))); // Verify deeply nested type is NOT removed
        
        let complex_items = properties.get("complex_items").unwrap();
        let complex_items_items = complex_items.get("items").unwrap();
        assert!(complex_items_items.get("oneOf").is_none()); // oneOf is simplified
        assert!(complex_items_items.get("properties").is_some());
        assert_eq!(complex_items_items.get("type"), Some(&json!("OBJECT"))); // type is NOT removed after simplification

        let array_items = properties.get("array_items").unwrap();
        let array_items_items = array_items.get("items").unwrap();
        assert!(array_items_items.is_object()); // items array is simplified to its first object
        assert!(array_items_items.get("properties").is_some());

        // Verify the second, more complex tool
        let complex_tool_json = &tool_declarations[1];
        let complex_params = complex_tool_json.get("parameters").unwrap();
        let complex_props = complex_params.get("properties").unwrap();

        // Check string array
        let string_array = complex_props.get("string_array").unwrap();
        assert_eq!(string_array.get("type"), Some(&json!("ARRAY")));
        assert_eq!(string_array.get("items").unwrap().get("type"), Some(&json!("STRING")));

        // Check object array
        let object_array = complex_props.get("object_array").unwrap();
        assert_eq!(object_array.get("type"), Some(&json!("ARRAY")));
        let object_array_items = object_array.get("items").unwrap();
        assert_eq!(object_array_items.get("type"), Some(&json!("OBJECT")));
        assert!(object_array_items.get("properties").is_some());

        // Check mixed enum is converted to strings
        let mixed_enum = complex_props.get("mixed_enum").unwrap();
        let enum_values = mixed_enum.get("enum").unwrap().as_array().unwrap();
        assert_eq!(enum_values[0], "A");
        assert_eq!(enum_values[1], "1");
        assert_eq!(enum_values[2], "null");
        assert_eq!(enum_values[3], "3.14");

        // Check empty items array is converted to an empty object
        let empty_items_array = complex_props.get("empty_items_array").unwrap();
        assert_eq!(empty_items_array.get("type"), Some(&json!("ARRAY")));
        let empty_items = empty_items_array.get("items").unwrap();
        assert!(empty_items.is_object());
        assert!(empty_items.as_object().unwrap().is_empty());
    }
    #[test]
    fn test_build_prompt_with_continuation_placeholder() {
        let mut session = create_mock_session_with_tools();
        let settings = Settings::default();
        let session_state = crate::session::SessionState::default();
        
        // 1. Add a tool call message
        let tool_call_msg = Message {
            id: Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(crate::components::shared::ToolCall {
                execution_id: Uuid::new_v4().to_string(),
                server_name: "weather_server".to_string(),
                tool_name: "get_weather".to_string(),
                arguments: "{}".to_string(),
                status: crate::components::shared::ToolCallStatus::Completed,
                response: "{\"temp\": 72}".to_string(),
                thought_signature: Some("Checking weather...".to_string()),
            }),
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
        };
        session.messages.push(tool_call_msg);

        // 2. Add an empty placeholder message (simulating the continuation start)
        let placeholder_msg = Message {
            id: Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::Text { content: "".to_string(), thought_signature: None, thought_summary: None },
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
        };
        session.messages.push(placeholder_msg);

        let builder = PromptBuilder::new(&session, &settings, &session_state);
        let prompt = builder.build_prompt("".to_string(), None);

        // 3. Verify System Instruction contains TOOL COMPLETION INSTRUCTION
        // The current implementation will likely FAIL this because it looks at the last message (placeholder)
        let system_instruction = if let Some(crate::components::llm::Part::Text { text, .. }) = prompt.system_instruction.as_ref().unwrap().parts.get(0) {
            text
        } else {
            panic!("System instruction should be text");
        };
        assert!(system_instruction.contains("TOOL COMPLETION INSTRUCTION"), "System instruction should contain tool completion instruction");

        // 4. Verify the contents do NOT contain the empty placeholder
        // The current implementation will likely FAIL this
        let last_content = prompt.contents.last().unwrap();
        if let Some(crate::components::llm::Part::Text { text, .. }) = last_content.parts.get(0) {
             assert!(!text.is_empty(), "Last content should not be empty placeholder");
        }
    }

    #[test]
    fn test_build_prompt_removes_meta_field() {
        // Define a tool with _meta fields at top level and nested in schema
        let tool: Tool = serde_json::from_value(json!({
            "name": "meta_tool",
            "description": "A tool with _meta fields",
            "_meta": { "internal_id": 123 },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "field1": {
                        "type": "string",
                        "_meta": "nested meta info"
                    }
                },
                "_meta": "schema meta info"
            }
        })).unwrap();

        let server = McpServerContext {
            name: "meta_server".to_string(),
            description: "Server with meta tools".to_string(),
            tools: vec![tool],
        };

        let mcp_context = McpContext { servers: vec![server] };
        
        let active_context = ActiveContext {
            mcp_tools: Some(mcp_context),
            ..Default::default()
        };

        let session = Session {
            id: "test_meta".to_string(),
            name: "Test Meta".to_string(),
            messages: vec![],
            active_context,
            last_updated: Utc::now(),
        };

        let settings = Settings::default();
        let session_state = crate::session::SessionState::default();
        let builder = PromptBuilder::new(&session, &settings, &session_state);

        let prompt = builder.build_prompt("test".to_string(), None);
        let tools = prompt.tools.expect("Should have tools");
        let tool_json = &tools[0].function_declarations[0];

        // Verify top-level _meta is removed
        assert!(tool_json.get("_meta").is_none(), "Top-level _meta should be removed");

        // Verify nested _meta is removed from parameters
        let parameters = tool_json.get("parameters").unwrap();
        assert!(parameters.get("_meta").is_none(), "Schema-level _meta should be removed");
        
        let field1 = parameters.get("properties").unwrap().get("field1").unwrap();
        assert!(field1.get("_meta").is_none(), "Nested _meta should be removed");
    }

    #[test]
    fn test_build_prompt_backfills_thought_signature() {
        let mut session = create_mock_session_with_tools();
        let settings = Settings::default();
        let session_state = crate::session::SessionState::default();
        
        let signature = "original_signature".to_string();

        // 1. First tool call with signature
        let tool_call_1 = Message {
            id: Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(crate::components::shared::ToolCall {
                execution_id: Uuid::new_v4().to_string(),
                server_name: "server".to_string(),
                tool_name: "tool1".to_string(),
                arguments: "{}".to_string(),
                status: crate::components::shared::ToolCallStatus::Completed,
                response: "{}".to_string(),
                thought_signature: Some(signature.clone()),
            }),
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
        };
        // Clear existing messages to be clean
        session.messages.clear();
        session.messages.push(tool_call_1);

        // 2. Second tool call WITHOUT signature
        let tool_call_2 = Message {
            id: Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(crate::components::shared::ToolCall {
                execution_id: Uuid::new_v4().to_string(),
                server_name: "server".to_string(),
                tool_name: "tool2".to_string(),
                arguments: "{}".to_string(),
                status: crate::components::shared::ToolCallStatus::Completed,
                response: "{}".to_string(),
                thought_signature: None,
            }),
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
        };
        session.messages.push(tool_call_2);

        let builder = PromptBuilder::new(&session, &settings, &session_state);
        let prompt = builder.build_prompt("User message".to_string(), None);

        // Verify contents
        // Expected: 
        // 0. Model: Tool Call 1 (with sig)
        // 1. User: Tool Result 1
        // 2. Model: Tool Call 2 (with BACKFILLED sig)
        // 3. User: Tool Result 2
        // 4. User: "User message"

        let model_msg_2 = &prompt.contents[2];
        assert_eq!(model_msg_2.role, "model");
        if let crate::components::llm::Part::FunctionCall { thought_signature, .. } = &model_msg_2.parts[0] {
            assert_eq!(thought_signature.as_ref().unwrap(), &signature, "Second tool call should have backfilled signature");
        } else {
            panic!("Expected FunctionCall part");
        }
    }

    #[test]
    fn test_build_prompt_prefixes_tool_names() {
        let session = create_mock_session_with_tools();
        let settings = Settings::default();
        let session_state = crate::session::SessionState::default();
        let builder = PromptBuilder::new(&session, &settings, &session_state);

        let prompt = builder.build_prompt("Verify tool prefixing".to_string(), None);
        let tools = prompt.tools.expect("Should have tools");
        let tool_declarations = &tools[0].function_declarations;

        // "weather_server" is the server name in mock
        // "get_weather" is the tool name
        
        let found_names: Vec<String> = tool_declarations.iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
            .collect();
        println!("Found tool names: {:?}", found_names);

        let weather_tool = tool_declarations.iter().find(|t| {
            t.get("name").and_then(|n| n.as_str()) == Some("weather_server_get_weather")
        });
        
        assert!(weather_tool.is_some(), "Tool name should be prefixed with server name. Found: {:?}", found_names);
    }
}
