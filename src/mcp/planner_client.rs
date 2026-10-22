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

/// The lane rule for create/update: the model may suggest delegating, never
/// decide it.
const LANE_NOTE: &str = "To-dos are in the human lane (the user's own time) by default. When an \
    outcome is clear and no human presence is needed you may SUGGEST handing it to an agent, but \
    create human to-dos unless the user said to delegate.";

/// The static part of `HOBBES_DISPATCH`'s description. The allowed models
/// are appended per request by [`annotate_dispatch_models`].
const DISPATCH_DESCRIPTION: &str = "Send a todo to a worker. target 'chat' opens a new Hobbes tab \
    that starts on the assignment immediately; target 'claude_code' launches a headless Claude \
    Code run in the todo's project directory (the project must have a path). Either way the todo \
    joins the agent lane, links to the worker, and its state (working / needs you / stuck / done) \
    and progress flow back onto it automatically; a headless run's permission requests appear in \
    the Fleet for approval. Keep the todo's title a short name (about 80 characters): put the brief in its \
    'notes' and any extra guidance in 'instructions', never in the title. Set the todo's \
    'outcome' first so the worker knows what done looks like. For target 'chat' you may pick the worker's model (connector + model): a cheaper, faster \
    model (e.g. a Haiku) for routine lookups, summaries and formatting; a stronger one for \
    multi-step reasoning or code. The choice must come from the allowed list; anything else is \
    refused.";

/// Append the user's allowed models to `HOBBES_DISPATCH`'s description in
/// the per-request tool list. The list comes from
/// `Settings::allowed_models` (sorted, deduplicated), so the description,
/// which sits in the cached tool prefix, changes only when the connector
/// settings do.
pub fn annotate_dispatch_models(
    tools: &mut [crate::llm::types::ToolDefinition],
    settings: &crate::settings::Settings,
) {
    let Some(tool) = tools.iter_mut().find(|t| t.name == "HOBBES_DISPATCH") else {
        return;
    };
    tool.description = dispatch_description(&settings.allowed_models());
}

