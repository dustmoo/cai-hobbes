use serde::{Deserialize, Serialize};

/// Threshold above which we use LLM-based tool selection instead of enabling all tools
pub const TOOL_SELECTION_THRESHOLD: usize = 25;

/// Maximum tools to select for a single toolkit to stay well under Composio's ~1000 tool limit
pub const MAX_TOOLS_PER_TOOLKIT: usize = 25;

/// Request structure for LLM tool selection
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSelectionRequest {
    pub toolkit_name: String,
    pub toolkit_description: Option<String>,
    pub available_tools: Vec<ToolCandidate>,
    pub max_tools: usize,
}

/// A tool candidate for selection
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCandidate {
    pub name: String,
    pub description: Option<String>,
}

/// Response structure from LLM tool selection
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSelectionResponse {
    pub selected_tools: Vec<String>,
    pub reasoning: String,
}

impl ToolSelectionRequest {
    pub fn new(
        toolkit_name: String,
        toolkit_description: Option<String>,
        tools: Vec<ToolCandidate>,
    ) -> Self {
        Self {
            toolkit_name,
            toolkit_description,
            available_tools: tools,
            max_tools: MAX_TOOLS_PER_TOOLKIT,
        }
    }
}

/// Build the prompt for the LLM to select tools
pub fn build_selection_prompt(request: &ToolSelectionRequest) -> String {
    let toolkit_desc = request.toolkit_description.clone()
        .unwrap_or_else(|| format!("{} integration toolkit", request.toolkit_name));
    
    // Build tool list with descriptions (truncated for large toolkits)
    let tool_list: String = request.available_tools.iter()
        .map(|t| {
            let desc = t.description.clone().unwrap_or_default();
            // Truncate long descriptions
            let desc_short = if desc.len() > 100 { format!("{}...", &desc[..100]) } else { desc };
            format!("- {}: {}", t.name, desc_short)
        })
        .collect::<Vec<_>>()
        .join("\n");
    
    format!(r#"You are selecting the most useful tools from a toolkit for an AI coding assistant.

Toolkit: {toolkit_name}
Description: {toolkit_desc}
Available tools: {tool_count}
Maximum to select: {max_tools}

Available tools:
{tool_list}

Select exactly {max_tools} of the most commonly needed tools for typical developer workflows.

Prioritize:
1. Core CRUD operations (create, read, update, delete, list)
2. Search and query operations
3. Common integrations (comments, labels, attachments, notifications)
4. Status and state management

Avoid:
- Highly specialized or rarely used operations
- Admin-only or dangerous operations
- Deprecated or legacy endpoints

Respond ONLY with valid JSON (no markdown, no explanation outside JSON):
{{
  "selected_tools": ["TOOL_NAME_1", "TOOL_NAME_2", ...],
  "reasoning": "Brief one-sentence explanation of selection criteria"
}}"#,
        toolkit_name = request.toolkit_name,
        toolkit_desc = toolkit_desc,
        tool_count = request.available_tools.len(),
        max_tools = request.max_tools,
        tool_list = tool_list,
    )
}

/// Parse the LLM response into a ToolSelectionResponse
pub fn parse_selection_response(response: &str) -> Result<ToolSelectionResponse, String> {
    let json_str = crate::components::shared::extract_json_from_response(response);
    
    match serde_json::from_str::<ToolSelectionResponse>(json_str) {
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(format!("Failed to parse tool selection response as JSON: {}", 
            if response.len() > 200 { &response[..200] } else { response })),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_valid_json() {
        let response = r#"{"selected_tools": ["GITHUB_CREATE_ISSUE", "GITHUB_LIST_REPOS"], "reasoning": "Core operations"}"#;
        let result = parse_selection_response(response).unwrap();
        assert_eq!(result.selected_tools.len(), 2);
        assert_eq!(result.reasoning, "Core operations");
    }
    
    #[test]
    fn test_parse_markdown_json() {
        let response = r#"Here's the selection:
```json
{"selected_tools": ["TOOL_A"], "reasoning": "Test"}
```"#;
        let result = parse_selection_response(response).unwrap();
        assert_eq!(result.selected_tools, vec!["TOOL_A"]);
    }
    
    #[test]
    fn test_parse_embedded_json() {
        let response = r#"I recommend: {"selected_tools": ["X"], "reasoning": "Y"} for your use case."#;
        let result = parse_selection_response(response).unwrap();
        assert_eq!(result.selected_tools, vec!["X"]);
    }
}
