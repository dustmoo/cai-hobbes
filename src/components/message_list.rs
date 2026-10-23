use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

use crate::components::activity_stream::{
    absorb_live_placeholder, group_turn_runs, run_header_provenance, ActivityStream,
};
use crate::components::tool_call_display::{PermissionPrompt, ToolCallDisplay};

use super::chat::{MessageBubble, WelcomeMessage};
use super::shared::{
    caption_shows_time_utc, day_divider_utc, DayDivider, MessageCaption, MessageContent,
    SessionIdContext,
};

const INITIAL_MESSAGES_TO_SHOW: usize = 20;

/// What occupies one slot in the rendered list: either a plain message, or a
/// whole turn's worth of tool calls / permission prompts / thinking collapsed
/// into a single [`ActivityStream`] block rendered where its first message sat.
enum RenderItem {
    Single(usize),
    Run(std::ops::Range<usize>),
}

/// The key for one rendered list slot: the id of the first message in a
/// `Run`, or the id of the single message. Every iteration of the keyed
/// list wraps its content in one plain `div` carrying this key, so the
/// list diff always sees the same root shape (see the `for item in items`
/// loop in `MessageList`).
fn item_key(item: &RenderItem, sorted_messages: &[crate::components::chat::Message]) -> String {
    match item {
        RenderItem::Run(run) => sorted_messages[run.start].id.to_string(),
        RenderItem::Single(idx) => sorted_messages[*idx].id.to_string(),
    }
}

/// The caption an item renders under itself — `(label, created_at)` — or
/// `None` when the item renders nothing (an empty text message whose thinking
/// was absorbed into an activity block). Mirrors the labels each arm of the
/// `MessageList` match paints, so the repeat and day-divider rules see exactly
/// the captions on screen.
fn item_caption<'a>(
    item: &RenderItem,
    sorted_messages: &'a [crate::components::chat::Message],
    is_generating: impl Fn(&Uuid) -> bool,
) -> Option<(&'a str, chrono::DateTime<chrono::Utc>)> {
    let message = match item {
        RenderItem::Run(run) => return Some(("Hobbes", sorted_messages[run.start].created_at)),
        RenderItem::Single(idx) => &sorted_messages[*idx],
    };
    let label = match &message.content {
        MessageContent::Text { content, .. } => {
            let renders = !content.trim().is_empty()
                || is_generating(&message.id)
                || !message.attachments.is_empty();
            if !renders {
                return None;
            }
            message.author.as_str()
        }
        MessageContent::ToolCall(_) | MessageContent::Error { .. } => message.author.as_str(),
        MessageContent::PermissionRequest(_) => "System",
        MessageContent::SkillCall(_) | MessageContent::SkillPermissionRequest(_) => "User",
        MessageContent::Relay(_) => message.author.as_str(),
    };
    Some((label, message.created_at))
}

/// Per-item timestamp chrome: the day divider to draw above it, and whether
/// its caption carries a time.
struct ItemTimestamp {
    divider: Option<String>,
    show_time: bool,
}

