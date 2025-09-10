use crate::components::llm::{Content, Part, SystemInstruction};
use crate::session::{Session, Tool};
use crate::settings::Settings;
use chrono::Utc;
use serde_json::{self, json};
use crate::components::chat::Message;
use crate::components::shared::MessageContent;

impl From<Message> for Content {
    fn from(msg: Message) -> Self {
        let role = if msg.author == "User" { "user" } else { "model" }.to_string();
        match msg.content {
            MessageContent::Text(text) => Content {
                role,
                parts: vec![Part { text }],
            },
            MessageContent::ToolCall(_) => {
                // We don't want to include tool calls in the history this way,
                // so we return an empty part that can be filtered out.
                Content {
                    role,
                    parts: vec![Part { text: String::new() }],
                }
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
}

impl<'a> PromptBuilder<'a> {
    pub fn new(session: &'a Session, settings: &'a Settings) -> Self {
        Self { session, settings }
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
                        // This is the critical fix. The rmcp crate generates an "inputSchema" field,
                        // but the Gemini API expects "parameters". We manually correct it here.
                        if let Some(obj) = tool_value.as_object_mut() {
                            if let Some(schema) = obj.remove("inputSchema") {
                                obj.insert("parameters".to_string(), schema);
                            }
                        }
                        
                        // Next, recursively remove any unsupported keys from the schema.
                        recursively_remove_keys(&mut tool_value, &["exclusiveMaximum", "exclusiveMinimum", "$schema", "additionalProperties", "outputSchema"]);

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
        active_context.system_persona = Some(persona);
        active_context.mcp_tools = None; // Exclude tools from the instruction text.

        let mut system_context_map = serde_json::Map::new();
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&active_context) {
            system_context_map = map;
        }

        let user_name = &active_context.conversation_summary.entities.user_name;
        if user_name.trim().is_empty() {
            system_context_map.insert(
                "user_instruction".to_string(),
                serde_json::Value::String(
                    "Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called.".to_string(),
                ),
            );
        } else {
            system_context_map.remove("user_instruction");
        }

        system_context_map.insert(
            "current_time".to_string(),
            json!({
                "iso_8601": Utc::now().to_rfc3339(),
                "timezone": "UTC"
            }),
        );

        let instruction_text = serde_json::to_string(&system_context_map).unwrap_or_default();
        let system_instruction = if !instruction_text.is_empty() && instruction_text != "{}" {
            Some(SystemInstruction {
                parts: vec![Part { text: instruction_text }],
            })
        } else {
            None
        };

        // 3. Construct the conversational contents.
        let mut contents = Vec::new();
        let history_len = self.settings.chat_history_length;
        let messages = &self.session.messages;
        let mut first_message_id = None;

        // 1. Add the first user message to preserve the original intent.
        if let Some(first_message) = messages.iter().find(|m| m.author == "User") {
            if let MessageContent::Text(_) = &first_message.content {
                 contents.push(first_message.clone().into());
                 first_message_id = Some(first_message.id);
            }
        }

        // 2. Add the last `history_len` messages.
        let start_index = messages.len().saturating_sub(history_len);
        
        for message in messages.iter().skip(start_index) {
            // Avoid duplicating the first message if it's within the recent window
            if Some(message.id) != first_message_id {
                let content: Content = message.clone().into();
                // Only add non-empty text messages
                if !content.parts.iter().any(|p| p.text.is_empty()) {
                    contents.push(content);
                }
            }
        }
        
        // 3. Add the current user message.
        contents.push(Content {
            role: "user".to_string(),
            parts: vec![Part { text: user_message }],
        });

        // 4. Assemble and return the final LlmPrompt object.
        LlmPrompt {
            system_instruction,
            contents,
            tools,
        }
    }
}

/// Recursively traverses a serde_json::Value and removes specified keys.
fn recursively_remove_keys(value: &mut serde_json::Value, keys_to_remove: &[&str]) {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys_to_remove {
                map.remove(*key);
            }
            for (_, val) in map.iter_mut() {
                recursively_remove_keys(val, keys_to_remove);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr.iter_mut() {
                recursively_remove_keys(val, keys_to_remove);
            }
        }
        _ => {}
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::prompt_builder::{recursively_remove_keys, PromptBuilder};
    use crate::mcp::manager::{McpContext, McpServerContext};
    use crate::session::{ActiveContext, ConversationSummary, ConversationSummaryEntities, Session};
    use crate::settings::Settings;
    use chrono::Utc;
    use rmcp::model::Tool;
    use serde_json::json;

    #[test]
    fn test_recursively_remove_keys() {
        let mut value = json!({
            "level1": {
                "exclusiveMaximum": 100,
                "level2": {
                    "exclusiveMinimum": 0,
                    "keep": "this"
                },
                "another_key": "value"
            },
            "level1_array": [
                { "exclusiveMaximum": 50, "data": "A" },
                { "exclusiveMinimum": 5, "data": "B" }
            ]
        });

        let keys_to_remove = ["exclusiveMaximum", "exclusiveMinimum"];
        recursively_remove_keys(&mut value, &keys_to_remove);

        let expected = json!({
            "level1": {
                "level2": {
                    "keep": "this"
                },
                "another_key": "value"
            },
            "level1_array": [
                { "data": "A" },
                { "data": "B" }
            ]
        });

        assert_eq!(value, expected);
    }

    fn create_mock_session_with_tools() -> Session {
        let tool1: Tool = serde_json::from_value(json!({
            "name": "get_weather",
            "description": "Get the current weather",
            "inputSchema": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city and state, e.g. San Francisco, CA",
                        "exclusiveMaximum": 100
                    }
                },
                "required": ["location"]
            }
        }))
        .unwrap();

        let server = McpServerContext {
            name: "weather_server".to_string(),
            description: "Provides weather information".to_string(),
            tools: vec![tool1],
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
        let builder = PromptBuilder::new(&session, &settings);

        let prompt = builder.build_prompt("What's the weather?".to_string(), None);

        let tools = prompt.tools.expect("Should have tools");
        let tool_declarations = &tools[0].function_declarations;
        assert_eq!(tool_declarations.len(), 1);

        let tool_json = &tool_declarations[0];

        // 1. Verify "inputSchema" was renamed to "parameters"
        assert!(tool_json.get("parameters").is_some());
        assert!(tool_json.get("inputSchema").is_none());

        // 2. Verify unsupported keys were removed
        let parameters = tool_json.get("parameters").unwrap();
        let properties = parameters.get("properties").unwrap();
        let location = properties.get("location").unwrap();
        assert!(location.get("exclusiveMaximum").is_none());
        assert!(location.get("type").is_some());
        assert!(parameters.get("$schema").is_none());
        assert!(parameters.get("additionalProperties").is_none());
    }
}
