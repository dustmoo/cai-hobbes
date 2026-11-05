//! Multi-profile reach: a chat pinned to one Composio profile (its *home*)
//! may also act in other profiles — phase 1: the Hobbes system session
//! (Heartbeat wakes, voice and typed turns alike) reaches every configured
//! profile.
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

/// A configured profile by id, else by exact name (pins written before
/// profiles had ids, as [`profile_for_server_name`] accepts).
pub fn find_profile<'a>(settings: &'a Settings, key: &str) -> Option<&'a crate::settings::ComposioProfile> {
    settings
        .composio_profiles
        .iter()
        .find(|p| p.id == key)
        .or_else(|| settings.composio_profiles.iter().find(|p| p.name == key))
}

/// Where a chat's home profile came from ([`ProfileScope::source`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeSource {
    /// The tab's own pin, which names a configured profile.
    Pinned,
    /// No pin: the global active profile.
    Global,
    /// Nothing to act in: the pin names a profile that was removed
    /// (`pinned_id`), or there is no pin and no valid global active
    /// profile (`None`). A removed pin never falls back to the global
    /// profile — that would move a tab holding one company's data into
    /// another company's tools.
    Missing { pinned_id: Option<String> },
}

/// Which Composio profile a chat acts in: its home, its reach (home first)
/// and where the home came from. The one resolver behind the prompt, the
/// summarizer's profile note, the flow guard, tool dispatch (streaming and
/// approval resume) and the profile chip, so none of them can disagree.
///
/// Rules: the pin, if that profile still exists; else (no pin) the global
/// active profile, if it exists; else [`ScopeSource::Missing`] — no home
/// and no reach. Never a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileScope {
    pub home: Option<ReachProfile>,
    pub reach: Vec<ReachProfile>,
    pub source: ScopeSource,
}

impl ProfileScope {
    /// The scope for `session_id` with pin `pin` and the profiles ticked on
    /// its chip (`selected`), for call sites that hold only those.
    pub fn resolve(settings: &Settings, session_id: &str, pin: Option<&str>, selected: &[String]) -> Self {
        let pin = pin.filter(|p| !p.is_empty());
        let as_reach = |p: &crate::settings::ComposioProfile| ReachProfile { id: p.id.clone(), name: p.name.clone() };
        let (home, source) = match pin {
            Some(id) => match find_profile(settings, id) {
                Some(p) => (Some(as_reach(p)), ScopeSource::Pinned),
                None => (None, ScopeSource::Missing { pinned_id: Some(id.to_string()) }),
            },
            None => match settings.get_active_profile() {
                Some(p) => (Some(as_reach(p)), ScopeSource::Global),
                None => (None, ScopeSource::Missing { pinned_id: None }),
            },
        };
        let reach = session_reach(settings, session_id, home.as_ref().map(|h| h.id.as_str()), selected);
        ProfileScope { home, reach, source }
    }

    /// The scope of a hydrated session.
    pub fn for_session(settings: &Settings, session: &crate::session::Session) -> Self {
        Self::resolve(settings, &session.id, session.composio_profile.as_deref(), &session.profile_reach)
    }

    pub fn home_id(&self) -> Option<&str> {
        self.home.as_ref().map(|h| h.id.as_str())
    }

    pub fn home_name(&self) -> Option<&str> {
        self.home.as_ref().map(|h| h.name.as_str())
    }

    /// The tab's pin names a profile that no longer exists (the chip warns).
    pub fn pin_removed(&self) -> bool {
        matches!(self.source, ScopeSource::Missing { pinned_id: Some(_) })
    }

    /// The pin a turn carries so its home stays put for the whole turn: the
    /// home's id (a tab without a pin keeps the global profile it started
    /// in, even if focus — and with it the global profile — moves), else
    /// the removed pin (still [`ScopeSource::Missing`] when resolved again).
    pub fn turn_pin(&self) -> Option<String> {
        match (&self.home, &self.source) {
            (Some(h), _) => Some(h.id.clone()),
            (None, ScopeSource::Missing { pinned_id }) => pinned_id.clone(),
            (None, _) => None,
        }
    }

    /// The tool result a Composio call gets when there is no profile to run
    /// it in; `None` when there is one.
    pub fn missing_refusal(&self) -> Option<String> {
        match &self.source {
            ScopeSource::Missing { pinned_id: Some(_) } => Some(
                "Not run: this tab's Composio profile was removed — pick one on the profile chip.".to_string(),
            ),
            ScopeSource::Missing { pinned_id: None } => Some(
                "Not run: this tab has no Composio profile — pick one on the profile chip.".to_string(),
            ),
            _ => None,
        }
    }
}

/// The profile name the summarizer's profile note gives for `session`: its
/// [`ProfileScope`] home, or "None" when nothing resolves — never the
/// global profile for a tab pinned elsewhere.
pub fn profile_note_name(settings: &Settings, session: &crate::session::Session) -> String {
    ProfileScope::for_session(settings, session).home_name().unwrap_or("None").to_string()
}

// ── The turn's profile ──────────────────────────────────────────────────

/// A turn's fixed profile: the tab's pin when the turn started (or was last
/// refreshed) and the pin the turn runs under (`None` = no profile).
#[derive(Clone)]
struct TurnPin {
    tab_pin: Option<String>,
    turn: Option<String>,
}

type TurnPins = std::sync::Mutex<std::collections::HashMap<String, TurnPin>>;

/// Session id → its current turn's profile, fixed when the turn starts
/// ([`begin_turn`]) and read by every round of it — the prompt, the tool
/// list, the guard and dispatch — so the global profile moving with focus
/// can't change companies mid-turn. A repin of the tab itself does: every
/// read compares the tab's pin with the one the turn was fixed under, and
/// a changed pin (the chip, a dispatch, the pin migration — any path)
/// re-resolves the turn to it.
#[cfg(not(test))]
fn turn_pins() -> &'static TurnPins {
    static PINS: std::sync::OnceLock<TurnPins> = std::sync::OnceLock::new();
    PINS.get_or_init(Default::default)
}

#[cfg(test)]
fn turn_pins() -> &'static TurnPins {
    thread_local! {
        static PINS: &'static TurnPins = Box::leak(Box::default());
    }
    PINS.with(|p| *p)
}

