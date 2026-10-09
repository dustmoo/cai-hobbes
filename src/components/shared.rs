use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq)]
pub struct SessionIdContext(pub Signal<String>);

#[derive(Clone, Copy, PartialEq)]
pub struct DraftContext(pub Signal<String>);

/// Set the chat draft programmatically (clear after send, autocomplete splice,
/// restore, quick-fix suggestions...).
///
/// The chat textarea is UNCONTROLLED (`initial_value`, not `value`) so that
/// re-renders can never clobber in-flight typing — a controlled binding races
/// the IPC round-trip and applies a stale value, snapping the caret to the end
/// and dropping typed-ahead characters. The flip side: every programmatic
/// draft change must write the DOM value explicitly, which this helper does
/// (signal + DOM value + auto-resize + optional caret/focus). Never call
/// `draft.set(...)` directly outside of the textarea's own `oninput`.
pub fn set_chat_draft(
    mut draft: Signal<String>,
    text: String,
    caret_utf16: Option<usize>,
    focus: bool,
) {
    // JSON-encode for safe embedding as a JS string literal
    let value_js = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_string());
    draft.set(text);
    let caret_js = match caret_utf16 {
        Some(pos) => format!("el.selectionStart = el.selectionEnd = {};", pos),
        None => String::new(),
    };
    let focus_js = if focus { "el.focus();" } else { "" };
    let _ = document::eval(&format!(
        r#"
        const el = document.getElementById('chat-textarea');
        if (el) {{
            el.value = {value_js};
            {focus_js}
            el.style.height = 'auto';
            el.style.height = el.scrollHeight + 'px';
            {caret_js}
        }}
        "#
    ));
}

#[derive(Clone, Copy, PartialEq)]
pub struct SessionToDeleteContext(pub Signal<String>);

/// Seed text awaiting the chat input after a planner todo is activated.
/// main.rs writes it in the same effect that opens the new tab, so ChatInput's
/// consuming effect necessarily runs after the render that unhid the chat —
/// deterministic hand-off, no racing on the command signal.
#[derive(Clone, Copy)]
pub struct PendingChatSeedContext(pub Signal<Option<String>>);

/// Context for surfacing async save errors to the UI via a dismissible toast.
/// When `Some(msg)`, the toast is visible with the given error message.
#[derive(Clone, Copy, PartialEq)]
pub struct SaveErrorContext(pub Signal<Option<String>>);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum MessageContent {
    Text {
        content: String,
        #[serde(default)]
        thought_signature: Option<String>,
        #[serde(default)]
        thought_summary: Option<String>,
        /// Which provider minted `thought_signature` (Claude and Gemini both
        /// write opaque, mutually-incompatible blobs into that field, and
        /// nothing else records which one produced a given blob). `None` on
        /// legacy rows predating this field and on messages with no signature.
        #[serde(default)]
        signature_provider: Option<crate::settings::LlmProvider>,
    },
    ToolCall(ToolCall),
    PermissionRequest(ToolCall),
    SkillCall(SkillCall),
    SkillPermissionRequest(SkillCall),
    Error {
        message: String,
    },
}

impl MessageContent {
    pub fn get_text_content(&self) -> Option<String> {
        match self {
            MessageContent::Text { content, .. } => Some(content.clone()),
            _ => None,
        }
    }

