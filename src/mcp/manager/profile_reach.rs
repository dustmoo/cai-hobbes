//! Multi-profile reach: a chat pinned to one Composio profile (its *home*)
//! may also act in other profiles — phase 1: the Heartbeat and Voice system
//! sessions reach every configured profile.
//!
//! - **One tool set.** Composio slugs are identical across profiles (and
//!   collapse to one provider-facing name), so the session sees each tool
//!   once: the home profile's tools plus any only another reachable profile
//!   has, each with a `profile` argument listing the profiles that have it
//!   ([`merge_reach`]).
//! - **Routing.** [`route_tool_call`] reads and strips `profile`, and stamps
//!   the call with its target (`server_name` = `composio-native:{name}`,
//!   `profile_id`), so dispatch, held approvals, the resume path, trust
//!   rules and the result card all follow the target profile.
//! - **Company boundary.** Each profile is a company (Clearmirror, and the
//!   clients it works for). [`cross_company_reason`] asks before a write
//!   lands outside home, or lands in one company after this turn read
//!   another's data; the trust rules can remember the answer, per project.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{is_composio_native, McpContext, McpServerContext, COMPOSIO_NATIVE_PREFIX};
use crate::components::chat::Message;
use crate::components::shared::{MessageContent, ToolCallStatus};
use crate::settings::Settings;

/// The per-call argument naming the target profile.
pub const PROFILE_ARG: &str = "profile";

/// The meta tool that runs another Composio tool by name.
const EXECUTE_TOOL: &str = "COMPOSIO_EXECUTE_TOOL";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachProfile {
    pub id: String,
    pub name: String,
}

/// The profiles `session_id` can act in, home first. Only system sessions
/// (Heartbeat, Voice) reach beyond home in phase 1; with no home profile
/// there is nothing to anchor to, so no reach either.
pub fn reach_for_session(settings: &Settings, session_id: &str, home_id: Option<&str>) -> Vec<ReachProfile> {
    let Some(home) = home_id.and_then(|id| settings.composio_profiles.iter().find(|p| p.id == id)) else {
        return Vec::new();
    };
    let mut out = vec![ReachProfile { id: home.id.clone(), name: home.name.clone() }];
    if crate::system_sessions::is_system(session_id) {
        out.extend(
            settings
                .composio_profiles
                .iter()
                .filter(|p| p.id != home.id && p.is_fully_configured())
                .map(|p| ReachProfile { id: p.id.clone(), name: p.name.clone() }),
        );
    }
    out
}

/// The system-prompt instruction for a session with reach.
pub fn reach_instruction(home: &str, others: &[&str]) -> String {
    format!(
        "Your home profile is '{home}': Composio tools act there unless you pass `profile`. You can \
         also act in {}: pass `profile` with that name on the tool call (its options list who has \
         the app connected). Each profile is a different company. Keep each company's content in \
         its own accounts — never copy one company's documents, emails or data into another's \
         tools unless the person explicitly asks; such writes are held for their approval.",
        others.join(", ")
    )
}

/// The provider-facing server name for a profile.
pub fn server_name_for(profile: &ReachProfile) -> String {
    format!("{COMPOSIO_NATIVE_PREFIX}:{}", profile.name)
}

/// The profile a `composio-native:{name or id}` server name points at.
pub fn profile_for_server_name<'a>(settings: &'a Settings, server_name: &str) -> Option<&'a str> {
    let suffix = server_name.strip_prefix(COMPOSIO_NATIVE_PREFIX)?.strip_prefix(':')?;
    settings
        .composio_profiles
        .iter()
        .find(|p| p.id == suffix || p.name == suffix)
        .map(|p| p.id.as_str())
}

fn native_entry(ctx: &McpContext) -> Option<&McpServerContext> {
    ctx.servers.iter().find(|s| is_composio_native(&s.name))
}

