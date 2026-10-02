//! Turn context ledger: live session context as append-only history.
//!
//! The live context (today's plan, the scratchpad, loaded skills, ...) used to
//! be resent in full on every request as a `<TURN_CONTEXT>` system block,
//! uncached. The ledger records it inside the conversation instead:
//!
//! - **Ledger entries.** At a user-turn boundary — the first build after a
//!   new user text message — each live key whose canonical rendering differs
//!   from what the model last saw gets an entry anchored after that user
//!   message. A key that became empty gets an explicit "cleared" entry.
//!   Mid-tool-loop builds never write entries: the model sees its own changes
//!   in the tool results.
//! - **Tails.** Every request carries a turn-scoped tail (clock + turn
//!   instructions), persisted anchored after the last message at build time
//!   and replayed unchanged on later requests. A retry (a tail already exists
//!   for that anchor) reuses it.
//! - **Trim snapshots.** When the history window moves (only at a user-turn
//!   boundary), entries restating the current context are written to render
//!   right after the pinned first user message.
//!
//! Entries render as `ChatRole::System` messages spliced in after their
//! anchor message, so with the session state fixed every build is a byte
//! prefix-extension of the previous one, except on a trim
//! (`LlmPrompt.history_rewritten`).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::components::chat::Message;
use crate::components::shared::MessageContent;
use crate::llm::types::{ChatMessage, ChatRole, ContentBlock};
use crate::session::{LedgerEntry, LedgerKind};

use super::system_context::canonicalize_json;

/// First line of every request tail.
pub(crate) const TAIL_HEADER: &str = "[Turn context: applies to this request only.]";
/// `LedgerEntry.key` of a tail.
pub(crate) const TAIL_KEY: &str = "tail";
/// Live key shown only in a trim snapshot.
pub(crate) const SUMMARY_KEY: &str = "conversation_summary";
/// Key of the note that opens a trim snapshot.
pub(crate) const TRIM_NOTE_KEY: &str = "history_trimmed";
/// `ActiveContext.extra` prefix of a tool-call snapshot.
pub(crate) const SNAPSHOT_PREFIX: &str = "tool_snapshot_";

const TRIM_NOTE: &str = "[History trimmed: earlier messages after the first one were omitted to fit the context window. Context values restated below are current as of this point.]";

/// True for a user-authored text message (a turn start).
pub(crate) fn is_user_text(m: &Message) -> bool {
    m.author == "User" && matches!(m.content, MessageContent::Text { .. })
}

