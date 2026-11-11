//! The cross-company flow guard: rules only, no model calls.
//!
//! Each Composio profile is a company. A chat that has read one company's
//! data must not quietly carry it into another company's accounts. Every
//! tool call is judged by three questions:
//!
//! 1. **What does the call do?** [`classify_flow`] sorts it into
//!    [`FlowClass::Read`] (fetches, lists, searches — never held),
//!    [`FlowClass::Act`] (changes state but carries no content: label,
//!    archive, mark read, move, trash) or [`FlowClass::Carry`] (puts
//!    content somewhere: send, draft, create, post, upload, write …). The
//!    leading verb of the tool name decides (the first word after the
//!    toolkit prefix that is a listed verb). A read verb joined to a write
//!    (`FIND_REPLACE`, `GET_OR_CREATE`) takes the write. Noun markers
//!    (`…_EVENTS_INSTANCES`, `…_CONVERSATIONS_HISTORY`) make a name a read
//!    only as its last word, with no verb anywhere and only plural nouns
//!    before it. An edit (add, update, patch, set …) is a Carry unless it is
//!    positively content-free (a label, star, reaction, status … object, or
//!    arguments that are all ids, times and short enum values); any other
//!    Act whose arguments hold free text is a Carry. `COMPOSIO_EXECUTE_TOOL`
//!    is judged by exactly what the dispatcher runs ([`execute_target`]).
//!    Anything unrecognised is a Carry (fail closed). The agent-time scope
//!    keeps its own, stricter test (`todo::agent_time::agent_time_scope`).
//! 2. **Is it held?** [`flow_hold`]: only calls into a Composio profile are
//!    judged (terminal, files, browser, other MCP servers, dispatch and
//!    relay messages are left to the normal permission flow and the call
//!    risk gate; a dispatch or relay hands the sender's read sources to the
//!    receiving chat instead, `hobbes_tabs::carry_sender_reads`). A Carry
//!    into profile T is held unless every pair (source → T) is allowed,
//!    where the sources are the profiles the chat has read
//!    (`profile_reach::session_read_profiles`) other than T. On a turn
//!    nobody is present for (a Heartbeat wake, or a fired timer, relay or
//!    returned work in any chat, [`flow_person`]) any non-Read call into a
//!    profile other than the chat's home is held as well.
//! 3. **What is remembered?** Pairs, not tools ([`FlowPolicy`]): a pair is
//!    allowed when either profile is personal (`ComposioProfile::personal`),
//!    when it is on the always-allowed list (`PermissionSettings::
//!    profile_flows`), or when it was allowed for this chat
//!    (`Session::allowed_flows`). Flow holds never consult or create trust
//!    rules; the rules the old guard wrote (`profile-flow:` servers) are
//!    dropped when settings load. A Carry let through by an allowance is
//!    logged as a trust decision ([`allowed_carry_key`]).
//!
//! A held call carries its pairs in `ToolCall::hold_rule_key`
//! ([`pair_key`], parsed back by [`parse_held_flow`]) so the approval bubble
//! can offer "Allow in this chat" and "Always allow" for exactly those
//! pairs (and release sibling calls of the same round, minted on the
//! person's turn, that those pairs now cover, [`released_by_allowance`]).
//! A hold on a turn nobody is present for carries no pairs: the person
//! approves that one call. Keys in the old format (`…>local`, `act>…`)
//! parse to nothing and are approved or denied once.

use std::collections::BTreeSet;

use serde_json::Value;

use super::profile_reach::ReachProfile;
use crate::context::permissions::ProfileFlow;
use crate::settings::Settings;
use crate::todo::agent_time::name_tokens;

/// Trust-rule server prefix the old guard remembered flows under (dropped
/// at load, `Settings::drop_legacy_flow_trust_rules`), and the prefix of
/// every flow hold key.
pub const FLOW_KEY_PREFIX: &str = "profile-flow:";

/// The prefix of a held call's pair key ([`pair_key`]).
pub const PAIR_KEY_PREFIX: &str = "profile-flow:v2:";

/// The meta tool that runs another Composio tool by name.
pub const EXECUTE_TOOL: &str = "COMPOSIO_EXECUTE_TOOL";

/// Hobbes' discovery / loading meta tools (`composio_client::meta`,
/// `hobbes-meta`): they read tool lists only. Any other meta tool is judged
/// by its name like an ordinary tool (`COMPOSIO_MULTI_EXECUTE_TOOL` → Carry).
const DISCOVERY_TOOLS: &[&str] = &[
    "COMPOSIO_DISCOVER_APPS",
    "COMPOSIO_GET_APP_TOOLS",
    "COMPOSIO_CLEAR_TOOLS",
    "MCP_LOAD_SERVER_TOOLS",
    "MCP_UNLOAD_SERVER_TOOLS",
];

/// What a call does with content, mildest first (so `max` is the strictest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlowClass {
    /// Only reads: never held.
    Read,
    /// Changes state without carrying content (label, archive, move, trash).
    Act,
    /// Puts content somewhere (send, draft, create, post, write, upload …).
    Carry,
}

/// Verbs that only read, wherever they lead the name.
const READ_VERBS: &[&str] = &[
    "get", "fetch", "list", "search", "find", "read", "query", "lookup", "retrieve", "download", "export",
    "describe", "browse", "scan", "inspect", "preview", "poll", "discover", "explore", "whoami",
];

/// Read verbs that are as often nouns: a read only as the first word after
/// the toolkit prefix (`GITHUB_VIEW_…`, never `…_OPEN_A_VIEW`).
const WEAK_READ_VERBS: &[&str] = &["view", "check", "count", "show", "look"];

/// Nouns that name a read (`GOOGLECALENDAR_EVENTS_INSTANCES`,
/// `SLACK_CONVERSATIONS_HISTORY`), never verbs: see [`marker_read`].
const READ_MARKERS: &[&str] = &["history", "info", "details", "stats", "instances", "view", "views"];

