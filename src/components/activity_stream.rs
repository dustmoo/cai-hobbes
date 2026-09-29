#![allow(non_snake_case)]
//! One activity block per assistant turn.
//!
//! Historically every tool call rendered its own `ToolCallDisplay` card, so a
//! ten-step turn produced ten stacked cards and the assistant's actual reply
//! scrolled off the top. The activity stream collapses a whole turn's
//! machinery — tool calls, permission prompts, thinking — into a single
//! bordered block with one row per step, live while the turn runs and
//! collapsed to a one-line summary once it finishes.
//!
//! The grouping is a pure function over the message list (see
//! [`group_turn_runs`]); the rendering is a thin shell on top of it, which is
//! why the interesting logic in this module is unit-testable without a Dioxus
//! runtime.

use std::ops::Range;

use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use feature_clipboard::copy_to_clipboard;
use uuid::Uuid;

use crate::components::chat::{ExpansionStateContext, Message};
use crate::components::shared::{MessageContent, ToolCall, ToolCallStatus, UsageData};
use crate::components::tool_call_display::{AuthRequiredPanel, Collapsible, PermissionPrompt};

/// The planner virtual server; its calls read as "todo" work, not generic tools.
const PLANNER_SERVER: &str = "hobbes-planner";
/// The terminal exec tool; its calls read as shell commands.
const TERMINAL_EXEC_TOOL: &str = "HOBBES_TERMINAL_EXEC";
/// `HOBBES_TERMINAL_EXEC`'s own default `timeout_secs` (see
/// `mcp/terminal_client.rs::DEFAULT_EXEC_TIMEOUT_SECS`) — the meta line only
/// calls out a timeout when the command overrode it.
const DEFAULT_EXEC_TIMEOUT_SECS: i64 = 30;
/// How many lines of a command's output show before "Show all N lines".
const COMMAND_TAIL_LINES: usize = 40;
/// How many JSONL records render before "Show all N records".
const JSONL_RECORD_CAP: usize = 20;

// ============================================================================
// Pure helpers (unit-tested below)
// ============================================================================

/// What a single step in the stream *is*, which decides its icon and how its
/// one line of substance is phrased.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// A shell command run through the terminal server.
    Command,
    /// A planner call (todo/plan/project mutation).
    Todo,
    /// Any other MCP or built-in tool.
    Tool,
    /// A step that came back with `ToolCallStatus::Error`.
    Error,
    /// A thinking summary, either attached to a tool call or standing alone.
    Thinking,
    /// A permission request awaiting the user.
    NeedsYou,
}

/// How a message participates in a turn's activity run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Carries a row of its own, and so can open or close a run: a tool call,
    /// a permission request, or an empty assistant `Text` whose only payload
    /// is a thinking summary (the old UI hid those entirely).
    Anchor,
    /// Renders nothing on its own but must not split a run: the empty
    /// assistant `Text` placeholder each continuation turn pushes before its
    /// tool calls. Treating these as breaks would shatter one turn into a
    /// block per continuation.
    Transparent,
    /// Ends the run: a user prompt, a real assistant reply, a skill call or
    /// skill permission request, an error bubble.
    Break,
}

/// Where a message sits in the grouping.
pub fn slot_of(message: &Message) -> Slot {
    match &message.content {
        MessageContent::ToolCall(_) | MessageContent::PermissionRequest(_) => Slot::Anchor,
        MessageContent::Text {
            content,
            thought_summary,
            ..
        } if content.trim().is_empty() && message.attachments.is_empty() => {
            match thought_summary.as_ref().filter(|t| !t.trim().is_empty()) {
                Some(_) => Slot::Anchor,
                None => Slot::Transparent,
            }
        }
        _ => Slot::Break,
    }
}

/// Maximal runs of a turn's activity: each starts and ends on an [`Slot::Anchor`]
/// and may swallow [`Slot::Transparent`] placeholders in between.
///
/// Anchoring at both ends is what keeps a plain prose turn (whose only empty
/// `Text` is the streaming placeholder) from producing an empty block, while
/// still absorbing the placeholders a multi-continuation turn leaves between
/// its tool calls.
pub fn group_turn_runs(messages: &[Message]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_anchor: Option<usize> = None;
    let flush =
        |start: &mut Option<usize>, last: &mut Option<usize>, runs: &mut Vec<Range<usize>>| {
            if let (Some(s), Some(l)) = (start.take(), last.take()) {
                runs.push(s..l + 1);
            }
            *start = None;
            *last = None;
        };
    for (i, message) in messages.iter().enumerate() {
        match slot_of(message) {
            Slot::Anchor => {
                start.get_or_insert(i);
                last_anchor = Some(i);
            }
            Slot::Transparent => {}
            Slot::Break => flush(&mut start, &mut last_anchor, &mut runs),
        }
    }
    flush(&mut start, &mut last_anchor, &mut runs);
    runs
}

/// While a turn is still in flight, the trailing empty assistant `Text`
/// placeholder — the dashed "Considering…" bubble the stream writes into —
/// belongs to the live block as its last row, not beside it as a second
/// bubble. Extends the final run over those trailing placeholders so
/// `message_list` renders one block and no stray single.
///
/// Only [`Slot::Transparent`] tails are absorbed: a placeholder that already
/// carries a thinking summary is an [`Slot::Anchor`] and is in the run
/// already, and any real text after the run is a [`Slot::Break`] that must
/// stay a bubble of its own.
pub fn absorb_live_placeholder(messages: &[Message], runs: &mut [Range<usize>]) {
    let Some(last) = runs.last_mut() else {
        return;
    };
    if last.end >= messages.len() {
        return;
    }
    if messages[last.end..]
        .iter()
        .all(|m| slot_of(m) == Slot::Transparent)
    {
        last.end = messages.len();
    }
}

/// Is this the empty assistant `Text` a live turn streams into?
fn is_empty_placeholder(message: &Message) -> bool {
    matches!(
        &message.content,
        MessageContent::Text { content, .. }
            if content.trim().is_empty() && message.attachments.is_empty()
    )
}

/// Label for the live tail row — the same words the dashed placeholder bubble
/// showed before the block absorbed it.
pub fn live_placeholder_label(thought_summary: Option<&str>) -> String {
    match thought_summary.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => first_sentence(t, 200),
        None => "Considering…".to_string(),
    }
}

/// Classify a tool call for its row icon and phrasing.
pub fn step_kind(tc: &ToolCall) -> StepKind {
    if tc.status == ToolCallStatus::Error {
        return StepKind::Error;
    }
    if tc.tool_name == TERMINAL_EXEC_TOOL {
        StepKind::Command
    } else if tc.server_name == PLANNER_SERVER {
        StepKind::Todo
    } else {
        StepKind::Tool
    }
}

/// Clock-style elapsed time for the live header and row offsets: `m:ss`,
/// growing to `h:mm:ss` past an hour.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Human phrasing for a finished duration: `47s`, `1m 42s`, `1h 2m`.
pub fn format_worked(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The last `n` non-empty-trailing lines of a streaming command response, for
/// the live tail shown under a running command row.
pub fn command_tail(response: &str, n: usize) -> String {
    let lines: Vec<&str> = response.trim_end_matches('\n').lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Cost (USD) and total tokens summed across a run's messages.
pub fn run_usage(messages: &[Message]) -> (f64, i32) {
    messages
        .iter()
        .fold((0.0, 0), |(cost, tokens), m| match &m.usage {
            Some(u) => (cost + u.cost.unwrap_or(0.0), tokens + u.total_tokens),
            None => (cost, tokens),
        })
}

/// Whether the cost figure is shown for `UiState::token_display_mode`.
/// Unrecognised modes fall back to "all" (the field's own default).
fn show_cost_for_mode(mode: &str) -> bool {
    mode != "none" && mode != "tokens"
}

/// Whether the token-count figure is shown for `UiState::token_display_mode`.
fn show_tokens_for_mode(mode: &str) -> bool {
    mode != "none" && mode != "cost"
}

/// The block header's cost/token readout, honouring `token_display_mode`
/// ("all" | "tokens" | "cost" | "none"). `None` means the setting says to
/// show nothing, so the caller should render no readout at all rather than
/// an empty one.
pub fn format_usage_summary(mode: &str, cost: f64, tokens: i32) -> Option<String> {
    match (show_cost_for_mode(mode), show_tokens_for_mode(mode)) {
        (false, false) => None,
        (true, false) => Some(format!("${cost:.4}")),
        (false, true) => Some(format!("{:.1}k tok", tokens as f64 / 1000.0)),
        (true, true) => Some(format!("${cost:.4} · {:.1}k tok", tokens as f64 / 1000.0)),
    }
}

/// Whether a row's expanded detail is showing. Thinking rows start open when
/// `thinking_open_by_default` (from `UiState::default_tool_thought_open`) is
/// set; every other row starts closed. Either way, a manual click on the row
/// (its key landing in `toggled`) flips that starting state — so the setting
/// picks the default and the click is always a toggle away from it.
pub fn is_row_open(
    kind: StepKind,
    key: &str,
    thinking_open_by_default: bool,
    toggled: &std::collections::HashSet<String>,
) -> bool {
    let default_open = kind == StepKind::Thinking && thinking_open_by_default;
    default_open ^ toggled.contains(key)
}

/// Best-effort exit code out of a terminal response (JSON field or a trailing
/// `exit code: N` line). `None` when nothing parseable is there.
pub fn parse_exit_code(response: &str) -> Option<i64> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(response) {
        for key in ["exit_code", "exitCode", "exit_status", "code"] {
            if let Some(n) = v.get(key).and_then(|c| c.as_i64()) {
                return Some(n);
            }
        }
    }
    let lower = response.to_lowercase();
    let idx = lower.rfind("exit code")?;
    response[idx + "exit code".len()..]
        .trim_start_matches([':', ' ', '='])
        .split_whitespace()
        .next()?
        .trim_end_matches(['.', ','])
        .parse::<i64>()
        .ok()
}

/// Trailing `\n(exit code: N)` marker `HOBBES_TERMINAL_EXEC` appends to its
/// response (see `mcp/terminal_client.rs::exec`) — the exit code already
/// surfaces in the row's meta line, so the output pane strips it rather than
/// showing it twice.
fn strip_exit_marker(response: &str) -> &str {
    let trimmed = response.trim_end();
    if let Some(idx) = trimmed.rfind("\n(exit code:") {
        let tail = trimmed[idx..].trim_end();
        if tail.ends_with(')') {
            return trimmed[..idx].trim_end();
        }
    }
    trimmed
}

/// Replace a leading `home`-prefixed path with `~`, for a compact cwd in the
/// command meta line. Pure (takes `home` explicitly) so it's testable without
/// depending on the process environment.
pub fn shorten_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// The last `n` lines of `text` and the total line count, for a "Show all N
/// lines" affordance under a long output pane.
pub fn tail_lines(text: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = total.saturating_sub(n);
    (lines[start..].join("\n"), total)
}

/// The shape a tool result's raw text should render as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultShape {
    /// A single JSON object or array.
    Json,
    /// Every non-empty line parses as its own JSON value.
    Jsonl,
    /// Neither — markdown-ish or plain text.
    Text,
}