/// The session's tool set across its reach: the home context, with tools
/// only another profile has added to the home Composio entry, and every
/// Composio tool given a `profile` argument naming the profiles that have
/// it. `others` are the other reachable profiles' contexts.
pub fn merge_reach(mut home_ctx: McpContext, home: &ReachProfile, others: &[(ReachProfile, McpContext)]) -> McpContext {
    // Tool name → (tool, profiles that have it), home first.
    let mut order: Vec<String> = Vec::new();
    let mut tools: std::collections::HashMap<String, (rmcp::model::Tool, Vec<String>)> = Default::default();
    let mut add = |tool: &rmcp::model::Tool, profile: &str| {
        let name = tool.name.to_string();
        match tools.get_mut(&name) {
            Some((_, have)) => {
                if !have.iter().any(|p| p == profile) {
                    have.push(profile.to_string());
                }
            }
            None => {
                order.push(name.clone());
                tools.insert(name, (tool.clone(), vec![profile.to_string()]));
            }
        }
    };
    if let Some(entry) = native_entry(&home_ctx) {
        for t in &entry.tools {
            add(t, &home.name);
        }
    }
    let mut slugs: BTreeSet<String> = home_ctx.connected_toolkit_slugs.iter().cloned().collect();
    for (profile, ctx) in others {
        if let Some(entry) = native_entry(ctx) {
            for t in &entry.tools {
                add(t, &profile.name);
            }
        }
        slugs.extend(ctx.connected_toolkit_slugs.iter().cloned());
    }
    if order.is_empty() {
        return home_ctx;
    }
    let merged: Vec<rmcp::model::Tool> = order
        .into_iter()
        .filter_map(|n| tools.remove(&n))
        .map(|(tool, have)| with_profile_arg(tool, &have, &home.name))
        .collect();
    match home_ctx.servers.iter_mut().find(|s| is_composio_native(&s.name)) {
        Some(entry) => entry.tools = merged,
        None => home_ctx.servers.push(McpServerContext {
            name: server_name_for(home),
            description: "Composio tools across your profiles".into(),
            tools: merged,
        }),
    }
    home_ctx.connected_toolkit_slugs = slugs.into_iter().collect();
    home_ctx
}

/// `tool` with a `profile` property listing the profiles that have it.
fn with_profile_arg(mut tool: rmcp::model::Tool, have: &[String], home: &str) -> rmcp::model::Tool {
    let mut schema = (*tool.input_schema).clone();
    let props = schema.entry("properties").or_insert_with(|| json!({}));
    if let Some(props) = props.as_object_mut() {
        let default = if have.iter().any(|p| p == home) {
            format!(" Omit for {home} (home).")
        } else {
            String::new()
        };
        props.insert(
            PROFILE_ARG.into(),
            json!({
                "type": "string",
                "enum": have,
                "description": format!(
                    "Which company's connected account to use.{default} Keep each company's content in its own account."
                ),
            }),
        );
    }
    if !schema.contains_key("type") {
        schema.insert("type".into(), json!("object"));
    }
    tool.input_schema = Arc::new(schema);
    tool
}

/// Keep reach tools a context refresh would drop: `scoped` (the home
/// profile's tools from a background refresh) takes any tool `previous`
/// had in its Composio entry that it lacks, and `previous`'s
/// `profile`-annotated schema for the ones it has.
pub fn carry_reach(mut scoped: McpContext, previous: Option<&McpContext>) -> McpContext {
    let Some(prev_entry) = previous.and_then(native_entry) else {
        return scoped;
    };
    let annotated = |t: &rmcp::model::Tool| {
        t.input_schema
            .get("properties")
            .and_then(|p| p.get(PROFILE_ARG))
            .is_some()
    };
    if !prev_entry.tools.iter().any(annotated) {
        return scoped;
    }
    match scoped.servers.iter_mut().find(|s| is_composio_native(&s.name)) {
        Some(entry) => {
            for t in prev_entry.tools.iter().filter(|t| annotated(t)) {
                match entry.tools.iter_mut().find(|e| e.name == t.name) {
                    Some(existing) => *existing = t.clone(),
                    None => entry.tools.push(t.clone()),
                }
            }
        }
        None => scoped.servers.push(prev_entry.clone()),
    }
    scoped
}

