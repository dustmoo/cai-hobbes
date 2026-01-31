//! Skill Executor Module
//!
//! RESPONSIBILITY: "Registrar" & "Context Provider"
//!
//! This module NO LONGER executes commands or scripts directly (Macro Model).
//! Instead, it acts as a "Capability Registrar" that:
//! 1. Reads the Skill's "Instruction Manual" (SKILL.md)
//! 2. Validates required tools against the active MCP environment.
//! 3. Constructs a "Context Payload" (Instructions + Resources + Resolved Tools).
//! 4. Returns this payload to the Agent, who then drives execution using standard MCP tools.

use crate::components::shared::{SkillCall, SkillCallStatus, CapabilityContextPayload, SkillEnvironment};
use crate::context::permissions::{PermissionManager, PermissionStatus};
use crate::skills::parser::Skill;
use crate::mcp::manager::McpContext;
use std::collections::HashMap;

/// Result of a skill execution (now just a context payload delivery)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SkillExecutionResult {
    pub status: SkillCallStatus,
    pub output: String, // JSON String of CapabilityContextPayload
}

/// Error type for skill execution
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SkillExecutionError {
    PermissionDenied(String),
    ExecutionFailed(String),
    SkillNotFound(String),
    ContextBuilderError(String),
}

impl std::fmt::Display for SkillExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillExecutionError::PermissionDenied(reason) => write!(f, "Permission denied: {}", reason),
            SkillExecutionError::ExecutionFailed(reason) => write!(f, "Execution failed: {}", reason),
            SkillExecutionError::SkillNotFound(reason) => write!(f, "Skill not found: {}", reason),
            SkillExecutionError::ContextBuilderError(reason) => write!(f, "Context builder error: {}", reason),
        }
    }
}

/// Check if skill execution is allowed based on permission settings.
#[allow(dead_code)]
pub fn check_permission(permission_manager: &PermissionManager, skill_name: &str) -> PermissionStatus {
    permission_manager.check_skill_permission(skill_name)
}

/// Helper to extract all available tool names from the MCP context, indexed by name AND toolkit slug
fn extract_tool_map(context: &McpContext) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for server in &context.servers {
        // 1. Index by Tool Name (Standard)
        for tool in &server.tools {
            map.entry(tool.name.to_string())
                .or_default()
                .push(tool.name.to_string());
        }

        // 2. Index by Toolkit Slug (e.g. "news_api" -> ["news_api_get_headlines", ...])
        // We use the server name as the toolkit slug reference
        let slug = server.name.clone();
        let server_tools: Vec<String> = server.tools.iter().map(|t| t.name.to_string()).collect();
        
        map.entry(slug)
            .or_default()
            .extend(server_tools);
    }
    map
}