/// Classify a tool result for the "Result" pane. JSON wins over JSONL (a
/// single-line object is valid JSON *and* trivially "every line parses", so
/// the whole-document parse is tried first); JSONL requires at least two
/// non-empty lines that each parse on their own.
pub fn detect_result_shape(text: &str) -> ResultShape {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ResultShape::Text;
    }
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return ResultShape::Json;
    }
    let lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > 1
        && lines
            .iter()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
    {
        return ResultShape::Jsonl;
    }
    ResultShape::Text
}

/// Pretty-print each record of a JSONL blob. Only meaningful once
/// [`detect_result_shape`] has already said [`ResultShape::Jsonl`]; a line
/// that somehow fails to parse here is silently dropped rather than shown
/// raw, since a mixed pane reads worse than a short one.
pub fn pretty_jsonl(text: &str) -> Vec<String> {
    text.trim()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
        })
        .collect()
}

/// What a [`json_spans`] token is, for the pane's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonSpan {
    /// An object key — the quoted string immediately before a `:`.
    Key,
    /// A string value.
    Str,
    /// A number.
    Num,
    /// Everything else: punctuation, whitespace, `true`/`false`/`null`.
    Other,
}

/// A cheap, line-agnostic tokenisation of a `serde_json::to_string_pretty`
/// render, for inline key/string/number colouring without pulling in the
/// full syntax highlighter (built for fenced code blocks, not a hover-fast
/// scan of a small object). Not a JSON parser: it never fails, it just walks
/// character runs, so malformed input degrades to a single [`JsonSpan::Other`]
/// run rather than an error.
pub fn json_spans(pretty: &str) -> Vec<(JsonSpan, String)> {
    let chars: Vec<char> = pretty.chars().collect();
    let n = chars.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < n {
                if chars[i] == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            let kind = if j < n && chars[j] == ':' {
                JsonSpan::Key
            } else {
                JsonSpan::Str
            };
            spans.push((kind, s));
        } else if c.is_ascii_digit() || (c == '-' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let start = i;
            i += 1;
            while i < n && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-'))
            {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            spans.push((JsonSpan::Num, s));
        } else {
            let start = i;
            while i < n
                && chars[i] != '"'
                && !chars[i].is_ascii_digit()
                && !(chars[i] == '-' && i + 1 < n && chars[i + 1].is_ascii_digit())
            {
                i += 1;
            }
            if i == start {
                // Never happens given the branches above, but guarantees
                // forward progress if it somehow did.
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            spans.push((JsonSpan::Other, s));
        }
    }
    spans
}

/// A named string argument out of a tool call's JSON arguments.
pub fn arg_str(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(|s| s.to_string())
}

/// The first string-valued argument, as a stand-in for "the useful one" when
/// we don't know the tool's schema. Truncated so a row stays one line.
pub fn first_string_arg(arguments: &str, max_chars: usize) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let s = v
        .as_object()?
        .values()
        .find_map(|val| val.as_str())
        .filter(|s| !s.trim().is_empty())?;
    Some(truncate_chars(s, max_chars))
}

/// First sentence of a thinking summary, for a one-line thinking row.
/// Markdown is stripped first (see [`plain_text`]) so a summary opening with
/// bold/heading/list markup doesn't leave literal `**`/`#`/`- ` in the row.
pub fn first_sentence(text: &str, max_chars: usize) -> String {
    let text = plain_text(text);
    let text = text.trim();
    let end = text
        .find(['.', '!', '?', '\n'])
        .map(|i| i + 1)
        .unwrap_or(text.len());
    truncate_chars(text[..end].trim(), max_chars)
}

/// Strip lightweight markdown that would otherwise show up as literal
/// punctuation in a one-line label: `**bold**`/`__bold__`/`` `code` ``
/// wrappers around words, leading/trailing `*`/`_`/`#`/backtick runs, and a
/// leading list marker (`- `, `* `, `1. `). Applied to thinking labels only —
/// tool labels are already rendered in a mono font and don't carry markdown.
pub fn plain_text(s: &str) -> String {
    let mut s = s.trim();

    // Leading list marker: "- ", "* ", or "1. " / "12. " etc.
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
        s = rest;
    } else if let Some(dot) = s.find(". ") {
        if !s[..dot].is_empty() && s[..dot].chars().all(|c| c.is_ascii_digit()) {
            s = &s[dot + 2..];
        }
    }

    // Leading heading markers ("## Heading").
    let s = s.trim_start_matches('#').trim_start();

    // Bold/emphasis wrappers can appear anywhere in the (already
    // first-sentence-trimmed) text, not just wrapping the whole string — so
    // strip the marker pairs globally rather than only at the edges.
    let s = s.replace("**", "").replace("__", "").replace('`', "");

    s.trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '#'))
        .to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The one line of substance a step row shows.
pub fn step_label(tc: &ToolCall) -> String {
    match step_kind(tc) {
        StepKind::Command => arg_str(&tc.arguments, "command")
            .map(|c| truncate_chars(c.trim(), 160))
            .unwrap_or_else(|| tc.tool_name.clone()),
        StepKind::Todo => {
            let base = format!("{} · {}", tc.server_name, tc.tool_name);
            match arg_str(&tc.arguments, "title") {
                Some(t) if !t.trim().is_empty() => {
                    format!("{base} \"{}\"", truncate_chars(t.trim(), 80))
                }
                _ => base,
            }
        }
        _ => {
            let base = format!("{} · {}", tc.server_name, tc.tool_name);
            match first_string_arg(&tc.arguments, 80) {
                Some(a) => format!("{base} — {a}"),
                None => base,
            }
        }
    }
}

// ============================================================================
// Row model
// ============================================================================

/// One rendered line in the stream, flattened out of the run's messages.
#[derive(Clone, PartialEq)]
pub struct StepRow {
    pub message_id: Uuid,
    /// Stable per-row key: several rows can come off one message (a thinking
    /// row plus the tool row it was attached to).
    pub key: String,
    pub kind: StepKind,
    pub label: String,
    pub offset_secs: i64,
    /// `None` while the step is still running.
    pub duration_secs: Option<i64>,
    pub running: bool,
    pub tool_call: Option<ToolCall>,
    pub usage: Option<UsageData>,
    /// Full, untruncated text for a [`StepKind::Thinking`] row — `label` is
    /// clipped to a lead-in for the collapsed line, this is what a click (or
    /// `default_tool_thought_open`) reveals underneath it.
    pub full_text: Option<String>,
}