#[component]
pub fn MessageList(
    stream_update_trigger: Signal<i32>,
    show_scroll_button: Signal<bool>,
    on_delete: EventHandler<Uuid>,
    on_fork: EventHandler<Uuid>,
    on_edit_save: EventHandler<(Uuid, String)>,
    on_edit_resend: EventHandler<(Uuid, String)>,
    on_comment: EventHandler<()>,
    /// Stops the in-flight turn. Routed through to the activity block's header
    /// Stop button; ChatWindow owns the actual cancel path (`cancel_message`).
    on_stop: EventHandler<()>,
) -> Element {
    let mut session_state = consume_context::<Signal<crate::session::SessionState>>();
    let ui_state = consume_context::<Signal<crate::settings::UiState>>();
    let settings = use_context::<Signal<crate::settings::Settings>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    // Session-scoped, not the bare `ChatCommand::TriggerAiAnalysis` signal —
    // that command carries no session id and routes to whichever tab happens
    // to be ACTIVE when it's processed. A skill's permission approval runs an
    // async execution (skills/execute_skill) that can outlive the click: if
    // the user switches tabs before it resolves, TriggerAiAnalysis would
    // trigger a continuation on the now-active tab instead of the one that
    // actually ran the skill — an empty-user-message continuation against a
    // session whose last message is already a finished assistant reply,
    // which Claude rejects ("conversation must end with a user message").
    // Writing directly into the owning session's slot in this set (the same
    // primitive the tool-call approval path already uses) makes the
    // continuation land on the right tab regardless of what's active later.
    let mut has_pending_approvals =
        use_context::<Signal<std::collections::HashSet<String>>>();
    let mut visible_message_count = use_signal(|| INITIAL_MESSAGES_TO_SHOW);
    let _ = stream_update_trigger.read();

    let SessionIdContext(current_session_id) = use_context::<SessionIdContext>();
    let session_id = current_session_id;

    // PROFILE COLOR: Derive from session-specific composio_profile, falling back to global default
    let profile_color = {
        let state = session_state.read();
        let settings_read = settings.read();
        let session_profile = state
            .sessions
            .get(&*session_id.read())
            .and_then(|s| s.composio_profile.as_ref());
        crate::components::shared::resolve_profile_color(session_profile, &settings_read)
            .unwrap_or_default()
    };

    // Fallback header provenance for activity blocks: the session's CURRENT
    // pins. Used only for legacy runs whose messages predate turn-time
    // stamping (see `run_header_provenance`, which prefers each run's own
    // stamped tool calls so a later profile/connector switch doesn't
    // relabel finished turns).
    let (profile_name, connector_label) = {
        let state = session_state.read();
        let settings_read = settings.read();
        match state.sessions.get(&*session_id.read()) {
            Some(session) => {
                let profile_name = session
                    .composio_profile
                    .as_ref()
                    .and_then(|id| settings_read.composio_profiles.iter().find(|p| &p.id == id))
                    .or_else(|| settings_read.get_active_profile())
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "default".to_string());
                let connector = settings_read
                    .connector_for_session(session)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "no connector".to_string());
                let model = settings_read.chat_model_for_session(session);
                (profile_name, format!("{connector} {model}"))
            }
            None => ("default".to_string(), String::new()),
        }
    };

    let stream_manager =
        consume_context::<crate::components::stream_manager::StreamManagerContext>();

    use_effect(move || {
        if let Some(cmd) = chat_command.read().clone() {
            if matches!(
                cmd,
                crate::components::chat_input::ChatCommand::ScrollToBottom
            ) {
                spawn(async move {
                    let _ = document::eval(
                        r#"
                        const el = document.getElementById('message-list');
                        if (el) { el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' }); }
                    "#,
                    )
                    .await;
                });
            }
        }
    });

    rsx! {
        div {
            class: "relative flex-1 min-h-0",
            div {
                id: "message-list",
                class: "overflow-y-auto p-4 space-y-4 h-full",
                onscroll: move |_| {
                    let mut show_scroll_button = show_scroll_button;
                    let mut visible_message_count = visible_message_count;
                    let session_state = session_state;
                    let session_id = session_id.read().clone();

                    spawn(async move {
                        let scroll_info = if let Ok(result) = document::eval(r#"
                            const el = document.getElementById('message-list');
                            if (el) {
                                const isAtTop = el.scrollTop === 0;
                                const threshold = 10;
                                const isAtBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= threshold;
                                return { isAtTop, isAtBottom };
                            }
                            return { isAtTop: false, isAtBottom: true };
                        "#).await {
                            result
                        } else {
                            serde_json::from_str("{\"isAtTop\":false, \"isAtBottom\":true}").unwrap()
                        };

                        let is_at_top = scroll_info.get("isAtTop").unwrap().as_bool().unwrap_or(false);
                        let is_at_bottom = scroll_info.get("isAtBottom").unwrap().as_bool().unwrap_or(true);

                        // The parent now controls visibility on content change.
                        // This handler just hides the button if the user scrolls down manually.
                        if is_at_bottom {
                            show_scroll_button.set(false);
                        } else {
                            show_scroll_button.set(true);
                        }

                        if is_at_top {
                            let total_messages = session_state.read().sessions.get(&session_id).map_or(0, |s| s.messages.len());
                            if *visible_message_count.read() < total_messages {
                                let _ = document::eval(r#"
                                    const el = document.getElementById('message-list');
                                    if (el) {
                                        window.prevScrollHeight = el.scrollHeight;
                                        window.prevScrollTop = el.scrollTop;
                                    }
                                "#).await;

                                let current_count = *visible_message_count.read();
                                visible_message_count.set(current_count + INITIAL_MESSAGES_TO_SHOW);

                                sleep(Duration::from_millis(20)).await;
                                let _ = document::eval(r#"
                                    const el = document.getElementById('message-list');
                                    if (el && window.prevScrollHeight) {
                                        const newScrollHeight = el.scrollHeight;
                                        const heightDifference = newScrollHeight - window.prevScrollHeight;
                                        el.scrollTop = window.prevScrollTop + heightDifference;
                                        delete window.prevScrollHeight;
                                        delete window.prevScrollTop;
                                    }
                                "#).await;
                            }
                        }
                    });
                },
                {
                    let state = session_state.read();
                    if let Some(session) = state.sessions.get(&*session_id.read()) {
                        let total_messages = session.messages.len();

                        // Sort messages by created_at timestamp to ensure chronological order
                        let mut sorted_messages = session.messages.clone();
                        sorted_messages.sort_by_key(|m| m.created_at);

                        // Group each turn's machinery into one activity block, then
                        // apply the visible-window skip at slot level so a block is
                        // never half-rendered.
                        let mut runs = group_turn_runs(&sorted_messages);
                        let is_streaming = stream_manager.is_session_streaming(&session_id.read());
                        // While the turn is live the dashed "Considering…"
                        // placeholder is part of the block, not a bubble under it.
                        if is_streaming {
                            absorb_live_placeholder(&sorted_messages, &mut runs);
                        }
                        let skip = total_messages.saturating_sub(*visible_message_count.read());
                        let items: Vec<(RenderItem, ItemTimestamp)> = {
                            let mut items = Vec::new();
                            let mut i = 0usize;
                            while i < total_messages {
                                match runs.iter().find(|r| r.start == i) {
                                    Some(r) => {
                                        items.push(RenderItem::Run(r.clone()));
                                        i = r.end;
                                    }
                                    None => {
                                        items.push(RenderItem::Single(i));
                                        i += 1;
                                    }
                                }
                            }
                            // Walk every item (not just the visible window) so the
                            // first visible caption still compares against the one
                            // above it; the oldest visible day always gets a divider.
                            let mut prev: Option<(&str, chrono::DateTime<chrono::Utc>)> = None;
                            let mut first_visible = true;
                            items
                                .into_iter()
                                .filter_map(|it| {
                                    let visible = match &it {
                                        RenderItem::Single(i) => *i >= skip,
                                        RenderItem::Run(r) => r.end > skip,
                                    };
                                    let caption = item_caption(&it, &sorted_messages, |id| {
                                        stream_manager.is_generating(id)
                                    });
                                    let stamp = match caption {
                                        Some(cur) => {
                                            let prev_ts = if visible && first_visible { None } else { prev.map(|p| p.1) };
                                            let stamp = ItemTimestamp {
                                                divider: day_divider_utc(prev_ts, cur.1),
                                                show_time: caption_shows_time_utc(prev, cur),
                                            };
                                            prev = Some(cur);
                                            stamp
                                        }
                                        None => ItemTimestamp { divider: None, show_time: true },
                                    };
                                    if !visible {
                                        return None;
                                    }
                                    if caption.is_some() {
                                        first_visible = false;
                                    }
                                    Some((it, stamp))
                                })
                                .collect::<Vec<(RenderItem, ItemTimestamp)>>()
                        };
                        let last_run_start = runs.last().map(|r| r.start);

                        if session.messages.is_empty() {
                            rsx! { WelcomeMessage {} }
                        } else {
                            rsx! {
                                if total_messages > *visible_message_count.read() {
                                    div {
                                        class: "flex justify-center",
                                        button {
                                            class: "text-sm text-purple-400 hover:text-purple-300 focus:outline-none",
                                            onclick: move |_| {
                                                let current_count = *visible_message_count.read();
                                                visible_message_count.set(current_count + INITIAL_MESSAGES_TO_SHOW);
                                            },
                                            "Load More"
                                        }
                                    }
                                }
                                for (item, stamp) in items {
                                    div {
                                        key: "{item_key(&item, &sorted_messages)}",
                                        if let Some(label) = stamp.divider.clone() {
                                            div { class: "pb-4", DayDivider { label } }
                                        }
                                        {
                                        let session_id = session_id.read().clone();
                                        match &item {
                                        RenderItem::Run(run) => {
                                            let run_msgs = sorted_messages[run.clone()].to_vec();
                                            // Live only while this session streams, for the
                                            // newest run, and only until the assistant's prose
                                            // reply lands after it.
                                            let is_live = is_streaming
                                                && last_run_start == Some(run.start)
                                                && !sorted_messages[run.end..].iter().any(|m| matches!(
                                                    &m.content,
                                                    MessageContent::Text { content, .. } if !content.trim().is_empty()
                                                ));
                                            let (run_profile_name, run_profile_color, run_connector_label) =
                                                run_header_provenance(
                                                    &run_msgs,
                                                    &settings.read(),
                                                    &profile_name,
                                                    &profile_color,
                                                    &connector_label,
                                                );
                                            rsx! {
                                                ActivityStream {
                                                    messages: run_msgs,
                                                    session_id: session_id.clone(),
                                                    is_live: is_live,
                                                    profile_color: run_profile_color,
                                                    profile_name: run_profile_name,
                                                    connector_label: run_connector_label,
                                                    on_stop: move |_| on_stop.call(()),
                                                    show_time: stamp.show_time,
                                                }
                                            }
                                        }
                                        RenderItem::Single(idx) => {
                                        let message = &sorted_messages[*idx];
                                        rsx! {
                                            match &message.content {
                                                MessageContent::Text { content: text, thought_signature: _, thought_summary: _, signature_provider: _ } => {
                                                    let stream_manager = consume_context::<crate::components::stream_manager::StreamManagerContext>();
                                                    let is_generating = stream_manager.is_generating(&message.id);
                                                    // Don't render empty bubbles — thinking data (thought_signature/thought_summary)
                                                    // is absorbed into the adjacent ToolCall card, so an empty text message
                                                    // with only thinking metadata should be hidden.
                                                    let should_render = !text.trim().is_empty() || is_generating || !message.attachments.is_empty();

                                                    if should_render {
                                                        rsx! {
                                                            MessageBubble {
                                                                message: message.clone(),
                                                                on_content_update: move |_| stream_update_trigger += 1,
                                                                on_selection: move |data| tracing::debug!("Selection event: {:?}", data),
                                                                on_delete: {
                                                                    let msg_id = message.id;
                                                                    move |_| on_delete.call(msg_id)
                                                                },
                                                                on_fork: {
                                                                    let msg_id = message.id;
                                                                    move |_| on_fork.call(msg_id)
                                                                },
                                                                on_edit_save: {
                                                                    let msg_id = message.id;
                                                                    move |text: String| on_edit_save.call((msg_id, text))
                                                                },
                                                                on_edit_resend: {
                                                                    let msg_id = message.id;
                                                                    move |text: String| on_edit_resend.call((msg_id, text))
                                                                },
                                                                on_comment: move |_| on_comment.call(()),
                                                                show_time: stamp.show_time,
                                                            }
                                                        }
                                                    } else {
                                                        // The list key now lives on the wrapper `div`
                                                        // this whole match is rendered inside of (see
                                                        // the `for item in items` loop), so every
                                                        // iteration's root shape is identical and this
                                                        // arm doesn't need its own key.
                                                        rsx! {}
                                                    }
                                                },
                                                MessageContent::ToolCall(tool_call) => {
                                                    let bubble_classes = "bg-input text-fg self-start mr-auto";
                                                    let container_classes = "flex justify-start";
                                                    rsx! {
                                                        div {
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: "relative group rounded-2xl {bubble_classes}",
                                                                    ToolCallDisplay {
                                                                        tool_call: tool_call.clone(),
                                                                        usage: message.usage.clone(),
                                                                        token_display_mode: ui_state.read().token_display_mode.clone()
                                                                    }
                                                                }
                                                                MessageCaption {
                                                                    label: message.author.clone(),
                                                                    created_at: message.created_at,
                                                                    show_time: stamp.show_time,
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                MessageContent::PermissionRequest(tool_call) => {
                                                    let container_classes = "flex justify-start";
                                                    rsx! {
                                                        div {
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                if crate::components::tool_call_display::is_skill_invoke_request(tool_call) {
                                                                    crate::components::tool_call_display::SkillToolPermissionPrompt {
                                                                        tool_call: tool_call.clone(),
                                                                        session_id: session_id.clone(),
                                                                        profile_color: profile_color.clone(),
                                                                    }
                                                                } else {
                                                                    PermissionPrompt {
                                                                        tool_call: tool_call.clone(),
                                                                        session_id: session_id.clone(),
                                                                    }
                                                                }
                                                                MessageCaption {
                                                                    label: "System".to_string(),
                                                                    created_at: message.created_at,
                                                                    show_time: stamp.show_time,
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                MessageContent::Error { .. } => {
                                                    rsx! {
                                                        MessageBubble {
                                                            message: message.clone(),
                                                            on_content_update: move |_| stream_update_trigger += 1,
                                                            on_selection: move |data| tracing::debug!("Selection event: {:?}", data),
                                                            on_delete: {
                                                                let msg_id = message.id;
                                                                move |_| on_delete.call(msg_id)
                                                            },
                                                            on_fork: {
                                                                let msg_id = message.id;
                                                                move |_| on_fork.call(msg_id)
                                                            },
                                                            on_comment: move |_| on_comment.call(()),
                                                            show_time: stamp.show_time,
                                                        }
                                                    }
                                                }
                                                MessageContent::Relay(note) => {
                                                    rsx! {
                                                        RelayBubble {
                                                            note: note.clone(),
                                                            created_at: message.created_at,
                                                            show_time: stamp.show_time,
                                                        }
                                                    }
                                                }
                                                MessageContent::SkillCall(skill_call) => {
                                                    let container_classes = "flex justify-end text-fg";
                                                    rsx! {
                                                        div {
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: format!("relative group rounded-2xl {} shadow-md overflow-hidden", skill_call.profile_color.clone().unwrap_or_else(|| profile_color.clone())),
                                                                    div {
                                                                        class: "px-4 pt-3 pb-1 text-[10px] font-bold opacity-60 uppercase tracking-[0.2em]",
                                                                        "> SKILL EXECUTION"
                                                                    }
                                                                    crate::components::skill_call_display::SkillCallDisplay {
                                                                        skill_call: skill_call.clone(),
                                                                        on_use_result: move |output: String| {
                                                                            chat_command.set(Some(crate::components::chat_input::ChatCommand::CopyToDraft(output)));
                                                                        },
                                                                        on_analyze: move |_| {
                                                                            // Trigger AI continuation via Command Bridge
                                                                            chat_command.set(Some(crate::components::chat_input::ChatCommand::TriggerAiAnalysis));
                                                                        },
                                                                    }
                                                                }
                                                                MessageCaption {
                                                                    label: "User".to_string(),
                                                                    created_at: message.created_at,
                                                                    show_time: stamp.show_time,
                                                                    align_right: true,
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                MessageContent::SkillPermissionRequest(skill_call) => {
                                                    let container_classes = "flex justify-end text-fg";
                                                    let execution_id = skill_call.execution_id.clone();
                                                    let message_id = message.id;
                                                    rsx! {
                                                        div {
                                                            class: "{container_classes} w-full",
                                                            div {
                                                                class: "flex flex-col max-w-full min-w-0",
                                                                div {
                                                                    class: format!("relative group rounded-2xl {} shadow-md overflow-hidden", skill_call.profile_color.clone().unwrap_or_else(|| profile_color.clone())),
                                                                    div {
                                                                        class: "px-4 pt-3 pb-1 text-[10px] font-bold opacity-60 uppercase tracking-[0.2em]",
                                                                        "> SKILL COMMAND"
                                                                    }
                                                                    crate::components::skill_call_display::SkillCallDisplay {
                                                                        skill_call: skill_call.clone(),
                                                                        show_permission_prompt: true,
                                                                        on_approve: {
                                                                            let session_id = session_id.clone();
                                                                            move |_exec_id: String| {
                                                                                let msg_id = message_id;
                                                                                let session_id = session_id.clone();
                                                                                spawn(async move {
                                                                                    // Get session's profile for profile-scoped MCP context
                                                                                    let (skill_call_clone, profile_id) = {
                                                                                        let state = session_state.read();
                                                                                        if let Some(session) = state.sessions.get(&session_id) {
                                                                                            let sc = session.messages.iter()
                                                                                                .find(|m| m.id == msg_id)
                                                                                                .and_then(|m| match &m.content {
                                                                                                    MessageContent::SkillPermissionRequest(sc) => Some(sc.clone()),
                                                                                                    _ => None,
                                                                                                });
                                                                                            // Get session's composio_profile (which is the stable ID)
                                                                                            let profile = session.composio_profile.clone();
                                                                                            (sc, profile)
                                                                                        } else { (None, None) }
                                                                                    };

                                                                                    // Get FRESH, profile-scoped MCP context using mcp_manager
                                                                                    // This follows the same lifecycle as chat.rs:539-557
                                                                                    if let Some(ref id) = profile_id {
                                                                                        let _ = mcp_manager.read().ensure_native_client_for_profile(id, &settings.read()).await;
                                                                                    }
                                                                                    let mut fresh_mcp_context = mcp_manager.read().get_mcp_context(profile_id).await;
                                                                                    // Enrich with toolkit slugs from Settings so on-demand toolkits
                                                                                    // don't produce false "Missing capability" warnings
                                                                                    fresh_mcp_context.enrich_from_settings(&settings.read());
                                                                                    let mcp_context = if fresh_mcp_context.servers.is_empty() {
                                                                                        None
                                                                                    } else {
                                                                                        Some(fresh_mcp_context)
                                                                                    };

                                                                                    if let Some(mut sc) = skill_call_clone {
                                                                                        tracing::info!("Executing skill: {}", sc.skill_name);
                                                                                        match crate::skills::execute_skill(&mut sc, mcp_context.as_ref()).await {
                                                                                            Ok(result) => {
                                                                                                tracing::info!("Skill executed: {:?}", result.status);
                                                                                                // Update message with completed SkillCall
                                                                                                let mut state = session_state.write();
                                                                                                let mut skill_events: Vec<crate::session_events::SessionEvent> = Vec::new();
                                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                                        msg.content = MessageContent::SkillCall(sc.clone());
                                                                                                        skill_events.push(crate::session_events::SessionEvent::ToolResult { message: msg.clone() });
                                                                                                    }
                                                                                                    // Persist skill into session.loaded_skills
                                                                                                    if sc.status == crate::components::shared::SkillCallStatus::Completed {
                                                                                                        session.loaded_skills.insert(
                                                                                                            sc.skill_name.clone(),
                                                                                                            sc.response.clone(),
                                                                                                        );
                                                                                                        skill_events.push(crate::session_events::SessionEvent::SkillLoaded {
                                                                                                            name: sc.skill_name.clone(),
                                                                                                            payload: sc.response.clone(),
                                                                                                        });
                                                                                                        tracing::info!("Persisted skill '{}' into session.loaded_skills (permission path)", sc.skill_name);
                                                                                                    }
                                                                                                }
                                                                                                drop(state);
                                                                                                crate::session_events::log_events(&session_id, skill_events);
                                                                                                // Trigger continuation on the session that actually ran the
                                                                                                // skill, not whichever tab is active right now (see the
                                                                                                // has_pending_approvals doc comment above).
                                                                                                has_pending_approvals.write().insert(session_id.clone());
                                                                                            }
                                                                                            Err(e) => {
                                                                                                tracing::error!("Skill execution failed: {}", e);
                                                                                                sc.status = crate::components::shared::SkillCallStatus::Error;
                                                                                                sc.response = e.to_string();
                                                                                                let mut state = session_state.write();
                                                                                                let mut error_snapshot: Option<crate::components::chat::Message> = None;
                                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                                        msg.content = MessageContent::SkillCall(sc);
                                                                                                        error_snapshot = Some(msg.clone());
                                                                                                    }
                                                                                                }
                                                                                                drop(state);
                                                                                                if let Some(message) = error_snapshot {
                                                                                                    crate::session_events::log_event(&session_id, crate::session_events::SessionEvent::ToolResult { message });
                                                                                                }
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                });
                                                                            }
                                                                        },
                                                                        on_deny: {
                                                                            let session_id = session_id.clone();
                                                                            move |_exec_id: String| {
                                                                                let msg_id = message_id;
                                                                                let session_id = session_id.clone();
                                                                                // Mark skill as denied
                                                                                let mut state = session_state.write();
                                                                                let mut denied_snapshot: Option<crate::components::chat::Message> = None;
                                                                                if let Some(session) = state.sessions.get_mut(&session_id) {
                                                                                    if let Some(msg) = session.messages.iter_mut().find(|m| m.id == msg_id) {
                                                                                        if let MessageContent::SkillPermissionRequest(sc) = &msg.content {
                                                                                            let mut denied_sc = sc.clone();
                                                                                            denied_sc.status = crate::components::shared::SkillCallStatus::Error;
                                                                                            denied_sc.response = "Permission denied by user.".to_string();
                                                                                            msg.content = MessageContent::SkillCall(denied_sc);
                                                                                            denied_snapshot = Some(msg.clone());
                                                                                        }
                                                                                    }
                                                                                }
                                                                                drop(state);
                                                                                // Denials journal as ToolResult — consistent with the
                                                                                // approval path: the deny mutates the same message in
                                                                                // place, and the projector upserts by message id.
                                                                                if let Some(message) = denied_snapshot {
                                                                                    crate::session_events::log_event(&session_id, crate::session_events::SessionEvent::ToolResult { message });
                                                                                }
                                                                                tracing::info!("Skill permission denied for execution_id: {}", execution_id);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                MessageCaption {
                                                                    label: "User".to_string(),
                                                                    created_at: message.created_at,
                                                                    show_time: stamp.show_time,
                                                                    align_right: true,
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        }
                                        }
                                    }
                                }
                            }
                        }
                        }
                    } else {
                        rsx! { WelcomeMessage {} }
                    }
                }
                if *show_scroll_button.read() {
                    button {
                        class: "absolute bottom-4 right-4 z-10 p-2 bg-btn-primary text-fg rounded-full shadow-lg hover:bg-btn-primary-hover focus:outline-none focus:ring-2 focus:ring-btn-primary transition-opacity duration-300 ease-in-out",
                        onclick: move |_| {
                            let _ = document::eval(r#"
                                const el = document.getElementById('message-list');
                                if (el) { el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' }); }
                            "#);
                        },
                        Icon {
                            width: 20,
                            height: 20,
                            icon: fi_icons::FiChevronDown
                        }
                    }
                }
            }
        }
    }
}

/// A relay longer than this (chars or lines) starts collapsed.
const RELAY_COLLAPSE_CHARS: usize = 600;
const RELAY_COLLAPSE_LINES: usize = 8;

/// A dispatch relay (`todo::relay`): the AI bubble in a slightly lighter
/// grey, under a small header naming the other chat. Clicking the header
/// switches to that chat; a long relay starts collapsed.
#[component]
fn RelayBubble(
    note: crate::todo::relay::RelayNote,
    created_at: chrono::DateTime<chrono::Utc>,
    show_time: bool,
) -> Element {
    use crate::todo::relay::{RelayDirection, RelayKind};
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let long = note.text.chars().count() > RELAY_COLLAPSE_CHARS
        || note.text.lines().count() > RELAY_COLLAPSE_LINES;
    let mut expanded = use_signal(|| false);
    let collapsed = long && !expanded();
    // Parent → worker points down-right, worker → parent up-left.
    let down = matches!(note.direction, RelayDirection::ToWorker | RelayDirection::FromParent);
    let kind = match note.kind {
        RelayKind::Question => Some("question"),
        RelayKind::Result => Some("result"),
        RelayKind::Report => Some("finished"),
        RelayKind::Progress | RelayKind::Message => None,
    };
    let header = format!("{} · {}", note.direction.label(), note.peer_title);
    let peer = note.peer_session_id.clone();
    rsx! {
        div {
            class: "flex justify-start w-full",
            div {
                class: "flex flex-col max-w-[calc(100%-2.5rem)] lg:max-w-[90%] min-w-0",
                div {
                    class: "rounded-2xl bg-card text-fg self-start mr-auto max-w-full",
                    div {
                        class: "rounded-2xl bg-white/5 px-4 py-2.5 text-sm leading-relaxed break-words",
                        button {
                            class: "flex items-center gap-1.5 text-xs text-fg-muted hover:text-fg transition-colors focus:outline-none mb-1 max-w-full",
                            title: "Open this chat",
                            onclick: move |_| {
                                chat_command.set(Some(
                                    crate::components::chat_input::ChatCommand::SwitchToSession(peer.clone()),
                                ));
                            },
                            if down {
                                Icon { width: 12, height: 12, icon: fi_icons::FiCornerDownRight }
                            } else {
                                Icon { width: 12, height: 12, icon: fi_icons::FiCornerUpLeft }
                            }
                            span { class: "truncate", "{header}" }
                            if let Some(k) = kind {
                                span { class: "shrink-0", "· {k}" }
                            }
                        }
                        div {
                            class: if collapsed { "max-h-24 overflow-hidden" } else { "" },
                            crate::components::markdown_renderer::MarkdownRenderer {
                                id: None,
                                content: note.text.clone(),
                                comments: None,
                                pending_highlight: None,
                            }
                        }
                        if long {
                            button {
                                class: "mt-1 text-xs text-fg-muted hover:text-fg focus:outline-none",
                                onclick: move |_| expanded.toggle(),
                                if collapsed { "Show more" } else { "Show less" }
                            }
                        }
                    }
                }
                MessageCaption {
                    label: "Relay".to_string(),
                    created_at: created_at,
                    show_time: show_time,
                }
            }
        }
    }
}