fn fix_turn(session_id: &str, tab_pin: Option<&str>, scope: &ProfileScope) {
    if let Ok(mut pins) = turn_pins().lock() {
        pins.insert(
            session_id.to_string(),
            TurnPin { tab_pin: tab_pin.map(str::to_string), turn: scope.turn_pin() },
        );
    }
}

/// Start a turn in `session`: resolve its scope now and fix it for the turn.
pub fn begin_turn(settings: &Settings, session: &crate::session::Session) -> ProfileScope {
    let scope = ProfileScope::for_session(settings, session);
    fix_turn(&session.id, session.composio_profile.as_deref(), &scope);
    scope
}

/// The scope of the current turn in `session_id` (whose pin is now
/// `session_pin`): from the pin fixed at [`begin_turn`] while the tab's
/// pin is unchanged; after a repin, or with no turn started this run, the
/// tab's own pin (and a repinned turn is re-fixed to it).
pub fn turn_scope_for(settings: &Settings, session_id: &str, session_pin: Option<&str>, selected: &[String]) -> ProfileScope {
    let fixed = turn_pins().lock().ok().and_then(|p| p.get(session_id).cloned());
    match fixed {
        Some(t) if t.tab_pin.as_deref() == session_pin => match t.turn {
            Some(pin) => ProfileScope::resolve(settings, session_id, Some(&pin), selected),
            // The turn started with no profile: it stays without one.
            None => ProfileScope { home: None, reach: Vec::new(), source: ScopeSource::Missing { pinned_id: None } },
        },
        Some(_) => {
            let scope = ProfileScope::resolve(settings, session_id, session_pin, selected);
            fix_turn(session_id, session_pin, &scope);
            scope
        }
        None => ProfileScope::resolve(settings, session_id, session_pin, selected),
    }
}

/// [`turn_scope_for`] for a hydrated session.
pub fn turn_scope(settings: &Settings, session: &crate::session::Session) -> ProfileScope {
    turn_scope_for(settings, &session.id, session.composio_profile.as_deref(), &session.profile_reach)
}

/// `ctx` without any Composio server or toolkit: what a chat with no
/// profile to act in is offered (its Composio calls would be refused).
pub fn without_composio(mut ctx: McpContext) -> McpContext {
    ctx.servers.retain(|s| !is_composio_native(&s.name));
    ctx.connected_toolkit_slugs.clear();
    ctx
}

/// The profile a tab's data belongs to, for counting where data came from:
/// its [`ProfileScope`] home, else a removed pin's id (that company's data
/// is still in the conversation). Not a profile to act in — use
/// [`ProfileScope`] for that.
pub fn data_profile(settings: &Settings, session_pin: Option<&str>) -> Option<String> {
    ProfileScope::resolve(settings, "", session_pin, &[]).turn_pin()
}

/// The pin an ordinary tab should have, when it differs from its own: the
/// migration rule for tabs without a pin, and id normalisation for legacy
/// name pins. `None` = leave the pin as it is. Every id returned names a
/// profile that exists now.
///
/// A tab without a pin gets (a) the profile its own most recent Composio
/// call ran in (the call's `profile_id` stamp, else the profile its
/// `composio-native:<id|name>` server name names) — preferring a call made
/// at home over one routed to a ticked profile — else (b) Hobbes' home
/// profile, else (c) nothing — it stays unpinned. Never the global
/// profile: that follows focus, so it says nothing about this tab.
/// System sessions keep their own rules (the caller skips them).
pub fn pin_fix(settings: &Settings, session: &crate::session::Session) -> Option<String> {
    if let Some(pin) = session.composio_profile.as_deref().filter(|p| !p.is_empty()) {
        // A legacy name pin: its id. A removed pin stays as it is (Missing).
        return find_profile(settings, pin).filter(|p| p.id != pin).map(|p| p.id.clone());
    }
    // The tab's Composio calls, newest first, with the profile each ran in.
    let calls = || {
        session.messages.iter().rev().filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc) | MessageContent::PermissionRequest(tc) if is_composio_native(&tc.server_name) => {
                let profile = tc
                    .profile_id
                    .as_deref()
                    .and_then(|id| find_profile(settings, id))
                    .map(|p| p.id.clone())
                    .or_else(|| profile_for_server_name(settings, &tc.server_name).map(str::to_string))?;
                let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
                Some((profile, asked_profile(&tc.tool_name, &args).is_some()))
            }
            _ => None,
        })
    };
    // A call made at home (not routed to a ticked profile with
    // `hobbes_profile`) says where the tab lives; a routed one only if
    // there is no other.
    let at_home = calls().find(|(_, routed)| !routed).map(|(p, _)| p);
    at_home
        .or_else(|| calls().next().map(|(p, _)| p))
        .or_else(|| settings.home_profile_id().map(str::to_string))
}

/// The profiles `session_id` can act in, home first. Only the live Hobbes
/// session reaches beyond home in phase 1 — not its rotated-out archives, which are read-only; with no home profile there is
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

/// A hydrated session's reach, home first ([`ProfileScope::for_session`]).
pub fn reach_of(settings: &Settings, session: &crate::session::Session) -> Vec<ReachProfile> {
    ProfileScope::for_session(settings, session).reach
}

/// What a tab's profile chip shows, from the same resolution dispatch uses
/// ([`ProfileScope`]): the home profile's id (`None` when nothing resolves),
/// whether its reach can be edited here (an ordinary tab with a home), and
/// the names of the other profiles it reaches, and whether the tab's pin
/// names a removed profile (the chip warns; its Composio calls are refused).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipReach {
    pub home: Option<String>,
    pub editable: bool,
    pub extra: Vec<String>,
    pub pin_removed: bool,
}

