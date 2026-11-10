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
//!    leading verb of the tool name decides (the first token after the
//!    toolkit prefix that is in a verb list), not any token anywhere; the
//!    Composio execute meta tools are judged by the call(s) they run; an
//!    Act whose arguments hold free text is a Carry; an unrecognised verb is
//!    a Carry (fail closed). The agent-time scope keeps its own, stricter
//!    test (`todo::agent_time::agent_time_scope`).
//! 2. **Is it held?** [`flow_hold`]: only calls into a Composio profile are
//!    judged (terminal, files, browser, other MCP servers, dispatch and
//!    relay messages are left to the normal permission flow and the call
//!    risk gate). A Carry into profile T is held unless every pair
//!    (source → T) is allowed, where the sources are the profiles the chat
//!    has read (`profile_reach::session_read_profiles`) other than T. On an
//!    unattended turn (a Heartbeat wake) any non-Read call into a profile
//!    other than the chat's home is held as well.
//! 3. **What is remembered?** Pairs, not tools ([`FlowPolicy`]): a pair is
//!    allowed when either profile is personal (`ComposioProfile::personal`),
//!    when it is on the always-allowed list (`PermissionSettings::
//!    profile_flows`), or when it was allowed for this chat
//!    (`Session::allowed_flows`). Flow holds never consult or create trust
//!    rules; the rules the old guard wrote (`profile-flow:` servers) are
//!    dropped when settings load.
//!
//! A held call carries its pairs in `ToolCall::hold_rule_key`
//! ([`pair_key`], parsed back by [`parse_held_flow`]) so the approval bubble
//! can offer "Allow in this chat" and "Always allow" for exactly those
//! pairs. Keys in the old format (`…>local`, `act>…`) parse to nothing and
//! are approved or denied once.

use std::collections::BTreeSet;

use serde_json::Value;

use super::profile_reach::ReachProfile;
use crate::context::permissions::ProfileFlow;
use crate::settings::Settings;

/// Trust-rule server prefix the old guard remembered flows under (dropped
/// at load, `Settings::drop_legacy_flow_trust_rules`), and the prefix of
/// every flow hold key.
pub const FLOW_KEY_PREFIX: &str = "profile-flow:";

/// The prefix of a held call's pair key ([`pair_key`]).
pub const PAIR_KEY_PREFIX: &str = "profile-flow:v2:";

/// The meta tool that runs another Composio tool by name.
const EXECUTE_TOOL: &str = "COMPOSIO_EXECUTE_TOOL";
/// The meta tool that runs several Composio tools in one call.
const MULTI_EXECUTE_TOOL: &str = "COMPOSIO_MULTI_EXECUTE_TOOL";

/// Hobbes' discovery / loading meta tools: they read tool lists only.
const DISCOVERY_TOOLS: &[&str] = &[
    "COMPOSIO_DISCOVER_APPS",
    "COMPOSIO_GET_APP_TOOLS",
    "COMPOSIO_CLEAR_TOOLS",
    "COMPOSIO_SEARCH_TOOLS",
    "COMPOSIO_GET_TOOL_SCHEMAS",
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

/// Verbs (and read markers) that only read.
const READ_VERBS: &[&str] = &[
    "get", "fetch", "list", "search", "find", "read", "query", "lookup", "look", "retrieve", "view", "download",
    "export", "check", "describe", "count", "show", "browse", "scan", "inspect", "preview", "poll", "discover",
    "explore", "history", "instances", "snapshot", "tree", "info", "details", "stats", "whoami",
];

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
    "untrash", "delete", "remove", "modify", "update", "patch", "edit", "set", "change", "rename", "mute",
    "unmute", "pin", "unpin", "snooze", "close", "reopen", "resolve", "assign", "unassign", "accept", "decline",
    "reject", "cancel", "complete", "enable", "disable", "approve", "lock", "unlock", "add", "stop", "start",
    "subscribe", "unsubscribe", "follow", "unfollow", "react", "join", "leave", "restore", "toggle", "clear",
    "merge", "kick", "ban", "unban", "block", "unblock", "hide", "unhide", "empty", "purge", "erase", "destroy",
    "drop",
];

/// Act verbs that edit things: their object decides ([`CONTENT_OBJECTS`]).
const EDIT_VERBS: &[&str] = &["add", "update", "edit", "patch", "modify", "set", "change"];

/// Objects whose edit writes content (`GOOGLEDOCS_UPDATE_DOCUMENT…`,
/// `GITHUB_ADD_COMMENT…`), unlike a label or reaction.
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

/// A string at least this long is free text whatever its key.
const LONG_TEXT: usize = 200;

/// The words of a tool name or argument key, lowercased: split on `_`, `-`,
/// `.`, spaces and camelCase boundaries.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The class of the leading verb of `tool` (and its position), if any: the
/// first word after the toolkit prefix that is in a verb list, else the
/// prefix itself (`read_file`).
fn leading_verb(tokens: &[String]) -> Option<(usize, FlowClass)> {
    let class_of = |t: &str| {
        if READ_VERBS.contains(&t) {
            Some(FlowClass::Read)
        } else if CARRY_VERBS.contains(&t) {
            Some(FlowClass::Carry)
        } else if ACT_VERBS.contains(&t) {
            Some(FlowClass::Act)
        } else {
            None
        }
    };
    let after_prefix = tokens.iter().enumerate().skip(1).find_map(|(i, t)| class_of(t).map(|c| (i, c)));
    after_prefix.or_else(|| tokens.first().and_then(|t| class_of(t)).map(|c| (0, c)))
}

