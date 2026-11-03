//! Multi-profile reach: a chat pinned to one Composio profile (its *home*)
//! may also act in other profiles — phase 1: the Heartbeat and Voice system
//! sessions reach every configured profile.
//!
//! - **One tool set.** Composio slugs are identical across profiles (and
//!   collapse to one provider-facing name), so the session sees each tool
//!   once: the home profile's tools plus any only another reachable profile
//!   has, each with a `profile` argument listing the profiles that have it
//!   ([`merge_reach`]).
//! - **Routing.** [`route_tool_call`] reads `profile` and stamps the call
//!   with its target (`server_name` = `composio-native:{name}`,
//!   `profile_id`); the argument stays in the stored call (history shows
//!   which company a call hit) and is stripped only on its way to Composio
//!   ([`strip_profile_arg`]).
//! - **Company boundary.** Each profile is a company (Clearmirror, and the
//!   clients it works for). [`cross_company_hold`] holds a write that lands
//!   outside home, or that follows reading another company's data anywhere
//!   in the session (it stays in the model's context and summary). The hold
//!   is remembered under its own trust key — the direction of the flow — so
//!   ordinary "always allow" rules never wave it through.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{is_composio_native, McpContext, McpServerContext, COMPOSIO_NATIVE_PREFIX};
use crate::components::chat::Message;
use crate::components::shared::{MessageContent, ToolCallStatus};
use crate::settings::Settings;

/// The per-call argument naming the target profile.
/// Named so it can't collide with a tool's own parameter (some Composio
/// tools take a `profile`).
pub const PROFILE_ARG: &str = "hobbes_profile";

/// The meta tool that runs another Composio tool by name.
const EXECUTE_TOOL: &str = "COMPOSIO_EXECUTE_TOOL";

/// Trust-rule server key prefix for cross-company holds.
pub const FLOW_KEY_PREFIX: &str = "profile-flow:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachProfile {
    pub id: String,
    pub name: String,
}

/// The home profile a session acts in: its pin, else the global active
/// profile (the same fallback the tool list and the prompt use).
pub fn home_profile(settings: &Settings, session_pin: Option<&str>) -> Option<String> {
    session_pin
        .map(str::to_string)
        .or_else(|| settings.get_active_profile().map(|p| p.id.clone()))
}