    /// One-line summary of this message content, suitable for context summaries
    /// and background summarization. Avoids duplicating match arms across call sites.
    pub fn display_summary(&self) -> String {
        match self {
            MessageContent::Text { content, .. } => content.clone(),
            MessageContent::ToolCall(tc) => format!("[Tool Call: {}]", tc.tool_name),
            MessageContent::PermissionRequest(tc) => {
                format!("[Permission Request for Tool: {}]", tc.tool_name)
            }
            MessageContent::SkillCall(sc) => format!("[Skill Call: {}]", sc.skill_name),
            MessageContent::SkillPermissionRequest(sc) => {
                format!("[Permission Request for Skill: {}]", sc.skill_name)
            }
            MessageContent::Error { message } => format!("[Error: {}]", message),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolCall {
    pub execution_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub arguments: String,
    pub status: ToolCallStatus,
    pub response: String,
    pub thought_signature: Option<String>,
    /// The actual thinking content (human-readable), separate from the encrypted signature
    #[serde(default)]
    pub thought_summary: Option<String>,
    /// Which provider minted `thought_signature` (Claude and Gemini both write
    /// opaque, mutually-incompatible blobs into that field, and nothing else
    /// records which one produced a given blob). `None` on legacy rows
    /// predating this field and on messages with no signature.
    #[serde(default)]
    pub signature_provider: Option<crate::settings::LlmProvider>,
    /// Filesystem path (`file:///…`) of a screenshot/image returned by this tool call.
    /// Set by stream_manager when MCP returns image content; read by prompt_builder to
    /// inject a `ContentBlock::Image` as vision input on the next continuation turn.
    /// Never embedded in tc.response — stays on disk to keep the session file small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_image_path: Option<String>,
    /// Knowledge-preserving summary of `response`, generated in the background
    /// after the turn completes (see Part 4 / `ToolResultSummarizer`). Once a
    /// result is historical and exceeds its context budget, the prompt builder
    /// substitutes this summary instead of hard-truncating — keeping the facts
    /// while reclaiming tokens. `None` until generated; the full `response` is
    /// always retained for pagination/re-budgeting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    /// When the result was written back onto this call. Set by every site that
    /// stamps `status`/`response` (the streaming path in `stream_manager` and
    /// the approval-resume path in `chat.rs`). Paired with the owning
    /// `Message::created_at` it yields the step's duration for the activity
    /// stream. `None` on legacy rows and on calls still in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The session's pinned Composio profile id at the moment this call was
    /// minted. Paired with `connector_name`/`model` below, this is what lets
    /// the activity stream label a finished turn with the provenance it
    /// actually ran under, instead of the session's *current* pins (which may
    /// have since changed). `None` on legacy rows and when no profile was
    /// pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// The resolved connector instance's display name at turn time (see
    /// `StreamManagerContext::effective_connector`). `None` on legacy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_name: Option<String>,
    /// The effective chat model at turn time. `None` on legacy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Id of the stream turn (tool round) that minted this call; every call
    /// the model emitted in one response shares it. Lets the prompt builder
    /// tell the latest round apart from earlier back-to-back rounds, which are
    /// otherwise adjacent `ToolCall` messages. `None` on legacy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_id: Option<String>,
    /// Why this call never finished, when something stopped it: the user's
    /// Stop, or Hobbes closing while it ran. A cancelled call is no longer
    /// `Running` (see [`ToolCall::mark_cancelled`]); the activity stream says
    /// "cancelled" only for these, never for a call still in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<String>,
}

/// Stop reason for a call cut off by the user's Stop.
pub const CANCELLED_BY_STOP: &str = "Stopped by you";
/// Stop reason for a call still in flight when Hobbes closed.
pub const CANCELLED_BY_RESTART: &str = "Hobbes closed while it ran";


pub enum StreamMessage {
    Text {
        content: String,
        thought_signature: Option<String>,
        thought_summary: Option<String>,
    },
    // Boxed: `ToolCall` carries several inline `String`/`Option<String>`
    // fields (provenance stamps included) and is far larger than the other
    // variants — boxing keeps `StreamMessage` itself small to pass around.
    ToolCall(Box<ToolCall>),
    Error {
        message: String,
    },
    Usage(UsageData),
}

/// Token usage data from LLM API response
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageData {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    #[serde(default)]
    pub thoughts_tokens: Option<i32>,
    #[serde(default)]
    pub cached_content_tokens: Option<i32>,
    /// Tokens written to the provider's prompt cache this request (Anthropic).
    #[serde(default)]
    pub cache_creation_tokens: Option<i32>,
    /// Calculated cost in USD
    #[serde(default)]
    pub cost: Option<f64>,
}

impl ToolCall {
    /// End a call that will never finish: an error result naming `reason`
    /// (the model sees it, and nothing re-runs it — the approval-resume path
    /// collects only `Running` calls), stamped `cancelled` for the UI.
    pub fn mark_cancelled(&mut self, reason: &str, now: chrono::DateTime<chrono::Utc>) {
        self.status = ToolCallStatus::Error;
        self.response = format!("Cancelled: {reason}. The call did not finish; its result is unknown.");
        self.completed_at = Some(now);
        self.cancelled = Some(reason.to_string());
    }

    pub fn new(
        server_name: String,
        tool_name: String,
        args: serde_json::Value,
        thought_signature: Option<String>,
        thought_summary: Option<String>,
    ) -> Self {
        Self {
            execution_id: uuid::Uuid::new_v4().to_string(),
            server_name,
            tool_name,
            arguments: args.to_string(),
            status: ToolCallStatus::Running,
            response: String::new(),
            thought_signature,
            thought_summary,
            signature_provider: None,
            cached_image_path: None,
            result_summary: None,
            completed_at: None,
            profile_id: None,
            connector_name: None,
            model: None,
            round_id: None,
            cancelled: None,
        }
    }

    /// Attach signature provenance to a tool call built via [`ToolCall::new`].
    /// `provider` is stamped only when `thought_signature` is actually present —
    /// a `None` signature never needs an owning provider. Kept as a separate
    /// builder (rather than a `ToolCall::new` parameter) so call sites in the
    /// individual LLM connectors — which don't yet know their own provider at
    /// construction time — don't all need updating; callers that DO know the
    /// provider (e.g. `stream_manager`, which resolves it from the session's
    /// connector) chain this on afterwards.
    pub fn with_signature_provider(mut self, provider: crate::settings::LlmProvider) -> Self {
        self.signature_provider = self.thought_signature.is_some().then_some(provider);
        self
    }

    /// Stamp turn-time provenance (profile / connector / model) onto a tool
    /// call as it's minted, so the activity stream can label a finished turn
    /// with what it actually ran under rather than the session's current
    /// pins. Called alongside `with_signature_provider` in `stream_manager`.
    pub fn with_turn_provenance(
        mut self,
        profile_id: Option<String>,
        connector_name: Option<String>,
        model: Option<String>,
    ) -> Self {
        self.profile_id = profile_id;
        self.connector_name = connector_name;
        self.model = model;
        self
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Copy, Debug, Default)]
pub enum ToolCallStatus {
    #[default]
    Running,
    Completed,
    Error,
    AuthRequired,
}

impl std::fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolCallStatus::Running => write!(f, "Running"),
            ToolCallStatus::Completed => write!(f, "Completed"),
            ToolCallStatus::Error => write!(f, "Error"),
            ToolCallStatus::AuthRequired => write!(f, "Auth Required"),
        }
    }
}