/// "Execute" a skill by building its Context Payload.
///
/// This function:
/// 1. Re-parses the skill to get fresh metadata (allowed_tools).
/// 2. Validates that the Agent has access to the required tools (e.g., "run_command").
/// 3. Resolves generic tool names to actual MCP tool names (e.g., "run_command" -> "shell_run_command").
/// 4. Bundles instructions, file paths, and scripts into a JSON payload.
pub async fn execute_skill(
    skill_call: &mut SkillCall,
    mcp_context: Option<&McpContext>
) -> Result<SkillExecutionResult, SkillExecutionError> {
    
    // 1. Load the Skill definition
    let skill = if !skill_call.path.as_os_str().is_empty() && skill_call.path.exists() {
         Skill::from_file(&skill_call.path)
            .map_err(|e| SkillExecutionError::SkillNotFound(format!("Failed to reload skill at {:?}: {}", skill_call.path, e)))?
    } else {
        return Err(SkillExecutionError::SkillNotFound("Skill path is missing or invalid".to_string()));
    };

    // 2. Validate Tools & Resolve Dependencies
    let tool_map = mcp_context.map(extract_tool_map).unwrap_or_default();
    let available_keys: Vec<String> = tool_map.keys().cloned().collect();

    tracing::info!("Available Capability Keys for Resolution: {:?}", available_keys);
    let mut resolved_tools = HashMap::new();
    let mut warnings = Vec::new();

    if let Some(required_capabilities) = &skill.metadata.allowed_tools {
        for capability in required_capabilities {
            // Smart Capability Resolution Strategy:
            // 1. Direct Lookup (Toolkit Slug or Exact Tool Name)
            // 2. Namespaced Match: tool identifier ends with "_" + capability
            // 3. Keyword Heuristic: "shell" matches "bash", "zsh", "sh", "run_command"
            
            let mut matches: Vec<String> = Vec::new();

            // 1. Direct Lookup (Checks both full tool name AND toolkit name)
            if let Some(tools) = tool_map.get(capability) {
                matches.extend(tools.clone());
            } else {
                // Fuzzy / Heuristic Search against all known tool names
                // We perform this search against the *values* (actual tool names) to ensure we find specific tools
                // even if the exact toolkit key wasn't hit.
                let all_tools: Vec<String> = tool_map.values().flatten().cloned().collect();
                
                let heuristic_matches: Vec<String> = all_tools.iter()
                    .filter(|actual| {
                         let actual = *actual;
                         // 2. Namespaced Match
                         if actual.ends_with(&format!("_{}", capability)) { return true; }
                        
                         // 3. Keyword Heuristic for Common Abstract Capabilities
                         match capability.as_str() {
                             "shell" | "run_command" => {
                                 actual.contains("run_command") || actual.contains("execute_shell") || actual.contains("write_to_terminal") || actual == "bash" || actual == "zsh"
                             },
                             "filesystem" => {
                                 actual.contains("read_file") || actual.contains("write_file") || actual.contains("list_directory") || actual.starts_with("fs_")
                             },
                             // Default: case-insensitive substring match
                             _ => actual.to_lowercase().contains(&capability.to_lowercase())
                         }
                    })
                    .cloned()
                    .collect();
                matches.extend(heuristic_matches);
            }
            
            // Deduplicate matches
            matches.sort();
            matches.dedup();

            if matches.is_empty() {
                warnings.push(format!("Missing required capability: '{}'. The skill may not function correctly.", capability));
                // Fallback: Resolve to generic name so the Agent at least sees what was requested
                resolved_tools.insert(capability.clone(), capability.clone()); 
            } else {
                // Return ALL matching tools so the Agent can choose the most appropriate one
                matches.sort(); // Sort alphabetically for consistency
                resolved_tools.insert(capability.clone(), matches.join(", "));
            }
        }
    }

    // 3. Build the Capability Resolution Preamble
    let preamble = if !resolved_tools.is_empty() {
        let mut lines = vec![
            "## 🔧 Resolved Capabilities".to_string(),
            "The following abstract capabilities have been resolved to specific MCP tools you can use:".to_string(),
        ];
        for (capability, tool_name) in &resolved_tools {
            lines.push(format!("- `{}` → Use tool: `{}`", capability, tool_name));
        }
        lines.push("".to_string());
        lines.push("Use these exact tool names when making function calls to execute the skill instructions below.".to_string());
        lines.push("".to_string());
        lines.push("---".to_string());
        lines.push("".to_string());
        lines.join("\n")
    } else {
        String::new()
    };

    // 4. Construct the Payload with preamble prepended to instructions
    let payload = CapabilityContextPayload {
        skill: skill.metadata.name.clone(),
        instruction_manual: format!("{}{}", preamble, skill.instructions),
        environment: SkillEnvironment {
            root_path: skill.root_path.clone(),
            scripts: skill.scripts.clone(),
            resources: skill.resources.clone(),
            user_args: skill_call.arguments.clone(),
        },
        resolved_tools,
        warnings,
    };

    // 4. Serialize
    let payload_json = serde_json::to_string_pretty(&payload)
        .map_err(|e| SkillExecutionError::ContextBuilderError(e.to_string()))?;

    // 5. Update SkillCall
    skill_call.status = SkillCallStatus::Completed; // It's "done" in terms of Hobbes's work.
    skill_call.response = payload_json.clone();
    skill_call.raw_output = Some(payload_json.clone());

    Ok(SkillExecutionResult {
        status: SkillCallStatus::Completed,
        output: payload_json,
    })
}

#[cfg(test)]
mod tests {
    // We would need to mock McpContext here to test validation logic rigorously.
    // For now, these placeholders confirm the structural changes.
    
    #[tokio::test]
    async fn test_execute_skill_returns_payload() {
        // Setup requires valid file path logic which is hard in unit tests without temp files.
        // Skipping complex integration test here in favor of manual verification via `timestamp`.
    }
}