/// Verbs that put content somewhere.
const CARRY_VERBS: &[&str] = &[
    "send", "draft", "create", "post", "reply", "respond", "forward", "comment", "upload", "write", "insert",
    "append", "share", "publish", "submit", "invite", "compose", "attach", "import", "copy", "duplicate", "put",
    "upsert", "replace", "save", "store", "push", "commit", "schedule", "book", "broadcast", "notify", "tweet",
    "retweet", "new", "make", "generate", "fill", "translate", "run", "execute", "exec", "call", "invoke",
    "trigger", "dispatch", "transfer",
];

/// Verbs that change state without carrying content of their own.
const ACT_VERBS: &[&str] = &[
    "label", "unlabel", "archive", "unarchive", "mark", "star", "unstar", "flag", "unflag", "move", "trash",
    "untrash", "delete", "remove", "mute", "unmute", "pin", "unpin", "snooze", "close", "reopen", "resolve",
    "assign", "unassign", "accept", "decline", "reject", "cancel", "complete", "enable", "disable", "approve",
    "lock", "unlock", "stop", "start", "subscribe", "unsubscribe", "follow", "unfollow", "react", "join",
    "leave", "restore", "toggle", "clear", "merge", "kick", "ban", "unban", "block", "unblock", "hide",
    "unhide", "empty", "purge", "erase", "destroy", "drop",
];

/// Edit verbs: a Carry unless positively content-free ([`edit_class`]).
const EDIT_VERBS: &[&str] = &["add", "update", "edit", "patch", "modify", "set", "change", "rename"];

/// Writes that, joined to a read verb (`FIND_REPLACE`, `FIND_AND_UPDATE`,
/// `GET_OR_CREATE`), make the name that write. Only unambiguous verbs: a
/// noun-like one (`GET_LABEL`, `GET_COMMIT`, `GET_DRAFT`) stays a read.
const JOINED_WRITES: &[&str] = &[
    "replace", "create", "update", "delete", "remove", "write", "insert", "append", "upsert", "modify", "edit",
    "add", "move", "trash",
];

/// Objects an edit can change without carrying content.
const CONTENT_FREE_OBJECTS: &[&str] = &[
    "label", "labels", "star", "stars", "reaction", "reactions", "emoji", "status", "flag", "flags", "read",
    "unread", "assignee", "assignees", "tag", "tags", "priority", "important", "pin", "pins",
];

/// Objects whose edit always writes content (`…_UPDATE_DOCUMENT…`,
/// `…_ADD_COMMENT…`), whatever the arguments look like.
const CONTENT_OBJECTS: &[&str] = &[
    "comment", "comments", "note", "notes", "reply", "replies", "document", "documents", "doc", "docs", "page",
    "pages", "content", "contents", "text", "body", "block", "blocks", "paragraph", "cell", "cells", "value",
    "values", "row", "rows", "sheet", "spreadsheet", "markdown", "file", "files", "description", "attachment",
    "attachments", "post", "posts", "wiki", "section", "slide", "slides", "html", "data", "record", "records",
];

/// Words that end a verb's object (`ADD_LABEL_TO_EMAIL`: the object is the label).
const OBJECT_STOPS: &[&str] = &["to", "from", "in", "into", "on", "onto", "for", "by", "with", "of", "at"];

/// Argument keys (judged by their last word) that hold free text.
const TEXT_KEYS: &[&str] = &[
    "body", "content", "contents", "description", "text", "message", "markdown", "html", "comment", "note",
    "notes", "summary", "caption", "subject", "title", "snippet", "reply",
];

/// Argument keys (last word) whose values identify rather than say anything.
const ID_KEYS: &[&str] = &["id", "ids", "url", "urls", "uri", "link", "token", "slug", "uuid", "sha", "etag", "ref"];

/// Argument keys (last word) holding times and dates.
const TIME_KEYS: &[&str] = &[
    "time", "date", "datetime", "start", "end", "due", "timezone", "zone", "tz", "at", "when", "duration",
    "until", "since", "timestamp", "hour", "hours", "minute", "minutes", "seconds", "day", "days",
];

/// Argument keys (last word) holding short enum-style values.
const ENUM_KEYS: &[&str] = &[
    "status", "state", "type", "kind", "color", "colour", "priority", "visibility", "mode", "role", "label",
    "labels", "flag", "flags", "starred", "read", "unread", "important", "archived", "pinned", "done",
    "completed", "position", "index", "order", "transparency",
];

/// A string at least this long is free text whatever its key.
const LONG_TEXT: usize = 200;

/// The last word of an argument key, lowercased (`addLabelIds` → `ids`).
fn key_word(key: &str) -> String {
    name_tokens(key).pop().unwrap_or_default()
}

/// Whether `args` hold free text: a string under a body/content/description-
/// style key (its last word in [`TEXT_KEYS`]), or any string of at least
/// [`LONG_TEXT`] characters outside id keys.
pub fn has_free_text(args: &Value) -> bool {
    fn walk(v: &Value, key: Option<&str>) -> bool {
        let last = key.map(key_word).unwrap_or_default();
        match v {
            Value::String(s) => {
                let s = s.trim();
                (!s.is_empty() && TEXT_KEYS.contains(&last.as_str()))
                    || (!ID_KEYS.contains(&last.as_str()) && s.chars().count() >= LONG_TEXT)
            }
            Value::Array(items) => items.iter().any(|i| walk(i, key)),
            Value::Object(map) => map.iter().any(|(k, v)| walk(v, Some(k))),
            _ => false,
        }
    }
    walk(args, None)
}

/// Whether every value in `args` is positively content-free: ids, times and
/// dates, and short enum-style values (no spaces, at most 40 characters)
/// under status / type / label-style keys. Anything under any other key —
/// a name, a cell value, a property — is content.
pub fn content_free_args(args: &Value) -> bool {
    fn ok(v: &Value, key: &str) -> bool {
        match v {
            Value::Null | Value::Bool(_) => true,
            Value::Array(items) => items.iter().all(|i| ok(i, key)),
            Value::Object(map) => map.iter().all(|(k, v)| ok(v, k)),
            Value::Number(_) | Value::String(_) => {
                let w = key_word(key);
                let w = w.as_str();
                let text = v.as_str().map(str::trim);
                if ID_KEYS.contains(&w) {
                    text.is_none_or(|s| s.chars().count() < LONG_TEXT)
                } else if TIME_KEYS.contains(&w) {
                    text.is_none_or(|s| s.chars().count() <= 64)
                } else if ENUM_KEYS.contains(&w) {
                    text.is_none_or(|s| s.chars().count() <= 40 && !s.contains(char::is_whitespace))
                } else {
                    false
                }
            }
        }
    }
    match args {
        Value::Object(map) => map.iter().all(|(k, v)| ok(v, k)),
        Value::Null => true,
        _ => false,
    }
}

