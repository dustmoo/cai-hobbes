use rmcp::model::Tool;

/// Native virtual MCP client for Hobbes core session-management tools.
///
/// This client owns the tool *definitions* only. Actual dispatch is handled
/// by the interception layer in `stream_manager.rs` (which has access to
/// `SessionState` that `McpManager` does not own).
///
/// Registering these as a real named server in `McpManager::servers` ensures
/// the AI can accurately introspect all built-in tools and their origins,
/// eliminating confusion between `HOBBES_*` and `MCP_*` tools.
#[derive(Clone)]
pub struct CoreClient;

impl CoreClient {
    pub fn new() -> Self {
        Self
    }

    pub fn list_tools(&self) -> Vec<Tool> {
        use std::sync::Arc;

        vec![
            Tool {
                name: "HOBBES_UPDATE_SCRATCHPAD".into(),
                description: Some(
                    "Write important facts, decisions, or discoveries to your persistent session \
                    scratchpad. The scratchpad survives context compression and history scrolling. \
                    OVERWRITE: include all information you want to retain — anything not included \
                    in this call is lost."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The full new scratchpad content. Be concise but complete — include any facts from the previous scratchpad you want to keep."
                            }
                        },
                        "required": ["content"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Update Scratchpad".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_PAGE_RESULT".into(),
                description: Some(
                    "Fetch the next page of a paginated tool result. \
                    Use the exact tool_call_id from the pagination footer \
                    (e.g. '[Page 1/3 — call HOBBES_PAGE_RESULT with tool_call_id=\"abc\"]'). \
                    Only call this when a result explicitly states more pages are available."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "tool_call_id": {
                                "type": "string",
                                "description": "The exact tool_call_id from the [Page X/Y] footer"
                            }
                        },
                        "required": ["tool_call_id"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Page Result".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_SET_TIMER".into(),
                description: Some(
                    "Schedule a timer/reminder. When it fires, Hobbes brings its window to the \
                    front. Choose mode: 'notify' just shows a reminder toast; 'prompt' runs a \
                    follow-up turn using the given prompt (use this to continue work later, e.g. \
                    'check if the build finished'). Specify the delay with EITHER 'delay' \
                    (\"10m\", \"1h30m\", \"45s\") OR 'delay_secs' (integer seconds)."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "delay": { "type": "string", "description": "Human delay, e.g. \"10m\", \"1h30m\", \"45s\". Provide this or delay_secs." },
                            "delay_secs": { "type": "integer", "description": "Delay in seconds. Provide this or delay." },
                            "mode": { "type": "string", "enum": ["notify", "prompt"], "description": "'notify' = reminder only; 'prompt' = run the prompt as a new turn. Defaults to notify." },
                            "prompt": { "type": "string", "description": "Required for mode 'prompt': the message to run when the timer fires." },
                            "label": { "type": "string", "description": "Short label shown in the reminder and the timer list." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Set Timer".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_LIST_TIMERS".into(),
                description: Some(
                    "List the pending timers scheduled for this session, with their ids.".into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("List Timers".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_CANCEL_TIMER".into(),
                description: Some(
                    "Cancel a pending timer by its id (from the set-timer confirmation or \
                    HOBBES_LIST_TIMERS)."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "timer_id": { "type": "string", "description": "The id of the timer to cancel (e.g. 'tmr_1a2b3c4d')." }
                        },
                        "required": ["timer_id"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Cancel Timer".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
        ]
    }
}