/// The profile a held (then approved) call runs in on the resume path: its
/// routed target when that is in the session's reach, else the session's.
pub fn resume_profile(
    settings: &Settings,
    session_id: &str,
    session_profile: Option<&str>,
    tool_call: &crate::components::shared::ToolCall,
) -> Option<String> {
    let home = session_profile
        .map(str::to_string)
        .or_else(|| settings.get_active_profile().map(|p| p.id.clone()));
    if !is_composio_native(&tool_call.server_name) {
        return home;
    }
    let reach = reach_for_session(settings, session_id, home.as_deref());
    let target = tool_call
        .profile_id
        .clone()
        .or_else(|| profile_for_server_name(settings, &tool_call.server_name).map(str::to_string));
    match target {
        Some(t) if reach.iter().any(|p| p.id == t) => Some(t),
        _ => home,
    }
}

/// What [`route_tool_call`] decided.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    /// Not a Composio call, or the session has no reach: unchanged.
    Unchanged,
    /// Runs in this profile (the call is restamped).
    To(ReachProfile),
    /// The named profile isn't reachable here; the call must fail with this.
    Refused(String),
}

/// Resolve the call's `profile` argument against `reach` (home first),
/// strip it, and restamp `server_name` / `arguments` / `profile_id` with the
/// target. No `profile` = home.
pub fn route_tool_call(reach: &[ReachProfile], tool_call: &mut crate::components::shared::ToolCall) -> Route {
    if reach.len() < 2 || !is_composio_native(&tool_call.server_name) {
        return Route::Unchanged;
    }
    let mut args: Value = serde_json::from_str(&tool_call.arguments).unwrap_or(Value::Null);
    let asked = args
        .as_object_mut()
        .and_then(|m| m.remove(PROFILE_ARG))
        .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    let target = match asked {
        None => reach[0].clone(),
        Some(want) => match reach.iter().find(|p| p.id == want || p.name.eq_ignore_ascii_case(&want)) {
            Some(p) => p.clone(),
            None => {
                let names: Vec<&str> = reach.iter().map(|p| p.name.as_str()).collect();
                return Route::Refused(format!(
                    "Profile \"{want}\" isn't available here. Use one of: {}.",
                    names.join(", ")
                ));
            }
        },
    };
    if let Ok(s) = serde_json::to_string(&args) {
        tool_call.arguments = s;
    }
    tool_call.server_name = server_name_for(&target);
    tool_call.profile_id = Some(target.id.clone());
    Route::To(target)
}

/// Profiles this turn has read Composio results from: completed Composio
/// calls since the last message the person (or a relay) sent.
pub fn turn_read_profiles(settings: &Settings, messages: &[Message]) -> BTreeSet<String> {
    let start = messages.iter().rposition(|m| m.author == "User").map(|i| i + 1).unwrap_or(0);
    messages[start..]
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc) if tc.status == ToolCallStatus::Completed && is_composio_native(&tc.server_name) => {
                tc.profile_id
                    .clone()
                    .or_else(|| profile_for_server_name(settings, &tc.server_name).map(str::to_string))
            }
            _ => None,
        })
        .collect()
}

/// Whether a Composio call changes something outside (by its name, or for
/// the execute meta tool, the tool it runs).
pub fn is_write(tool_name: &str, args: &Value) -> bool {
    let (tool, inner) = if tool_name == EXECUTE_TOOL {
        (
            args.get("tool_name").and_then(Value::as_str).unwrap_or(tool_name),
            args.get("arguments").cloned().unwrap_or(Value::Null),
        )
    } else {
        (tool_name, args.clone())
    };
    crate::todo::agent_time::agent_time_scope_allows(tool, &inner) != crate::todo::agent_time::ScopeVerdict::Allow
}