// ============================================================================
// Skill Types (mirrors Tool types for Claude Skills integration)
// ============================================================================

#[derive(Clone, Serialize, Deserialize, PartialEq, Copy, Debug, Default)]
pub enum SkillCallStatus {
    #[default]
    Pending, // Awaiting user permission
    Running,   // Currently executing
    Completed, // Successfully finished
    Error,     // Execution failed
}

impl std::fmt::Display for SkillCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillCallStatus::Pending => write!(f, "Pending"),
            SkillCallStatus::Running => write!(f, "Running"),
            SkillCallStatus::Completed => write!(f, "Completed"),
            SkillCallStatus::Error => write!(f, "Error"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct SkillCall {
    pub execution_id: String,
    pub skill_name: String,
    pub arguments: String,
    pub status: SkillCallStatus,
    pub response: String,
    pub instructions: String, // Processed skill instructions
    #[serde(default)]
    pub path: std::path::PathBuf, // Path to skill file
    #[serde(default)]
    pub has_scripts: bool, // Whether skill has executable scripts
    #[serde(default)]
    pub raw_output: Option<String>, // Clean output for use_result
    #[serde(default)]
    pub profile_color: Option<String>, // Historical profile color for rendering
}

impl SkillCall {
    #[allow(dead_code)]
    pub fn new(skill_name: String, arguments: String, instructions: String) -> Self {
        Self {
            execution_id: uuid::Uuid::new_v4().to_string(),
            skill_name,
            arguments,
            status: SkillCallStatus::Pending,
            response: String::new(),
            instructions,
            path: std::path::PathBuf::new(),
            has_scripts: false,
            raw_output: None,
            profile_color: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SkillEnvironment {
    pub root_path: std::path::PathBuf,
    pub scripts: Vec<String>,
    pub resources: Vec<String>,
    pub user_args: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CapabilityContextPayload {
    pub skill: String,
    pub instruction_manual: String,
    pub environment: SkillEnvironment,
    pub resolved_tools: std::collections::HashMap<String, String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub status: ToolCallStatus,
    pub response: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub call: ToolCall,
    pub result: ToolResult,
    #[serde(default)]
    pub profile_color: Option<String>, // Historical profile color for rendering
}

/// Resolve the profile color from a session-specific profile identifier,
/// falling back to the global active profile's color.
pub fn resolve_profile_color(
    session_profile: Option<&String>,
    settings: &crate::settings::Settings,
) -> Option<String> {
    let identifier = session_profile.or(settings.active_composio_profile.as_ref());
    identifier
        .and_then(|val| settings.composio_profiles.iter().find(|p| &p.id == val))
        .map(|p| p.color.clone())
        .or_else(|| settings.get_active_profile().map(|p| p.color.clone()))
}

/// Helper to extract JSON content from an LLM response string.
/// It handles markdown blocks (```json ... ```) and raw JSON objects.
pub fn extract_json_from_response(text: &str) -> &str {
    let text = text.trim();

    // 1. Try markdown JSON block
    if let (Some(s), Some(e)) = (text.find("```json"), text.rfind("```")) {
        if s < e {
            return text[s + 7..e].trim();
        }
    }

    // 2. Try finding literal JSON object braces
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if s < e {
            return &text[s..=e];
        }
    }

    // 3. Fallback to raw text
    text
}

#[component]
pub fn ChatBarIconButton<I: dioxus_free_icons::IconShape + Copy + Clone + PartialEq + 'static>(
    icon: I,
    onclick: EventHandler<MouseEvent>,
    #[props(default = true)] visible: bool,
    #[props(default = String::new())] title: String,
) -> Element {
    if !visible {
        return rsx! {};
    }

    rsx! {
        button {
            class: "p-2 rounded-full text-fg-muted hover:bg-card hover:text-fg focus:outline-none focus:ring-2 focus:ring-gray-600 transition-colors",
            onclick: move |evt| onclick.call(evt),
            title: "{title}",
            dioxus_free_icons::Icon {
                width: 20,
                height: 20,
                icon: icon
            }
        }
    }
}

// --- Message timestamps ---------------------------------------------------
//
// Every caption under a message reads `Author · 8:04 AM`, and a divider marks
// the first message of each local day. The pure helpers work on naive local
// times so they can be tested without depending on the machine's timezone;
// the `*_utc` wrappers convert a stored `created_at` to local time.

fn to_local(ts: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDateTime {
    ts.with_timezone(&chrono::Local).naive_local()
}

/// The short time shown in a caption, e.g. `8:04 AM`.
pub fn caption_time(ts: chrono::NaiveDateTime) -> String {
    ts.format("%-I:%M %p").to_string()
}

/// The full stamp shown on hover, e.g. `Fri, Sep 25, 2026 · 8:04:29 AM`.
pub fn full_timestamp(ts: chrono::NaiveDateTime) -> String {
    ts.format("%a, %b %-d, %Y · %-I:%M:%S %p").to_string()
}

/// Label for the divider before the first message of `day`:
/// `Today · Fri, Sep 25`, `Yesterday · Thu, Sep 24`, `Tue, Sep 15`, or
/// `Tue, Dec 9, 2025` once it's outside the current year.
pub fn day_divider_label(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let short = day.format("%a, %b %-d");
    if day == today {
        format!("Today · {short}")
    } else if today.pred_opt() == Some(day) {
        format!("Yesterday · {short}")
    } else if day.year() == today.year() {
        short.to_string()
    } else {
        day.format("%a, %b %-d, %Y").to_string()
    }
}

/// Whether a caption should carry its time. A caption repeats the previous
/// one's time only when the author and the local minute both match (e.g. an
/// activity block and the reply that follows it in the same minute), and then
/// the time is dropped.
pub fn caption_shows_time(
    prev: Option<(&str, chrono::NaiveDateTime)>,
    cur: (&str, chrono::NaiveDateTime),
) -> bool {
    use chrono::Timelike;
    match prev {
        Some((prev_author, prev_ts)) => {
            prev_author != cur.0
                || prev_ts.date() != cur.1.date()
                || prev_ts.hour() != cur.1.hour()
                || prev_ts.minute() != cur.1.minute()
        }
        None => true,
    }
}

/// [`caption_shows_time`] over stored UTC stamps.
pub fn caption_shows_time_utc(
    prev: Option<(&str, chrono::DateTime<chrono::Utc>)>,
    cur: (&str, chrono::DateTime<chrono::Utc>),
) -> bool {
    caption_shows_time(prev.map(|(a, ts)| (a, to_local(ts))), (cur.0, to_local(cur.1)))
}

/// The divider label to render before a message stamped `cur`, or `None` when
/// it falls on the same local day as the previous rendered message.
pub fn day_divider_utc(
    prev: Option<chrono::DateTime<chrono::Utc>>,
    cur: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let day = to_local(cur).date();
    if prev.map(|p| to_local(p).date()) == Some(day) {
        return None;
    }
    Some(day_divider_label(day, chrono::Local::now().date_naive()))
}

/// The muted caption under a message: `Hobbes · 8:04 AM`, with the full
/// stamp on hover. `show_time: false` renders the label alone (see
/// [`caption_shows_time`]).
#[component]
pub fn MessageCaption(
    label: String,
    created_at: chrono::DateTime<chrono::Utc>,
    #[props(default = true)] show_time: bool,
    #[props(default = false)] align_right: bool,
) -> Element {
    let align = if align_right { "text-right" } else { "text-left" };
    let local = to_local(created_at);
    rsx! {
        div {
            class: "text-xs text-fg-muted mt-1 px-2 {align}",
            "{label}"
            if show_time {
                span {
                    class: "cursor-default",
                    title: "{full_timestamp(local)}",
                    " · {caption_time(local)}"
                }
            }
        }
    }
}

/// Centered hairline marking the first message of a local day.
#[component]
pub fn DayDivider(label: String) -> Element {
    rsx! {
        div {
            class: "flex items-center gap-3 pt-1",
            div { class: "flex-1 border-t border-faint" }
            div { class: "text-[11px] font-semibold tracking-wide text-fg-muted", "{label}" }
            div { class: "flex-1 border-t border-faint" }
        }
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, s)
            .unwrap()
    }

    #[test]
    fn caption_time_is_12h_without_leading_zero() {
        assert_eq!(caption_time(at(2026, 9, 25, 8, 4, 29)), "8:04 AM");
        assert_eq!(caption_time(at(2026, 9, 24, 21, 12, 0)), "9:12 PM");
        assert_eq!(caption_time(at(2026, 9, 25, 0, 5, 0)), "12:05 AM");
    }

    #[test]
    fn full_timestamp_has_date_and_seconds() {
        assert_eq!(
            full_timestamp(at(2026, 9, 25, 8, 4, 29)),
            "Fri, Sep 25, 2026 · 8:04:29 AM"
        );
    }

    #[test]
    fn divider_labels_by_distance_from_today() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 25).unwrap();
        let day = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        assert_eq!(day_divider_label(today, today), "Today · Fri, Sep 25");
        assert_eq!(day_divider_label(day(9, 24), today), "Yesterday · Thu, Sep 24");
        assert_eq!(day_divider_label(day(9, 15), today), "Tue, Sep 15");
        assert_eq!(
            day_divider_label(NaiveDate::from_ymd_opt(2025, 12, 9).unwrap(), today),
            "Tue, Dec 9, 2025"
        );
    }

    #[test]
    fn yesterday_crosses_a_year_boundary() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let dec31 = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(day_divider_label(dec31, today), "Yesterday · Thu, Dec 31");
    }

    #[test]
    fn time_hidden_only_for_same_author_same_minute() {
        let t = at(2026, 9, 24, 21, 12, 5);
        assert!(caption_shows_time(None, ("Hobbes", t)));
        // Author changed.
        assert!(caption_shows_time(Some(("User", t)), ("Hobbes", at(2026, 9, 24, 21, 12, 40))));
        // Same author, same minute: activity block then its reply.
        assert!(!caption_shows_time(Some(("Hobbes", t)), ("Hobbes", at(2026, 9, 24, 21, 12, 58))));
        // Same author, a later minute: a long tool run still shows.
        assert!(caption_shows_time(Some(("Hobbes", t)), ("Hobbes", at(2026, 9, 24, 21, 14, 0))));
        // Same clock minute on a different day.
        assert!(caption_shows_time(Some(("Hobbes", t)), ("Hobbes", at(2026, 9, 25, 21, 12, 5))));
    }
}

