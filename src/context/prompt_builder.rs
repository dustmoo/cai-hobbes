use crate::components::llm::{Content, Part, SystemInstruction};
use crate::session::{Session, Tool};
use crate::settings::Settings;
use serde_json::{self};

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
        last_agent_message: Option<String>,
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
                        recursively_remove_keys(&mut tool_value, &["exclusiveMaximum", "exclusiveMinimum"]);

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
        if let Some(agent_msg) = last_agent_message {
            if !agent_msg.is_empty() {
                contents.push(Content {
                    role: "model".to_string(),
                    parts: vec![Part { text: agent_msg }],
                });
            }
        }
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