/// Why a write needs the person: it lands outside home, or in one company
/// after this turn read another's data. `None` = proceed as usual.
pub fn cross_company_reason(
    target: &ReachProfile,
    home: &ReachProfile,
    read_from: &BTreeSet<String>,
    reach: &[ReachProfile],
    write: bool,
) -> Option<String> {
    if !write {
        return None;
    }
    let name_of = |id: &str| reach.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
    let others: Vec<String> = read_from.iter().filter(|id| **id != target.id).map(|id| name_of(id)).collect();
    if !others.is_empty() {
        return Some(format!(
            "This writes to {} after reading from {} in this turn — check it doesn't carry one company's content into another's.",
            target.name,
            others.join(", ")
        ));
    }
    (target.id != home.id).then(|| {
        format!("This acts in {}, outside {} (Hobbes' home profile).", target.name, home.name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::ToolCall;

    fn tool(name: &str) -> rmcp::model::Tool {
        let schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        rmcp::model::Tool::new(name.to_string(), "d".to_string(), Arc::new(schema.as_object().unwrap().clone()))
    }

    fn ctx(profile: &str, tools: &[&str]) -> McpContext {
        McpContext {
            servers: vec![
                McpServerContext { name: "terminal".into(), description: String::new(), tools: vec![tool("RUN")] },
                McpServerContext {
                    name: format!("composio-native:{profile}"),
                    description: String::new(),
                    tools: tools.iter().map(|t| tool(t)).collect(),
                },
            ],
            connected_toolkit_slugs: vec![profile.to_lowercase()],
        }
    }

    fn p(id: &str, name: &str) -> ReachProfile {
        ReachProfile { id: id.into(), name: name.into() }
    }

    fn profile_enum(ctx: &McpContext, tool_name: &str) -> Vec<String> {
        let entry = native_entry(ctx).unwrap();
        let t = entry.tools.iter().find(|t| t.name == tool_name).unwrap();
        serde_json::from_value(t.input_schema["properties"][PROFILE_ARG]["enum"].clone()).unwrap()
    }

    #[test]
    fn merged_tools_appear_once_with_the_profiles_that_have_them() {
        let home = p("cm", "Clearmirror");
        let merged = merge_reach(
            ctx("Clearmirror", &["GMAIL_SEND_EMAIL", "GOOGLEDOCS_CREATE"]),
            &home,
            &[(p("pg", "Puget"), ctx("Puget", &["GMAIL_SEND_EMAIL", "JIRA_CREATE_ISSUE"]))],
        );
        let entry = native_entry(&merged).unwrap();
        assert_eq!(entry.name, "composio-native:Clearmirror");
        let names: Vec<_> = entry.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["GMAIL_SEND_EMAIL", "GOOGLEDOCS_CREATE", "JIRA_CREATE_ISSUE"]);
        assert_eq!(profile_enum(&merged, "GMAIL_SEND_EMAIL"), ["Clearmirror", "Puget"]);
        assert_eq!(profile_enum(&merged, "GOOGLEDOCS_CREATE"), ["Clearmirror"]);
        assert_eq!(profile_enum(&merged, "JIRA_CREATE_ISSUE"), ["Puget"]);
        // Non-Composio servers untouched; toolkit slugs unioned.
        assert_eq!(merged.servers[0].tools[0].name, "RUN");
        assert_eq!(merged.connected_toolkit_slugs, ["clearmirror", "puget"]);
        // The original schema survives.
        assert!(entry.tools[0].input_schema["properties"].get("q").is_some());
    }

    #[test]
    fn a_refresh_keeps_reach_tools() {
        let home = p("cm", "Clearmirror");
        let merged = merge_reach(ctx("Clearmirror", &["A"]), &home, &[(p("pg", "Puget"), ctx("Puget", &["B"]))]);
        let refreshed = carry_reach(ctx("Clearmirror", &["A"]), Some(&merged));
        assert_eq!(profile_enum(&refreshed, "B"), ["Puget"]);
        assert_eq!(profile_enum(&refreshed, "A"), ["Clearmirror"]);
        // Nothing annotated before: nothing carried.
        let plain = carry_reach(ctx("Clearmirror", &["A"]), Some(&ctx("Clearmirror", &["Z"])));
        assert_eq!(native_entry(&plain).unwrap().tools.len(), 1);
    }

    fn call(args: Value) -> ToolCall {
        ToolCall::new("composio-native:Clearmirror".into(), "GMAIL_SEND_EMAIL".into(), args, None, None)
    }

    #[test]
    fn calls_route_to_the_named_profile_and_default_home() {
        let reach = vec![p("cm", "Clearmirror"), p("pg", "Puget")];
        let mut c = call(json!({"to": "a@b.c", "profile": "puget"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("pg", "Puget")));
        assert_eq!(c.server_name, "composio-native:Puget");
        assert_eq!(c.profile_id.as_deref(), Some("pg"));
        let args: Value = serde_json::from_str(&c.arguments).unwrap();
        assert!(args.get("profile").is_none(), "profile must not reach Composio");

        let mut c = call(json!({"to": "a@b.c"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("cm", "Clearmirror")));

        let mut c = call(json!({"profile": "Acme"}));
        assert!(matches!(route_tool_call(&reach, &mut c), Route::Refused(m) if m.contains("Clearmirror, Puget")));

        // No reach, or not Composio: untouched.
        let mut c = call(json!({"profile": "Puget"}));
        assert_eq!(route_tool_call(&reach[..1], &mut c), Route::Unchanged);
        let mut t = ToolCall::new("terminal".into(), "RUN".into(), json!({}), None, None);
        assert_eq!(route_tool_call(&reach, &mut t), Route::Unchanged);
    }

    #[test]
    fn writes_across_companies_ask_reads_do_not() {
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let reach = vec![cm.clone(), pg.clone()];
        let none = BTreeSet::new();
        let read_cm: BTreeSet<String> = ["cm".to_string()].into();
        let read_pg: BTreeSet<String> = ["pg".to_string()].into();
        // Reads never ask.
        assert_eq!(cross_company_reason(&pg, &cm, &read_cm, &reach, false), None);
        // A write at home with only home data: as today.
        assert_eq!(cross_company_reason(&cm, &cm, &read_cm, &reach, true), None);
        // Clearmirror content into Puget's tools: ask, naming both.
        let r = cross_company_reason(&pg, &cm, &read_cm, &reach, true).unwrap();
        assert!(r.contains("writes to Puget after reading from Clearmirror"));
        // Puget content into Clearmirror's tools: ask too.
        assert!(cross_company_reason(&cm, &cm, &read_pg, &reach, true).unwrap().contains("reading from Puget"));
        // Any write outside home asks.
        assert!(cross_company_reason(&pg, &cm, &none, &reach, true).unwrap().contains("outside Clearmirror"));
    }

    #[test]
    fn the_prompt_names_home_and_the_boundary() {
        let i = reach_instruction("Clearmirror", &["Puget", "Acme"]);
        assert!(i.contains("home profile is 'Clearmirror'"));
        assert!(i.contains("Puget, Acme"));
        assert!(i.contains("different company"));
    }

    #[test]
    fn writes_are_told_from_reads() {
        assert!(is_write("GMAIL_SEND_EMAIL", &json!({})));
        assert!(!is_write("GMAIL_FETCH_EMAILS", &json!({})));
        assert!(is_write(EXECUTE_TOOL, &json!({"tool_name": "GOOGLEDOCS_CREATE_DOCUMENT", "arguments": {}})));
        assert!(!is_write(EXECUTE_TOOL, &json!({"tool_name": "GOOGLEDOCS_GET_DOCUMENT", "arguments": {}})));
    }

    #[test]
    fn the_turn_remembers_which_companies_it_read() {
        let settings = Settings::default();
        let mk = |author: &str, server: &str, pid: Option<&str>, status| {
            let mut tc = ToolCall::new(server.into(), "X_GET".into(), json!({}), None, None);
            tc.profile_id = pid.map(str::to_string);
            tc.status = status;
            let mut m = crate::voice::session::transcript_message(true, "x");
            m.author = author.into();
            m.content = MessageContent::ToolCall(tc);
            m
        };
        let mut user = crate::voice::session::transcript_message(true, "hi");
        user.author = "User".into();
        let msgs = vec![
            mk("Hobbes", "composio-native:Puget", Some("old"), ToolCallStatus::Completed), // before the turn
            user,
            mk("Hobbes", "composio-native:Clearmirror", Some("cm"), ToolCallStatus::Completed),
            mk("Hobbes", "composio-native:Puget", Some("pg"), ToolCallStatus::Running), // not read yet
            mk("Hobbes", "terminal", Some("cm2"), ToolCallStatus::Completed), // not Composio
        ];
        let read = turn_read_profiles(&settings, &msgs);
        assert_eq!(read.into_iter().collect::<Vec<_>>(), ["cm"]);
    }
}