#[cfg(test)]
mod signature_provider_tests {
    use super::*;
    use crate::settings::LlmProvider;

    /// A legacy `ToolCall` JSON row (predates `signature_provider`) must still
    /// deserialize, with the new field defaulting to `None`.
    #[test]
    fn legacy_tool_call_json_deserializes_with_none_provider() {
        let legacy_json = serde_json::json!({
            "execution_id": "exec-1",
            "server_name": "srv",
            "tool_name": "tool",
            "arguments": "{}",
            "status": "Completed",
            "response": "ok",
            "thought_signature": "abc123",
        });
        let tc: ToolCall =
            serde_json::from_value(legacy_json).expect("legacy ToolCall should deserialize");
        assert_eq!(tc.signature_provider, None);
        assert_eq!(tc.thought_signature.as_deref(), Some("abc123"));
    }

    /// A stamped `ToolCall` round-trips its `signature_provider` through serde.
    #[test]
    fn stamped_tool_call_round_trips_provider() {
        let mut tc = ToolCall::new(
            "srv".to_string(),
            "tool".to_string(),
            serde_json::json!({}),
            Some("sig-blob".to_string()),
            None,
        );
        tc = tc.with_signature_provider(LlmProvider::Claude);
        assert_eq!(tc.signature_provider, Some(LlmProvider::Claude));

        let json = serde_json::to_value(&tc).expect("serialize");
        let round_tripped: ToolCall = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round_tripped.signature_provider, Some(LlmProvider::Claude));
    }

    /// A legacy `ToolCall` JSON row (predates `profile_id`/`connector_name`/
    /// `model`) must still deserialize, with all three defaulting to `None`.
    #[test]
    fn legacy_tool_call_json_deserializes_with_none_provenance() {
        let legacy_json = serde_json::json!({
            "execution_id": "exec-1",
            "server_name": "srv",
            "tool_name": "tool",
            "arguments": "{}",
            "status": "Completed",
            "response": "ok",
        });
        let tc: ToolCall =
            serde_json::from_value(legacy_json).expect("legacy ToolCall should deserialize");
        assert_eq!(tc.profile_id, None);
        assert_eq!(tc.connector_name, None);
        assert_eq!(tc.model, None);
    }

    /// A stamped `ToolCall` round-trips its turn provenance through serde.
    #[test]
    fn stamped_tool_call_round_trips_turn_provenance() {
        let tc = ToolCall::new(
            "srv".to_string(),
            "tool".to_string(),
            serde_json::json!({}),
            None,
            None,
        )
        .with_turn_provenance(
            Some("p2".to_string()),
            Some("Claude work".to_string()),
            Some("claude-fable-5-1".to_string()),
        );

        let json = serde_json::to_value(&tc).expect("serialize");
        let round_tripped: ToolCall = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round_tripped.profile_id.as_deref(), Some("p2"));
        assert_eq!(round_tripped.connector_name.as_deref(), Some("Claude work"));
        assert_eq!(round_tripped.model.as_deref(), Some("claude-fable-5-1"));
    }

    /// A legacy `MessageContent::Text` JSON row (predates `signature_provider`)
    /// must still deserialize, with the new field defaulting to `None`.
    #[test]
    fn legacy_text_content_json_deserializes_with_none_provider() {
        let legacy_json = serde_json::json!({
            "Text": {
                "content": "hello",
                "thought_signature": "sig",
                "thought_summary": "summary"
            }
        });
        let content: MessageContent = serde_json::from_value(legacy_json)
            .expect("legacy MessageContent::Text should deserialize");
        match content {
            MessageContent::Text {
                signature_provider,
                thought_signature,
                ..
            } => {
                assert_eq!(signature_provider, None);
                assert_eq!(thought_signature.as_deref(), Some("sig"));
            }
            other => panic!("expected Text variant, got {:?}", other),
        }
    }

    /// A stamped `MessageContent::Text` round-trips its `signature_provider`.
    #[test]
    fn stamped_text_content_round_trips_provider() {
        let content = MessageContent::Text {
            content: "hello".to_string(),
            thought_signature: Some("sig".to_string()),
            thought_summary: None,
            signature_provider: Some(LlmProvider::Claude),
        };
        let json = serde_json::to_value(&content).expect("serialize");
        let round_tripped: MessageContent = serde_json::from_value(json).expect("deserialize");
        match round_tripped {
            MessageContent::Text {
                signature_provider, ..
            } => {
                assert_eq!(signature_provider, Some(LlmProvider::Claude));
            }
            other => panic!("expected Text variant, got {:?}", other),
        }
    }
}