pub fn chip_reach(settings: &Settings, session: &crate::session::Session) -> ChipReach {
    let scope = ProfileScope::for_session(settings, session);
    let pin_removed = scope.pin_removed();
    let reach = scope.reach;
    let home = reach.first().map(|h| h.id.clone());
    let editable = home.is_some() && !crate::system_sessions::is_system(&session.id);
    ChipReach {
        home,
        editable,
        extra: if editable { reach.into_iter().skip(1).map(|p| p.name).collect() } else { Vec::new() },
        pin_removed,
    }
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

/// The profile an approved call runs in on the resume path, from the tab's
/// [`ProfileScope`] (the same resolution the streaming path used to guard
/// it), always explicit: with no profile to act in a Composio call is
/// refused, never dispatched without one.
///
/// The profile a Composio call was stamped with at mint (`profile_id`: its
/// routed target, else the turn's home) is where it runs — `hobbes_profile`
/// argument or not — and only while that profile is still the tab's home or
/// in its reach; otherwise it is refused, never moved to the current home
/// (repinning between hold and approval would run a flow nobody was shown).
/// A call that is neither routed nor flow-held, in an ordinary tab without
/// reach, runs in the tab's current home — unless it was stamped with
/// another profile (minted before a repin), which is refused too. System sessions always
/// route by the stamp (not `reach.len()`: a system session whose other
/// profiles dropped out must still refuse a call routed to one of them; an
/// archive counts too, so a stale held call in it is refused rather than
/// run at home).
pub fn resume_profile_in(
    settings: &Settings,
    session_id: &str,
    session_profile: Option<&str>,
    selected: &[String],
    tool_call: &crate::components::shared::ToolCall,
) -> Result<Option<String>, String> {
    let scope = ProfileScope::resolve(settings, session_id, session_profile, selected);
    let home = scope.home_id().map(str::to_string);
    if !is_composio_native(&tool_call.server_name) {
        return Ok(home);
    }
    if let Some(refusal) = scope.missing_refusal() {
        return Err(refusal);
    }
    let reach = &scope.reach;
    let target = tool_call
        .profile_id
        .clone()
        .or_else(|| profile_for_server_name(settings, &tool_call.server_name).map(str::to_string));
    let name = || tool_call.server_name.trim_start_matches(COMPOSIO_NATIVE_PREFIX).trim_start_matches(':').to_string();
    if crate::system_sessions::is_system(session_id) {
        return match target {
            None => Ok(home),
            Some(t) if reach.iter().any(|p| p.id == t) => Ok(Some(t)),
            Some(_) => Err(format!(
                "Not run: the profile this was meant for ({}) is no longer available, and it won't be run in another company's account.",
                name()
            )),
        };
    }
    let args: Value = serde_json::from_str(&tool_call.arguments).unwrap_or(Value::Null);
    let routed = asked_profile(&tool_call.tool_name, &args).is_some();
    if reach.len() < 2 && !routed && tool_call.hold_rule_key.is_none() {
        // Minted in another profile (the tab was repinned since): refused,
        // never run in the company the tab moved to.
        return match tool_call.profile_id.as_deref() {
            Some(t) if find_profile(settings, t).map(|p| p.id.as_str()) != home.as_deref() => Err(format!(
                "Not run: this was asked for in another profile ({}) than this tab's current one, and it won't be run in another company's account.",
                settings.profile_name_for_id(t).unwrap_or(t)
            )),
            _ => Ok(home),
        };
    }
    match target {
        Some(t) if reach.iter().any(|p| p.id == t) => Ok(Some(t)),
        None if !routed => Ok(home),
        _ => Err(format!(
            "Not run: the profile this was meant for ({}) is no longer this tab's home or in its reach, and it won't be run in another company's account.",
            name()
        )),
    }
}

/// The summary sources a new chat seeded with `old`'s memory ("New chat
/// with memory") starts with: what `old`'s summary holds, its home, every
/// Composio result still in its messages and every read it carried — no
/// clock, since the copied summary may hold any of it.
pub fn inherited_sources(settings: &Settings, old: &crate::session::Session) -> Vec<String> {
    let mut out: BTreeSet<String> = summary_sources(old).into_iter().collect();
    out.extend(old.carried_reads.iter().map(|r| r.profile_id.clone()));
    out.extend(data_profile(settings, old.composio_profile.as_deref()));
    out.into_iter().collect()
}

/// Whether the cross-company hold applies to a call: the session has reach,
/// or it holds data read from a profile other than its current `home` —
/// through reach since narrowed, or at an earlier home before a repin. A
/// tab that has only ever used one profile is never checked.
pub fn flow_checked(reach: &[ReachProfile], read_from: &BTreeSet<String>) -> bool {
    match reach.first() {
        // No profile to act in: Composio calls are refused, but other
        // writes (dispatch, relay, terminal) can still carry data the
        // conversation read.
        None => !read_from.is_empty(),
        Some(home) => reach.len() >= 2 || read_from.iter().any(|id| id != &home.id),
    }
}

/// The profiles a session's next summary may hold data from: what its
/// current summary holds plus every Composio result still in its messages.
/// Profile ids where the call was stamped, else its server name (resolved
/// when read, by [`session_read_profiles`]).
pub fn summary_sources(session: &crate::session::Session) -> Vec<String> {
    let mut out: BTreeSet<String> =
        session.summary_sources.iter().cloned().collect();
    out.extend(tab_read_sources(&session.messages));
    for m in &session.messages {
        if let MessageContent::ToolCall(tc) = &m.content {
            if tc.status == ToolCallStatus::Completed && is_composio_native(&tc.server_name) {
                out.insert(tc.profile_id.clone().unwrap_or_else(|| tc.server_name.clone()));
            }
        }
    }
    out.into_iter().collect()
}

/// The profiles the completed `HOBBES_TAB_READ` calls in `messages` brought
/// in (`ToolCall::read_sources`, harness-stamped). Unlike an ordinary
/// Composio read in a system session, a tab read counts with no clock for
/// as long as its message is in the session: it is a deliberate import of
/// another conversation, kept verbatim in history.
pub fn tab_read_sources(messages: &[Message]) -> BTreeSet<String> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc) if tc.status == ToolCallStatus::Completed => Some(tc.read_sources.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// What a session's summary keeps holding after its messages are gone:
/// for an ordinary tab, [`summary_sources`]; for a system session, only its
/// stored summary sources and its tab reads ([`tab_read_sources`]) — its
/// ordinary Composio reads keep the [`READ_WINDOW`] clock. Fed into
/// `adopt_summary` and carried through rotation.
pub fn durable_summary_sources(session: &crate::session::Session) -> Vec<String> {
    if !crate::system_sessions::is_system(&session.id) {
        return summary_sources(session);
    }
    system_summary_sources(session)
}

/// [`durable_summary_sources`] for a system session: its stored summary
/// sources and its tab reads, never its clocked Composio reads. Rotation
/// uses it directly (only system sessions rotate).
pub fn system_summary_sources(session: &crate::session::Session) -> Vec<String> {
    let mut out: BTreeSet<String> = session.summary_sources.iter().cloned().collect();
    out.extend(tab_read_sources(&session.messages));
    out.into_iter().collect()
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

/// [`route_tool_call`] within a [`ProfileScope`]: with no profile to act in
/// ([`ScopeSource::Missing`]) a Composio call is refused with the scope's
/// reason, never left to the MCP manager to pick a client.
pub fn route_in_scope(scope: &ProfileScope, tool_call: &mut crate::components::shared::ToolCall) -> Route {
    match scope.missing_refusal() {
        Some(refusal) if is_composio_native(&tool_call.server_name) => Route::Refused(refusal),
        _ => route_tool_call(&scope.reach, tool_call),
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

/// Profiles whose data is in play in this session.
///
/// - **System sessions** (Heartbeat, Voice): Composio results read within
///   [`READ_WINDOW`] of `now` — across messages, since read data stays in
///   the model's context — plus reads a rotated session carried over from
///   the session it replaced (`Session::carried_reads`; the data may be in
///   the carried summary). Other conversations read with `HOBBES_TAB_READ`
///   count with no clock while their message stays
///   ([`tab_read_sources`]), and so does the summary that kept them
///   (`Session::summary_sources`, which for a system session holds only
///   tab-read sources, [`durable_summary_sources`]).
/// - **Ordinary tabs**: no clock. A tab keeps its history in context for
///   days, so every Composio result still in its messages counts, and so
///   does every profile its conversation summary holds data from
///   (`Session::summary_sources`), after the messages are gone, and every
///   read carried in from elsewhere (`carried_reads`).
pub fn session_read_profiles(
    settings: &Settings,
    session: &crate::session::Session,
    now: chrono::DateTime<chrono::Utc>,
) -> BTreeSet<String> {
    if !crate::system_sessions::is_system(&session.id) {
        let carried = session.carried_reads.iter().map(|r| r.profile_id.clone());
        return summary_sources(session)
            .into_iter()
            .filter_map(|s| {
                if s.starts_with(COMPOSIO_NATIVE_PREFIX) {
                    profile_for_server_name(settings, &s).map(str::to_string)
                } else {
                    // A stamped id — kept even if the profile was deleted
                    // since: its data is still in the conversation.
                    Some(s)
                }
            })
            .chain(carried)
            .collect();
    }
    let mut out: BTreeSet<String> =
        recent_reads(settings, &session.messages, now).into_iter().map(|r| r.profile_id).collect();
    out.extend(
        session
            .carried_reads
            .iter()
            .filter(|r| now - r.read_at <= READ_WINDOW)
            .map(|r| r.profile_id.clone()),
    );
    // Other conversations read in (no clock while the message stays), and
    // what the summary kept of them.
    out.extend(tab_read_sources(&session.messages));
    out.extend(session.summary_sources.iter().map(|s| {
        if s.starts_with(COMPOSIO_NATIVE_PREFIX) {
            profile_for_server_name(settings, s).map(str::to_string).unwrap_or_else(|| s.clone())
        } else {
            s.clone()
        }
    }));
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
#[cfg(test)]
pub fn cross_company_hold(
    target: Option<&ReachProfile>,
    home: &ReachProfile,
    read_from: &BTreeSet<String>,
    reach: &[ReachProfile],
    write: bool,
) -> Option<Hold> {
    cross_company_hold_in(target, Some(home), read_from, reach, write)
}

/// [`cross_company_hold`] for a chat that may have no home (its profile was
/// removed): every company it read from counts as a source, so a write
/// outside the connected accounts is still held.
pub fn cross_company_hold_in(
    target: Option<&ReachProfile>,
    home: Option<&ReachProfile>,
    read_from: &BTreeSet<String>,
    reach: &[ReachProfile],
    write: bool,
) -> Option<Hold> {
    if !write {
        return None;
    }
    let dest_id = target.or(home).map_or("", |t| t.id.as_str());
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
    match (target, home) {
        (Some(t), Some(home)) if t.id != home.id => Some(Hold {
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
        crate::system_sessions::set_current("reach-test-voice");
        let mut session = crate::session_events::blank_session();
        session.id = "reach-test-voice".into();
        session.messages = msgs;
        let read = session_read_profiles(&settings, &session, now);
        assert_eq!(read.into_iter().collect::<Vec<_>>(), ["cm", "pg"]);
        // A system session's reads age out of the window.
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
        crate::system_sessions::set_current("reach-test-voice");
        let settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        let mut held = call(json!({"hobbes_profile": "Puget"}));
        held.server_name = "composio-native:Puget".into();
        held.profile_id = Some("pg".into());
        // Still reachable: runs in Puget.
        assert_eq!(resume_profile_in(&settings, "reach-test-voice", Some("cm"), &[], &held), Ok(Some("pg".into())));
        // Puget lost its key before the approval: refused, not run at home.
        let broken = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", false)]);
        assert!(resume_profile_in(&broken, "reach-test-voice", Some("cm"), &[], &held).is_err());
        // An ordinary tab without reach: a call routed to Puget is refused,
        // and with no pin and no global profile there is nowhere to run it
        // (it used to dispatch with no profile, for the manager to pick).
        assert!(resume_profile_in(&settings, "a-normal-tab", Some("cm"), &[], &held).is_err());
        let refused = resume_profile_in(&settings, "a-normal-tab", None, &[], &held).unwrap_err();
        assert!(refused.contains("no Composio profile"), "{refused}");
    }

    #[test]
    fn an_unpinned_system_session_still_routes() {
        crate::system_sessions::set_current("reach-test-hb");
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let reach = ProfileScope::resolve(&settings, "reach-test-hb", None, &[]).reach;
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
        crate::system_sessions::set_current("reach-test-hb");
        assert_eq!(
            session_reach(&settings, "reach-test-hb", Some("cm"), &["gone".to_string()]),
            reach_for_session(&settings, "reach-test-hb", Some("cm"))
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
        // A held call resumes in the tab's home — explicitly, also for an
        // unpinned tab (the global profile; it used to dispatch with None).
        let held = call(json!({"to": "a@b.c"}));
        assert_eq!(resume_profile_in(&settings, "p2-plain", Some("cm"), &[], &held), Ok(Some("cm".into())));
        assert_eq!(resume_profile_in(&settings, "p2-plain", None, &[], &held), Ok(Some("cm".into())));
        // A removed pin: refused, never the global profile.
        let removed = resume_profile_in(&settings, "p2-plain", Some("gone"), &[], &held).unwrap_err();
        assert!(removed.contains("profile was removed"), "{removed}");
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
        let read_from = session_read_profiles(&settings, &session, now);
        assert!(read_from.contains("pg"));
        assert!(flow_checked(&reach, &read_from));
        let h = cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>cm");
        assert!(h.reason.contains("read Puget data"), "named although Puget left the reach");
    }

    // ── Review fixes ────────────────────────────────────────────────────

    fn tab(id: &str, home: &str, reach: &[&str]) -> crate::session::Session {
        let mut s = crate::session_events::blank_session();
        s.id = id.into();
        s.composio_profile = Some(home.into());
        s.profile_reach = reach.iter().map(|r| r.to_string()).collect();
        s
    }

    /// A read made at home, without `hobbes_profile` (stamped at mint).
    fn read_at_home(profile: &ReachProfile, at: chrono::DateTime<chrono::Utc>) -> Message {
        let mut m = read_via_reach(profile, at);
        if let MessageContent::ToolCall(tc) = &mut m.content {
            tc.arguments = json!({}).to_string();
        }
        m
    }

    /// 1. A held write with no `hobbes_profile` runs where it was stamped —
    /// never in a profile the tab was repinned to after the hold.
    #[test]
    fn a_repin_never_reroutes_a_held_write() {
        let settings = reach_settings();
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let session = tab("p2-repin-resume", "cm", &["pg"]);
        // Home Clearmirror, Puget ticked: a write with no argument routes home
        // (stamped cm) and, with Puget data read, is held as pg>cm.
        let mut held = call(json!({"to": "a@b.c"}));
        assert_eq!(route_tool_call(&reach_of(&settings, &session), &mut held), Route::To(cm.clone()));
        let read_from: BTreeSet<String> = ["cm".to_string(), "pg".to_string()].into();
        let hold = cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap();
        assert_eq!(hold.rule_key, "profile-flow:pg>cm");
        held.hold_rule_key = Some(hold.rule_key);
        // Approved while still home: runs in Clearmirror.
        assert_eq!(resume_profile_in(&settings, &session.id, Some("cm"), &["pg".into()], &held), Ok(Some("cm".into())));
        // "Make Puget home", then approve: refused, not run in Puget.
        assert!(resume_profile_in(&settings, &session.id, Some("pg"), &[], &held).is_err());
        // Still in reach after the repin: runs where it was shown, Clearmirror.
        assert_eq!(resume_profile_in(&settings, &session.id, Some("pg"), &["cm".into()], &held), Ok(Some("cm".into())));
        // An unheld call in a tab without reach, minted in Puget before a
        // repin to Clearmirror: refused (it used to run in the new home).
        let mut plain = call(json!({"to": "a@b.c"}));
        plain.profile_id = Some(pg.id.clone());
        assert!(resume_profile_in(&settings, "p2-repin-plain", Some("cm"), &[], &plain).is_err());
        // Minted at the current home: runs there.
        plain.profile_id = Some(cm.id.clone());
        assert_eq!(resume_profile_in(&settings, "p2-repin-plain", Some("cm"), &[], &plain), Ok(Some("cm".into())));
    }

    /// 2. Ordinary tabs keep foreign data in play with no clock; system
    /// sessions keep the window.
    #[test]
    fn an_ordinary_tab_reads_never_age_out() {
        let settings = reach_settings();
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let then = chrono::Utc::now() - chrono::Duration::minutes(150);
        let now = chrono::Utc::now();
        let mut session = tab("p2-no-clock", "cm", &["pg"]);
        session.messages.push(read_via_reach(&pg, then));
        let read_from = session_read_profiles(&settings, &session, now);
        assert!(read_from.contains("pg"), "a read 2.5 h old still counts in an ordinary tab");
        let h = cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>cm");

        // Summarised, then the messages are gone: the summary still counts.
        let (value, _) = crate::session::adopt_summary(&mut session, Default::default(), &[]);
        assert_eq!(value[crate::session::SUMMARY_SOURCES_KEY], json!(["pg"]), "journaled with the summary");
        assert!(
            serde_json::to_value(&session.active_context.conversation_summary).unwrap().get(crate::session::SUMMARY_SOURCES_KEY).is_none(),
            "kept off the summary the prompt renders"
        );
        session.messages.clear();
        assert_eq!(session_read_profiles(&settings, &session, now), BTreeSet::from(["pg".to_string()]));
        // The next summary keeps it (it was written from this one).
        let _ = crate::session::adopt_summary(&mut session, Default::default(), &[]);
        assert_eq!(session.summary_sources, ["pg"]);
        // A replay rebuild keeps it.
        let mut replayed = crate::session_events::blank_session();
        crate::session_events::apply_for_test(&mut replayed, &crate::session_events::SessionEvent::SummaryComputed { summary: value });
        assert_eq!(replayed.summary_sources, ["pg"]);
        // An undo that resets the summary clears it.
        let m = read_at_home(&cm, now);
        let id = m.id.to_string();
        session.messages.push(m);
        session.truncate_from_message(&id);
        assert!(session.summary_sources.is_empty());
        assert!(session_read_profiles(&settings, &session, now).is_empty());

        // A system session keeps the 2 h window exactly.
        crate::system_sessions::set_current("reach-test-voice");
        let mut voice = tab("reach-test-voice", "cm", &[]);
        voice.messages.push(read_via_reach(&pg, then));
        assert!(session_read_profiles(&settings, &voice, now).is_empty());
    }

    /// 3. A repin keeps the old home's data under the hold; a tab that only
    /// ever used one profile is unchanged.
    #[test]
    fn a_repin_keeps_the_flow_check() {
        let settings = reach_settings();
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let now = chrono::Utc::now();
        // Read Puget at home (no argument), then "Make Clearmirror home".
        let mut session = tab("p2-repin-flow", "pg", &[]);
        session.messages.push(read_at_home(&pg, now));
        session.composio_profile = Some("cm".into());
        let reach = reach_of(&settings, &session);
        assert_eq!(reach, vec![cm.clone()]);
        let read_from = session_read_profiles(&settings, &session, now);
        assert!(flow_checked(&reach, &read_from));
        let h = cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap();
        assert_eq!(h.rule_key, "profile-flow:pg>cm");

        // A one-profile tab: never checked.
        let mut single = tab("p2-single", "cm", &[]);
        single.messages.push(read_at_home(&cm, now));
        let reach = reach_of(&settings, &single);
        assert!(!flow_checked(&reach, &session_read_profiles(&settings, &single, now)));
        // No home at all (profile removed): checked once data was read
        // (it used to switch the guard off).
        assert!(flow_checked(&[], &BTreeSet::from(["pg".to_string()])));
    }

    /// 4. The chip's home is dispatch's home, not a display fallback.
    #[test]
    fn the_chip_shows_the_home_calls_run_in() {
        let settings = reach_settings();
        let chip = chip_reach(&settings, &tab("p2-chip", "cm", &["pg", "gone"]));
        assert_eq!(chip, ChipReach { home: Some("cm".into()), editable: true, extra: vec!["Puget".into()], pin_removed: false });
        // Pinned to a deleted profile: dispatch has no home, so neither does
        // the chip (not the global active profile), and reach can't be edited.
        let chip = chip_reach(&settings, &tab("p2-chip-gone", "gone", &["pg"]));
        // A removed pin: no home and the chip warns.
        assert_eq!(chip, ChipReach { home: None, editable: false, extra: vec![], pin_removed: true });
        // Unpinned: the global active profile, as dispatch.
        let mut unpinned = tab("p2-chip-unpinned", "x", &["pg"]);
        unpinned.composio_profile = None;
        assert_eq!(chip_reach(&settings, &unpinned).home.as_deref(), Some("cm"));
        // System sessions: no editing.
        crate::system_sessions::set_current("reach-test-hb");
        assert!(!chip_reach(&settings, &tab("reach-test-hb", "cm", &[])).editable);
    }

    /// A. "New chat with memory": the copied summary keeps the old tab's
    /// sources (its summary's, its reads and its home), journaled.
    #[test]
    fn a_new_chat_with_memory_keeps_the_sources() {
        let settings = reach_settings();
        let (cm, pg) = (p("cm", "Clearmirror"), p("pg", "Puget"));
        let now = chrono::Utc::now();
        let mut old = tab("p2-mem-old", "pg", &[]);
        old.summary_sources = vec!["ac".into()];
        old.messages.push(read_at_home(&pg, now - chrono::Duration::days(2)));
        let sources = inherited_sources(&settings, &old);
        assert_eq!(sources, ["ac", "pg"]);
        // The new tab (no messages) starts at Clearmirror.
        let mut fresh = tab("p2-mem-new", "cm", &[]);
        fresh.summary_sources = sources;
        let read_from = session_read_profiles(&settings, &fresh, now);
        assert!(read_from.contains("pg"));
        assert!(flow_checked(&reach_of(&settings, &fresh), &read_from));
        assert_eq!(
            cross_company_hold(Some(&cm), &cm, &read_from, &all_profiles(&settings), true).unwrap().rule_key,
            "profile-flow:ac+pg>cm"
        );
        // Journaled with the seeded summary, so a replay keeps it.
        let payload = crate::session::summary_payload(&fresh.active_context.conversation_summary, &fresh.summary_sources);
        let mut replayed = crate::session_events::blank_session();
        crate::session_events::apply_for_test(&mut replayed, &crate::session_events::SessionEvent::SummaryComputed { summary: payload });
        assert_eq!(replayed.summary_sources, fresh.summary_sources);
    }

    /// B. An undo while the summary is being written doesn't drop the
    /// sources of the data it was written from.
    #[test]
    fn an_undo_during_summarising_keeps_the_sources() {
        let settings = reach_settings();
        let pg = p("pg", "Puget");
        let now = chrono::Utc::now();
        let mut session = tab("p2-undo-inflight", "cm", &[]);
        session.messages.push(read_at_home(&pg, now));
        // Snapshot taken before the LLM call…
        let generated_from = summary_sources(&session);
        // …the user undoes past the read while it is in flight…
        let id = session.messages[0].id.to_string();
        session.truncate_from_message(&id);
        assert!(session_read_profiles(&settings, &session, now).is_empty());
        // …and the summary of that read arrives.
        let _ = crate::session::adopt_summary(&mut session, Default::default(), &generated_from);
        assert_eq!(session.summary_sources, ["pg"]);
        assert!(session_read_profiles(&settings, &session, now).contains("pg"));
    }

    /// Carried reads have no clock in an ordinary tab either.
    #[test]
    fn carried_reads_never_age_out_in_an_ordinary_tab() {
        let settings = reach_settings();
        let now = chrono::Utc::now();
        let mut session = tab("p2-carried", "cm", &[]);
        session.carried_reads =
            vec![crate::session::CarriedRead { profile_id: "pg".into(), read_at: now - chrono::Duration::hours(3) }];
        assert!(session_read_profiles(&settings, &session, now).contains("pg"));
    }

    /// 5. Repinning to a ticked profile takes it out of the reach list.
    #[test]
    fn a_new_home_leaves_the_reach_list() {
        let mut session = tab("p2-new-home", "cm", &["pg", "ac"]);
        assert_eq!(session.drop_home_from_reach(), None, "home wasn't ticked");
        session.composio_profile = Some("pg".into());
        assert_eq!(session.drop_home_from_reach(), Some(vec!["ac".to_string()]));
        assert_eq!(session.profile_reach, ["ac"]);
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

    /// The tab states the resolver must cover, with the rule every site
    /// used before (`home_profile`: the pin passed through unvalidated, else
    /// the validated global) beside the scope. Rows where the two differ are
    /// the behaviour changes the call-site commits made.
    #[test]
    fn profile_scope_table() {
        let home_profile = |settings: &Settings, pin: Option<&str>| {
            pin.map(str::to_string).or_else(|| settings.get_active_profile().map(|p| p.id.clone()))
        };
        crate::system_sessions::set_current("scope-test-hobbes");
        let base = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        let with_global = |g: Option<&str>| {
            let mut s = base.clone();
            s.active_composio_profile = g.map(str::to_string);
            s
        };
        let cm = Some(p("cm", "Clearmirror"));
        let pg = Some(p("pg", "Puget"));
        struct Row {
            what: &'static str,
            pin: Option<&'static str>,
            global: Option<&'static str>,
            legacy_home: Option<&'static str>,
            home: Option<ReachProfile>,
            source: ScopeSource,
        }
        let rows = [
            Row { what: "pinned, exists", pin: Some("pg"), global: Some("cm"), legacy_home: Some("pg"), home: pg.clone(), source: ScopeSource::Pinned },
            Row {
                what: "pinned, removed; global valid",
                pin: Some("gone"),
                global: Some("cm"),
                // Legacy: the dead id was treated as home (no reach, and
                // dispatch passed it through); never the global profile.
                legacy_home: Some("gone"),
                home: None,
                source: ScopeSource::Missing { pinned_id: Some("gone".into()) },
            },
            Row { what: "pinned (empty string)", pin: Some(""), global: Some("cm"), legacy_home: Some(""), home: cm.clone(), source: ScopeSource::Global },
            Row { what: "unpinned, global valid", pin: None, global: Some("cm"), legacy_home: Some("cm"), home: cm.clone(), source: ScopeSource::Global },
            Row { what: "unpinned, global dangling", pin: None, global: Some("gone"), legacy_home: None, home: None, source: ScopeSource::Missing { pinned_id: None } },
            Row { what: "unpinned, no global", pin: None, global: None, legacy_home: None, home: None, source: ScopeSource::Missing { pinned_id: None } },
        ];
        for r in rows {
            let settings = with_global(r.global);
            assert_eq!(home_profile(&settings, r.pin).as_deref(), r.legacy_home, "legacy: {}", r.what);
            let scope = ProfileScope::resolve(&settings, "plain-tab", r.pin, &[]);
            assert_eq!(scope.home, r.home, "home: {}", r.what);
            assert_eq!(scope.source, r.source, "source: {}", r.what);
            // Reach is anchored on home: none without one.
            assert_eq!(scope.reach.first(), scope.home.as_ref(), "reach: {}", r.what);
            let missing = matches!(scope.source, ScopeSource::Missing { .. });
            assert_eq!(missing, scope.home.is_none(), "missing: {}", r.what);
            assert_eq!(scope.missing_refusal().is_some(), missing, "refusal: {}", r.what);
        }
    }

    #[test]
    fn profile_scope_reach_and_refusals() {
        crate::system_sessions::set_current("scope-test-hobbes");
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true), ("ac", "Acme", false)]);
        settings.active_composio_profile = Some("cm".into());
        // An ordinary tab: home plus the ticked profiles that are set up.
        let s = ProfileScope::resolve(&settings, "plain-tab", Some("cm"), &["pg".into(), "ac".into()]);
        assert_eq!(s.reach, vec![p("cm", "Clearmirror"), p("pg", "Puget")]);
        // The Hobbes session reaches every set-up profile, ticks or not.
        let h = ProfileScope::resolve(&settings, "scope-test-hobbes", Some("pg"), &[]);
        assert_eq!(h.reach, vec![p("pg", "Puget"), p("cm", "Clearmirror")]);
        // A removed pin: no reach even with ticks, and a refusal naming the chip.
        let gone = ProfileScope::resolve(&settings, "plain-tab", Some("gone"), &["pg".into()]);
        assert!(gone.reach.is_empty());
        assert!(gone.pin_removed());
        assert!(gone.missing_refusal().unwrap().contains("profile was removed"));
        let none = ProfileScope::resolve(&Settings::default(), "plain-tab", None, &[]);
        assert!(!none.pin_removed());
        assert!(none.missing_refusal().unwrap().contains("no Composio profile"));
    }

    /// The summarizer's profile note follows the tab, not the focused tab's
    /// (global) profile — it used to name the global profile always.
    #[test]
    fn summary_note_names_the_tabs_own_profile() {
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let mut s = tab("plain-tab", "pg", &[]);
        assert_eq!(profile_note_name(&settings, &s), "Puget");
        s.composio_profile = None;
        assert_eq!(profile_note_name(&settings, &s), "Clearmirror");
        s.composio_profile = Some("gone".into());
        assert_eq!(profile_note_name(&settings, &s), "None");
    }

    /// Streaming dispatch: a tab whose profile doesn't resolve refuses its
    /// Composio calls (it used to dispatch with the dead pin, or with no
    /// profile at all for the manager to pick); other tools run.
    #[test]
    fn a_tab_without_a_profile_refuses_composio_calls() {
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let gone = ProfileScope::resolve(&settings, "plain-tab", Some("gone"), &[]);
        let mut c = call(json!({}));
        match route_in_scope(&gone, &mut c) {
            Route::Refused(m) => assert!(m.contains("profile was removed"), "{m}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let mut terminal = call(json!({}));
        terminal.server_name = "terminal".into();
        assert_eq!(route_in_scope(&gone, &mut terminal), Route::Unchanged);
        // Unpinned with a valid global profile: home is the global one.
        let global = ProfileScope::resolve(&settings, "plain-tab", None, &[]);
        assert_eq!(global.home_id(), Some("cm"));
        assert_eq!(global.turn_pin().as_deref(), Some("cm"));
        assert_eq!(route_in_scope(&global, &mut call(json!({}))), Route::Unchanged);
        // A removed pin stays removed when the turn resolves it again.
        assert_eq!(gone.turn_pin().as_deref(), Some("gone"));
    }

    /// A tab whose profile was removed still holds data it read: the guard
    /// stays on for writes outside the connected accounts.
    #[test]
    fn a_tab_without_a_profile_keeps_the_flow_guard() {
        let read_pg: BTreeSet<String> = ["pg".to_string()].into();
        assert!(flow_checked(&[], &read_pg));
        assert!(!flow_checked(&[], &BTreeSet::new()));
        let named = vec![p("pg", "Puget")];
        let hold = cross_company_hold_in(None, None, &read_pg, &named, true).expect("held");
        assert_eq!(hold.rule_key, "profile-flow:pg>local");
        assert!(hold.reason.contains("read Puget data"));
        assert!(cross_company_hold_in(None, None, &read_pg, &named, false).is_none());
    }

    /// A turn keeps the profile it started in while the global profile
    /// moves with focus; a turn that started with no profile stays so.
    #[test]
    fn a_turn_keeps_its_profile_across_rounds() {
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let s = tab("turn-tab", "pg", &[]);
        assert_eq!(turn_scope(&settings, &s).home_id(), Some("pg"), "no turn yet: the tab's pin");
        begin_turn(&settings, &s);
        // An unpinned tab keeps the global profile it started in while
        // focus moves the global profile.
        let mut unpinned = tab("turn-unpinned", "x", &[]);
        unpinned.composio_profile = None;
        begin_turn(&settings, &unpinned);
        settings.active_composio_profile = Some("pg".into());
        assert_eq!(turn_scope(&settings, &unpinned).home_id(), Some("cm"));
        let mut none = tab("turn-none", "x", &[]);
        none.composio_profile = None;
        let empty = Settings::default();
        begin_turn(&empty, &none);
        // A global profile appearing mid-turn isn't picked up.
        assert!(turn_scope(&settings, &none).home.is_none());
        let ctx = without_composio(ctx("Clearmirror", &["A"]));
        assert!(natives(&ctx).is_empty());
        assert!(ctx.connected_toolkit_slugs.is_empty());
        assert_eq!(ctx.servers.len(), 1, "other servers stay");
    }

    /// A repin mid-turn moves the turn's later rounds (continuations after
    /// an approval) to the new home, while a held call minted in the old
    /// profile is still refused on resume.
    #[test]
    fn a_repin_mid_turn_moves_the_turn_and_refuses_the_old_call() {
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("cm".into());
        let mut s = tab("repin-turn", "pg", &[]);
        begin_turn(&settings, &s);
        let mut held = call(json!({"to": "a@b.c"}));
        held.profile_id = Some("pg".into());
        // Repinned on the chip while the call waits for approval.
        s.composio_profile = Some("cm".into());
        let next = turn_scope(&settings, &s);
        assert_eq!(next.home_id(), Some("cm"), "the next continuation acts in the new home");
        assert_eq!(turn_scope_for(&settings, &s.id, Some("cm"), &[]).home_id(), Some("cm"), "re-fixed");
        assert!(resume_profile_in(&settings, &s.id, Some("cm"), &[], &held).is_err());
        // A repin from no pin (the migration) re-resolves too.
        let mut m = tab("repin-migrated", "x", &[]);
        m.composio_profile = None;
        begin_turn(&settings, &m);
        m.composio_profile = Some("pg".into());
        assert_eq!(turn_scope(&settings, &m).home_id(), Some("pg"));
    }

    #[test]
    fn the_prompt_names_home_and_the_boundary() {
        let i = reach_instruction("Clearmirror", &["Puget", "Acme"]);
        assert!(i.contains("home profile is 'Clearmirror'"));
        assert!(i.contains("Puget, Acme"));
        assert!(i.contains("different company"));
    }

    #[test]
    fn pin_fix_follows_the_tabs_own_calls_then_hobbes_home() {
        let mut settings = settings_with(&[("cm", "Clearmirror", true), ("pg", "Puget", true)]);
        settings.active_composio_profile = Some("pg".into());
        let now = chrono::Utc::now();
        let mut s = crate::session_events::blank_session();
        s.id = "fix-tab".into();
        // No calls, no Hobbes home: stays unpinned — never the (focus-driven) global.
        assert_eq!(pin_fix(&settings, &s), None);
        settings.home_composio_profile = Some("cm".into());
        assert_eq!(pin_fix(&settings, &s), Some("cm".into()));
        // Its own calls win, the most recent one first; a server name counts
        // when the call carries no stamp.
        let mut by_name = read_via_reach(&p("pg", "Puget"), now);
        if let MessageContent::ToolCall(tc) = &mut by_name.content {
            tc.profile_id = None;
        }
        s.messages.push(read_at_home(&p("cm", "Clearmirror"), now));
        s.messages.push(by_name);
        // The newest call was routed to Puget (a ticked profile): the call
        // made at home, Clearmirror, says where the tab lives.
        assert_eq!(pin_fix(&settings, &s), Some("cm".into()));
        let mut at_home_pg = read_at_home(&p("pg", "Puget"), now);
        if let MessageContent::ToolCall(tc) = &mut at_home_pg.content {
            tc.profile_id = None;
        }
        s.messages.push(at_home_pg);
        assert_eq!(pin_fix(&settings, &s), Some("pg".into()));
        // Only routed calls: the newest of them.
        let mut routed_only = crate::session_events::blank_session();
        routed_only.messages.push(read_via_reach(&p("pg", "Puget"), now));
        assert_eq!(pin_fix(&settings, &routed_only), Some("pg".into()));
        // A stamp naming a removed profile is skipped.
        let mut gone = read_via_reach(&p("gone", "Gone"), now);
        if let MessageContent::ToolCall(tc) = &mut gone.content {
            tc.server_name = "composio-native:Gone".into();
        }
        s.messages.push(gone);
        assert_eq!(pin_fix(&settings, &s), Some("pg".into()));
        // A legacy name pin becomes its id; an id pin, or a removed one, is left.
        s.composio_profile = Some("Puget".into());
        assert_eq!(pin_fix(&settings, &s), Some("pg".into()));
        s.composio_profile = Some("pg".into());
        assert_eq!(pin_fix(&settings, &s), None);
        s.composio_profile = Some("gone".into());
        assert_eq!(pin_fix(&settings, &s), None);
        // A name pin resolves in the scope too, to the id.
        let scope = ProfileScope::resolve(&settings, "fix-tab", Some("Puget"), &[]);
        assert_eq!(scope.home_id(), Some("pg"));
        assert_eq!(data_profile(&settings, Some("Puget")).as_deref(), Some("pg"));
    }
}