fn is_empty_value(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Canonical rendering of one live value (sorted keys, pretty JSON; a bare
/// string renders as itself). `None` when the value is empty.
pub(crate) fn canonical_body(value: &serde_json::Value) -> Option<String> {
    if is_empty_value(value) {
        return None;
    }
    let mut v = value.clone();
    canonicalize_json(&mut v);
    match &v {
        serde_json::Value::String(s) => Some(s.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

/// A conversation summary with neither prose nor a current task carries
/// nothing worth restating.
fn summary_is_empty(v: &serde_json::Value) -> bool {
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().is_empty();
    field("summary") && field("current_task")
}

/// The text a ledger entry renders as. Pure in the entry, so a persisted
/// entry renders the same bytes on every later request.
pub(crate) fn render_entry(entry: &LedgerEntry) -> String {
    match entry.kind {
        LedgerKind::Tail => entry.body.clone(),
        _ if entry.key == TRIM_NOTE_KEY => TRIM_NOTE.to_string(),
        _ if entry.body.is_empty() => format!(
            "[Context update: {}. Cleared; it no longer has a value.]",
            entry.key
        ),
        _ => format!(
            "[Context update: {}. Replaces earlier values.]\n{}",
            entry.key, entry.body
        ),
    }
}

/// Body of a request tail: header, clock, then the turn instructions.
pub(crate) fn tail_body(now_rfc3339: &str, turn_instructions: &str) -> String {
    let mut body = format!("{TAIL_HEADER}\nCurrent time: {now_rfc3339} (local)");
    if !turn_instructions.trim().is_empty() {
        body.push('\n');
        body.push_str(turn_instructions.trim());
    }
    body
}

pub(crate) fn system_message(text: String) -> ChatMessage {
    ChatMessage {
        role: ChatRole::System,
        content: vec![ContentBlock::Text { text }],
    }
}

/// The live context of one build, split by how the ledger treats each key.
pub(crate) struct LiveContext {
    /// Keys recorded at user-turn boundaries → canonical body (non-empty
    /// values only). Includes snapshots of tool calls outside the window.
    pub boundary: BTreeMap<String, String>,
    /// Canonical conversation summary, shown only in a trim snapshot.
    pub summary: Option<String>,
}

/// A key that gets an explicit "cleared" entry when its value disappears.
/// Tool snapshots don't: one vanishing (eviction, rewind) leaves the facts it
/// recorded true for the call it describes.
pub(crate) fn is_clearable(key: &str) -> bool {
    !key.starts_with(SNAPSHOT_PREFIX) && key != SUMMARY_KEY
}

/// Split the composed live map for a window starting at `window_start`.
///
/// A `tool_snapshot_<execution_id>` is live only while its tool call is
/// outside the window (before `window_start`, or not in `messages` at all);
/// inside the window the call and its result are already in the history.
pub(crate) fn live_context(
    volatile: &serde_json::Map<String, serde_json::Value>,
    messages: &[Message],
    window_start: usize,
) -> LiveContext {
    let call_index: HashMap<&str, usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match &m.content {
            MessageContent::ToolCall(tc) => Some((tc.execution_id.as_str(), i)),
            _ => None,
        })
        .collect();
    let mut boundary = BTreeMap::new();
    let mut summary = None;
    for (key, value) in volatile {
        if key == SUMMARY_KEY {
            if !summary_is_empty(value) {
                summary = canonical_body(value);
            }
            continue;
        }
        if let Some(exec_id) = key.strip_prefix(SNAPSHOT_PREFIX) {
            if call_index.get(exec_id).is_some_and(|&i| i >= window_start) {
                continue;
            }
        }
        if let Some(body) = canonical_body(value) {
            boundary.insert(key.clone(), body);
        }
    }
    LiveContext { boundary, summary }
}

/// Where the ledger sits relative to the outgoing history window.
pub(crate) struct WindowView<'a> {
    /// The pinned first user message, when it renders above the window
    /// (before `window_start`). Its entries render right after it.
    pub pinned_above: Option<uuid::Uuid>,
    /// The window anchor (`None` = the window starts at the chat start).
    pub trim_anchor: Option<uuid::Uuid>,
    /// Position of every window message (session order).
    pub window_pos: &'a HashMap<uuid::Uuid, usize>,
}

impl WindowView<'_> {
    /// Render-order sort key of a non-tail entry, `None` when not rendered.
    fn order(&self, idx: usize, e: &LedgerEntry) -> Option<(usize, usize)> {
        match e.kind {
            LedgerKind::Tail => None,
            LedgerKind::Trim => {
                (self.trim_anchor.is_some() && e.trim_anchor == self.trim_anchor)
                    .then_some((1, idx))
            }
            LedgerKind::Ledger => {
                if Some(e.after_message_id) == self.pinned_above {
                    Some((0, idx))
                } else {
                    self.window_pos
                        .get(&e.after_message_id)
                        .map(|&p| (2 + p, idx))
                }
            }
        }
    }

    /// The value of every key as the model sees it after the rendered
    /// entries: the last rendered entry per key (empty body = cleared).
    pub fn latest_visible(&self, ledger: &[LedgerEntry]) -> HashMap<String, String> {
        let mut ordered: Vec<((usize, usize), &LedgerEntry)> = ledger
            .iter()
            .enumerate()
            .filter_map(|(i, e)| self.order(i, e).map(|o| (o, e)))
            .filter(|(_, e)| e.key != TRIM_NOTE_KEY)
            .collect();
        ordered.sort_by_key(|(o, _)| *o);
        ordered
            .into_iter()
            .map(|(_, e)| (e.key.clone(), e.body.clone()))
            .collect()
    }

    /// Keys with a `Ledger` entry anchored inside the window (not counting
    /// the pinned message above it).
    pub fn keys_in_window(&self, ledger: &[LedgerEntry]) -> HashSet<String> {
        ledger
            .iter()
            .filter(|e| e.kind == LedgerKind::Ledger && self.window_pos.contains_key(&e.after_message_id))
            .map(|e| e.key.clone())
            .collect()
    }
}

/// Entries for a trim snapshot: the opening note, then — sorted by key —
/// every live key whose current value the model would no longer see, and
/// the conversation summary. A key with an entry inside the new window is
/// left to it (a stale one is superseded by this boundary's own update); a
/// key whose visible value — e.g. an entry on the pinned first message — is
/// already current needs no restating.
pub(crate) fn trim_snapshot(
    live: &LiveContext,
    view: &WindowView<'_>,
    ledger: &[LedgerEntry],
    after_message_id: uuid::Uuid,
) -> Vec<LedgerEntry> {
    let in_window = view.keys_in_window(ledger);
    let visible = view.latest_visible(ledger);
    let entry = |key: &str, body: String| LedgerEntry {
        id: uuid::Uuid::new_v4(),
        after_message_id,
        kind: LedgerKind::Trim,
        key: key.to_string(),
        body,
        trim_anchor: view.trim_anchor,
    };
    let mut out = vec![entry(TRIM_NOTE_KEY, String::new())];
    let mut keyed: BTreeMap<&str, &String> = live
        .boundary
        .iter()
        .filter(|(k, v)| !in_window.contains(k.as_str()) && visible.get(k.as_str()) != Some(*v))
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    if let Some(summary) = &live.summary {
        keyed.insert(SUMMARY_KEY, summary);
    }
    out.extend(keyed.into_iter().map(|(k, v)| entry(k, v.clone())));
    out
}

/// Boundary entries: one per key whose current value differs from what the
/// model sees, anchored after `anchor` (the turn's user message).
pub(crate) fn boundary_updates(
    live: &LiveContext,
    latest: &HashMap<String, String>,
    anchor: uuid::Uuid,
) -> Vec<LedgerEntry> {
    let mut keys: Vec<&String> = live.boundary.keys().collect();
    let mut cleared: Vec<&String> = latest
        .iter()
        .filter(|(k, v)| !v.is_empty() && is_clearable(k) && !live.boundary.contains_key(*k))
        .map(|(k, _)| k)
        .collect();
    keys.append(&mut cleared);
    keys.sort();
    keys.into_iter()
        .filter_map(|key| {
            let body = live.boundary.get(key).cloned().unwrap_or_default();
            (latest.get(key) != Some(&body)).then(|| LedgerEntry {
                id: uuid::Uuid::new_v4(),
                after_message_id: anchor,
                kind: LedgerKind::Ledger,
                key: key.clone(),
                body,
                trim_anchor: None,
            })
        })
        .collect()
}