/// Whether `args` hold free text: a string under a body/content/description-
/// style key (its last word in [`TEXT_KEYS`]), or any string of at least
/// [`LONG_TEXT`] characters outside id / url keys.
pub fn has_free_text(args: &Value) -> bool {
    fn walk(v: &Value, key: Option<&str>) -> bool {
        let last = key.map(|k| words(k).pop().unwrap_or_default()).unwrap_or_default();
        let id_like = matches!(last.as_str(), "id" | "ids" | "url" | "urls" | "uri" | "link" | "token");
        match v {
            Value::String(s) => {
                let s = s.trim();
                (!s.is_empty() && TEXT_KEYS.contains(&last.as_str())) || (!id_like && s.chars().count() >= LONG_TEXT)
            }
            Value::Array(items) => items.iter().any(|i| walk(i, key)),
            Value::Object(map) => map.iter().any(|(k, v)| walk(v, Some(k))),
            _ => false,
        }
    }
    walk(args, None)
}

/// The tool an execute meta tool runs, and its arguments (an object, or a
/// JSON string holding one).
fn inner_call(call: &Value) -> Option<(String, Value)> {
    let name = ["tool_slug", "tool_name", "tool", "slug", "action"]
        .iter()
        .find_map(|k| call.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let args = ["arguments", "params", "input", "args"]
        .iter()
        .find_map(|k| call.get(*k))
        .cloned()
        .unwrap_or(Value::Null);
    let args = match args {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    Some((name, args))
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
        return match inner_call(args) {
            Some((inner, inner_args)) => classify_at(&inner, &inner_args, depth + 1),
            None => FlowClass::Carry,
        };
    }
    if tool_name == MULTI_EXECUTE_TOOL {
        // The strictest of the calls it runs; nothing readable = Carry.
        let calls = args
            .get("tools")
            .or_else(|| args.get("calls"))
            .or_else(|| args.get("actions"))
            .and_then(Value::as_array);
        let classes: Vec<FlowClass> = calls
            .map(|c| {
                c.iter()
                    .map(|call| match inner_call(call) {
                        Some((inner, inner_args)) => classify_at(&inner, &inner_args, depth + 1),
                        None => FlowClass::Carry,
                    })
                    .collect()
            })
            .unwrap_or_default();
        return classes.into_iter().max().unwrap_or(FlowClass::Carry);
    }
    let tokens = words(tool_name);
    let Some((at, class)) = leading_verb(&tokens) else {
        return FlowClass::Carry;
    };
    match class {
        FlowClass::Act => {
            let edits_content = EDIT_VERBS.contains(&tokens[at].as_str())
                && tokens[at + 1..]
                    .iter()
                    .take_while(|t| !OBJECT_STOPS.contains(&t.as_str()))
                    .any(|t| CONTENT_OBJECTS.contains(&t.as_str()));
            if edits_content || has_free_text(args) {
                FlowClass::Carry
            } else {
                FlowClass::Act
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
/// Empty sources = an unattended act outside home, approved once only.
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
            return Some(FlowHold {
                reason: format!(
                    "This sends content into {} from a chat that has read {}.",
                    target.name,
                    data_of(names, &sources)
                ),
                rule_key: pair_key(&sources, &target.id),
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
            ("browser_snapshot", json!({}), Read),
            ("directory_tree", json!({"path": "/tmp"}), Read),
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
            // An Act carrying free text is a Carry.
            ("GOOGLECALENDAR_PATCH_EVENT", json!({"event_id": "e", "description": "Puget Q3 numbers"}), Carry),
            ("GOOGLECALENDAR_UPDATE_EVENT", json!({"event_id": "e", "summary": "Puget sync"}), Carry),
            ("SLACK_ADD_REACTION_TO_AN_ITEM", json!({"name": "x".repeat(250)}), Carry),
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
            (EXECUTE_TOOL, json!({"tool_slug": "GMAIL_ADD_LABEL_TO_EMAIL", "arguments": "{\"message_id\":\"m\"}"}), Act),
            (EXECUTE_TOOL, json!({}), Carry),
            // Multi-execute: the strictest inner call.
            (
                MULTI_EXECUTE_TOOL,
                json!({"tools": [{"tool_slug": "GMAIL_FETCH_EMAILS", "arguments": {}}, {"tool_slug": "GMAIL_SEND_EMAIL", "arguments": {}}]}),
                Carry,
            ),
            (
                MULTI_EXECUTE_TOOL,
                json!({"tools": [{"tool_slug": "GMAIL_FETCH_EMAILS", "arguments": {}}, {"tool_slug": "GMAIL_GET_PROFILE", "arguments": {}}]}),
                Read,
            ),
            (MULTI_EXECUTE_TOOL, json!({}), Carry),
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
        // A local tool whose only verb is its first word.
        assert_eq!(classify_flow("read_file", &json!({"path": "/x"})), Read);
        assert_eq!(classify_flow("getFileInfo", &json!({})), Read);
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
        // A foreign Carry on an unattended turn offers the pair.
        let h = hold(Carry, Some(&pg), &read(&["cm"]), false, &policy).unwrap();
        assert_eq!(h.rule_key, "profile-flow:v2:cm>pg");
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
