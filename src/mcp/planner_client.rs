use rmcp::model::Tool;

/// Native virtual MCP client for the built-in planner (`hobbes-planner`).
///
/// This client owns the tool *definitions* only. Actual dispatch is handled by
/// the interception layer in `components::builtin_tools` (which has access to
/// the `PlannerState` signal that `McpManager` does not own).
///
/// Registering these as a real named server in `McpManager::servers` ensures
/// the AI can accurately introspect all built-in tools and their origins, and
/// lets the whole planner be advertised or withheld as a unit.
#[derive(Clone)]
pub struct PlannerClient;

/// Shared tail for every planner tool description. The list being global (not
/// session-scoped like the scratchpad) is the one property the model must never
/// be allowed to forget, so each tool states it rather than relying on one.
const SHARED_NOTE: &str = "The to-do list is global and shared across all chat tabs — todos \
    created in other conversations are visible and editable here.";

impl PlannerClient {
    pub fn new() -> Self {
        Self
    }

    pub fn list_tools(&self) -> Vec<Tool> {
        use std::sync::Arc;

        vec![
            Tool {
                name: "HOBBES_TODO_CREATE".into(),
                description: Some(format!(
                    "Create one or more todos in the user's planner. Pass an array so a \
                    whole capture session is one call. Dates are YYYY-MM-DD (or the \
                    literal strings 'today' / 'tomorrow'). {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "todos": {
                                "type": "array",
                                "description": "The todos to create.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "title": { "type": "string", "description": "Short imperative title, e.g. 'Draft the proposal'." },
                                        "notes": { "type": "string", "description": "Longer free-form notes (markdown)." },
                                        "bucket": { "type": "string", "enum": ["inbox", "anytime", "someday"], "description": "List for an unscheduled todo. Defaults to inbox." },
                                        "scheduled_for": { "type": "string", "description": "The day the user intends to do it (YYYY-MM-DD, 'today' or 'tomorrow'). Distinct from deadline." },
                                        "time_of_day": { "type": "string", "enum": ["morning", "afternoon", "evening"], "description": "Rough slot within the scheduled day." },
                                        "deadline": { "type": "string", "description": "The day it is actually due (YYYY-MM-DD, 'today' or 'tomorrow')." },
                                        "estimate_minutes": { "type": "integer", "description": "Estimated focused minutes of work." },
                                        "project_id": { "type": "string", "description": "Id of an existing project to file it under." },
                                        "linked_session": { "type": "string", "description": "Fleet session carrying this todo: a session id from HOBBES_FLEET_STATUS, or a unique live session name." },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Free-form tags." },
                                        "checklist": { "type": "array", "items": { "type": "string" }, "description": "Sub-step titles." }
                                    },
                                    "required": ["title"]
                                }
                            }
                        },
                        "required": ["todos"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Create Todos".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TODO_UPDATE".into(),
                description: Some(format!(
                    "Update one or more todos: patch any mutable field, or set 'status' to \
                    complete, cancel, or reopen them. Setting status to in_progress starts focus mode: only one todo is ever in focus, the previous one is paused automatically with its elapsed time banked. Setting status back to 'open' on an in-progress todo pauses its timer and banks the elapsed time — this is how you stop a focus timer you started. Pass an explicit JSON null to CLEAR an \
                    optional field (scheduled_for, time_of_day, deadline, estimate_minutes, \
                    project_id) — omitted fields are left untouched. Replacing 'checklist' \
                    resets item completion. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "updates": {
                                "type": "array",
                                "description": "The patches to apply, one per todo.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Id of the todo to update (from HOBBES_TODO_LIST or a previous create)." },
                                        "title": { "type": "string" },
                                        "notes": { "type": "string" },
                                        "status": { "type": "string", "enum": ["open", "in_progress", "completed", "cancelled"], "description": "'completed' finishes it, 'cancelled' abandons it, 'open' reopens it." },
                                        "bucket": { "type": "string", "enum": ["inbox", "anytime", "someday"] },
                                        "scheduled_for": { "type": ["string", "null"], "description": "YYYY-MM-DD, 'today' or 'tomorrow'; null clears it." },
                                        "time_of_day": { "type": ["string", "null"], "enum": ["morning", "afternoon", "evening", null], "description": "null clears it." },
                                        "deadline": { "type": ["string", "null"], "description": "YYYY-MM-DD, 'today' or 'tomorrow'; null clears it." },
                                        "estimate_minutes": { "type": ["integer", "null"], "description": "null clears it." },
                                        "project_id": { "type": ["string", "null"], "description": "null clears it." },
                                        "linked_session": { "type": ["string", "null"], "description": "Fleet session carrying this todo (id from HOBBES_FLEET_STATUS or unique live name); null clears the link. Setting status to in_progress auto-links the calling session when unlinked." },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Replaces the full tag list." },
                                        "checklist": { "type": "array", "items": { "type": "string" }, "description": "Replaces the full checklist." }
                                    },
                                    "required": ["id"]
                                }
                            }
                        },
                        "required": ["updates"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Update Todos".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TODO_LIST".into(),
                description: Some(format!(
                    "List todos. Either pass 'view' (today, inbox, upcoming, anytime, \
                    someday, logbook) or any combination of the filters; with no arguments \
                    it shows today. Output is capped at 50 lines. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "view": { "type": "string", "enum": ["today", "inbox", "upcoming", "anytime", "someday", "logbook"], "description": "Named list to show. Ignored when filters are given." },
                            "project_id": { "type": "string", "description": "Only todos in this project." },
                            "tag": { "type": "string", "description": "Only todos carrying this tag." },
                            "text": { "type": "string", "description": "Case-insensitive substring match on title and notes." },
                            "date_from": { "type": "string", "description": "Earliest scheduled_for to include (YYYY-MM-DD, 'today' or 'tomorrow')." },
                            "date_to": { "type": "string", "description": "Latest scheduled_for to include (YYYY-MM-DD, 'today' or 'tomorrow')." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("List Todos".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_PLAN_DAY".into(),
                description: Some(format!(
                    "Plan a day, Sunsama-style: schedule the given todos onto the date in \
                    the given order, apply estimate overrides, and return the capacity \
                    arithmetic (planned vs available). If the plan overcommits the day the \
                    response says so explicitly — surface that warning to the user instead \
                    of silently accepting it. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "date": { "type": "string", "description": "The day to plan (YYYY-MM-DD, 'today' or 'tomorrow'). Defaults to today." },
                            "items": {
                                "type": "array",
                                "description": "Todos to schedule onto the day, in the order they should be worked.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Id of the todo to schedule." },
                                        "estimate_minutes": { "type": "integer", "description": "Override the todo's estimate for this plan." },
                                        "time_of_day": { "type": "string", "enum": ["morning", "afternoon", "evening"] }
                                    },
                                    "required": ["id"]
                                }
                            },
                            "capacity_minutes": { "type": "integer", "description": "Override the day's capacity (defaults to the stored day plan or the settings default)." }
                        },
                        "required": ["items"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Plan Day".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TIME_BLOCK".into(),
                description: Some(format!(
                    "Create, move, or delete a block on the day timeline. Times are local \
                    'HH:MM'; the response warns (without failing) when the block overlaps \
                    an existing one. Link 'todo_id' when the block is time for a to-do — \
                    a block without todo_id is a calendar-only event (e.g. a meeting) that \
                    does NOT appear in the to-do lists or count toward the day's capacity. \
                    Resizing a linked block re-estimates its to-do. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["create", "move", "delete"] },
                            "id": { "type": "string", "description": "Block id — required for move and delete." },
                            "todo_id": { "type": "string", "description": "Optional todo this block is working on." },
                            "title": { "type": "string", "description": "Block title. For create, defaults to the linked todo's title." },
                            "date": { "type": "string", "description": "Day of the block (YYYY-MM-DD, 'today' or 'tomorrow'). Required for create." },
                            "start": { "type": "string", "description": "Local start time 'HH:MM'. Required for create." },
                            "end": { "type": "string", "description": "Local end time 'HH:MM', after start. Required for create." }
                        },
                        "required": ["action"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Time Block".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_CALENDAR_LIST".into(),
                description: Some(
                    "List the user's external calendar events (meetings and all-day events) \
                    for a range of local dates. This is READ-ONLY mirrored data from the \
                    user's calendar subscriptions, synced periodically — Hobbes cannot \
                    create, edit, or RSVP to calendar events, and the mirror may lag the \
                    real calendar by a few minutes. Use it to check availability or see \
                    commitments beyond today before planning or timeboxing work."
                        .to_string()
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "start_date": { "type": "string", "description": "First day to list (YYYY-MM-DD, 'today' or 'tomorrow'). Defaults to today." },
                            "days": { "type": "integer", "description": "How many days to list, starting at start_date. Default 1, max 14 (larger values are clamped)." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("List Calendar Events".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_DISPATCH".into(),
                description: Some(
                    "Send a todo to a worker. target 'chat' opens a new Hobbes tab that \
                    starts on the assignment immediately; target 'claude_code' launches a \
                    headless Claude Code run in the todo's project directory (the project \
                    must have a path). Either way the todo links to the worker and progress \
                    flows back onto it automatically; a headless run's permission requests \
                    appear in the Fleet for approval."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "todo_id": { "type": "string", "description": "The todo to dispatch." },
                            "target": { "type": "string", "enum": ["chat", "claude_code"], "description": "Where the work runs." },
                            "instructions": { "type": "string", "description": "Extra guidance appended to the assignment." },
                            "model": { "type": "string", "description": "Model for claude_code runs (e.g. 'sonnet', 'opus')." }
                        },
                        "required": ["todo_id", "target"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Dispatch Todo".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_PROJECT_UPSERT".into(),
                description: Some(format!(
                    "Create or update projects and areas. Items without an 'id' are \
                    created; items with an 'id' patch the existing record. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "projects": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Omit to create a new project." },
                                        "title": { "type": "string", "description": "Required when creating." },
                                        "notes": { "type": "string" },
                                        "area_id": { "type": "string", "description": "Area to file the project under." },
                                        "deadline": { "type": "string", "description": "YYYY-MM-DD, 'today' or 'tomorrow'." },
                                        "status": { "type": "string", "enum": ["open", "in_progress", "completed", "cancelled"] },
                                        "path": { "type": "string", "description": "Repo/folder root for this project (e.g. ~/Sites/puget) — used to match coding-agent sessions to the project. Empty string clears it." }
                                    }
                                }
                            },
                            "areas": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Omit to create a new area." },
                                        "title": { "type": "string", "description": "Required when creating." }
                                    }
                                }
                            }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Upsert Projects & Areas".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
        ]
    }
}
