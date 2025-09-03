use crate::mcp::manager::{McpContext, McpManager, McpServerContext};
use crate::session::Session;
use crate::settings::Settings;
use serde_json::{self, json};
pub struct PromptBuilder;

impl PromptBuilder {
    pub fn new() -> Self {
        Self {}
    }

    /// Builds a context string from the active session's context.
    pub async fn build_context_string(
        &self,
        session: &Session,
        settings: &Settings,
        mcp_manager: &McpManager,
    ) -> String {
        let servers = mcp_manager.servers.lock().await;
        let mut mcp_servers = Vec::new();

        for (_, server) in servers.iter() {
            mcp_servers.push(McpServerContext {
                name: server.config.name.clone(),
                description: server.config.description.clone(),
                tools: server.tools.clone(),
            });
        }

        let mcp_context = McpContext {
            servers: mcp_servers,
        };

        let mut active_context = session.active_context.clone();
        active_context.system_persona = Some(settings.persona.clone());
        active_context.mcp_tools = Some(mcp_context);

        // Check for user_name directly via the typed struct.
        let user_name = &active_context.conversation_summary.entities.user_name;

        if user_name.trim().is_empty() {
            active_context.user_instruction = Some("Your user's name is not in the current SYSTEM_CONTEXT. Please ask them what they would like to be called.".to_string());
        } else {
            // If the user's name is present, ensure the instruction to ask for it is removed.
            active_context.user_instruction = None;
        }

        let context_json = serde_json::to_string_pretty(&active_context).unwrap_or_default();
        format!("<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>\n", context_json)
    }
    pub fn build_tool_result_context(&self, tool_name: &str, result: &serde_json::Value) -> String {
        let tool_result = json!({
            "tool_result": {
                "tool_name": tool_name,
                "result": result
            }
        });
        let context_json = serde_json::to_string_pretty(&tool_result).unwrap_or_default();
        format!("<SYSTEM_CONTEXT>\n{}\n</SYSTEM_CONTEXT>\n", context_json)
    }
}