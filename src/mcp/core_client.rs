use rmcp::model::{JsonObject, Tool};

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

        vec![
            Tool::new(
                "HOBBES_UPDATE_SCRATCHPAD",
                "Write important facts, decisions, or discoveries to your persistent session \
                    scratchpad. The scratchpad survives context compression and history scrolling. \
                    OVERWRITE: include all information you want to retain — anything not included \
                    in this call is lost.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The full new scratchpad content. Be concise but complete — include any facts from the previous scratchpad you want to keep."
                            }
                        },
                        "required": ["content"]
                    })).unwrap_or_default(),
            ).with_title("Update Scratchpad"),
            Tool::new(
                "HOBBES_WRITE_HANDOFF",
                "Write a handoff note to your future self in this chat: what you are doing, \
                    what you expect to happen next and what you will do then, and what would \
                    change your plan. It is shown back to you, with its age, at the start of \
                    later turns. Write one when you finish a meaningful piece of work, before you \
                    wait on something (a timer, a worker, the user), or when your plan changes. \
                    Replaces the previous note; not a summary of the conversation.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "doing": { "type": "string", "description": "What you are working on right now, in a sentence." },
                            "expect_next": { "type": "string", "description": "What you expect to happen next and what you will do about it." },
                            "would_change_mind": { "type": "string", "description": "Optional: what would change this plan." }
                        },
                        "required": ["doing", "expect_next"]
                    })).unwrap_or_default(),
            ).with_title("Write Handoff"),
            Tool::new(
                "HOBBES_PAGE_RESULT",
                "Fetch the next page of a paginated tool result. \
                    Use the exact tool_call_id from the pagination footer \
                    (e.g. '[Page 1/3 — call HOBBES_PAGE_RESULT with tool_call_id=\"abc\"]'). \
                    Only call this when a result explicitly states more pages are available.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "tool_call_id": {
                                "type": "string",
                                "description": "The exact tool_call_id from the [Page X/Y] footer"
                            }
                        },
                        "required": ["tool_call_id"]
                    })).unwrap_or_default(),
            ).with_title("Page Result"),
            Tool::new(
                "HOBBES_INVOKE_SKILL",
                "Load one of the available skills (listed in your system context under \
                    'available_skills') into this session. Returns the skill's full instruction \
                    manual and resolved tool capabilities; the skill then stays loaded for the \
                    rest of the session. Only invoke a skill that clearly matches the user's \
                    current request.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Exact name of the skill to load (from available_skills)."
                            },
                            "arguments": {
                                "type": "string",
                                "description": "Optional arguments for the skill, like the text after /name when a user invokes it."
                            }
                        },
                        "required": ["name"]
                    })).unwrap_or_default(),
            ).with_title("Invoke Skill"),
            Tool::new(
                "HOBBES_UNLOAD_SKILL",
                "Unload a skill you loaded earlier (listed under 'loaded_skills' in your \
                    system context), dropping its instruction manual from your context. Use this \
                    when you are done with a skill and it no longer applies to the conversation. \
                    You can load it again later with HOBBES_INVOKE_SKILL.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Exact name of the loaded skill to unload."
                            }
                        },
                        "required": ["name"]
                    })).unwrap_or_default(),
            ).with_title("Unload Skill"),
            Tool::new(
                "HOBBES_SET_TIMER",
                "Schedule a timer/reminder. When it fires, Hobbes brings its window to the \
                    front. Choose mode: 'notify' just shows a reminder toast; 'prompt' runs a \
                    follow-up turn using the given prompt (use this to continue work later, e.g. \
                    'check if the build finished'). Specify the delay with EITHER 'delay' \
                    (\"10m\", \"1h30m\", \"45s\") OR 'delay_secs' (integer seconds). The delay \
                    is the deadline: with 'until' it fires as soon as that condition is met, or at \
                    the deadline, whichever comes first. Always give a 'reason': it is shown back \
                    to you when the timer fires.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "delay": { "type": "string", "description": "Human delay, e.g. \"10m\", \"1h30m\", \"45s\". Provide this or delay_secs." },
                            "delay_secs": { "type": "integer", "description": "Delay in seconds. Provide this or delay." },
                            "mode": { "type": "string", "enum": ["notify", "prompt"], "description": "'notify' = reminder only; 'prompt' = run the prompt as a new turn. Defaults to notify." },
                            "prompt": { "type": "string", "description": "Required for mode 'prompt': the message to run when the timer fires." },
                            "label": { "type": "string", "description": "Short label shown in the reminder and the timer list." },
                            "reason": { "type": "string", "description": "Why you are setting it and what you plan to do when it fires, in a sentence or two. Shown back to you on waking." },
                            "until_worker_done": { "type": "string", "description": "A dispatched to-do's id: fire as soon as its worker stops working (done, needs you or stuck) or the to-do is completed, instead of waiting for the full delay." }
                        }
                    })).unwrap_or_default(),
            ).with_title("Set Timer"),
            Tool::new(
                "HOBBES_LIST_TIMERS",
                "List the pending timers scheduled for this session, with their ids.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {}
                    })).unwrap_or_default(),
            ).with_title("List Timers"),
            Tool::new(
                "HOBBES_FLEET_STATUS",
                "Live status of the user's coding-agent fleet: every Claude Code \
                    terminal session and Hobbes chat tab observed today — status \
                    (working / needs_attention / idle / ended), minutes of agent time, \
                    re-entry brief (what it did, what it's blocked on), and any pending \
                    permission approval. Read-only. Optionally filter by a name or \
                    path substring.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "session": {
                                "type": "string",
                                "description": "Optional case-insensitive substring to match against session names or paths (e.g. 'puget')."
                            }
                        }
                    })).unwrap_or_default(),
            ).with_title("Fleet Status"),
            Tool::new(
                "HOBBES_CANCEL_TIMER",
                "Cancel a pending timer by its id (from the set-timer confirmation or \
                    HOBBES_LIST_TIMERS).",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "timer_id": { "type": "string", "description": "The id of the timer to cancel (e.g. 'tmr_1a2b3c4d')." }
                        },
                        "required": ["timer_id"]
                    })).unwrap_or_default(),
            ).with_title("Cancel Timer"),
        ]
    }

    /// The dispatch relay tools (`todo::relay`). Deliberately NOT in
    /// [`list_tools`](Self::list_tools): the prompt builder offers each only
    /// to a session on a parent ↔ worker link (`relay::relay_tools_offered`),
    /// so ordinary chats never carry them. Descriptions are fixed strings,
    /// so the cached tool prefix holds.
    pub fn relay_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "HOBBES_MESSAGE_PARENT",
                "Send a message to the chat that dispatched you (your parent). kind \
                    'question' asks it something and starts a turn there; 'result' hands back \
                    what you produced; 'progress' is a quiet note that starts nothing. The \
                    parent reads it as information from another agent. Only while your to-do is \
                    open (plus one final 'result'); a short completion report is sent \
                    automatically when you finish, so don't repeat it.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "The message, self-contained and short." },
                            "kind": { "type": "string", "enum": ["progress", "question", "result"], "description": "Defaults to progress." }
                        },
                        "required": ["text"]
                    })).unwrap_or_default(),
            ).with_title("Message Parent"),
            Tool::new(
                "HOBBES_MESSAGE_WORKER",
                "Send a message to one of the chat workers you dispatched; it starts a turn \
                    in that worker's chat, which reads it as information from another agent. \
                    Only your own workers, only while their to-do is open.",
                serde_json::from_value::<JsonObject>(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "worker": { "type": "string", "description": "The worker's to-do id or title, or its chat session id." },
                            "text": { "type": "string", "description": "The message, self-contained and short." }
                        },
                        "required": ["worker", "text"]
                    })).unwrap_or_default(),
            ).with_title("Message Worker"),
        ]
    }
}