/// Flatten a run of messages into rows, pinning permission requests to the top
/// (a prompt waiting on the user is the only thing in the block that needs an
/// answer, so it must not be buried mid-list).
/// `live_now` is `Some(now)` only while the block is live; it turns a trailing
/// empty placeholder into the block's pulsing last row (offset measured from
/// `now`, no duration) instead of leaving it to render as a second bubble.
pub fn build_rows(
    messages: &[Message],
    turn_start: chrono::DateTime<chrono::Utc>,
    live_now: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<StepRow> {
    let mut pinned = Vec::new();
    let mut rows = Vec::new();
    // The tail placeholder, if this block is live and ends on one.
    let live_tail = live_now
        .and_then(|_| messages.len().checked_sub(1))
        .filter(|i| is_empty_placeholder(&messages[*i]));
    for (i, m) in messages.iter().enumerate() {
        let offset = (m.created_at - turn_start).num_seconds();
        if live_tail == Some(i) {
            let summary = match &m.content {
                MessageContent::Text { thought_summary, .. } => thought_summary.clone(),
                _ => None,
            };
            let now = live_now.unwrap_or(m.created_at);
            rows.push(StepRow {
                message_id: m.id,
                key: format!("{}:live", m.id),
                kind: StepKind::Thinking,
                label: live_placeholder_label(summary.as_deref()),
                offset_secs: (now - turn_start).num_seconds(),
                duration_secs: None,
                running: true,
                tool_call: None,
                usage: None,
                full_text: summary.clone(),
            });
            continue;
        }
        match &m.content {
            MessageContent::PermissionRequest(tc) => pinned.push(StepRow {
                message_id: m.id,
                key: format!("{}:perm", m.id),
                kind: StepKind::NeedsYou,
                label: step_label(tc),
                offset_secs: offset,
                duration_secs: None,
                running: false,
                tool_call: Some(tc.clone()),
                usage: m.usage.clone(),
                full_text: None,
            }),
            MessageContent::ToolCall(tc) => {
                if let Some(thought) = tc.thought_summary.as_ref().filter(|t| !t.trim().is_empty())
                {
                    rows.push(thinking_row(m.id, "tc", thought, offset));
                }
                let duration = tc
                    .completed_at
                    .map(|c| (c - m.created_at).num_seconds().max(0));
                rows.push(StepRow {
                    message_id: m.id,
                    key: format!("{}:{}", m.id, tc.execution_id),
                    kind: step_kind(tc),
                    label: step_label(tc),
                    offset_secs: offset,
                    duration_secs: duration,
                    running: tc.status == ToolCallStatus::Running,
                    tool_call: Some(tc.clone()),
                    usage: m.usage.clone(),
                    full_text: None,
                });
            }
            MessageContent::Text {
                thought_summary, ..
            } => {
                if let Some(thought) = thought_summary.as_ref().filter(|t| !t.trim().is_empty()) {
                    rows.push(thinking_row(m.id, "text", thought, offset));
                }
            }
            _ => {}
        }
    }
    pinned.extend(rows);
    pinned
}

fn thinking_row(id: Uuid, tag: &str, thought: &str, offset: i64) -> StepRow {
    StepRow {
        message_id: id,
        key: format!("{id}:think:{tag}"),
        kind: StepKind::Thinking,
        label: first_sentence(thought, 200),
        offset_secs: offset,
        duration_secs: None,
        running: false,
        tool_call: None,
        usage: None,
        full_text: Some(thought.to_string()),
    }
}

/// Counts used by the finished header: total steps (thinking excluded),
/// commands, todos, failures.
pub fn row_counts(rows: &[StepRow]) -> (usize, usize, usize, usize) {
    let mut steps = 0;
    let mut commands = 0;
    let mut todos = 0;
    let mut failed = 0;
    for r in rows {
        match r.kind {
            StepKind::Thinking => continue,
            StepKind::Command => commands += 1,
            StepKind::Todo => todos += 1,
            StepKind::Error => failed += 1,
            _ => {}
        }
        steps += 1;
    }
    (steps, commands, todos, failed)
}

/// Header provenance for a run: profile name, profile colour class, and the
/// `"{connector} {model}"` label — resolved from the FIRST tool-call or
/// permission-request message in the run that carries turn-time stamps (see
/// `ToolCall::with_turn_provenance`). This is what keeps a finished turn's
/// label stable across a later profile/connector switch: the block reads
/// what it actually ran under, not the session's current pins.
///
/// Falls back to the session's *current* pins (`fallback_*`) for legacy runs
/// that predate stamping, where nothing in the run carries provenance.
pub fn run_header_provenance(
    messages: &[Message],
    settings: &crate::settings::Settings,
    fallback_profile_name: &str,
    fallback_profile_color: &str,
    fallback_connector_label: &str,
) -> (String, String, String) {
    let stamped = messages.iter().find_map(|m| {
        let tc = match &m.content {
            MessageContent::ToolCall(tc) | MessageContent::PermissionRequest(tc) => Some(tc),
            _ => None,
        }?;
        (tc.profile_id.is_some() || tc.connector_name.is_some() || tc.model.is_some()).then(|| {
            (
                tc.profile_id.clone(),
                tc.connector_name.clone(),
                tc.model.clone(),
            )
        })
    });

    match stamped {
        Some((profile_id, connector_name, model)) => {
            let profile = profile_id
                .as_ref()
                .and_then(|id| settings.composio_profiles.iter().find(|p| &p.id == id));
            let name = profile
                .map(|p| p.name.clone())
                .unwrap_or_else(|| fallback_profile_name.to_string());
            let color = profile
                .map(|p| p.color.clone())
                .unwrap_or_else(|| fallback_profile_color.to_string());
            let connector = connector_name.unwrap_or_else(|| "no connector".to_string());
            let model = model.unwrap_or_default();
            (name, color, format!("{connector} {model}"))
        }
        None => (
            fallback_profile_name.to_string(),
            fallback_profile_color.to_string(),
            fallback_connector_label.to_string(),
        ),
    }
}

// ============================================================================
// Component
// ============================================================================

/// Which rows the finished, expanded block shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowFilter {
    All,
    Commands,
    Thinking,
}

#[derive(Props, Clone, PartialEq)]
pub struct ActivityStreamProps {
    /// The run of messages making up one turn's activity.
    pub messages: Vec<Message>,
    pub session_id: String,
    /// The turn is still in flight: header ticks, block stays expanded.
    pub is_live: bool,
    /// Tailwind bg class of this run's provenance profile, painted as the
    /// block's 3px left edge (border-color utilities don't exist for it).
    /// Resolved by the caller via [`run_header_provenance`], so it reflects
    /// the profile pinned when the turn actually ran, not the session's
    /// current pins.
    pub profile_color: String,
    /// Header provenance for this run, resolved by the caller via
    /// [`run_header_provenance`] from the run's own stamped tool calls
    /// (falling back to the session's current pins for legacy history).
    pub profile_name: String,
    pub connector_label: String,
    pub on_stop: EventHandler<()>,
    /// Whether the caption carries its time; the list drops it when the
    /// previous caption shares this author and minute.
    #[props(default = true)]
    pub show_time: bool,
}