/// The profiles `session_id` can act in, home first. Only the live system
/// sessions (Heartbeat, Voice) reach beyond home in phase 1 — not their
/// rotated-out archives, which are read-only; with no home profile there is
/// nothing to anchor to, so no reach either.
pub fn reach_for_session(settings: &Settings, session_id: &str, home_id: Option<&str>) -> Vec<ReachProfile> {
    let Some(home) = home_id.and_then(|id| settings.composio_profiles.iter().find(|p| p.id == id)) else {
        return Vec::new();
    };
    let mut out = vec![ReachProfile { id: home.id.clone(), name: home.name.clone() }];
    if crate::system_sessions::is_current(session_id) {
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

/// The profiles a session can act in, home first, with phase 2's per-tab
/// reach: an ordinary tab reaches its home plus the profiles ticked on its
/// profile chip (`Session::profile_reach`) that still exist and are set up —
/// a deleted or broken profile drops out silently. Nothing ticked is exactly
/// [`reach_for_session`] (home only). System sessions ignore `selected` and
/// keep their own reach.
pub fn session_reach(
    settings: &Settings,
    session_id: &str,
    home_id: Option<&str>,
    selected: &[String],
) -> Vec<ReachProfile> {
    let mut out = reach_for_session(settings, session_id, home_id);
    if selected.is_empty() || out.is_empty() || crate::system_sessions::is_system(session_id) {
        return out;
    }
    let home_id = out[0].id.clone();
    out.extend(
        settings
            .composio_profiles
            .iter()
            .filter(|p| p.id != home_id && p.is_fully_configured() && selected.iter().any(|s| s == &p.id))
            .map(|p| ReachProfile { id: p.id.clone(), name: p.name.clone() }),
    );
    out
}

/// [`session_reach`] for a hydrated session: its home (pin, else the global
/// active profile) and its reach, home first.
pub fn reach_of(settings: &Settings, session: &crate::session::Session) -> Vec<ReachProfile> {
    let home = home_profile(settings, session.composio_profile.as_deref());
    session_reach(settings, &session.id, home.as_deref(), &session.profile_reach)
}

/// Every configured profile as a [`ReachProfile`] — names for a hold's
/// wording when a profile read earlier has since left the reach.
pub fn all_profiles(settings: &Settings) -> Vec<ReachProfile> {
    settings
        .composio_profiles
        .iter()
        .map(|p| ReachProfile { id: p.id.clone(), name: p.name.clone() })
        .collect()
}

/// Whether `session` called a Composio tool through reach (naming a
/// profile with [`PROFILE_ARG`]) within [`READ_WINDOW`] of `now`. Such a
/// session stays under the cross-company hold after its reach is narrowed:
/// taking a profile off the chip doesn't take its data out of the
/// conversation, and doesn't reset the reads already recorded.
pub fn used_reach(session: &crate::session::Session, now: chrono::DateTime<chrono::Utc>) -> bool {
    session.messages.iter().any(|m| match &m.content {
        MessageContent::ToolCall(tc) if is_composio_native(&tc.server_name) => {
            now - tc.completed_at.unwrap_or(m.created_at) <= READ_WINDOW
                && serde_json::from_str::<Value>(&tc.arguments)
                    .ok()
                    .and_then(|a| asked_profile(&tc.tool_name, &a))
                    .is_some()
        }
        _ => false,
    })
}

/// [`resume_profile`] with an ordinary tab's own reach (`selected`). A held
/// call the model routed with [`PROFILE_ARG`] runs in the profile it was
/// routed to — refused if that profile has since left the tab's reach (or
/// was deleted), never moved home. Unrouted calls, and system sessions,
/// behave exactly as before.
pub fn resume_profile_in(
    settings: &Settings,
    session_id: &str,
    session_profile: Option<&str>,
    selected: &[String],
    tool_call: &crate::components::shared::ToolCall,
) -> Result<Option<String>, String> {
    if crate::system_sessions::is_system(session_id) || !is_composio_native(&tool_call.server_name) {
        return resume_profile(settings, session_id, session_profile, tool_call);
    }
    let args: Value = serde_json::from_str(&tool_call.arguments).unwrap_or(Value::Null);
    if asked_profile(&tool_call.tool_name, &args).is_none() {
        return Ok(session_profile.map(str::to_string));
    }
    let home = home_profile(settings, session_profile);
    let reach = session_reach(settings, session_id, home.as_deref(), selected);
    let target = tool_call
        .profile_id
        .clone()
        .or_else(|| profile_for_server_name(settings, &tool_call.server_name).map(str::to_string));
    match target {
        Some(t) if reach.iter().any(|p| p.id == t) => Ok(Some(t)),
        _ => Err(format!(
            "Not run: the profile this was meant for ({}) is no longer in this tab's reach, and it won't be run in another company's account.",
            tool_call.server_name.trim_start_matches(COMPOSIO_NATIVE_PREFIX).trim_start_matches(':')
        )),
    }
}

/// The bubble's "always allow" wording for a flow key, in profile names.
pub fn flow_label(settings: &Settings, rule_key: &str, tool: &str) -> Option<String> {
    let rest = rule_key.strip_prefix(FLOW_KEY_PREFIX)?;
    let (from, to) = rest.split_once('>')?;
    let name = |id: &str| settings.profile_name_for_id(id).unwrap_or(id).to_string();
    let to_name = if to == "local" { "commands and other tabs".to_string() } else { name(to) };
    Some(if from == "act" {
        format!("Always allow {tool} in {to_name} (with no other company's data in play)")
    } else {
        let from: Vec<String> = from.split('+').map(name).collect();
        format!("Always allow {tool} to carry {} data into {to_name}", from.join(" and "))
    })
}

/// The system-prompt instruction for a session with reach.
pub fn reach_instruction(home: &str, others: &[&str]) -> String {
    format!(
        "Your home profile is '{home}': Composio tools act there unless you pass `hobbes_profile`. You can \
         also act in {}: pass `hobbes_profile` with that name on the tool call (its options list who has \
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
        .find(|p| p.id == suffix)
        .or_else(|| settings.composio_profiles.iter().find(|p| p.name == suffix))
        .map(|p| p.id.as_str())
}

/// Every Composio tool in `ctx`, across all its Composio entries (force-
/// loaded and on-demand tools are separate entries with the same name).
fn native_tools(ctx: &McpContext) -> impl Iterator<Item = &rmcp::model::Tool> {
    ctx.servers.iter().filter(|s| is_composio_native(&s.name)).flat_map(|s| s.tools.iter())
}

fn has_profile_arg(t: &rmcp::model::Tool) -> bool {
    t.input_schema.get("properties").and_then(|p| p.get(PROFILE_ARG)).is_some()
}

/// Replace all Composio entries of `ctx` with one entry holding `tools`.
fn set_native_tools(ctx: &mut McpContext, name: String, description: String, tools: Vec<rmcp::model::Tool>) {
    let at = ctx.servers.iter().position(|s| is_composio_native(&s.name));
    ctx.servers.retain(|s| !is_composio_native(&s.name));
    let entry = McpServerContext { name, description, tools };
    match at {
        Some(i) => ctx.servers.insert(i.min(ctx.servers.len()), entry),
        None => ctx.servers.push(entry),
    }
}

/// The session's tool set across its reach: the home context, its Composio
/// entries folded into one that also carries the tools only another profile
/// has, and every Composio tool given a `profile` argument naming the
/// profiles that have it. `others` are the other reachable profiles'
/// contexts.
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
    for t in native_tools(&home_ctx) {
        add(t, &home.name);
    }
    let mut slugs: BTreeSet<String> = home_ctx.connected_toolkit_slugs.iter().cloned().collect();
    for (profile, ctx) in others {
        for t in native_tools(ctx) {
            add(t, &profile.name);
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
    set_native_tools(&mut home_ctx, server_name_for(home), "Composio tools across your profiles".into(), merged);
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

/// Keep reach tools a context refresh would drop. Only for sessions that
/// still have reach (`reach_names`, home first; empty = none): `scoped` (the
/// home profile's tools from a background refresh) is folded into one
/// Composio entry and takes `previous`'s `profile`-annotated tools, keeping
/// only profile options still in reach.
pub fn carry_reach(mut scoped: McpContext, previous: Option<&McpContext>, reach_names: &[String]) -> McpContext {
    if reach_names.len() < 2 {
        return scoped;
    }
    let Some(prev) = previous else { return scoped };
    let carried: Vec<rmcp::model::Tool> = native_tools(prev).filter(|t| has_profile_arg(t)).cloned().collect();
    if carried.is_empty() {
        return scoped;
    }
    let mut out: Vec<rmcp::model::Tool> = Vec::new();
    for t in native_tools(&scoped) {
        if !out.iter().any(|o| o.name == t.name) {
            out.push(t.clone());
        }
    }
    for t in carried {
        let have: Vec<String> = t.input_schema["properties"][PROFILE_ARG]["enum"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).filter(|n| reach_names.iter().any(|r| r == n)).map(str::to_string).collect())
            .unwrap_or_default();
        if have.is_empty() {
            continue;
        }
        let t = with_profile_arg(t, &have, &reach_names[0]);
        match out.iter_mut().find(|o| o.name == t.name) {
            Some(existing) => *existing = t,
            None => out.push(t),
        }
    }
    let name = scoped
        .servers
        .iter()
        .find(|s| is_composio_native(&s.name))
        .map(|s| s.name.clone())
        .unwrap_or_else(|| format!("{COMPOSIO_NATIVE_PREFIX}:{}", reach_names[0]));
    set_native_tools(&mut scoped, name, "Composio tools across your profiles".into(), out);
    scoped
}

/// The `profile` the model asked for: top level, or (for the execute meta
/// tool) inside its `arguments`.
fn asked_profile(tool_name: &str, args: &Value) -> Option<String> {
    let top = args.get(PROFILE_ARG);
    let nested = (tool_name == EXECUTE_TOOL).then(|| args.get("arguments").and_then(|a| a.get(PROFILE_ARG))).flatten();
    top.or(nested)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Remove `profile` before a call goes to Composio (top level, and inside
/// the execute meta tool's `arguments`).
pub fn strip_profile_arg(tool_name: &str, args: &mut Value) {
    if let Some(m) = args.as_object_mut() {
        m.remove(PROFILE_ARG);
        if tool_name == EXECUTE_TOOL {
            if let Some(inner) = m.get_mut("arguments").and_then(Value::as_object_mut) {
                inner.remove(PROFILE_ARG);
            }
        }
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

/// Resolve the call's `profile` against `reach` (home first) and restamp
/// `server_name` / `profile_id` with the target. No `profile` = home. The
/// stored arguments keep `profile`; strip it before dispatch.
pub fn route_tool_call(reach: &[ReachProfile], tool_call: &mut crate::components::shared::ToolCall) -> Route {
    if !is_composio_native(&tool_call.server_name) {
        return Route::Unchanged;
    }
    let args: Value = serde_json::from_str(&tool_call.arguments).unwrap_or(Value::Null);
    let asked = asked_profile(&tool_call.tool_name, &args);
    if reach.len() < 2 {
        // No reach (or it shrank since the tools were listed): a call naming
        // another profile must not quietly run here.
        return match asked {
            Some(want) if !reach.first().is_some_and(|h| h.id == want || h.name.eq_ignore_ascii_case(&want)) => {
                Route::Refused(format!("Profile \"{want}\" isn't available here."))
            }
            _ => Route::Unchanged,
        };
    }
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
    tool_call.server_name = server_name_for(&target);
    tool_call.profile_id = Some(target.id.clone());
    Route::To(target)
}

/// The profile a held (then approved) call runs in on the resume path.
/// Sessions without reach keep the old behaviour exactly (the session's own
/// pin, `None` included). With reach, the call's routed target — refused if
/// that profile is no longer reachable, never silently moved to home.
pub fn resume_profile(
    settings: &Settings,
    session_id: &str,
    session_profile: Option<&str>,
    tool_call: &crate::components::shared::ToolCall,
) -> Result<Option<String>, String> {
    if !is_composio_native(&tool_call.server_name) {
        return Ok(session_profile.map(str::to_string));
    }
    // Only system sessions route; everything else keeps its own pin. (Not
    // `reach.len()`: a system session whose other profiles dropped out must
    // still refuse a call routed to one of them. An archive counts here, so
    // a stale held call in it is refused rather than run at home: its reach
    // is home only.)
    if !crate::system_sessions::is_system(session_id) {
        return Ok(session_profile.map(str::to_string));
    }
    let home = home_profile(settings, session_profile);
    let reach = reach_for_session(settings, session_id, home.as_deref());
    let target = tool_call
        .profile_id
        .clone()
        .or_else(|| profile_for_server_name(settings, &tool_call.server_name).map(str::to_string));
    match target {
        None => Ok(home),
        Some(t) if reach.iter().any(|p| p.id == t) => Ok(Some(t)),
        Some(_) => Err(format!(
            "Not run: the profile this was meant for ({}) is no longer available, and it won't be run in another company's account.",
            tool_call.server_name.trim_start_matches(COMPOSIO_NATIVE_PREFIX).trim_start_matches(':')
        )),
    }
}

/// How long a read counts: data read from a company within this window is
/// treated as still in play. Long enough to span a voice conversation or a
/// run of Heartbeat wakes (not just the latest message); short enough that
/// one read doesn't hold every write in a long-lived session for good.
pub const READ_WINDOW: chrono::Duration = chrono::Duration::hours(2);

/// The Composio reads in `messages` within [`READ_WINDOW`] of `now`: which
/// profile, and when.
pub fn recent_reads(
    settings: &Settings,
    messages: &[Message],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::session::CarriedRead> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc)
                if tc.status == ToolCallStatus::Completed && is_composio_native(&tc.server_name) =>
            {
                let read_at = tc.completed_at.unwrap_or(m.created_at);
                if now - read_at > READ_WINDOW {
                    return None;
                }
                let profile_id = tc
                    .profile_id
                    .clone()
                    .or_else(|| profile_for_server_name(settings, &tc.server_name).map(str::to_string))?;
                Some(crate::session::CarriedRead { profile_id, read_at })
            }
            _ => None,
        })
        .collect()
}

/// `reads` still inside [`READ_WINDOW`] of `now`, newest per profile (sorted
/// by profile id, so equal inputs give equal lists).
pub fn merge_reads(
    reads: impl IntoIterator<Item = crate::session::CarriedRead>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::session::CarriedRead> {
    let mut newest: std::collections::BTreeMap<String, chrono::DateTime<chrono::Utc>> = Default::default();
    for r in reads.into_iter().filter(|r| now - r.read_at <= READ_WINDOW) {
        let at = newest.entry(r.profile_id).or_insert(r.read_at);
        if r.read_at > *at {
            *at = r.read_at;
        }
    }
    newest
        .into_iter()
        .map(|(profile_id, read_at)| crate::session::CarriedRead { profile_id, read_at })
        .collect()
}

/// A session's reads still in play: those in its own messages and those it
/// carries, newest per profile.
pub fn session_recent_reads(
    settings: &Settings,
    session: &crate::session::Session,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::session::CarriedRead> {
    merge_reads(recent_reads(settings, &session.messages, now).into_iter().chain(session.carried_reads.iter().cloned()), now)
}

/// Profiles this session has read Composio results from within
/// [`READ_WINDOW`] of `now` — across messages, since read data stays in the
/// model's context — plus reads a rotated system session carried over from
/// the session it replaced (`Session::carried_reads`; the data may be in
/// the carried summary).
pub fn session_read_profiles(
    settings: &Settings,
    session: &crate::session::Session,
    now: chrono::DateTime<chrono::Utc>,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> =
        recent_reads(settings, &session.messages, now).into_iter().map(|r| r.profile_id).collect();
    out.extend(
        session
            .carried_reads
            .iter()
            .filter(|r| now - r.read_at <= READ_WINDOW)
            .map(|r| r.profile_id.clone()),
    );
    out
}

/// Whether a call changes something outside. Composio meta tools that only
/// discover or load tools don't; the execute meta tool is judged by the
/// tool it runs.
pub fn is_write(tool_name: &str, args: &Value) -> bool {
    // Only Hobbes' own discovery / loading meta tools; any other COMPOSIO_*
    // tool (multi-execute, workbench, …) is judged like a normal tool.
    if matches!(tool_name, "COMPOSIO_DISCOVER_APPS" | "COMPOSIO_GET_APP_TOOLS" | "COMPOSIO_CLEAR_TOOLS") {
        return false;
    }
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

/// Whether a call that isn't routed to a Composio profile moves data out of
/// the session: terminal commands and other MCP writes, and handing work or
/// messages to another tab. Hobbes' own space (timers, todos, planner,
/// scratchpad) is not another company.
pub fn local_write(server_name: &str, tool_name: &str, args: &Value) -> bool {
    use super::{HOBBES_CORE_SERVER, HOBBES_META_SERVER, HOBBES_PLANNER_SERVER};
    if matches!(server_name, HOBBES_CORE_SERVER | HOBBES_META_SERVER | HOBBES_PLANNER_SERVER) || tool_name.starts_with("HOBBES_") {
        return matches!(tool_name, "HOBBES_DISPATCH" | "HOBBES_MESSAGE_PARENT" | "HOBBES_MESSAGE_WORKER");
    }
    is_write(tool_name, args)
}

/// A held cross-company write: why, and the trust key that remembers it.
#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    pub reason: String,
    /// `profile-flow:{source ids}>{target id or "local"}` (data flowing) or
    /// `profile-flow:act>{target id}` (acting outside home, no foreign data)
    /// — distinct, so allowing one never allows the other; matched only by
    /// trust rules created from such a bubble.
    pub rule_key: String,
}

/// Hold a write that lands outside home, or after reading another company's
/// data. `target` = the Composio profile written to; `None` = a non-Composio
/// write (terminal, dispatch, …), which counts as home-side.
pub fn cross_company_hold(
    target: Option<&ReachProfile>,
    home: &ReachProfile,
    read_from: &BTreeSet<String>,
    reach: &[ReachProfile],
    write: bool,
) -> Option<Hold> {
    if !write {
        return None;
    }
    let dest_id = target.map_or(home.id.as_str(), |t| t.id.as_str());
    let name_of = |id: &str| reach.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
    let sources: Vec<&String> = read_from.iter().filter(|id| id.as_str() != dest_id).collect();
    let to_key = target.map_or("local".to_string(), |t| t.id.clone());
    if !sources.is_empty() {
        let from: Vec<String> = sources.iter().map(|id| name_of(id)).collect();
        let what = match target {
            Some(t) => format!("This writes to {}", t.name),
            None => "This acts outside your connected accounts".to_string(),
        };
        let ids: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
        return Some(Hold {
            reason: format!(
                "{what} in a conversation that has read {} data — check it doesn't carry one company's content into another's.",
                from.join(" and ")
            ),
            rule_key: format!("{FLOW_KEY_PREFIX}{}>{to_key}", ids.join("+")),
        });
    }
    match target {
        Some(t) if t.id != home.id => Some(Hold {
            reason: format!("This acts in {}, outside {} (this chat's home profile).", t.name, home.name),
            rule_key: format!("{FLOW_KEY_PREFIX}act>{}", t.id),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::ToolCall;

    fn tool(name: &str) -> rmcp::model::Tool {
        let schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        rmcp::model::Tool::new(name.to_string(), "d".to_string(), Arc::new(schema.as_object().unwrap().clone()))
    }

    fn entry(profile: &str, tools: &[&str]) -> McpServerContext {
        McpServerContext {
            name: format!("composio-native:{profile}"),
            description: String::new(),
            tools: tools.iter().map(|t| tool(t)).collect(),
        }
    }

    fn ctx(profile: &str, tools: &[&str]) -> McpContext {
        McpContext {
            servers: vec![
                McpServerContext { name: "terminal".into(), description: String::new(), tools: vec![tool("RUN")] },
                entry(profile, tools),
            ],
            connected_toolkit_slugs: vec![profile.to_lowercase()],
        }
    }

    fn p(id: &str, name: &str) -> ReachProfile {
        ReachProfile { id: id.into(), name: name.into() }
    }

    fn natives(ctx: &McpContext) -> Vec<&McpServerContext> {
        ctx.servers.iter().filter(|s| is_composio_native(&s.name)).collect()
    }

    fn profile_enum(ctx: &McpContext, tool_name: &str) -> Vec<String> {
        let t = native_tools(ctx).find(|t| t.name == tool_name).unwrap();
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
        let n = natives(&merged);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].name, "composio-native:Clearmirror");
        let names: Vec<_> = n[0].tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["GMAIL_SEND_EMAIL", "GOOGLEDOCS_CREATE", "JIRA_CREATE_ISSUE"]);
        assert_eq!(profile_enum(&merged, "GMAIL_SEND_EMAIL"), ["Clearmirror", "Puget"]);
        assert_eq!(profile_enum(&merged, "GOOGLEDOCS_CREATE"), ["Clearmirror"]);
        assert_eq!(profile_enum(&merged, "JIRA_CREATE_ISSUE"), ["Puget"]);
        assert_eq!(merged.servers[0].tools[0].name, "RUN");
        assert_eq!(merged.connected_toolkit_slugs, ["clearmirror", "puget"]);
        assert!(n[0].tools[0].input_schema["properties"].get("q").is_some());
    }

    #[test]
    fn on_demand_entries_fold_in_without_duplicates() {
        // Force-loaded and discovered tools are separate entries per profile.
        let mut home_ctx = ctx("Clearmirror", &["GMAIL_SEND_EMAIL"]);
        home_ctx.servers.push(entry("Clearmirror", &["NOTION_CREATE_PAGE"]));
        let mut puget = ctx("Puget", &["NOTION_CREATE_PAGE"]);
        puget.servers.push(entry("Puget", &["JIRA_CREATE_ISSUE"]));
        let merged = merge_reach(home_ctx, &p("cm", "Clearmirror"), &[(p("pg", "Puget"), puget)]);
        assert_eq!(natives(&merged).len(), 1, "one Composio entry, so no duplicate declarations");
        assert_eq!(profile_enum(&merged, "NOTION_CREATE_PAGE"), ["Clearmirror", "Puget"]);
        assert_eq!(profile_enum(&merged, "JIRA_CREATE_ISSUE"), ["Puget"]);
    }

    #[test]
    fn a_refresh_keeps_reach_tools_only_while_reach_lasts() {
        let reach = vec!["Clearmirror".to_string(), "Puget".to_string()];
        let merged = merge_reach(ctx("Clearmirror", &["A"]), &p("cm", "Clearmirror"), &[(p("pg", "Puget"), ctx("Puget", &["A", "B"]))]);
        let mut refreshed_ctx = ctx("Clearmirror", &["A"]);
        refreshed_ctx.servers.push(entry("Clearmirror", &["A"])); // a dynamic duplicate
        let refreshed = carry_reach(refreshed_ctx, Some(&merged), &reach);
        assert_eq!(natives(&refreshed).len(), 1);
        assert_eq!(profile_enum(&refreshed, "B"), ["Puget"]);
        assert_eq!(profile_enum(&refreshed, "A"), ["Clearmirror", "Puget"]);
        // Puget left the reach: its options and its own tools go.
        let shrunk = carry_reach(ctx("Clearmirror", &["A"]), Some(&merged), &["Clearmirror".to_string(), "Acme".to_string()]);
        assert!(native_tools(&shrunk).all(|t| t.name != "B"));
        // No reach: never touched.
        let plain = carry_reach(ctx("Clearmirror", &["A"]), Some(&merged), &["Clearmirror".to_string()]);
        assert_eq!(plain, ctx("Clearmirror", &["A"]));
    }

    fn call(args: Value) -> ToolCall {
        ToolCall::new("composio-native:Clearmirror".into(), "GMAIL_SEND_EMAIL".into(), args, None, None)
    }

    #[test]
    fn calls_route_to_the_named_profile_and_default_home() {
        let reach = vec![p("cm", "Clearmirror"), p("pg", "Puget")];
        let mut c = call(json!({"to": "a@b.c", "hobbes_profile": "puget"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("pg", "Puget")));
        assert_eq!(c.server_name, "composio-native:Puget");
        assert_eq!(c.profile_id.as_deref(), Some("pg"));
        // History keeps the profile; dispatch strips it.
        let mut args: Value = serde_json::from_str(&c.arguments).unwrap();
        assert_eq!(args[PROFILE_ARG], "puget");
        strip_profile_arg(&c.tool_name, &mut args);
        assert!(args.get(PROFILE_ARG).is_none());

        let mut c = call(json!({"to": "a@b.c"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("cm", "Clearmirror")));

        let mut c = call(json!({"hobbes_profile": "Acme"}));
        assert!(matches!(route_tool_call(&reach, &mut c), Route::Refused(m) if m.contains("Clearmirror, Puget")));

        // Reach shrank to home: naming another profile is refused, naming home is fine.
        let mut c = call(json!({"hobbes_profile": "Puget"}));
        assert!(matches!(route_tool_call(&reach[..1], &mut c), Route::Refused(_)));
        let mut c = call(json!({"hobbes_profile": "Clearmirror"}));
        assert_eq!(route_tool_call(&reach[..1], &mut c), Route::Unchanged);
        // A tool's own `profile` parameter is left alone.
        let mut c = call(json!({"profile": "linkedin-handle"}));
        assert_eq!(route_tool_call(&reach[..1], &mut c), Route::Unchanged);
        let mut c = call(json!({"profile": "linkedin-handle"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("cm", "Clearmirror")));
        let mut a: Value = serde_json::from_str(&c.arguments).unwrap();
        strip_profile_arg(&c.tool_name, &mut a);
        assert_eq!(a["profile"], "linkedin-handle");
        let mut t = ToolCall::new("terminal".into(), "RUN".into(), json!({}), None, None);
        assert_eq!(route_tool_call(&reach, &mut t), Route::Unchanged);
    }

    #[test]
    fn the_execute_tool_routes_and_strips_a_nested_profile() {
        let reach = vec![p("cm", "Clearmirror"), p("pg", "Puget")];
        let mut c = ToolCall::new(
            "composio-native:Clearmirror".into(),
            EXECUTE_TOOL.into(),
            json!({"tool_name": "JIRA_CREATE_ISSUE", "arguments": {"hobbes_profile": "Puget", "title": "x"}}),
            None,
            None,
        );
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("pg", "Puget")));
        let mut args: Value = serde_json::from_str(&c.arguments).unwrap();
        strip_profile_arg(EXECUTE_TOOL, &mut args);
        assert!(args["arguments"].get(PROFILE_ARG).is_none());
        assert_eq!(args["arguments"]["title"], "x");
    }

    #[test]
    fn writes_across_companies_hold_under_their_own_trust_key() {
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let reach = vec![cm.clone(), pg.clone()];
        let none = BTreeSet::new();
        let read_cm: BTreeSet<String> = ["cm".to_string()].into();
        let read_pg: BTreeSet<String> = ["pg".to_string()].into();
        // Reads never hold.
        assert_eq!(cross_company_hold(Some(&pg), &cm, &read_cm, &reach, false), None);
        // A write at home with only home data: as today.
        assert_eq!(cross_company_hold(Some(&cm), &cm, &read_cm, &reach, true), None);
        // Clearmirror content into Puget's tools.
        let h = cross_company_hold(Some(&pg), &cm, &read_cm, &reach, true).unwrap();
        assert!(h.reason.contains("writes to Puget") && h.reason.contains("read Clearmirror data"));
        assert_eq!(h.rule_key, "profile-flow:cm>pg");
        // Puget content into Clearmirror's tools.
        let h = cross_company_hold(Some(&cm), &cm, &read_pg, &reach, true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>cm");
        // Puget content out through a non-Composio write (terminal, dispatch).
        let h = cross_company_hold(None, &cm, &read_pg, &reach, true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>local");
        // Home data out through a non-Composio write: as today.
        assert_eq!(cross_company_hold(None, &cm, &read_cm, &reach, true), None);
        // Any write outside home holds.
        let h = cross_company_hold(Some(&pg), &cm, &none, &reach, true).unwrap();
        assert!(h.reason.contains("outside Clearmirror"));
        // Distinct from "Clearmirror data into Puget", so allowing one never allows the other.
        assert_eq!(h.rule_key, "profile-flow:act>pg");
    }

    #[test]
    fn flow_keys_never_match_ordinary_rules() {
        // An "always allow Puget GMAIL_SEND_EMAIL" rule from an ordinary tab.
        let ordinary = crate::context::permissions::TrustRule {
            id: "r1".into(),
            server: "composio-native:Puget".into(),
            tool: Some("GMAIL_SEND_EMAIL".into()),
            command_prefix: None,
            project_id: None,
            created_at: chrono::Utc::now(),
        };
        let ctx = crate::context::trust::TrustCtx { project_id: None };
        let rules = [ordinary];
        assert!(crate::context::trust::evaluate(&rules, "profile-flow:cm>pg", "GMAIL_SEND_EMAIL", &json!({}), &ctx).is_none());
    }

    #[test]
    fn writes_are_told_from_reads() {
        assert!(is_write("GMAIL_SEND_EMAIL", &json!({})));
        assert!(!is_write("GMAIL_FETCH_EMAILS", &json!({})));
        assert!(is_write(EXECUTE_TOOL, &json!({"tool_name": "GOOGLEDOCS_CREATE_DOCUMENT", "arguments": {}})));
        assert!(!is_write(EXECUTE_TOOL, &json!({"tool_name": "GOOGLEDOCS_GET_DOCUMENT", "arguments": {}})));
        // Discovery / loading meta tools are not writes.
        assert!(!is_write("COMPOSIO_DISCOVER_APPS", &json!({})));
        assert!(!is_write("COMPOSIO_CLEAR_TOOLS", &json!({})));
        // Other COMPOSIO_* tools are judged by their verbs.
        assert!(is_write("COMPOSIO_MULTI_EXECUTE_TOOL", &json!({})));
    }

    #[test]
    fn hobbes_own_space_is_not_another_company() {
        assert!(!local_write("hobbes-core", "HOBBES_SET_TIMER", &json!({})));
        assert!(!local_write("hobbes-planner", "HOBBES_TODO_CREATE", &json!({})));
        // Data leaving the session for another tab or the shell.
        assert!(local_write("hobbes-planner", "HOBBES_DISPATCH", &json!({})));
        assert!(local_write("hobbes-core", "HOBBES_MESSAGE_WORKER", &json!({})));
        assert!(local_write("hobbes-terminal", "RUN_COMMAND", &json!({"command": "curl -X POST x"})));
    }

    #[test]
    fn flow_labels_say_what_is_allowed() {
        let settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        assert_eq!(
            flow_label(&settings, "profile-flow:cm>pg", "GMAIL_SEND_EMAIL").unwrap(),
            "Always allow GMAIL_SEND_EMAIL to carry Clearmirror data into Puget"
        );
        assert!(flow_label(&settings, "profile-flow:act>pg", "X").unwrap().contains("in Puget (with no other company's data"));
        assert_eq!(flow_label(&settings, "composio-native:Puget", "X"), None);
    }

    #[test]
    fn reads_are_remembered_for_the_whole_session() {
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
            mk("Hobbes", "composio-native:Puget", Some("pg"), ToolCallStatus::Completed), // an earlier wake
            user,
            mk("Hobbes", "composio-native:Clearmirror", Some("cm"), ToolCallStatus::Completed),
            mk("Hobbes", "composio-native:Acme", Some("ac"), ToolCallStatus::Running), // not read yet
            mk("Hobbes", "terminal", Some("x"), ToolCallStatus::Completed), // not Composio
        ];
        let now = chrono::Utc::now();
        let mut session = crate::session_events::blank_session();
        session.messages = msgs;
        let read = session_read_profiles(&settings, &session, now);
        assert_eq!(read.into_iter().collect::<Vec<_>>(), ["cm", "pg"]);
        // Reads age out of the window.
        assert!(session_read_profiles(&settings, &session, now + READ_WINDOW + chrono::Duration::minutes(1)).is_empty());
    }

    fn settings_with(profiles: &[(&str, &str, bool)]) -> Settings {
        let mut s = Settings::default();
        s.composio_profiles = profiles
            .iter()
            .map(|(id, name, ready)| crate::settings::ComposioProfile {
                id: id.to_string(),
                name: name.to_string(),
                user_id: ready.then(|| "u".to_string()),
                api_key: ready.then(|| "k".to_string()),
                ..Default::default()
            })
            .collect();
        s
    }

    #[test]
    fn approval_never_moves_a_call_to_another_company() {
        crate::system_sessions::register(crate::system_sessions::SystemSession::Voice, "reach-test-voice");
        let settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        let mut held = call(json!({"hobbes_profile": "Puget"}));
        held.server_name = "composio-native:Puget".into();
        held.profile_id = Some("pg".into());
        // Still reachable: runs in Puget.
        assert_eq!(resume_profile(&settings, "reach-test-voice", Some("cm"), &held), Ok(Some("pg".into())));
        // Puget lost its key before the approval: refused, not run at home.
        let broken = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", false)]);
        assert!(resume_profile(&broken, "reach-test-voice", Some("cm"), &held).is_err());
        // An ordinary tab: exactly as before (its own pin, even None).
        assert_eq!(resume_profile(&settings, "a-normal-tab", None, &held), Ok(None));
        assert_eq!(resume_profile(&settings, "a-normal-tab", Some("cm"), &held), Ok(Some("cm".into())));
    }

    #[test]
    fn an_unpinned_system_session_still_routes() {
        crate::system_sessions::register(crate::system_sessions::SystemSession::Heartbeat, "reach-test-hb");
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let home = home_profile(&settings, None);
        let reach = reach_for_session(&settings, "reach-test-hb", home.as_deref());
        let mut c = call(json!({"hobbes_profile": "Puget"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::To(p("pg", "Puget")));
    }

    // ── Phase 2: per-tab reach ──────────────────────────────────────────

    fn reach_settings() -> Settings {
        let mut s = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true), ("ac", "Acme", false)]);
        s.active_composio_profile = Some("cm".into());
        s
    }

    #[test]
    fn an_ordinary_tab_reaches_home_plus_its_ticked_profiles() {
        let settings = reach_settings();
        // Nothing ticked: home only, exactly as before.
        assert_eq!(session_reach(&settings, "p2-tab", Some("cm"), &[]), vec![p("cm", "Clearmirror")]);
        assert_eq!(session_reach(&settings, "p2-tab", Some("cm"), &[]), reach_for_session(&settings, "p2-tab", Some("cm")));
        // Deleted and not-set-up profiles drop out silently; home isn't repeated.
        let ticked: Vec<String> = ["gone", "ac", "pg", "cm"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            session_reach(&settings, "p2-tab", Some("cm"), &ticked),
            vec![p("cm", "Clearmirror"), p("pg", "Puget")]
        );
        // Only deleted profiles ticked: still single-profile.
        assert_eq!(session_reach(&settings, "p2-tab", Some("cm"), &["gone".to_string()]).len(), 1);
        // System sessions keep their own reach; the tab list is ignored.
        crate::system_sessions::register(crate::system_sessions::SystemSession::Heartbeat, "p2-hb");
        assert_eq!(
            session_reach(&settings, "p2-hb", Some("cm"), &["gone".to_string()]),
            reach_for_session(&settings, "p2-hb", Some("cm"))
        );
    }

    #[test]
    fn nothing_ticked_changes_no_tools_and_routes_nothing() {
        let settings = reach_settings();
        let reach = session_reach(&settings, "p2-plain", Some("cm"), &[]);
        // The tool refresh leaves a single-profile context untouched (no
        // `hobbes_profile` argument added anywhere).
        let names: Vec<String> = reach.iter().map(|p| p.name.clone()).collect();
        let plain = carry_reach(ctx("Clearmirror", &["A"]), Some(&ctx("Clearmirror", &["A"])), &names);
        assert_eq!(plain, ctx("Clearmirror", &["A"]));
        assert!(native_tools(&plain).all(|t| !has_profile_arg(t)));
        // Calls run as before; naming another company is refused.
        let mut c = call(json!({"to": "a@b.c"}));
        assert_eq!(route_tool_call(&reach, &mut c), Route::Unchanged);
        let mut c = call(json!({"hobbes_profile": "Puget"}));
        assert!(matches!(route_tool_call(&reach, &mut c), Route::Refused(_)));
        // A held call resumes in the tab's pin, None included.
        let held = call(json!({"to": "a@b.c"}));
        assert_eq!(resume_profile_in(&settings, "p2-plain", Some("cm"), &[], &held), Ok(Some("cm".into())));
        assert_eq!(resume_profile_in(&settings, "p2-plain", None, &[], &held), Ok(None));
    }

    fn read_via_reach(profile: &ReachProfile, at: chrono::DateTime<chrono::Utc>) -> Message {
        let mut tc = ToolCall::new(
            server_name_for(profile),
            "GMAIL_FETCH_EMAILS".into(),
            json!({"hobbes_profile": profile.name}),
            None,
            None,
        );
        tc.profile_id = Some(profile.id.clone());
        tc.status = ToolCallStatus::Completed;
        tc.completed_at = Some(at);
        let mut m = crate::voice::session::transcript_message(true, "x");
        m.author = "Hobbes".into();
        m.content = MessageContent::ToolCall(tc);
        m
    }

    #[test]
    fn narrowing_reach_refuses_the_profile_but_keeps_its_reads() {
        let settings = reach_settings();
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let now = chrono::Utc::now();
        let mut session = crate::session_events::blank_session();
        session.id = "p2-narrowed".into();
        session.composio_profile = Some("cm".into());
        session.profile_reach = vec!["pg".into()];
        assert_eq!(reach_of(&settings, &session), vec![cm.clone(), pg.clone()]);
        // The tab read Puget mail through reach…
        session.messages.push(read_via_reach(&pg, now));
        // …and a write to Puget was held, awaiting approval.
        let mut held = call(json!({"hobbes_profile": "Puget"}));
        assert_eq!(route_tool_call(&reach_of(&settings, &session), &mut held), Route::To(pg.clone()));
        assert_eq!(resume_profile_in(&settings, &session.id, Some("cm"), &session.profile_reach, &held), Ok(Some("pg".into())));

        // Puget is unticked.
        session.profile_reach.clear();
        let reach = reach_of(&settings, &session);
        assert_eq!(reach, vec![cm.clone()]);
        // A new call naming Puget is refused at mint…
        let mut c = call(json!({"hobbes_profile": "Puget"}));
        assert!(matches!(route_tool_call(&reach, &mut c), Route::Refused(_)));
        // …and the held one is refused on approval, never moved home.
        assert!(resume_profile_in(&settings, &session.id, Some("cm"), &session.profile_reach, &held).is_err());
        // The Puget read still counts: a write at home is held as a flow.
        assert!(used_reach(&session, now));
        let read_from = session_read_profiles(&settings, &session, now);
        assert!(read_from.contains("pg"));
        let h = cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>cm");
        assert!(h.reason.contains("read Puget data"), "named although Puget left the reach");
        // Once the read window passes, the tab is back to single-profile.
        assert!(!used_reach(&session, now + READ_WINDOW + chrono::Duration::minutes(1)));
    }

    #[test]
    fn a_flow_allowed_in_a_project_is_allowed_only_there() {
        let rule = crate::context::permissions::TrustRule {
            id: "r-flow".into(),
            server: "profile-flow:pg>cm".into(),
            tool: Some("GMAIL_SEND_EMAIL".into()),
            command_prefix: None,
            project_id: Some("proj-cm".into()),
            created_at: chrono::Utc::now(),
        };
        let rules = [rule];
        let eval = |key: &str, project: Option<&str>| {
            crate::context::trust::evaluate(
                &rules,
                key,
                "GMAIL_SEND_EMAIL",
                &json!({}),
                &crate::context::trust::TrustCtx { project_id: project },
            )
            .is_some()
        };
        assert!(eval("profile-flow:pg>cm", Some("proj-cm")));
        assert!(!eval("profile-flow:pg>cm", Some("proj-other")));
        assert!(!eval("profile-flow:pg>cm", None));
        // The other direction is its own decision.
        assert!(!eval("profile-flow:cm>pg", Some("proj-cm")));
    }

    #[test]
    fn the_prompt_names_home_and_the_boundary() {
        let i = reach_instruction("Clearmirror", &["Puget", "Acme"]);
        assert!(i.contains("home profile is 'Clearmirror'"));
        assert!(i.contains("Puget, Acme"));
        assert!(i.contains("different company"));
    }
}