/// `HOBBES_DISPATCH`'s description with the allowed list for `target: chat`.
pub fn dispatch_description(allowed: &[crate::settings::AllowedModel]) -> String {
    if allowed.is_empty() {
        return format!("{DISPATCH_DESCRIPTION} No models are configured for choice; omit connector and model.");
    }
    format!(
        "{DISPATCH_DESCRIPTION} Allowed for target 'chat' (connector: models): {}.",
        crate::settings::allowed_models_listing(allowed)
    )
}

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
                    literal strings 'today' / 'tomorrow'). {} {}",
                    LANE_NOTE, SHARED_NOTE
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
                                        "title": { "type": "string", "description": "A short name, about 80 characters at most, e.g. 'Draft the proposal'. Never a brief or instructions: put those in 'notes'." },
                                        "notes": { "type": "string", "description": "Longer free-form notes (markdown). The brief, context and instructions for the work go here." },
                                        "bucket": { "type": "string", "enum": ["inbox", "anytime", "someday"], "description": "List for an unscheduled todo. Defaults to inbox." },
                                        "scheduled_for": { "type": "string", "description": "The day the user intends to do it (YYYY-MM-DD, 'today' or 'tomorrow'). Distinct from deadline." },
                                        "time_of_day": { "type": "string", "enum": ["morning", "afternoon", "evening"], "description": "Rough slot within the scheduled day." },
                                        "deadline": { "type": "string", "description": "The day it is actually due (YYYY-MM-DD, 'today' or 'tomorrow')." },
                                        "estimate_minutes": { "type": "integer", "description": "Estimated focused minutes of work." },
                                        "priority": { "type": "string", "enum": ["urgent_important", "important", "urgent", "neither"], "description": "Eisenhower quadrant: urgent_important (Q1, do first), important (Q2, schedule), urgent (Q3, delegate), neither (Q4, drop). Leave unset unless the user states or clearly implies it." },
                                        "project_id": { "type": "string", "description": "Id of an existing project to file it under (HOBBES_PROJECT_LIST shows them)." },
                                        "linked_session": { "type": "string", "description": "Fleet session carrying this todo: a session id from HOBBES_FLEET_STATUS, or a unique live session name." },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Free-form tags." },
                                        "checklist": { "type": "array", "items": { "type": "string" }, "description": "Sub-step titles." },
                                        "lane": { "type": "string", "enum": ["human", "agent"], "description": "Who spends time on it. Defaults to human. Use 'agent' ONLY when the user said to delegate it (otherwise suggest it in your reply)." },
                                        "kind": { "type": "string", "enum": ["commitment", "chore", "your_time"], "description": "Human to-dos only: commitment (needs the user personally), chore (quick task to batch), your_time (the user's own non-work time)." },
                                        "time_category": { "type": "string", "description": "For kind your_time: the user's your-time category (e.g. 'Reading'), matching a goal from HOBBES_TIME_GOALS." },
                                        "outcome": { "type": "string", "description": "Agent to-dos: the definition of done the agent works toward, e.g. 'PR open with the fix and tests passing'." }
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
                    priority, project_id, kind, time_category, outcome) — omitted fields are left \
                    untouched. Replacing 'checklist' resets item completion. {} {}",
                    LANE_NOTE, SHARED_NOTE
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
                                        "title": { "type": "string", "description": "A short name, about 80 characters at most. Brief or instructions go in 'notes'." },
                                        "notes": { "type": "string" },
                                        "status": { "type": "string", "enum": ["open", "in_progress", "completed", "cancelled"], "description": "'completed' finishes it, 'cancelled' abandons it, 'open' reopens it." },
                                        "bucket": { "type": "string", "enum": ["inbox", "anytime", "someday"] },
                                        "scheduled_for": { "type": ["string", "null"], "description": "YYYY-MM-DD, 'today' or 'tomorrow'; null clears it." },
                                        "time_of_day": { "type": ["string", "null"], "enum": ["morning", "afternoon", "evening", null], "description": "null clears it." },
                                        "deadline": { "type": ["string", "null"], "description": "YYYY-MM-DD, 'today' or 'tomorrow'; null clears it." },
                                        "estimate_minutes": { "type": ["integer", "null"], "description": "null clears it." },
                                        "priority": { "type": ["string", "null"], "enum": ["urgent_important", "important", "urgent", "neither", null], "description": "Eisenhower quadrant (Q1–Q4); null clears it." },
                                        "project_id": { "type": ["string", "null"], "description": "Project id from HOBBES_PROJECT_LIST; null clears it." },
                                        "linked_session": { "type": ["string", "null"], "description": "Fleet session carrying this todo (id from HOBBES_FLEET_STATUS or unique live name); null clears the link. Setting status to in_progress auto-links the calling session when unlinked." },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Replaces the full tag list." },
                                        "checklist": { "type": "array", "items": { "type": "string" }, "description": "Replaces the full checklist." },
                                        "lane": { "type": "string", "enum": ["human", "agent"], "description": "Move between lanes. Set 'agent' only when the user said to delegate it; HOBBES_DISPATCH does this for you when it starts a worker." },
                                        "kind": { "type": ["string", "null"], "enum": ["commitment", "chore", "your_time", null], "description": "Human to-dos only; null clears it." },
                                        "time_category": { "type": ["string", "null"], "description": "Your-time category for kind your_time; null clears it." },
                                        "outcome": { "type": ["string", "null"], "description": "Agent to-dos: the definition of done; null clears it." }
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
                    "Plan a day, Sunsama-style: schedule the given human to-dos onto the date \
                    in the given order, apply estimate overrides, and return the capacity \
                    arithmetic (planned vs available). The response leads with the user's \
                    your-time status: protected your-time blocks are planned AROUND and never \
                    moved. Pass chores_slots to batch the day's chores into one or two Chores \
                    blocks. Agent-lane to-dos are never placed on the day; the response lists \
                    the agent lane separately (to dispatch / working / needs you). If the plan \
                    overcommits the day the response says so explicitly — surface that warning \
                    to the user instead of silently accepting it. {}",
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
                            "capacity_minutes": { "type": "integer", "description": "Override the day's capacity (defaults to the stored day plan or the settings default)." },
                            "chores_slots": {
                                "type": "array",
                                "maxItems": 2,
                                "description": "Up to two local slots to batch the day's chores into (replaces earlier chore batches that day). Must not overlap protected your-time.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "start": { "type": "string", "description": "Local 'HH:MM'." },
                                        "end": { "type": "string", "description": "Local 'HH:MM', after start." }
                                    },
                                    "required": ["start", "end"]
                                }
                            }
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
                    "List, create, move, or delete blocks on the day timeline. To change an \
                    existing block (reschedule, shorten, push to tomorrow) MOVE it by id — list \
                    first if you don't have the id; never create a replacement and leave the \
                    old one behind. Times are local 'HH:MM'; the response warns (without failing) when the block overlaps \
                    an existing one. Link 'todo_id' when the block is time for a to-do — \
                    a block without todo_id is a calendar-only event (e.g. a meeting) that \
                    does NOT appear in the to-do lists or count toward the day's capacity. \
                    Resizing a linked block re-estimates its to-do. kind 'your_time' creates a \
                    PROTECTED block for the user's own time (needs a category); work cannot be \
                    placed over protected time, and protected blocks are never moved or deleted \
                    unless the user explicitly agreed (then pass user_confirmed: true). For the \
                    category, use one of the user's your-time goals (HOBBES_TIME_GOALS get, or \
                    planner_today.your_time.week) unless the user named another; any other name is \
                    tracked with no target and the response says so. {}",
                    SHARED_NOTE
                ).into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["list", "create", "move", "delete"], "description": "list: the blocks (with ids) for 'date' (default today) and 'days' (default 1, max 14). move: change an existing block's date, start, end, or title; unspecified parts are kept." },
                            "id": { "type": "string", "description": "Block id — required for move and delete." },
                            "days": { "type": "integer", "description": "List only: how many days from 'date'. Default 1, max 14." },
                            "todo_id": { "type": "string", "description": "Optional todo this block is working on." },
                            "title": { "type": "string", "description": "Block title. For create, defaults to the linked todo's title." },
                            "date": { "type": "string", "description": "Day of the block (YYYY-MM-DD, 'today' or 'tomorrow'). Required for create; for move, the new day; for list, the first day." },
                            "start": { "type": "string", "description": "Local start time 'HH:MM'. Required for create." },
                            "end": { "type": "string", "description": "Local end time 'HH:MM', after start. Required for create." },
                            "kind": { "type": "string", "enum": ["work", "your_time", "chores"], "description": "Create only. work (default): a to-do timebox or event; your_time: protected time for the user (needs category, no todo_id); chores: a batched chores slot." },
                            "category": { "type": "string", "description": "Your-time category for kind your_time: prefer one of the user's goal categories (e.g. 'Outside'). On move, re-files a your_time block (needs user_confirmed)." },
                            "override_protected": { "type": "boolean", "description": "Create only: place work over protected your-time. ONLY when the user explicitly asked to use that time." },
                            "user_confirmed": { "type": "boolean", "description": "Move/delete of a protected your-time block: true ONLY after the user explicitly agreed in this conversation." }
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
                description: Some(DISPATCH_DESCRIPTION.into()),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "todo_id": { "type": "string", "description": "The todo to dispatch." },
                            "target": { "type": "string", "enum": ["chat", "claude_code"], "description": "Where the work runs." },
                            "instructions": { "type": "string", "description": "Extra guidance appended to the assignment. Put it here, not in the todo's title." },
                            "connector": { "type": "string", "description": "chat only: the connector to run the new tab on (a connector name from the allowed list). Omit for the user's default." },
                            "model": { "type": "string", "description": "chat: a model slug from the allowed list (omit for the user's default). claude_code: the Claude Code model (e.g. 'sonnet', 'opus')." }
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
                name: "HOBBES_PROJECT_LIST".into(),
                description: Some(
                    "List the user's projects with their ids: title, aliases (other names \
                    the project answers to), area, path, status, open to-do count and last \
                    activity. 'query' filters by name (title or alias, case/punctuation \
                    insensitive, near-misses included). Open projects only unless \
                    include_closed is true. Call this to find a project's id before filing \
                    to-dos into it or updating it, and before creating a project. Areas are \
                    listed first with their ids (set a project's area_id to file it). Possible \
                    duplicates are flagged: suggest merging them with HOBBES_PROJECT_MERGE (the \
                    user approves every merge), or the user can merge them in Planner \u{25b8} \
                    Manage projects."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "A project name or part of one, e.g. 'hobbes'." },
                            "include_closed": { "type": "boolean", "description": "Also list completed and archived projects. Defaults to false." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("List Projects".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_PROJECT_MERGE".into(),
                description: Some(
                    "Merge duplicate projects: fold the projects in merge_ids into keep_id. Their \
                    to-dos and tagged chats move to keep_id, their names become its aliases, and \
                    they are removed. It can't be undone, so the user is asked to approve every \
                    merge (the prompt states what moves). Look the ids up with \
                    HOBBES_PROJECT_LIST first, and talk it through with the user before calling: \
                    suggest which to merge and which to keep separate."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "keep_id": { "type": "string", "description": "The project to keep (from HOBBES_PROJECT_LIST)." },
                            "merge_ids": { "type": "array", "items": { "type": "string" }, "description": "Projects to fold into keep_id. At least one; not keep_id itself." }
                        },
                        "required": ["keep_id", "merge_ids"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Merge Projects".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_PROJECT_UPSERT".into(),
                description: Some(format!(
                    "Create or update projects and areas. Call HOBBES_PROJECT_LIST before \
                    creating a project; prefer an existing one and pass its id. Items with an \
                    'id' patch that record. A project item without an 'id' is matched by name \
                    first: if an existing project already has that title or alias it is \
                    updated instead (the response names its id); if only similar projects \
                    exist the item is refused with the candidates — pass one's id, or \
                    create_new: true when it truly is a different project. To combine \
                    duplicates use HOBBES_PROJECT_MERGE, not an upsert. An area item without an \
                    'id' that names an existing area (same name, ignoring case and punctuation) \
                    updates that area; areas are never deleted here. {}",
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
                                        "id": { "type": "string", "description": "The project to update (from HOBBES_PROJECT_LIST). Omit only to create." },
                                        "title": { "type": "string", "description": "Required when creating." },
                                        "aliases": { "type": "array", "items": { "type": "string" }, "description": "Other names the project answers to (repo name, nickname). Replaces the list." },
                                        "create_new": { "type": "boolean", "description": "Create even though similarly named projects exist. Only after checking HOBBES_PROJECT_LIST; an exact name match is always updated, never duplicated." },
                                        "notes": { "type": "string" },
                                        "area_id": { "type": "string", "description": "Area to file the project under." },
                                        "deadline": { "type": "string", "description": "YYYY-MM-DD, 'today' or 'tomorrow'." },
                                        "status": { "type": "string", "enum": ["open", "in_progress", "completed", "archived"], "description": "completed / archived projects leave the Planner nav." },
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
            Tool {
                name: "HOBBES_DECISIONS".into(),
                description: Some(
                    "The decision queue: agent to-dos waiting on the user's judgment. \
                    'list' (default) shows each question with its options, estimated decision \
                    time, worker and brief. 'resolve' records the user's answer (todo_id, \
                    choice — an option's text or 1-based number — and an optional note) and \
                    hands it back to the worker; only resolve with an answer the user actually \
                    gave. Permission gates cannot be resolved here — the user approves those in \
                    the Fleet. 'ask' is for a worker (a Hobbes chat working an agent to-do) to \
                    raise a decision: todo_id, a one-line question, options, est_minutes, and \
                    did/blocked/next; then stop and wait for the answer."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["list", "resolve", "ask"], "description": "Defaults to list." },
                            "todo_id": { "type": "string", "description": "resolve / ask: the agent to-do." },
                            "choice": { "type": "string", "description": "resolve: the user's answer — an option's text, its 1-based number, or their own words." },
                            "note": { "type": "string", "description": "resolve: anything the user added." },
                            "question": { "type": "string", "description": "ask: one line." },
                            "options": { "type": "array", "items": { "type": "string" }, "description": "ask: the choices, most likely first." },
                            "est_minutes": { "type": "integer", "description": "ask: how long deciding should take." },
                            "did": { "type": "string", "description": "ask: what you did so far, one line." },
                            "blocked": { "type": "string", "description": "ask: what you are blocked on." },
                            "next": { "type": "string", "description": "ask: what you will do once answered." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Decision Queue".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TIME_GOALS".into(),
                description: Some(
                    "The user's your-time goals: weekly targets for what their time is FOR \
                    (reading, gaming, getting outside…), and this week's progress against \
                    them (protected your-time blocks plus time logged on your-time to-dos). \
                    'get' (default) reads, and also lists categories in use on blocks that have \
                    no goal; 'set' upserts goals by category, renames categories in 'rename' \
                    (blocks and to-dos move along; renaming into an existing goal merges into it), \
                    and removes goals listed in 'remove' (their blocks stay). Only set goals the \
                    user stated."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["get", "set"], "description": "Defaults to get." },
                            "goals": {
                                "type": "array",
                                "description": "set: goals to create or update, matched by category (case-insensitive).",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "category": { "type": "string" },
                                        "weekly_target_minutes": { "type": "integer", "description": "Required for a new category." },
                                        "colour": { "type": ["string", "null"], "description": "#RRGGBB; null clears it." }
                                    },
                                    "required": ["category"]
                                }
                            },
                            "rename": {
                                "type": "array",
                                "description": "set: rename a category (its goal, blocks and your-time to-dos), e.g. [{\"from\": \"Break\", \"to\": \"Outside\"}].",
                                "items": {
                                    "type": "object",
                                    "properties": { "from": { "type": "string" }, "to": { "type": "string" } },
                                    "required": ["from", "to"]
                                }
                            },
                            "remove": { "type": "array", "items": { "type": "string" }, "description": "set: goals to delete (their blocks stay, tracked without a target)." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Your-Time Goals".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
        ]
    }

    /// `HOBBES_AGENT_TIME` — deliberately NOT in [`Self::list_tools`], so no
    /// ordinary chat is ever offered it. The agent-time runtime adds this
    /// definition to the prompt only for the leased agent-time session
    /// (`PlannerState::is_agent_time_session`); dispatch refuses it from any
    /// other session regardless.
    #[cfg_attr(not(test), allow(dead_code))] // consumer: the agent-time runtime (wave 2)
    pub fn agent_time_tools() -> Vec<Tool> {
        use std::sync::Arc;
        vec![Tool {
            name: "HOBBES_AGENT_TIME".into(),
            description: Some(
                "Your granted agent time: a window in which you do finished work in the open, \
                ending in one review the user reads later. start(intent, todo_id?) first — \
                declare what you will do and why, in one or two lines; pass todo_id only for a \
                queued or stuck agent-lane to-do. Four jobs: PREPARE briefs, drafts and \
                background for the user's tasks they flagged \"Prep this\" (and only those); \
                FINISH agent-lane work; NOTICE changes worth making to the user's stuff \
                (proposals they accept or dismiss); TEND Hobbes's own upkeep when idle \
                (corrections to things you told the user, your own failed calls and stalls, \
                learning what connected tools can do, tidying memory — read-only outside \
                Hobbes). deliver(job: prepare|finish|tend, title, body, kind?, todo_id?, \
                correction?) files finished work for the user to keep, send back or discard; \
                prepare needs the flagged to-do's todo_id; correction (tend only) must say what \
                was wrong and how you checked. propose(kind, title, body) files a Notice \
                proposal (note, todo, skill, research, other; a todo with lane 'agent' is work \
                you would do yourself; kind offer + todo_id offers to take one of the user's \
                own tasks, which only their acceptance hands over). ask(question, options) \
                saves a question for the user's review instead of stopping — then carry on; \
                the answer reaches your next window. Every tool call is recorded on the log \
                automatically; log(summary) is an optional note on your next step. Never \
                change the user's to-dos, plan or anything outside Hobbes directly. \
                end(summary) closes the session. Refused when the grant, day, minutes or \
                spend cap don't allow it."
                    .into(),
            ),
            input_schema: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["start", "log", "deliver", "propose", "ask", "end"] },
                        "intent": { "type": "string", "description": "start: what you will do and why, one or two lines." },
                        "summary": { "type": "string", "description": "log (optional note, attached to your next call) / end: one line." },
                        "job": { "type": "string", "enum": ["prepare", "finish", "tend"], "description": "deliver: which job the finished work is for." },
                        "kind": { "type": "string", "enum": ["note", "todo", "skill", "research", "other", "offer"], "description": "propose (required) / deliver (optional). offer = propose to take one of the user's tasks (needs todo_id); never for deliver." },
                        "todo_id": { "type": "string", "description": "start: the agent-lane to-do you work on (optional). deliver(prepare): the flagged to-do you prepared for (required). deliver(finish): the agent-lane to-do it finishes (optional). propose(offer): the user's to-do you offer to take." },
                        "correction": { "type": "boolean", "description": "deliver(tend) only: this corrects something you told the user. Say what was wrong and how you checked." },
                        "lane": { "type": "string", "enum": ["agent", "human"], "description": "propose(todo): 'agent' when it is work you would do yourself (it lands in the agent lane, and accepting it can start it); 'human' (default) for a suggestion for the user." },
                        "title": { "type": "string", "description": "deliver / propose: one line." },
                        "body": { "type": "string", "description": "deliver: the finished work itself (markdown). propose: the content (markdown)." },
                        "question": { "type": "string", "description": "ask: one question for the user." },
                        "options": { "type": "array", "items": { "type": "string" }, "description": "ask: suggested answers (up to 6)." }
                    },
                    "required": ["action"]
                }))
                .unwrap_or_default(),
            ),
            title: Some("Agent Time".to_string()),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_description_names_the_allowed_models_stably() {
        let settings = crate::settings::test_delegation_settings();
        let mut tools: Vec<crate::llm::types::ToolDefinition> = PlannerClient::new()
            .list_tools()
            .iter()
            .map(|t| crate::llm::types::ToolDefinition::from_mcp(t, "hobbes-planner"))
            .collect();
        let before: Vec<String> = tools.iter().map(|t| t.description.clone()).collect();
        annotate_dispatch_models(&mut tools, &settings);
        let d = &tools.iter().find(|t| t.name == "HOBBES_DISPATCH").unwrap().description;
        assert!(d.contains("a cheaper, faster model (e.g. a Haiku)"), "{d}");
        assert!(d.contains("must come from the allowed list"), "{d}");
        assert!(d.ends_with(
            "Allowed for target 'chat' (connector: models): Claude: claude-fable-5-1, \
             claude-haiku-5-5, claude-opus-5-5, claude-sonnet-5-5; Local: qwen-3.6."
        ), "{d}");
        // Only HOBBES_DISPATCH changes, and re-annotating is a no-op (the
        // cached tool prefix stays byte-identical across requests).
        for (t, b) in tools.iter().zip(&before) {
            if t.name != "HOBBES_DISPATCH" {
                assert_eq!(&t.description, b);
            }
        }
        let once = d.clone();
        annotate_dispatch_models(&mut tools, &settings);
        assert_eq!(tools.iter().find(|t| t.name == "HOBBES_DISPATCH").unwrap().description, once);
        // No connectors: says so instead of listing nothing.
        assert!(dispatch_description(&[]).contains("No models are configured"));
    }
}