/// **What `COMPOSIO_EXECUTE_TOOL` runs**: its `tool_name` and `arguments`
/// (an empty object when absent) — the one reading both the dispatcher
/// (`mcp::manager::tool_call`) and the classifier use, so what is judged is
/// what runs. `None` when `tool_name` isn't a string (the dispatcher
/// refuses the call).
pub fn execute_target(args: &Value) -> Option<(&str, Value)> {
    let name = args.get("tool_name")?.as_str()?;
    let inner = args.get("arguments").cloned().unwrap_or_else(|| Value::Object(Default::default()));
    Some((name, inner))
}

/// The class of the verb `tokens[at]` (with `tokens` the name's words after
/// the toolkit prefix) for a call with `args`.
fn verb_class(tokens: &[String], at: usize, args: &Value) -> FlowClass {
    let verb = tokens[at].as_str();
    if CARRY_VERBS.contains(&verb) {
        FlowClass::Carry
    } else if EDIT_VERBS.contains(&verb) {
        edit_class(&tokens[at + 1..], args)
    } else if ACT_VERBS.contains(&verb) {
        if has_free_text(args) {
            FlowClass::Carry
        } else {
            FlowClass::Act
        }
    } else {
        FlowClass::Read
    }
}

/// An edit is a Carry unless it carries no free text, its object (the
/// words before the first preposition) is no content object, and either
/// that object is content-free (a label, star, reaction …) or every
/// argument is ([`content_free_args`]).
fn edit_class(after: &[String], args: &Value) -> FlowClass {
    let object: Vec<&str> =
        after.iter().map(String::as_str).take_while(|t| !OBJECT_STOPS.contains(t)).collect();
    if object.iter().any(|t| CONTENT_OBJECTS.contains(t)) {
        return FlowClass::Carry;
    }
    let object_free = object.iter().any(|t| CONTENT_FREE_OBJECTS.contains(t));
    if !has_free_text(args) && (object_free || content_free_args(args)) {
        FlowClass::Act
    } else {
        FlowClass::Carry
    }
}

/// Whether `t` is a verb candidate at position `at` of the name's words
/// after the prefix.
fn is_candidate(t: &str, at: usize) -> bool {
    READ_VERBS.contains(&t)
        || (at == 0 && WEAK_READ_VERBS.contains(&t))
        || CARRY_VERBS.contains(&t)
        || ACT_VERBS.contains(&t)
        || EDIT_VERBS.contains(&t)
}

/// A name with no verb is a read only when its last word is a read marker
/// and every word before it (after the prefix) is a plural noun
/// (`EVENTS_INSTANCES`, `CONVERSATIONS_HISTORY`); `SYNC_HISTORY` or
/// `OPEN_A_VIEW` may hide an unlisted verb, so they fail closed.
fn marker_read(body: &[String]) -> bool {
    let Some((last, before)) = body.split_last() else { return false };
    READ_MARKERS.contains(&last.as_str())
        && before.iter().all(|t| t.len() > 2 && t.ends_with('s') && !t.ends_with("ss"))
}

/// **Classify a call** by what it does with content (see the module doc).
/// `args` should already be without `hobbes_profile`
/// (`profile_reach::strip_profile_arg`).
pub fn classify_flow(tool_name: &str, args: &Value) -> FlowClass {
    classify_at(tool_name, args, 0)
}

fn classify_at(tool_name: &str, args: &Value, depth: u8) -> FlowClass {
    if depth > 3 {
        return FlowClass::Carry;
    }
    if DISCOVERY_TOOLS.contains(&tool_name) {
        return FlowClass::Read;
    }
    if tool_name == EXECUTE_TOOL {
        let Some((inner, inner_args)) = execute_target(args) else {
            return FlowClass::Carry;
        };
        // A JSON string is judged as the object it holds; anything else
        // that isn't an object can't be read.
        let inner_args = match inner_args {
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v @ Value::Object(_)) => v,
                _ => return FlowClass::Carry,
            },
            v @ (Value::Object(_) | Value::Null) => v,
            _ => return FlowClass::Carry,
        };
        return classify_at(inner.trim(), &inner_args, depth + 1);
    }
    let tokens = name_tokens(tool_name);
    let body: &[String] = if tokens.len() > 1 { &tokens[1..] } else { &tokens };
    let Some(at) = body.iter().enumerate().position(|(i, t)| is_candidate(t, i)) else {
        return if marker_read(body) { FlowClass::Read } else { FlowClass::Carry };
    };
    match verb_class(body, at, args) {
        FlowClass::Read => {
            // A read joined to a write takes the write (`FIND_REPLACE`,
            // `FIND_AND_UPDATE`, `GET_OR_CREATE`).
            let mut next = at + 1;
            if body.get(next).is_some_and(|t| t == "and" || t == "or") {
                next += 1;
            }
            match body.get(next) {
                Some(t) if JOINED_WRITES.contains(&t.as_str()) => verb_class(body, next, args),
                _ => FlowClass::Read,
            }
        }
        other => other,
    }
}

/// The pairs a chat may move content along without asking.
pub struct FlowPolicy<'a> {
    personal: BTreeSet<&'a str>,
    always: &'a [ProfileFlow],
    chat: &'a [ProfileFlow],
}

impl<'a> FlowPolicy<'a> {
    /// The settings' personal profiles and always-allowed pairs, plus the
    /// pairs allowed for this chat (`Session::allowed_flows`).
    pub fn new(settings: &'a Settings, chat: &'a [ProfileFlow]) -> Self {
        FlowPolicy {
            personal: settings.composio_profiles.iter().filter(|p| p.personal).map(|p| p.id.as_str()).collect(),
            always: &settings.permission_settings.profile_flows,
            chat,
        }
    }