#[component]
pub fn ActivityStream(props: ActivityStreamProps) -> Element {
    let ExpansionStateContext(mut expansion_state) = use_context::<ExpansionStateContext>();
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    let mut filter = use_signal(|| RowFilter::All);
    let mut open_rows = use_signal(std::collections::HashSet::<String>::new);
    // Bumped once a second while live so elapsed times re-render; see Ticker.
    let tick = use_signal(|| 0u32);

    let messages = props.messages.clone();
    if messages.is_empty() {
        return rsx! {};
    }
    let turn_start = messages[0].created_at;
    let block_key = format!("activity:{}", messages[0].id);

    // Read the tick so the live header/rows re-render every second.
    let _ = tick.read();
    let now = chrono::Utc::now();

    let rows = build_rows(&messages, turn_start, props.is_live.then_some(now));
    let (steps, commands, todos, failed) = row_counts(&rows);
    let (cost, tokens) = run_usage(&messages);
    let token_mode = ui_state.read().token_display_mode.clone();
    let thinking_open_by_default = ui_state.read().default_tool_thought_open;

    let last_activity = messages
        .iter()
        .map(|m| match &m.content {
            MessageContent::ToolCall(tc) => tc.completed_at.unwrap_or(m.created_at),
            _ => m.created_at,
        })
        .max()
        .unwrap_or(turn_start);
    let elapsed_secs = if props.is_live {
        (now - turn_start).num_seconds()
    } else {
        (last_activity - turn_start).num_seconds()
    };

    let cancelled = !props.is_live && rows.iter().any(|r| r.running);
    let warn = failed > 0 || cancelled;

    // A live block is always expanded; a finished one remembers the user's
    // choice across re-renders, defaulting to collapsed.
    let expanded = props.is_live
        || expansion_state
            .read()
            .get(&block_key)
            .copied()
            .unwrap_or(false);

    let border_class = if warn {
        "border-yellow-700/50"
    } else if props.is_live {
        "border-subtle"
    } else {
        "border-faint"
    };

    // The absorbed live placeholder is a *thinking* row, not work in flight —
    // the header says plain "thinking" in that state rather than echoing the
    // row's label.
    let running_label = rows
        .iter()
        .find(|r| r.running && r.kind != StepKind::Thinking)
        .map(|r| r.label.clone())
        .unwrap_or_else(|| "thinking".to_string());

    let visible_rows: Vec<StepRow> = if props.is_live || *filter.read() == RowFilter::All {
        rows.clone()
    } else {
        let want = *filter.read();
        rows.iter()
            .filter(|r| match want {
                RowFilter::Commands => r.kind == StepKind::Command,
                RowFilter::Thinking => r.kind == StepKind::Thinking,
                RowFilter::All => true,
            })
            .cloned()
            .collect()
    };

    let toggle_key = block_key.clone();
    let header_height = if expanded { "h-9" } else { "h-8" };
    let live_usage_text = match format_usage_summary(&token_mode, cost, tokens) {
        Some(usage) => format!("{steps} steps · {usage}"),
        None => format!("{steps} steps"),
    };
    let finished_usage_text = format_usage_summary(&token_mode, cost, tokens);

    rsx! {
        div {
            class: "flex justify-start w-full",
            div {
                class: "flex flex-col max-w-full min-w-0 w-full",
                div {
                    class: "flex overflow-hidden rounded-lg border {border_class} bg-section",
                    // Profile edge: a 3px bar, because the profile colour only
                    // exists as a Tailwind *background* utility.
                    div { class: "w-[3px] shrink-0 {props.profile_color}" }
                    div {
                        class: "flex flex-col min-w-0 flex-1",

                        // ---- Header -------------------------------------
                        div {
                            class: "flex items-center gap-2 px-3 text-xs {header_height} cursor-pointer select-none",
                            onclick: move |_| {
                                let current = expansion_state.read().get(&toggle_key).copied().unwrap_or(false);
                                expansion_state.write().insert(toggle_key.clone(), !current);
                            },
                            if props.is_live {
                                Icon {
                                    width: 14,
                                    height: 14,
                                    class: "animate-pulse shrink-0".to_string(),
                                    icon: crate::components::pixel_icons::HobbesTiger,
                                }
                                span { class: "font-medium text-fg shrink-0", "Working" }
                                span {
                                    class: "font-mono tabular-nums text-fg shrink-0",
                                    "{format_elapsed(elapsed_secs)}"
                                }
                                span {
                                    class: "text-fg-muted truncate min-w-0",
                                    "as {props.profile_name} · {props.connector_label} · {running_label}"
                                }
                                div {
                                    class: "ml-auto flex items-center gap-2 shrink-0",
                                    span {
                                        class: "font-mono text-fg-muted",
                                        "{live_usage_text}"
                                    }
                                    button {
                                        class: "px-2 py-0.5 rounded border border-faint text-fg-muted hover:text-fg hover:bg-card transition-colors",
                                        onclick: move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            props.on_stop.call(());
                                        },
                                        "Stop"
                                    }
                                }
                            } else {
                                if warn {
                                    Icon {
                                        width: 14,
                                        height: 14,
                                        class: "shrink-0".to_string(),
                                        fill: "#eab308".to_string(),
                                        icon: fi_icons::FiAlertCircle,
                                    }
                                } else {
                                    Icon {
                                        width: 14,
                                        height: 14,
                                        class: "shrink-0".to_string(),
                                        fill: "#86efac".to_string(),
                                        icon: fi_icons::FiCheck,
                                    }
                                }
                                span { class: "font-medium text-fg shrink-0", "Worked {format_worked(elapsed_secs)}" }
                                span {
                                    class: "text-fg-muted truncate min-w-0",
                                    "{summary_counts(steps, commands, todos)}"
                                }
                                if failed > 0 {
                                    span { class: "text-yellow-500 shrink-0", "{failed} failed" }
                                } else if cancelled {
                                    span { class: "text-yellow-500 shrink-0", "cancelled" }
                                }
                                if let Some(usage_text) = &finished_usage_text {
                                    div {
                                        class: "ml-auto flex items-center gap-2 shrink-0 font-mono text-fg-muted",
                                        "{usage_text}"
                                    }
                                }
                            }
                            if expanded {
                                Icon {
                                    width: 14, height: 14,
                                    class: "shrink-0 text-fg-muted".to_string(),
                                    icon: fi_icons::FiChevronDown,
                                }
                            } else {
                                Icon {
                                    width: 14, height: 14,
                                    class: "shrink-0 text-fg-muted".to_string(),
                                    icon: fi_icons::FiChevronRight,
                                }
                            }
                        }

                        // ---- Filters (finished + expanded only) ---------
                        if expanded && !props.is_live {
                            div {
                                class: "flex items-center gap-1 px-3 pb-1 text-[11px]",
                                for (label, want) in [("All", RowFilter::All), ("Commands", RowFilter::Commands), ("Thinking", RowFilter::Thinking)] {
                                    button {
                                        key: "{label}",
                                        class: filter_class(*filter.read() == want),
                                        onclick: move |_| filter.set(want),
                                        "{label}"
                                    }
                                }
                            }
                        }

                        // ---- Rows ---------------------------------------
                        if expanded {
                            div {
                                class: "flex flex-col pb-1",
                                for row in visible_rows {
                                    ActivityRow {
                                        key: "{row.key}",
                                        row: row.clone(),
                                        session_id: props.session_id.clone(),
                                        open: is_row_open(
                                            row.kind,
                                            &row.key,
                                            thinking_open_by_default,
                                            &open_rows.read(),
                                        ),
                                        on_toggle: {
                                            let key = row.key.clone();
                                            move |_| {
                                                let mut set = open_rows.write();
                                                if !set.remove(&key) {
                                                    set.insert(key.clone());
                                                }
                                            }
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
                crate::components::shared::MessageCaption {
                    label: "Hobbes".to_string(),
                    created_at: messages[0].created_at,
                    show_time: props.show_time,
                }
            }
        }
        if props.is_live {
            Ticker { tick }
        }
    }
}

fn filter_class(selected: bool) -> &'static str {
    if selected {
        "px-2 py-0.5 rounded bg-btn-primary text-fg"
    } else {
        "px-2 py-0.5 rounded border border-faint text-fg-muted hover:text-fg"
    }
}

fn summary_counts(steps: usize, commands: usize, todos: usize) -> String {
    let mut parts = vec![format!("{steps} steps")];
    if commands > 0 {
        parts.push(format!("{commands} commands"));
    }
    if todos > 0 {
        parts.push(format!("{todos} todos"));
    }
    parts.join(" · ")
}

/// Renders nothing; exists only so the once-a-second re-render loop is tied to
/// the *mounted lifetime* of a live block. Unmounting it (the turn finished)
/// drops the future, so a finished block costs no timer.
#[component]
fn Ticker(tick: Signal<u32>) -> Element {
    use_future(move || async move {
        let mut tick = tick;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            tick += 1;
        }
    });
    rsx! {}
}

#[derive(Props, Clone, PartialEq)]
struct ActivityRowProps {
    row: StepRow,
    session_id: String,
    open: bool,
    on_toggle: EventHandler<()>,
}

#[component]
fn ActivityRow(props: ActivityRowProps) -> Element {
    let row = props.row.clone();
    let is_permission = row.kind == StepKind::NeedsYou;

    // Permission prompts keep their own full-fidelity UI (approve/deny plus
    // trust-rule authoring); the stream only pins them to the top.
    if is_permission {
        let tc = row.tool_call.clone().unwrap_or_default();
        return rsx! {
            div {
                class: "px-3 py-2",
                PermissionPrompt { tool_call: tc, session_id: props.session_id.clone() }
            }
        };
    }

    let duration_text = match row.duration_secs {
        Some(s) => format_elapsed(s),
        None if row.running => String::new(),
        None => String::new(),
    };
    let row_bg: &str = if row.running {
        "bg-card"
    } else {
        "hover:bg-section"
    };
    let icon_pulse: &str = if row.running { "animate-pulse" } else { "" };
    let label_class: &str = match row.kind {
        StepKind::Thinking => "truncate italic text-fg-muted",
        StepKind::Command => "truncate font-mono text-fg",
        _ => "truncate text-fg",
    };

    let tail = row
        .tool_call
        .as_ref()
        .filter(|tc| row.running && step_kind(tc) == StepKind::Command)
        .map(|tc| command_tail(&tc.response, 5))
        .filter(|t| !t.trim().is_empty());

    rsx! {
        div {
            class: "flex flex-col",
            div {
                class: "grid grid-cols-[40px_20px_1fr_56px] gap-[10px] items-center px-3 h-7 text-xs cursor-pointer {row_bg}",
                onclick: move |_| props.on_toggle.call(()),
                span { class: "font-mono text-fg-muted text-[11px]", "{format_elapsed(row.offset_secs)}" }
                span {
                    class: "flex items-center justify-center {icon_pulse}",
                    {step_icon(row.kind, row.running)}
                }
                span { class: "{label_class}", "{row.label}" }
                span { class: "font-mono text-fg-muted text-[11px] text-right", "{duration_text}" }
            }
            if let Some(tail) = tail {
                pre {
                    class: "mx-3 mb-1 px-2 py-1 rounded bg-app font-mono text-[11px] text-fg-muted max-h-[88px] overflow-hidden whitespace-pre-wrap break-all",
                    "{tail}"
                }
            }
            if props.open {
                if let Some(tc) = row.tool_call.clone() {
                    RowDetail {
                        tool_call: tc,
                        usage: row.usage.clone(),
                        kind: row.kind,
                        duration_secs: row.duration_secs,
                        running: row.running,
                    }
                } else if row.kind == StepKind::Thinking {
                    if let Some(full) = row.full_text.clone().filter(|t| !t.trim().is_empty()) {
                        div {
                            class: "px-3 pb-2",
                            blockquote {
                                class: "border-l-2 border-l-faint pl-3 text-xs text-fg-muted italic whitespace-pre-wrap",
                                "{full}"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn step_icon(kind: StepKind, running: bool) -> Element {
    let running_fill = "#84A4D1".to_string();
    match kind {
        StepKind::Command => rsx! {
            Icon {
                width: 14, height: 14,
                fill: if running { running_fill } else { "#86efac".to_string() },
                icon: fi_icons::FiTerminal,
            }
        },
        StepKind::Todo => rsx! {
            Icon {
                width: 14, height: 14,
                fill: if running { running_fill } else { "currentColor".to_string() },
                icon: fi_icons::FiCheckSquare,
            }
        },
        StepKind::Error => rsx! {
            Icon { width: 14, height: 14, fill: "#ef4444".to_string(), icon: fi_icons::FiXCircle }
        },
        StepKind::Thinking => rsx! {
            Icon { width: 14, height: 14, class: "text-fg-muted".to_string(), icon: fi_icons::FiCpu }
        },
        StepKind::NeedsYou => rsx! {
            Icon { width: 14, height: 14, fill: "#eab308".to_string(), icon: fi_icons::FiAlertCircle }
        },
        StepKind::Tool => rsx! {
            Icon {
                width: 14, height: 14,
                fill: if running { running_fill } else { "currentColor".to_string() },
                icon: fi_icons::FiBox,
            }
        },
    }
}

/// Meta line for a COMMAND row: `server · exit N · duration · cwd … · timeout
/// …s · cost/tokens`, with the cwd and timeout parts only present when the
/// arguments carry them (and the timeout only when it isn't the tool's own
/// default).
fn command_meta_line(
    tool_call: &ToolCall,
    usage: Option<&UsageData>,
    duration_secs: Option<i64>,
    token_mode: &str,
) -> String {
    let mut parts = vec![tool_call.server_name.clone()];
    if let Some(code) = parse_exit_code(&tool_call.response) {
        parts.push(format!("exit {code}"));
    }
    if let Some(d) = duration_secs {
        parts.push(format_worked(d));
    }
    if let Some(cwd) = arg_str(&tool_call.arguments, "cwd") {
        let home = std::env::var("HOME").unwrap_or_default();
        parts.push(format!("cwd {}", shorten_home(&cwd, &home)));
    }
    if let Some(timeout) = arg_i64(&tool_call.arguments, "timeout_secs") {
        if timeout != DEFAULT_EXEC_TIMEOUT_SECS {
            parts.push(format!("timeout {timeout}s"));
        }
    }
    push_usage_parts(&mut parts, usage, token_mode);
    parts.join(" · ")
}

/// Meta line for a TOOL/planner row: `server · TOOL_NAME · duration ·
/// cost/tokens`.
fn tool_meta_line(
    tool_call: &ToolCall,
    usage: Option<&UsageData>,
    duration_secs: Option<i64>,
    token_mode: &str,
) -> String {
    let mut parts = vec![tool_call.server_name.clone(), tool_call.tool_name.clone()];
    if let Some(d) = duration_secs {
        parts.push(format_elapsed(d));
    }
    push_usage_parts(&mut parts, usage, token_mode);
    parts.join(" · ")
}

fn push_usage_parts(parts: &mut Vec<String>, usage: Option<&UsageData>, token_mode: &str) {
    let Some(u) = usage else { return };
    if show_cost_for_mode(token_mode) {
        if let Some(c) = u.cost {
            parts.push(format!("${c:.4}"));
        }
    }
    if show_tokens_for_mode(token_mode) {
        parts.push(format!("{:.1}k tok", u.total_tokens as f64 / 1000.0));
    }
}

/// A named integer argument out of a tool call's JSON arguments.
fn arg_i64(arguments: &str, key: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_i64()
}

fn json_span_class(kind: JsonSpan) -> &'static str {
    match kind {
        JsonSpan::Key => "text-primary-300",
        JsonSpan::Str => "text-fg",
        JsonSpan::Num => "text-accent-300",
        JsonSpan::Other => "text-fg-muted",
    }
}

/// A `bg-app` pane rendering pretty-printed JSON with cheap key/string/number
/// colouring (see [`json_spans`]).
#[component]
fn JsonPane(pretty: String) -> Element {
    rsx! {
        div {
            class: "bg-app border border-faint rounded-md p-2.5 font-mono text-[11px] leading-[1.55] whitespace-pre-wrap break-words max-h-[320px] overflow-auto",
            for (kind, text) in json_spans(&pretty) {
                span { class: json_span_class(kind), "{text}" }
            }
        }
    }
}

/// The "Result" pane for a TOOL row, shaped by [`detect_result_shape`]: a
/// JSON object/array pretty-prints as one [`JsonPane`]; JSONL pretty-prints
/// each record with a thin rule between them, capped at
/// [`JSONL_RECORD_CAP`] with a "Show all N records" expander; anything else
/// renders as plain pre-wrapped text.
#[component]
fn ResultPane(response: String) -> Element {
    let mut show_all = use_signal(|| false);
    match detect_result_shape(&response) {
        ResultShape::Json => {
            let pretty = serde_json::from_str::<serde_json::Value>(response.trim())
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| response.clone());
            rsx! { JsonPane { pretty } }
        }
        ResultShape::Jsonl => {
            let records = pretty_jsonl(&response);
            let total = records.len();
            let show_now = *show_all.read();
            let visible: Vec<String> = if show_now {
                records
            } else {
                records.into_iter().take(JSONL_RECORD_CAP).collect()
            };
            rsx! {
                div {
                    class: "bg-app border border-faint rounded-md font-mono text-[11px] leading-[1.55] text-fg max-h-[320px] overflow-auto",
                    if total > JSONL_RECORD_CAP && !show_now {
                        button {
                            class: "block text-[11px] text-primary-300 hover:text-fg px-2 pt-2",
                            onclick: move |_| show_all.set(true),
                            "Show all {total} records"
                        }
                    }
                    for (i, record) in visible.iter().enumerate() {
                        div {
                            key: "{i}",
                            class: if i > 0 { "px-2 py-1.5 border-t border-faint" } else { "px-2 py-1.5" },
                            for (kind, text) in json_spans(record) {
                                span { class: json_span_class(kind), "{text}" }
                            }
                        }
                    }
                }
            }
        }
        ResultShape::Text => rsx! {
            div {
                class: "bg-app border border-faint rounded-md p-2.5 font-mono text-[11px] leading-[1.55] whitespace-pre-wrap break-words max-h-[320px] overflow-auto text-fg",
                "{response}"
            }
        },
    }
}

/// COMMAND row detail: meta line + copy buttons, then a single terminal-style
/// output pane (no separate Arguments block — the row's label already is the
/// command).
#[component]
fn CommandRowDetail(
    tool_call: ToolCall,
    usage: Option<UsageData>,
    duration_secs: Option<i64>,
    running: bool,
) -> Element {
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    let token_mode = ui_state.read().token_display_mode.clone();
    let mut show_all = use_signal(|| false);

    let meta = command_meta_line(&tool_call, usage.as_ref(), duration_secs, &token_mode);
    let output = strip_exit_marker(&tool_call.response).to_string();
    let is_error = parse_exit_code(&tool_call.response).is_some_and(|c| c != 0);
    let border_extra = if running {
        "border-l-2 border-l-accent-500"
    } else if is_error {
        "border-l-2 border-l-red-500"
    } else {
        ""
    };

    let (tail, total_lines) = tail_lines(&output, COMMAND_TAIL_LINES);
    let show_now = *show_all.read();
    let shown = if show_now || total_lines <= COMMAND_TAIL_LINES {
        output.clone()
    } else {
        tail
    };

    let command = arg_str(&tool_call.arguments, "command").unwrap_or_default();
    let command_for_copy = command.clone();
    let output_for_copy = output.clone();

    rsx! {
        div {
            class: "px-3 pb-2 flex flex-col gap-1",
            div {
                class: "flex items-center gap-2",
                div { class: "text-[11px] text-fg-muted font-mono truncate flex-1", "{meta}" }
                if !output.trim().is_empty() {
                    button {
                        class: "text-[11px] text-primary-300 hover:text-fg shrink-0",
                        onclick: move |_| { let _ = copy_to_clipboard(&output_for_copy); },
                        "Copy output"
                    }
                }
                if !command.trim().is_empty() {
                    button {
                        class: "text-[11px] text-primary-300 hover:text-fg shrink-0",
                        onclick: move |_| { let _ = copy_to_clipboard(&command_for_copy); },
                        "Copy command"
                    }
                }
            }
            div {
                class: "bg-app border border-faint rounded-md p-2.5 font-mono text-[11px] leading-[1.55] whitespace-pre-wrap break-words max-h-[320px] overflow-auto text-fg {border_extra}",
                if output.trim().is_empty() {
                    span { class: "text-fg-muted", "(no output)" }
                } else {
                    if total_lines > COMMAND_TAIL_LINES && !show_now {
                        button {
                            class: "block text-[11px] text-primary-300 hover:text-fg mb-1",
                            onclick: move |_| show_all.set(true),
                            "Show all {total_lines} lines"
                        }
                    }
                    "{shown}"
                }
            }
        }
    }
}

/// TOOL/planner row detail: meta line + copy, the result (summary first when
/// one exists, raw result tucked under a closed toggle), then Arguments
/// collapsed by default — except for planner rows, opened by default since
/// "what did it set" is the question there.
#[component]
fn ToolRowDetail(
    tool_call: ToolCall,
    usage: Option<UsageData>,
    duration_secs: Option<i64>,
    is_planner: bool,
    arguments_open_default: bool,
    token_mode: String,
) -> Element {
    let meta = tool_meta_line(&tool_call, usage.as_ref(), duration_secs, &token_mode);
    let response = tool_call.response.clone();
    let response_for_copy = response.clone();
    let has_summary = tool_call
        .result_summary
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let summary_text = tool_call.result_summary.clone().unwrap_or_default();
    let pretty_args = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| tool_call.arguments.clone());
    let arguments_open = arguments_open_default || is_planner;

    rsx! {
        div {
            class: "px-3 pb-2 flex flex-col gap-1.5",
            div {
                class: "flex items-center gap-2",
                div { class: "text-[11px] text-fg-muted font-mono truncate flex-1", "{meta}" }
                if !response.trim().is_empty() {
                    button {
                        class: "text-[11px] text-primary-300 hover:text-fg shrink-0",
                        onclick: move |_| { let _ = copy_to_clipboard(&response_for_copy); },
                        "Copy result"
                    }
                }
            }
            if has_summary {
                div {
                    class: "flex flex-col gap-1",
                    span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Summary" }
                    p { class: "text-xs text-fg", "{summary_text}" }
                }
                if !response.trim().is_empty() {
                    Collapsible {
                        label: "Raw result".to_string(),
                        open_default: false,
                        ResultPane { response: response.clone() }
                    }
                }
            } else if !response.trim().is_empty() {
                div {
                    class: "flex flex-col gap-1",
                    span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Result" }
                    ResultPane { response: response.clone() }
                }
            }
            Collapsible {
                label: "Arguments".to_string(),
                open_default: arguments_open,
                JsonPane { pretty: pretty_args }
            }
        }
    }
}

/// The inline expansion under a clicked row, shaped by [`StepKind`]: a
/// command shows its terminal output (no separate Arguments — the row label
/// already is the command); a tool/planner call shows its result first,
/// arguments tucked away; an errored or auth-required call gets the same red
/// framing regardless of kind.
#[component]
fn RowDetail(
    tool_call: ToolCall,
    usage: Option<UsageData>,
    kind: StepKind,
    duration_secs: Option<i64>,
    running: bool,
) -> Element {
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    let token_mode = ui_state.read().token_display_mode.clone();
    let show_arguments_default = ui_state.read().default_tool_arguments_open;

    if tool_call.status == ToolCallStatus::AuthRequired {
        let meta = tool_meta_line(&tool_call, usage.as_ref(), duration_secs, &token_mode);
        return rsx! {
            div {
                class: "px-3 pb-2 flex flex-col gap-1",
                div { class: "text-[11px] text-red-400 font-mono truncate", "{meta}" }
                AuthRequiredPanel { url: tool_call.response.clone() }
            }
        };
    }

    if tool_call.status == ToolCallStatus::Error {
        let meta = if kind == StepKind::Command {
            command_meta_line(&tool_call, usage.as_ref(), duration_secs, &token_mode)
        } else {
            tool_meta_line(&tool_call, usage.as_ref(), duration_secs, &token_mode)
        };
        let text = strip_exit_marker(&tool_call.response).to_string();
        return rsx! {
            div {
                class: "px-3 pb-2 flex flex-col gap-1",
                div { class: "text-[11px] text-red-400 font-mono truncate", "{meta}" }
                if !text.trim().is_empty() {
                    div {
                        class: "bg-app border border-faint border-l-2 border-l-red-500 rounded-md p-2.5 font-mono text-[11px] leading-[1.55] whitespace-pre-wrap break-words max-h-[320px] overflow-auto text-fg",
                        "{text}"
                    }
                }
            }
        };
    }

    if kind == StepKind::Command {
        return rsx! {
            CommandRowDetail { tool_call, usage, duration_secs, running }
        };
    }

    rsx! {
        ToolRowDetail {
            tool_call,
            usage,
            duration_secs,
            is_planner: kind == StepKind::Todo,
            arguments_open_default: show_arguments_default,
            token_mode,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shared::MessageContent;
    use chrono::{Duration, Utc};

    fn msg(author: &str, content: MessageContent) -> Message {
        Message {
            id: Uuid::new_v4(),
            author: author.to_string(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn text(author: &str, body: &str, thought: Option<&str>) -> Message {
        msg(
            author,
            MessageContent::Text {
                content: body.to_string(),
                thought_signature: None,
                thought_summary: thought.map(|t| t.to_string()),
                signature_provider: None,
            },
        )
    }

    fn tool(server: &str, name: &str, args: &str, status: ToolCallStatus) -> ToolCall {
        ToolCall {
            execution_id: Uuid::new_v4().to_string(),
            server_name: server.to_string(),
            tool_name: name.to_string(),
            arguments: args.to_string(),
            status,
            response: String::new(),
            thought_signature: None,
            thought_summary: None,
            signature_provider: None,
            cached_image_path: None,
            result_summary: None,
            completed_at: None,
            profile_id: None,
            connector_name: None,
            model: None,
            round_id: None,
        }
    }

    fn tool_msg(tc: ToolCall) -> Message {
        msg("Hobbes", MessageContent::ToolCall(tc))
    }

    // ---- group_turn_runs ----

    /// While the turn is live, the dashed "Considering…" placeholder bubble is
    /// the block's last row rather than a second bubble under it — and once
    /// prose lands in it, it leaves the run again (covered below).
    #[test]
    fn live_run_absorbs_the_trailing_streaming_placeholder() {
        let t0 = Utc::now();
        let mut user = text("User", "go", None);
        user.created_at = t0;
        let mut call = tool_msg(tool(
            "hobbes-terminal",
            TERMINAL_EXEC_TOOL,
            r#"{"command":"date"}"#,
            ToolCallStatus::Completed,
        ));
        call.created_at = t0 + Duration::seconds(1);
        // The continuation's placeholder: empty, still being streamed into.
        let mut placeholder = text("Hobbes", "", None);
        placeholder.created_at = t0 + Duration::seconds(2);
        let placeholder_id = placeholder.id;
        let messages = vec![user, call, placeholder];

        let mut runs = group_turn_runs(&messages);
        assert_eq!(runs, vec![1..2], "a bare placeholder is not an anchor");
        absorb_live_placeholder(&messages, &mut runs);
        assert_eq!(runs, vec![1..3], "the live block swallows the placeholder");
        // Nothing is left over for message_list to render as a Single.
        assert_eq!(runs[0].end, messages.len());

        let run = &messages[runs[0].clone()];
        let now = t0 + Duration::seconds(5);
        let rows = build_rows(run, run[0].created_at, Some(now));
        let last = rows.last().expect("the placeholder row");
        assert_eq!(last.message_id, placeholder_id);
        assert_eq!(last.kind, StepKind::Thinking);
        assert_eq!(last.label, "Considering…");
        assert!(last.running, "the row pulses while the turn thinks");
        assert_eq!(last.duration_secs, None);
        assert_eq!(last.offset_secs, 4, "offset is measured from now");
        // It is a live-only row: a finished block never grows it.
        assert_eq!(build_rows(run, run[0].created_at, None).len(), rows.len() - 1);
        // Steps counts ignore thinking, so the header still reads one step.
        assert_eq!(row_counts(&rows).0, 1);
    }

    /// Once real prose streams into the placeholder it is a `Break` again, so
    /// it stays out of the run and renders as the normal bubble below it.
    #[test]
    fn a_placeholder_with_prose_is_not_absorbed() {
        let messages = vec![
            text("User", "go", None),
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            text("Hobbes", "here you go", None),
        ];
        let mut runs = group_turn_runs(&messages);
        absorb_live_placeholder(&messages, &mut runs);
        assert_eq!(runs, vec![1..2], "prose ends the run, it never joins it");
    }

    /// The absorbed row shows what the bubble showed: the thinking summary's
    /// first sentence once there is one, "Considering…" until then.
    #[test]
    fn live_placeholder_label_follows_the_thinking_summary() {
        assert_eq!(live_placeholder_label(None), "Considering…");
        assert_eq!(live_placeholder_label(Some("  ")), "Considering…");
        assert_eq!(
            live_placeholder_label(Some("**Checking** the calendar. Then the todos.")),
            "Checking the calendar."
        );
    }

    /// Regression: the activity block used to render BELOW the reply it
    /// produced. A turn whose stream emits prose *and* a tool call leaves the
    /// reply in the placeholder pushed at turn start, so the tool call (minted
    /// later) sorts after it. Finalization now moves the finished reply past
    /// the turn's calls, and the run must group ahead of it.
    #[test]
    fn reply_finalized_after_tool_calls_renders_below_the_run() {
        let t0 = Utc::now();
        let mut user = text("User", "what's on today?", None);
        user.created_at = t0;
        // Pushed at turn start, still empty while the stream runs.
        let mut placeholder = text("Hobbes", "", None);
        placeholder.created_at = t0 + Duration::seconds(1);
        let placeholder_id = placeholder.id;
        // Minted when the stream emitted the call — after the placeholder.
        let mut call = tool_msg(tool(
            "hobbes-terminal",
            TERMINAL_EXEC_TOOL,
            r#"{"command":"date"}"#,
            ToolCallStatus::Completed,
        ));
        call.created_at = t0 + Duration::seconds(3);
        let call_id = call.id;

        let mut messages = vec![user, placeholder, call];
        // The prose that finalization writes into the placeholder.
        if let MessageContent::Text { content, .. } = &mut messages[1].content {
            *content = "You have three things today.".to_string();
        }

        // Before the fix: the reply sits at index 1, so the run is index 2 —
        // the block renders under the answer.
        assert_eq!(group_turn_runs(&messages), vec![2..3]);

        // Finalization moves the reply past the turn's tool calls.
        crate::session::move_message_to_end(
            &mut messages,
            &placeholder_id,
            t0 + Duration::seconds(4),
        )
        .expect("placeholder is in the vec");

        // message_list sorts by created_at before grouping; vec order and
        // timestamps agree, so the sort is a no-op here.
        let mut sorted = messages.clone();
        sorted.sort_by_key(|m| m.created_at);
        assert_eq!(
            sorted.iter().map(|m| m.id).collect::<Vec<_>>(),
            messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            "vec order must already be created_at order"
        );
        assert_eq!(sorted[1].id, call_id);
        assert_eq!(sorted[2].id, placeholder_id);
        assert_eq!(group_turn_runs(&sorted), vec![1..2]);

        // And the rows built for that run measure from the call itself.
        let rows = build_rows(&sorted[1..2], sorted[1].created_at, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].offset_secs, 0);
    }

    #[test]
    fn run_between_two_user_messages_is_grouped() {
        let messages = vec![
            text("User", "do a thing", None),
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            tool_msg(tool("srv", "B", "{}", ToolCallStatus::Completed)),
            text("Hobbes", "done", None),
            text("User", "next", None),
        ];
        assert_eq!(group_turn_runs(&messages), vec![1..3]);
    }

    #[test]
    fn run_at_the_tail_is_grouped() {
        let messages = vec![
            text("User", "go", None),
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Running)),
        ];
        assert_eq!(group_turn_runs(&messages), vec![1..2]);
    }

    #[test]
    fn skill_calls_break_a_run() {
        let messages = vec![
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            msg(
                "User",
                MessageContent::SkillCall(crate::components::shared::SkillCall::default()),
            ),
            tool_msg(tool("srv", "B", "{}", ToolCallStatus::Completed)),
        ];
        assert_eq!(group_turn_runs(&messages), vec![0..1, 2..3]);
    }

    #[test]
    fn empty_thinking_text_joins_the_run() {
        let messages = vec![
            text("User", "go", None),
            text("Hobbes", "   ", Some("Let me check the logs.")),
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
        ];
        assert_eq!(group_turn_runs(&messages), vec![1..3]);
    }

    #[test]
    fn non_empty_text_ends_the_run() {
        let messages = vec![
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            text("Hobbes", "here you go", Some("thinking about it")),
            tool_msg(tool("srv", "B", "{}", ToolCallStatus::Completed)),
        ];
        assert_eq!(group_turn_runs(&messages), vec![0..1, 2..3]);
    }

    #[test]
    fn permission_requests_are_activity() {
        let messages = vec![msg(
            "Hobbes",
            MessageContent::PermissionRequest(tool("srv", "A", "{}", ToolCallStatus::Running)),
        )];
        assert_eq!(group_turn_runs(&messages), vec![0..1]);
    }

    #[test]
    fn continuation_placeholders_do_not_split_a_turn() {
        // Each continuation pushes an empty Hobbes Text before its tool calls;
        // one turn must still be one block.
        let messages = vec![
            text("User", "go", None),
            text("Hobbes", "", None),
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            text("Hobbes", "", None),
            tool_msg(tool("srv", "B", "{}", ToolCallStatus::Completed)),
            text("Hobbes", "all done", None),
        ];
        assert_eq!(group_turn_runs(&messages), vec![2..5]);
    }

    #[test]
    fn a_prose_only_turn_produces_no_block() {
        let messages = vec![text("User", "hi", None), text("Hobbes", "", None)];
        assert!(group_turn_runs(&messages).is_empty());
    }

    // ---- step_kind ----

    #[test]
    fn step_kind_classifies_terminal_planner_and_other() {
        assert_eq!(
            step_kind(&tool(
                "hobbes-terminal",
                "HOBBES_TERMINAL_EXEC",
                "{}",
                ToolCallStatus::Completed
            )),
            StepKind::Command
        );
        assert_eq!(
            step_kind(&tool(
                "hobbes-planner",
                "HOBBES_TODO_CREATE",
                "{}",
                ToolCallStatus::Completed
            )),
            StepKind::Todo
        );
        assert_eq!(
            step_kind(&tool("gmail", "SEND", "{}", ToolCallStatus::Completed)),
            StepKind::Tool
        );
    }

    #[test]
    fn step_kind_error_wins_over_shape() {
        assert_eq!(
            step_kind(&tool(
                "hobbes-terminal",
                "HOBBES_TERMINAL_EXEC",
                "{}",
                ToolCallStatus::Error
            )),
            StepKind::Error
        );
    }

    // ---- formatting ----

    #[test]
    fn format_elapsed_is_clock_shaped() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(47), "0:47");
        assert_eq!(format_elapsed(102), "1:42");
        assert_eq!(format_elapsed(3661), "1:01:01");
        assert_eq!(format_elapsed(-5), "0:00");
    }

    #[test]
    fn format_worked_is_word_shaped() {
        assert_eq!(format_worked(47), "47s");
        assert_eq!(format_worked(102), "1m 42s");
        assert_eq!(format_worked(3720), "1h 2m");
    }

    #[test]
    fn command_tail_keeps_the_last_n_lines() {
        let out = "one\ntwo\nthree\nfour\nfive\nsix\n";
        assert_eq!(command_tail(out, 5), "two\nthree\nfour\nfive\nsix");
        assert_eq!(command_tail("only", 5), "only");
        assert_eq!(command_tail("", 5), "");
    }

    // ---- usage sums ----

    #[test]
    fn run_usage_sums_cost_and_tokens() {
        let mut a = tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed));
        a.usage = Some(UsageData {
            total_tokens: 1200,
            cost: Some(0.0025),
            ..Default::default()
        });
        let mut b = tool_msg(tool("srv", "B", "{}", ToolCallStatus::Completed));
        b.usage = Some(UsageData {
            total_tokens: 800,
            cost: Some(0.0011),
            ..Default::default()
        });
        let c = tool_msg(tool("srv", "C", "{}", ToolCallStatus::Completed));
        let (cost, tokens) = run_usage(&[a, b, c]);
        assert_eq!(tokens, 2000);
        assert!((cost - 0.0036).abs() < 1e-9);
    }

    // ---- token_display_mode ----

    #[test]
    fn usage_summary_all_shows_cost_and_tokens() {
        let s = format_usage_summary("all", 1.2345, 2500).unwrap();
        assert_eq!(s, "$1.2345 · 2.5k tok");
    }

    #[test]
    fn usage_summary_cost_only_hides_tokens() {
        let s = format_usage_summary("cost", 1.2345, 2500).unwrap();
        assert_eq!(s, "$1.2345");
    }

    #[test]
    fn usage_summary_tokens_only_hides_cost() {
        let s = format_usage_summary("tokens", 1.2345, 2500).unwrap();
        assert_eq!(s, "2.5k tok");
    }

    #[test]
    fn usage_summary_none_hides_both() {
        assert_eq!(format_usage_summary("none", 1.2345, 2500), None);
    }

    #[test]
    fn usage_summary_unknown_mode_falls_back_to_all() {
        // An unrecognised persisted value should behave like the field's own
        // documented default ("all") rather than silently going blank.
        let s = format_usage_summary("bogus", 0.5, 1000).unwrap();
        assert_eq!(s, "$0.5000 · 1.0k tok");
    }

    // ---- default_tool_thought_open / row expansion ----

    #[test]
    fn thinking_row_open_by_default_when_setting_is_on() {
        let toggled = std::collections::HashSet::new();
        assert!(is_row_open(StepKind::Thinking, "k", true, &toggled));
    }

    #[test]
    fn thinking_row_closed_by_default_when_setting_is_off() {
        let toggled = std::collections::HashSet::new();
        assert!(!is_row_open(StepKind::Thinking, "k", false, &toggled));
    }

    #[test]
    fn non_thinking_row_closed_by_default_regardless_of_setting() {
        let toggled = std::collections::HashSet::new();
        assert!(!is_row_open(StepKind::Tool, "k", true, &toggled));
    }

    #[test]
    fn clicking_a_row_toggles_away_from_its_default() {
        let mut toggled = std::collections::HashSet::new();
        // Setting on: starts open, a click (key now toggled) closes it.
        toggled.insert("k".to_string());
        assert!(!is_row_open(StepKind::Thinking, "k", true, &toggled));
        // Setting off: starts closed, a click opens it.
        assert!(is_row_open(StepKind::Tool, "k", false, &toggled));
    }

    // ---- row building ----

    #[test]
    fn permission_rows_are_pinned_first() {
        let start = Utc::now();
        let mut a = tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed));
        a.created_at = start;
        let mut p = msg(
            "Hobbes",
            MessageContent::PermissionRequest(tool("srv", "P", "{}", ToolCallStatus::Running)),
        );
        p.created_at = start + Duration::seconds(5);
        let rows = build_rows(&[a, p], start, None);
        assert_eq!(rows[0].kind, StepKind::NeedsYou);
        assert_eq!(rows[1].kind, StepKind::Tool);
        assert_eq!(rows[0].offset_secs, 5);
    }

    #[test]
    fn duration_comes_from_completed_at_minus_created_at() {
        let start = Utc::now();
        let mut tc = tool("srv", "A", "{}", ToolCallStatus::Completed);
        tc.completed_at = Some(start + Duration::seconds(12));
        let mut m = tool_msg(tc);
        m.created_at = start;
        let rows = build_rows(&[m], start, None);
        assert_eq!(rows[0].duration_secs, Some(12));
        assert!(!rows[0].running);
    }

    #[test]
    fn thinking_row_precedes_its_tool_row() {
        let start = Utc::now();
        let mut tc = tool("srv", "A", "{}", ToolCallStatus::Completed);
        tc.thought_summary = Some("I should list the files. Then read them.".to_string());
        let mut m = tool_msg(tc);
        m.created_at = start;
        let rows = build_rows(&[m], start, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, StepKind::Thinking);
        assert_eq!(rows[0].label, "I should list the files.");
    }

    #[test]
    fn row_counts_exclude_thinking_and_count_failures() {
        let start = Utc::now();
        let messages = vec![
            tool_msg(tool(
                "hobbes-terminal",
                "HOBBES_TERMINAL_EXEC",
                "{}",
                ToolCallStatus::Completed,
            )),
            tool_msg(tool(
                "hobbes-planner",
                "HOBBES_TODO_CREATE",
                "{}",
                ToolCallStatus::Completed,
            )),
            tool_msg(tool("gmail", "SEND", "{}", ToolCallStatus::Error)),
            text("Hobbes", "", Some("hmm")),
        ];
        let rows = build_rows(&messages, start, None);
        assert_eq!(row_counts(&rows), (3, 1, 1, 1));
    }

    // ---- labels / parsing ----

    #[test]
    fn command_label_is_the_command_argument() {
        let tc = tool(
            "hobbes-terminal",
            "HOBBES_TERMINAL_EXEC",
            r#"{"command":"cargo test --release","cwd":"/tmp"}"#,
            ToolCallStatus::Completed,
        );
        assert_eq!(step_label(&tc), "cargo test --release");
    }

    #[test]
    fn todo_label_quotes_the_title() {
        let tc = tool(
            "hobbes-planner",
            "HOBBES_TODO_CREATE",
            r#"{"title":"Ship the stream"}"#,
            ToolCallStatus::Completed,
        );
        assert_eq!(
            step_label(&tc),
            "hobbes-planner · HOBBES_TODO_CREATE \"Ship the stream\""
        );
    }

    #[test]
    fn tool_label_uses_the_first_string_argument() {
        let tc = tool(
            "gmail",
            "SEND",
            r#"{"to":"a@b.c"}"#,
            ToolCallStatus::Completed,
        );
        assert_eq!(step_label(&tc), "gmail · SEND — a@b.c");
    }

    #[test]
    fn exit_code_parses_from_json_and_from_text() {
        assert_eq!(parse_exit_code(r#"{"exit_code":0}"#), Some(0));
        assert_eq!(parse_exit_code("boom\nexit code: 127"), Some(127));
        assert_eq!(parse_exit_code("no codes here"), None);
    }

    #[test]
    fn first_sentence_stops_at_punctuation() {
        assert_eq!(first_sentence("One. Two. Three.", 200), "One.");
        assert_eq!(first_sentence("no punctuation", 200), "no punctuation");
    }

    // ---- plain_text (markdown stripping for thinking labels) ----

    #[test]
    fn plain_text_strips_bold_and_inline_code() {
        assert_eq!(plain_text("**Data** is `here`"), "Data is here");
    }

    #[test]
    fn plain_text_strips_leading_list_marker() {
        assert_eq!(plain_text("- do the thing"), "do the thing");
    }

    #[test]
    fn plain_text_strips_leading_heading() {
        assert_eq!(plain_text("## Heading"), "Heading");
    }

    #[test]
    fn plain_text_leaves_plain_prose_unchanged() {
        assert_eq!(
            plain_text("Just a normal sentence."),
            "Just a normal sentence."
        );
    }

    #[test]
    fn first_sentence_strips_markdown_before_truncating() {
        // Regression: a thinking summary opening with markdown bold used to
        // render the literal "**" in the row (seen as "**Da…").
        assert_eq!(
            first_sentence("**Data** looks stale. Refetching.", 200),
            "Data looks stale."
        );
    }

    // ---- run_header_provenance ----

    fn tool_with_provenance(
        profile_id: Option<&str>,
        connector_name: Option<&str>,
        model: Option<&str>,
    ) -> ToolCall {
        let mut tc = tool("srv", "A", "{}", ToolCallStatus::Completed);
        tc.profile_id = profile_id.map(str::to_string);
        tc.connector_name = connector_name.map(str::to_string);
        tc.model = model.map(str::to_string);
        tc
    }

    fn settings_with_profile(id: &str, name: &str, color: &str) -> crate::settings::Settings {
        let mut settings = crate::settings::Settings::default();
        settings
            .composio_profiles
            .push(crate::settings::ComposioProfile {
                id: id.to_string(),
                name: name.to_string(),
                base_url: None,
                entity_id: None,
                user_id: None,
                api_key: None,
                toolkit_configs: vec![],
                color: color.to_string(),
                chrome_profile_directory: None,
                retained_curation: Default::default(),
            });
        settings
    }

    #[test]
    fn stamped_run_uses_its_own_provenance_over_the_session_pin() {
        let settings = settings_with_profile("p2", "Work", "bg-orange-600");
        let messages = vec![tool_msg(tool_with_provenance(
            Some("p2"),
            Some("Claude work"),
            Some("claude-fable-5-1"),
        ))];
        let (name, color, label) = run_header_provenance(
            &messages,
            &settings,
            "Personal",
            "bg-blue-600",
            "Gemini work gemini-3-pro",
        );
        assert_eq!(name, "Work");
        assert_eq!(color, "bg-orange-600");
        assert_eq!(label, "Claude work claude-fable-5-1");
    }

    #[test]
    fn unstamped_legacy_run_falls_back_to_the_session_pin() {
        let settings = crate::settings::Settings::default();
        let messages = vec![tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed))];
        let (name, color, label) = run_header_provenance(
            &messages,
            &settings,
            "Personal",
            "bg-blue-600",
            "Gemini work gemini-3-pro",
        );
        assert_eq!(name, "Personal");
        assert_eq!(color, "bg-blue-600");
        assert_eq!(label, "Gemini work gemini-3-pro");
    }

    #[test]
    fn run_uses_the_later_stamp_when_the_first_message_is_unstamped() {
        let settings = settings_with_profile("p2", "Work", "bg-orange-600");
        let messages = vec![
            tool_msg(tool("srv", "A", "{}", ToolCallStatus::Completed)),
            tool_msg(tool_with_provenance(
                Some("p2"),
                Some("Claude work"),
                Some("claude-fable-5-1"),
            )),
        ];
        let (name, color, label) = run_header_provenance(
            &messages,
            &settings,
            "Personal",
            "bg-blue-600",
            "Gemini work gemini-3-pro",
        );
        assert_eq!(name, "Work");
        assert_eq!(color, "bg-orange-600");
        assert_eq!(label, "Claude work claude-fable-5-1");
    }

    // ---- detect_result_shape ----

    #[test]
    fn detects_a_json_object() {
        assert_eq!(
            detect_result_shape(r#"{"a": 1, "b": [1,2]}"#),
            ResultShape::Json
        );
    }

    #[test]
    fn detects_a_json_array() {
        assert_eq!(detect_result_shape("[1, 2, 3]"), ResultShape::Json);
    }

    #[test]
    fn detects_jsonl_when_every_line_parses() {
        let text = "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n";
        assert_eq!(detect_result_shape(text), ResultShape::Jsonl);
    }

    #[test]
    fn a_single_line_is_json_not_jsonl() {
        // One line is unambiguous: the whole-document JSON parse wins.
        assert_eq!(detect_result_shape("{\"id\":1}"), ResultShape::Json);
    }

    #[test]
    fn plain_prose_is_text() {
        assert_eq!(
            detect_result_shape("Sent the email to a@b.c."),
            ResultShape::Text
        );
    }

    #[test]
    fn a_mixed_blob_where_only_some_lines_parse_is_text() {
        let text = "{\"id\":1}\nnot json\n{\"id\":3}";
        assert_eq!(detect_result_shape(text), ResultShape::Text);
    }

    #[test]
    fn empty_result_is_text() {
        assert_eq!(detect_result_shape(""), ResultShape::Text);
        assert_eq!(detect_result_shape("   "), ResultShape::Text);
    }

    // ---- pretty_jsonl ----

    #[test]
    fn pretty_jsonl_splits_and_pretty_prints_each_record() {
        let text = "{\"id\":1}\n{\"id\":2}";
        let records = pretty_jsonl(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], "{\n  \"id\": 1\n}");
        assert_eq!(records[1], "{\n  \"id\": 2\n}");
    }

    #[test]
    fn pretty_jsonl_skips_blank_lines() {
        let text = "{\"id\":1}\n\n\n{\"id\":2}\n";
        assert_eq!(pretty_jsonl(text).len(), 2);
    }

    // ---- tail_lines ----

    #[test]
    fn tail_lines_keeps_the_last_n_and_reports_the_total() {
        let text = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (shown, total) = tail_lines(&text, 40);
        assert_eq!(total, 50);
        assert_eq!(shown.lines().count(), 40);
        assert_eq!(shown.lines().next(), Some("line 11"));
        assert_eq!(shown.lines().last(), Some("line 50"));
    }

    #[test]
    fn tail_lines_returns_everything_when_under_the_cap() {
        let (shown, total) = tail_lines("one\ntwo", 40);
        assert_eq!(total, 2);
        assert_eq!(shown, "one\ntwo");
    }

    // ---- shorten_home ----

    #[test]
    fn shorten_home_replaces_the_home_prefix() {
        assert_eq!(
            shorten_home("/Users/dustmoo/Sites/cai-hobbes", "/Users/dustmoo"),
            "~/Sites/cai-hobbes"
        );
    }

    #[test]
    fn shorten_home_leaves_unrelated_paths_alone() {
        assert_eq!(shorten_home("/tmp/foo", "/Users/dustmoo"), "/tmp/foo");
    }

    #[test]
    fn shorten_home_does_not_shorten_a_sibling_directory_sharing_the_prefix() {
        // "/Users/dustmoons" must not be treated as under "/Users/dustmoo".
        assert_eq!(
            shorten_home("/Users/dustmoons/x", "/Users/dustmoo"),
            "/Users/dustmoons/x"
        );
    }

    #[test]
    fn shorten_home_handles_the_bare_home_dir() {
        assert_eq!(shorten_home("/Users/dustmoo", "/Users/dustmoo"), "~");
    }

    // ---- json_spans ----

    #[test]
    fn json_spans_tokenises_keys_strings_and_numbers() {
        let pretty = serde_json::to_string_pretty(&serde_json::json!({
            "name": "hobbes",
            "count": 3
        }))
        .unwrap();
        let spans = json_spans(&pretty);
        let keys: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == JsonSpan::Key)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(keys, vec!["\"name\"", "\"count\""]);

        let strings: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == JsonSpan::Str)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(strings, vec!["\"hobbes\""]);

        let numbers: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == JsonSpan::Num)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(numbers, vec!["3"]);

        // Reassembling every span's text reproduces the source exactly.
        let reassembled: String = spans.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(reassembled, pretty);
    }

    #[test]
    fn json_spans_handles_a_negative_number() {
        let spans = json_spans(r#"{"delta": -12.5}"#);
        assert!(spans
            .iter()
            .any(|(k, s)| *k == JsonSpan::Num && s == "-12.5"));
    }
}