    /// Whether content may move from `from` to `to` without asking. A
    /// removed profile is never personal.
    pub fn allows(&self, from: &str, to: &str) -> bool {
        let listed = |l: &[ProfileFlow]| l.iter().any(|f| f.from == from && f.to == to);
        self.personal.contains(from) || self.personal.contains(to) || listed(self.always) || listed(self.chat)
    }
}

/// A held flow: why (a sentence starting "This ", spoken as is on a voice
/// turn) and its pair key ([`pair_key`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FlowHold {
    pub reason: String,
    pub rule_key: String,
}

/// The key a held call carries: its unallowed sources and its target.
/// Empty sources = a hold on a turn nobody was present for (an act outside
/// home, or a carry of foreign data), approved once only.
pub fn pair_key(sources: &[&str], target: &str) -> String {
    format!("{PAIR_KEY_PREFIX}{}>{target}", sources.join("+"))
}

/// A held call's pairs, parsed from its key: `None` for anything but a
/// [`pair_key`] (an old-format flow key, a server name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldFlow {
    pub sources: Vec<String>,
    pub target: String,
}

impl HeldFlow {
    /// One pair per source, each into the target.
    pub fn pairs(&self) -> Vec<ProfileFlow> {
        self.sources.iter().map(|s| ProfileFlow { from: s.clone(), to: self.target.clone() }).collect()
    }
}

pub fn parse_held_flow(key: &str) -> Option<HeldFlow> {
    let rest = key.strip_prefix(PAIR_KEY_PREFIX)?;
    let (from, to) = rest.split_once('>')?;
    if to.is_empty() || to.contains('>') {
        return None;
    }
    Some(HeldFlow {
        sources: from.split('+').filter(|s| !s.is_empty()).map(str::to_string).collect(),
        target: to.to_string(),
    })
}

/// A profile's name, or "a removed profile" for an id that no longer names one.
pub fn profile_label(names: &[ReachProfile], id: &str) -> String {
    names.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| "a removed profile".to_string())
}

/// "Puget", "Puget and Acme", "Puget, Acme and a removed profile".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "Puget data", or "data from a removed profile" when one isn't named.
fn data_of(names: &[ReachProfile], ids: &[&str]) -> String {
    let labels: Vec<String> = ids.iter().map(|id| profile_label(names, id)).collect();
    if ids.iter().all(|id| names.iter().any(|p| p.id == *id)) {
        format!("{} data", join_names(&labels))
    } else {
        format!("data from {}", join_names(&labels))
    }
}

/// **Decide the hold** for a call of `class` into `target` (`None` = not a
/// Composio profile), in a chat whose home is `home`, that has read
/// `read_from`, on a turn the person started (`person`) or not. `names`
/// names profiles for the reason ([`profile_reach::all_profiles`]).
///
/// [`profile_reach::all_profiles`]: super::profile_reach::all_profiles
pub fn flow_hold(
    class: FlowClass,
    target: Option<&ReachProfile>,
    home: Option<&ReachProfile>,
    read_from: &BTreeSet<String>,
    person: bool,
    policy: &FlowPolicy,
    names: &[ReachProfile],
) -> Option<FlowHold> {
    if class == FlowClass::Read {
        return None;
    }
    let target = target?;
    if class == FlowClass::Carry {
        let sources: Vec<&str> = read_from
            .iter()
            .map(String::as_str)
            .filter(|s| *s != target.id && !policy.allows(s, &target.id))
            .collect();
        if !sources.is_empty() {
            let data = data_of(names, &sources);
            // On a turn nobody is present for, the person approves this one
            // call: no pairs in the key, so no allowance offered or applied
            // ([`released_by_allowance`]).
            return Some(if person {
                FlowHold {
                    reason: format!("This sends content into {} from a chat that has read {data}.", target.name),
                    rule_key: pair_key(&sources, &target.id),
                }
            } else {
                FlowHold {
                    reason: format!(
                        "This sends content into {} from a chat that has read {data}, on a turn nobody is watching.",
                        target.name
                    ),
                    rule_key: pair_key(&[], &target.id),
                }
            });
        }
    }
    let outside_home = home.is_none_or(|h| h.id != target.id);
    if !person && outside_home {
        let reason = match home {
            Some(h) => format!(
                "This acts in {}, outside {} (this chat's home profile), on a turn nobody is watching.",
                target.name, h.name
            ),
            None => format!("This acts in {} on a turn nobody is watching.", target.name),
        };
        return Some(FlowHold { reason, rule_key: pair_key(&[], &target.id) });
    }
    None
}

/// Whether the flow guard treats a turn as the person's: a typed or voice
/// turn (`TurnOrigin::is_person`) that the person actually started — not
/// one a fired timer, a relay or returned work started in an ordinary chat
/// (`absent`, `chat_queue::turn_absent`; the call risk gate reads it too,
/// the Heartbeat and agent-time gates treat such a turn as the person's).
pub fn flow_person(origin: crate::session::TurnOrigin, absent: bool) -> bool {
    origin.is_person() && !absent
}

/// The pair key of a Carry that [`flow_hold`] let through only because the
/// policy allows its pairs (a settings pair, a chat pair or a personal
/// profile): logged as a trust decision so standing allowances leave a
/// record. `None` when nothing foreign was in play.
pub fn allowed_carry_key(
    class: FlowClass,
    target: Option<&ReachProfile>,
    read_from: &BTreeSet<String>,
    policy: &FlowPolicy,
) -> Option<String> {
    let target = target.filter(|_| class == FlowClass::Carry)?;
    let sources: Vec<&str> = read_from.iter().map(String::as_str).filter(|s| *s != target.id).collect();
    (!sources.is_empty() && sources.iter().all(|s| policy.allows(s, &target.id))).then(|| pair_key(&sources, &target.id))
}

/// The calls still held in `messages` that `policy` now covers, released
/// along with the call the person just allowed (round `round_id`): flow
/// holds from that same round whose every pair is allowed, minted on a turn
/// the person started (typed or voice — never a Heartbeat, unattended or
/// legacy unstamped call), and not risk-held. Calls left over from earlier
/// rounds, unattended acts and carries (no pairs) and old-format keys are
/// never released. The caller adds what this can't see: no other gate
/// would hold the call (`tool_call_display::release_allowed_flow_holds`).
pub fn released_by_allowance(
    messages: &[crate::components::chat::Message],
    policy: &FlowPolicy,
    round_id: &str,
) -> Vec<crate::components::shared::ToolCall> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            crate::components::shared::MessageContent::PermissionRequest(tc)
                if tc.risk_reason.is_none()
                    && tc.round_id.as_deref() == Some(round_id)
                    && tc.origin.is_some_and(|o| o.is_person()) =>
            {
                let held = parse_held_flow(tc.hold_rule_key.as_deref()?)?;
                (!held.sources.is_empty() && held.pairs().iter().all(|f| policy.allows(&f.from, &f.to)))
                    .then(|| tc.clone())
            }
            _ => None,
        })
        .collect()
}

/// The bubble's wording for a held flow's choices: "Always allow Puget
/// content into Clearmirror.ai" (one label for all its pairs).
pub fn always_label(settings: &Settings, held: &HeldFlow) -> String {
    let names = super::profile_reach::all_profiles(settings);
    let from: Vec<String> = held.sources.iter().map(|s| profile_label(&names, s)).collect();
    format!("Always allow {} content into {}", join_names(&from), profile_label(&names, &held.target))
}

/// "Puget → Clearmirror.ai", for the settings list.
pub fn pair_label(settings: &Settings, flow: &ProfileFlow) -> String {
    let names = super::profile_reach::all_profiles(settings);
    format!("{} → {}", profile_label(&names, &flow.from), profile_label(&names, &flow.to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use FlowClass::*;

    fn p(id: &str, name: &str) -> ReachProfile {
        ReachProfile { id: id.into(), name: name.into() }
    }

    fn settings(profiles: &[(&str, &str, bool)]) -> Settings {
        let mut s = Settings::default();
        s.composio_profiles = profiles
            .iter()
            .map(|(id, name, personal)| crate::settings::ComposioProfile {
                id: id.to_string(),
                name: name.to_string(),
                personal: *personal,
                ..Default::default()
            })
            .collect();
        s
    }

    fn flow(from: &str, to: &str) -> ProfileFlow {
        ProfileFlow { from: from.into(), to: to.into() }
    }

    fn read(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classification_table() {
        let rows: &[(&str, Value, FlowClass)] = &[
            // Reads the old guard held as writes.
            ("GMAIL_FETCH_MESSAGE_BY_MESSAGE_ID", json!({"message_id": "x"}), Read),
            ("GMAIL_GET_PROFILE", json!({}), Read),
            ("GMAIL_GET_LABEL", json!({"label_id": "L1"}), Read),
            ("SLACK_FETCH_MESSAGE_THREAD_FROM_A_CONVERSATION", json!({"channel": "C1", "ts": "1"}), Read),
            ("GOOGLECALENDAR_EVENTS_INSTANCES", json!({"eventId": "e"}), Read),
            ("SLACK_CONVERSATIONS_HISTORY", json!({"channel": "C1"}), Read),
            ("GITHUB_LIST_COMMITS", json!({"repo": "r"}), Read),
            ("GITHUB_LIST_COMMENTS_FOR_AN_ISSUE", json!({}), Read),
            ("GITHUB_GET_A_COMMIT", json!({}), Read),
            ("GMAIL_GET_DRAFT", json!({}), Read),
            ("GMAIL_FETCH_EMAILS", json!({"query": "from:a"}), Read),
            ("GMAIL_LIST_DRAFTS", json!({}), Read),
            ("COMPOSIO_DISCOVER_APPS", json!({}), Read),
            ("COMPOSIO_GET_APP_TOOLS", json!({}), Read),
            ("COMPOSIO_CLEAR_TOOLS", json!({}), Read),
            ("MCP_LOAD_SERVER_TOOLS", json!({}), Read),
            // Acts: state changes with no content.
            ("GMAIL_ADD_LABEL_TO_EMAIL", json!({"message_id": "m", "add_label_ids": ["L1"]}), Act),
            ("GMAIL_BATCH_MODIFY_MESSAGES", json!({"messageIds": ["a", "b"], "addLabelIds": ["L"]}), Act),
            ("GMAIL_MODIFY_THREAD_LABELS", json!({"thread_id": "t", "remove_label_ids": ["UNREAD"]}), Act),
            ("GMAIL_DELETE_DRAFT", json!({"draft_id": "d"}), Act),
            ("GMAIL_MOVE_TO_TRASH", json!({"message_id": "m"}), Act),
            ("SLACK_ADD_REACTION_TO_AN_ITEM", json!({"name": "thumbsup"}), Act),
            ("GOOGLECALENDAR_PATCH_EVENT", json!({"event_id": "e", "start_time": "10:00"}), Act),
            (
                "GOOGLECALENDAR_PATCH_EVENT",
                json!({"calendar_id": "primary", "event_id": "e", "start": {"dateTime": "2026-11-12T10:00:00", "timeZone": "America/Denver"}, "event_duration_hour": 1}),
                Act,
            ),
            // An Act carrying free text is a Carry.
            ("GOOGLECALENDAR_PATCH_EVENT", json!({"event_id": "e", "description": "Puget Q3 numbers"}), Carry),
            ("GOOGLECALENDAR_UPDATE_EVENT", json!({"event_id": "e", "summary": "Puget sync"}), Carry),
            ("SLACK_ADD_REACTION_TO_AN_ITEM", json!({"name": "x".repeat(250)}), Carry),
            // An edit is a Carry unless positively content-free.
            ("GOOGLESHEETS_BATCH_UPDATE", json!({"spreadsheet_id": "s", "values": [["Puget Q3", "1.2M"]]}), Carry),
            ("GOOGLESHEETS_VALUES_UPDATE", json!({"spreadsheet_id": "s", "range": "Sheet1!A1", "values": [[1200000]]}), Carry),
            ("TRELLO_ADD_CARDS", json!({"name": "Puget follow-up", "desc": "Call Sam"}), Carry),
            ("HUBSPOT_UPDATE_CONTACT", json!({"contact_id": "1", "properties": {"company": "Puget"}}), Carry),
            ("NOTION_RENAME_PAGE", json!({"page_id": "p", "name": "Puget plan"}), Carry),
            ("ASANA_UPDATE_TASK", json!({"task_gid": "1", "completed": true, "due_on": "2026-11-12"}), Carry),
            ("ASANA_UPDATE_TASK", json!({"task_id": "1", "completed": true, "due_date": "2026-11-12"}), Act),
            ("LINEAR_SET_ISSUE_STATUS", json!({"issue_id": "1", "status": "Done"}), Act),
            // Editing a content object is a Carry.
            ("GOOGLEDOCS_UPDATE_DOCUMENT_MARKDOWN", json!({"id": "d"}), Carry),
            ("GITHUB_ADD_COMMENT_TO_ISSUE", json!({"issue": 1}), Carry),
            // Carries.
            ("GMAIL_SEND_EMAIL", json!({"to": "a@b.c"}), Carry),
            ("GMAIL_CREATE_EMAIL_DRAFT", json!({}), Carry),
            ("GMAIL_REPLY_TO_THREAD", json!({}), Carry),
            ("GOOGLEDOCS_CREATE_DOCUMENT_MARKDOWN", json!({}), Carry),
            ("SLACK_SEND_MESSAGE", json!({}), Carry),
            ("SLACK_CHAT_POST_MESSAGE", json!({}), Carry),
            ("GOOGLEDRIVE_UPLOAD_FILE", json!({}), Carry),
            // The execute meta tool is judged by the call it runs.
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_FETCH_EMAILS", "arguments": {}}), Read),
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_SEND_EMAIL", "arguments": {"body": "hi"}}), Carry),
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_ADD_LABEL_TO_EMAIL", "arguments": "{\"message_id\":\"m\"}"}), Act),
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_FETCH_EMAILS"}), Read),
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_SEND_EMAIL", "arguments": "not json"}), Carry),
            (EXECUTE_TOOL, json!({"tool_name": "GMAIL_FETCH_EMAILS", "arguments": [1]}), Carry),
            (EXECUTE_TOOL, json!({"tool_name": 7}), Carry),
            (EXECUTE_TOOL, json!({}), Carry),
            // Only `tool_name` runs: a `tool_slug` beside it is ignored both ways.
            (EXECUTE_TOOL, json!({"tool_slug": "GMAIL_FETCH_EMAILS", "tool_name": "GMAIL_SEND_EMAIL", "arguments": {}}), Carry),
            (EXECUTE_TOOL, json!({"tool_slug": "GMAIL_SEND_EMAIL", "arguments": {}}), Carry),
            // A meta tool Hobbes doesn't offer is judged by its name.
            ("COMPOSIO_MULTI_EXECUTE_TOOL", json!({"tools": [{"tool_slug": "GMAIL_FETCH_EMAILS"}]}), Carry),
            ("COMPOSIO_REMOTE_WORKBENCH", json!({}), Carry),
            // Unknown verbs fail closed.
            ("ACME_FROBNICATE_WIDGET", json!({}), Carry),
            ("FROBNICATE", json!({}), Carry),
        ];
        for (name, args, want) in rows {
            assert_eq!(classify_flow(name, args), *want, "{name} {args}");
        }
    }

    #[test]
    fn the_leading_verb_wins_not_any_token() {
        // "message", "label", "profile" anywhere no longer make a read a write.
        assert_eq!(classify_flow("GMAIL_GET_MESSAGE_LABEL_PROFILE", &json!({})), Read);
        // The verb, not the object: "send" in the object of a read.
        assert_eq!(classify_flow("SLACK_LIST_SEND_QUEUE", &json!({})), Read);
        // A read joined to a write takes the write.
        assert_eq!(classify_flow("GOOGLESHEETS_FIND_REPLACE", &json!({"find": "a", "replacement": "b"})), Carry);
        assert_eq!(classify_flow("ACME_FIND_AND_UPDATE", &json!({"record": "x"})), Carry);
        assert_eq!(classify_flow("ACME_GET_OR_CREATE_CONTACT", &json!({})), Carry);
        assert_eq!(classify_flow("ACME_SEARCH_AND_DELETE", &json!({"ids": ["1"]})), Act);
        // Noun markers: a read only as the last word, after plural nouns and no verb.
        assert_eq!(classify_flow("ACME_LOG_CALL_DETAILS", &json!({})), Carry);
        assert_eq!(classify_flow("ACME_OPEN_A_VIEW", &json!({})), Carry);
        assert_eq!(classify_flow("ACME_SYNC_HISTORY", &json!({})), Carry);
        assert_eq!(classify_flow("ACME_HISTORY_EXPORT_SYNC", &json!({})), Read, "export is a read verb");
        assert_eq!(classify_flow("SLACK_USERS_INFO", &json!({})), Read);
        assert_eq!(classify_flow("ACME_ADDRESS_INFO", &json!({})), Carry, "-ss is no plural");
        // Weak read verbs only right after the prefix.
        assert_eq!(classify_flow("GITHUB_VIEW_REPO", &json!({})), Read);
        assert_eq!(classify_flow("ACME_SAVE_VIEW", &json!({})), Carry);
        // Markers that existed only for local tools are gone.
        assert_eq!(classify_flow("browser_snapshot", &json!({})), Carry);
        assert_eq!(classify_flow("directory_tree", &json!({})), Carry);
    }

    #[test]
    fn content_free_args_are_ids_times_and_short_enums() {
        assert!(content_free_args(&json!({"messageIds": ["a"], "addLabelIds": ["L"], "starred": true})));
        assert!(content_free_args(&json!({"start": {"dateTime": "2026-11-12T10:00", "timeZone": "UTC"}})));
        assert!(content_free_args(&json!({"status": "IN_PROGRESS"})));
        assert!(content_free_args(&json!({})));
        assert!(!content_free_args(&json!({"status": "waiting on Puget legal"})));
        assert!(!content_free_args(&json!({"name": "x"})));
        assert!(!content_free_args(&json!({"values": [[1]]})));
        assert!(!content_free_args(&json!("x")));
    }

    #[test]
    fn the_dispatcher_and_the_classifier_read_the_same_target() {
        let args = json!({"tool_slug": "A_GET", "tool_name": "B_SEND", "arguments": {"to": "x"}});
        let (name, inner) = execute_target(&args).unwrap();
        assert_eq!((name, inner), ("B_SEND", json!({"to": "x"})));
        assert_eq!(execute_target(&json!({"tool_name": "B"})).unwrap().1, json!({}));
        assert!(execute_target(&json!({"tool_slug": "B"})).is_none());
    }

    #[test]
    fn person_flow_and_audit_and_release() {
        use crate::session::TurnOrigin;
        assert!(flow_person(TurnOrigin::User, false));
        assert!(flow_person(TurnOrigin::Voice, false));
        assert!(!flow_person(TurnOrigin::User, true), "a timer or relay in an ordinary chat");
        assert!(!flow_person(TurnOrigin::Heartbeat, false));

        let s = settings(&[("cm", "Clearmirror.ai", false), ("pg", "Puget", false), ("ac", "Acme", false)]);
        let cm = p("cm", "Clearmirror.ai");
        let chat = vec![flow("pg", "cm")];
        let policy = FlowPolicy::new(&s, &chat);
        // Let through by the chat pair: recorded.
        assert_eq!(allowed_carry_key(Carry, Some(&cm), &read(&["pg"]), &policy).as_deref(), Some("profile-flow:v2:pg>cm"));
        // Nothing foreign, a read, an act, or a hold: nothing to record.
        assert_eq!(allowed_carry_key(Carry, Some(&cm), &read(&["cm"]), &policy), None);
        assert_eq!(allowed_carry_key(Read, Some(&cm), &read(&["pg"]), &policy), None);
        assert_eq!(allowed_carry_key(Act, Some(&cm), &read(&["pg"]), &policy), None);
        assert_eq!(allowed_carry_key(Carry, Some(&cm), &read(&["ac", "pg"]), &policy), None);

        // Sibling holds the allowance now covers.
        let held_in = |exec: &str, key: &str, risk: Option<&str>, origin: Option<TurnOrigin>, round: &str| {
            let mut m = crate::voice::session::transcript_message(true, "x");
            m.content = crate::components::shared::MessageContent::PermissionRequest(crate::components::shared::ToolCall {
                execution_id: exec.into(),
                hold_rule_key: Some(key.into()),
                risk_reason: risk.map(str::to_string),
                origin,
                round_id: Some(round.into()),
                ..Default::default()
            });
            m
        };
        let held = |exec: &str, key: &str, risk: Option<&str>| held_in(exec, key, risk, Some(TurnOrigin::User), "r1");
        let msgs = vec![
            held("covered", "profile-flow:v2:pg>cm", None),
            held("other-pair", "profile-flow:v2:ac+pg>cm", None),
            held("risky", "profile-flow:v2:pg>cm", Some("deletes x")),
            held("unattended", "profile-flow:v2:>cm", None),
            held("old", "profile-flow:pg>cm", None),
            held_in("voice", "profile-flow:v2:pg>cm", None, Some(TurnOrigin::Voice), "r1"),
            held_in("heartbeat", "profile-flow:v2:pg>cm", None, Some(TurnOrigin::Heartbeat), "r1"),
            held_in("legacy", "profile-flow:v2:pg>cm", None, None, "r1"),
            held_in("earlier-round", "profile-flow:v2:pg>cm", None, Some(TurnOrigin::User), "r0"),
        ];
        let out: Vec<String> = released_by_allowance(&msgs, &policy, "r1").into_iter().map(|t| t.execution_id).collect();
        assert_eq!(out, ["covered", "voice"]);
        let both = vec![flow("pg", "cm"), flow("ac", "cm")];
        let out: Vec<String> =
            released_by_allowance(&msgs, &FlowPolicy::new(&s, &both), "r1").into_iter().map(|t| t.execution_id).collect();
        assert_eq!(out, ["covered", "other-pair", "voice"]);
        // Another round: only its own holds.
        let out: Vec<String> = released_by_allowance(&msgs, &policy, "r0").into_iter().map(|t| t.execution_id).collect();
        assert_eq!(out, ["earlier-round"]);
    }

    #[test]
    fn free_text_is_judged_by_key_and_length() {
        assert!(has_free_text(&json!({"description": "x"})));
        assert!(has_free_text(&json!({"event": {"messageBody": "x"}})));
        assert!(!has_free_text(&json!({"message_id": "x", "label_ids": ["a"]})));
        assert!(!has_free_text(&json!({"description": "   "})));
        assert!(has_free_text(&json!({"anything": "y".repeat(LONG_TEXT)})));
        assert!(!has_free_text(&json!({"message_ids": "y".repeat(LONG_TEXT)})));
    }

    /// The hold matrix: Clearmirror home, Puget and Acme set up.
    #[test]
    fn hold_matrix() {
        let s = settings(&[("cm", "Clearmirror.ai", false), ("pg", "Puget", false), ("ac", "Acme", false)]);
        let names = super::super::profile_reach::all_profiles(&s);
        let (cm, pg) = (p("cm", "Clearmirror.ai"), p("pg", "Puget"));
        let none: Vec<ProfileFlow> = Vec::new();
        let policy = FlowPolicy::new(&s, &none);
        let hold = |class, target: Option<&ReachProfile>, r: &BTreeSet<String>, person, pol: &FlowPolicy| {
            flow_hold(class, target, Some(&cm), r, person, pol, &names)
        };
        let pg_read = read(&["pg"]);

        // Reads are never held, whatever was read and whoever asks.
        assert_eq!(hold(Read, Some(&cm), &pg_read, true, &policy), None);
        assert_eq!(hold(Read, Some(&pg), &pg_read, false, &policy), None);
        // Non-Composio targets are never held by this guard.
        assert_eq!(hold(Carry, None, &pg_read, true, &policy), None);
        assert_eq!(hold(Carry, None, &pg_read, false, &policy), None);
        // Act on a person's turn: not held, at home or not, foreign data or not.
        assert_eq!(hold(Act, Some(&cm), &pg_read, true, &policy), None);
        assert_eq!(hold(Act, Some(&pg), &read(&["cm"]), true, &policy), None);
        // Carry with a foreign source: held, named in profile names.
        let h = hold(Carry, Some(&cm), &pg_read, true, &policy).expect("held");
        assert_eq!(h.reason, "This sends content into Clearmirror.ai from a chat that has read Puget data.");
        assert_eq!(h.rule_key, "profile-flow:v2:pg>cm");
        // Carry with only the target's own data, or nothing read: not held
        // on a person's turn, even outside home.
        assert_eq!(hold(Carry, Some(&cm), &read(&["cm"]), true, &policy), None);
        assert_eq!(hold(Carry, Some(&pg), &read(&[]), true, &policy), None);
        assert_eq!(hold(Carry, Some(&pg), &pg_read, true, &policy), None);

        // Allowed by the settings' pair list.
        let mut always = s.clone();
        always.permission_settings.profile_flows = vec![flow("pg", "cm")];
        assert_eq!(hold(Carry, Some(&cm), &pg_read, true, &FlowPolicy::new(&always, &none)), None);
        // Direction matters: pg→cm doesn't allow cm→pg.
        assert!(hold(Carry, Some(&pg), &read(&["cm"]), true, &FlowPolicy::new(&always, &none)).is_some());
        // Allowed in this chat.
        let chat = vec![flow("pg", "cm")];
        assert_eq!(hold(Carry, Some(&cm), &pg_read, true, &FlowPolicy::new(&s, &chat)), None);
        // Allowed because one side is personal (either side).
        let personal = settings(&[("cm", "Clearmirror.ai", false), ("pg", "Puget", true)]);
        assert_eq!(hold(Carry, Some(&cm), &pg_read, true, &FlowPolicy::new(&personal, &none)), None);
        assert_eq!(hold(Carry, Some(&pg), &read(&["cm"]), true, &FlowPolicy::new(&personal, &none)), None);

        // Two sources, only one allowed: still held, naming only the other.
        let two = read(&["ac", "pg"]);
        let h = hold(Carry, Some(&cm), &two, true, &FlowPolicy::new(&s, &chat)).expect("held");
        assert_eq!(h.rule_key, "profile-flow:v2:ac>cm");
        assert!(h.reason.contains("read Acme data"), "{}", h.reason);
        // Neither allowed: both listed.
        let h = hold(Carry, Some(&cm), &two, true, &policy).unwrap();
        assert_eq!(h.rule_key, "profile-flow:v2:ac+pg>cm");
        assert!(h.reason.contains("Acme and Puget data"), "{}", h.reason);

        // A removed profile still counts as a source.
        let h = hold(Carry, Some(&cm), &read(&["gone"]), true, &policy).unwrap();
        assert!(h.reason.ends_with("read data from a removed profile."), "{}", h.reason);
        assert_eq!(h.rule_key, "profile-flow:v2:gone>cm");

        // Unattended: any non-Read outside home is held (allow once only)…
        let h = hold(Act, Some(&pg), &read(&[]), false, &policy).expect("held");
        assert!(h.reason.contains("outside Clearmirror.ai"), "{}", h.reason);
        assert_eq!(parse_held_flow(&h.rule_key).unwrap().sources, Vec::<String>::new());
        assert!(hold(Carry, Some(&pg), &read(&[]), false, &policy).is_some());
        // …even when every pair is allowed.
        assert!(hold(Carry, Some(&pg), &read(&["cm"]), false, &FlowPolicy::new(&personal, &none)).is_some());
        // …but not at home (the Heartbeat gate has its own rules).
        assert_eq!(hold(Act, Some(&cm), &pg_read, false, &policy), None);
        // A foreign Carry on an unattended turn is approved once: no pairs to
        // allow or release (outside home and at home alike).
        let h = hold(Carry, Some(&pg), &read(&["cm"]), false, &policy).unwrap();
        assert_eq!(h.rule_key, "profile-flow:v2:>pg");
        assert!(h.reason.contains("from a chat that has read Clearmirror.ai data, on a turn nobody is watching"), "{}", h.reason);
        let h = hold(Carry, Some(&cm), &pg_read, false, &policy).unwrap();
        assert_eq!(parse_held_flow(&h.rule_key).unwrap().sources, Vec::<String>::new());
    }

    #[test]
    fn held_keys_parse_and_old_keys_do_not() {
        let held = parse_held_flow("profile-flow:v2:ac+pg>cm").unwrap();
        assert_eq!(held.sources, ["ac", "pg"]);
        assert_eq!(held.target, "cm");
        assert_eq!(held.pairs(), vec![flow("ac", "cm"), flow("pg", "cm")]);
        // Old-format keys: approved or denied once, never parsed.
        for old in ["profile-flow:pg>local", "profile-flow:act>pg", "profile-flow:ac+pg>cm", "composio-native:Puget", ""] {
            assert_eq!(parse_held_flow(old), None, "{old}");
        }
        assert_eq!(parse_held_flow("profile-flow:v2:pg>"), None);
    }

    #[test]
    fn labels_use_profile_names() {
        let s = settings(&[("cm", "Clearmirror.ai", false), ("pg", "Puget", false), ("ac", "Acme", false)]);
        let held = parse_held_flow("profile-flow:v2:pg>cm").unwrap();
        assert_eq!(always_label(&s, &held), "Always allow Puget content into Clearmirror.ai");
        let held = parse_held_flow("profile-flow:v2:ac+gone>cm").unwrap();
        assert_eq!(always_label(&s, &held), "Always allow Acme and a removed profile content into Clearmirror.ai");
        assert_eq!(pair_label(&s, &flow("pg", "cm")), "Puget → Clearmirror.ai");
    }
}
