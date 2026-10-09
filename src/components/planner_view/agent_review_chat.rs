//! The chat rail on the Agent time review: "Talk about this run".
//!
//! It shows the last few plain messages of the agent-time chat and a
//! composer. A card's chat icon sets a reference chip ("On: <title>"); a
//! message sent with a chip is kept on that result as a note
//! ([`PlannerState::add_result_note`]) and prefixed `[On: <title>] ` in the
//! chat so Hobbes knows which result is meant. Sending only queues the
//! message to the chat: replies use agent time only if the person asks for
//! more work.

use chrono::{NaiveDate, Utc};
use dioxus::prelude::*;

use crate::components::chat::Message;
use crate::components::chat_input::ChatCommand;
use crate::components::chat_queue::{queue_push, request_drain, QueuedMessage, CHAT_QUEUE};
use crate::components::shared::MessageContent;
use crate::session::SessionState;
use crate::todo::store;
use crate::todo::PlannerState;

use super::today;

/// How many messages the rail shows.
pub(crate) const RAIL_MESSAGES: usize = 12;

/// The DOM id of the composer, so a card's chat icon can focus it.
pub(crate) const COMPOSER_ID: &str = "agent-review-composer";

/// What the composer is "on": a result card (proposal id) and its title.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatRef {
    pub id: String,
    pub title: String,
}

/// One bubble in the rail.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bubble {
    pub from_user: bool,
    pub text: String,
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The chat the rail talks in: today's agent-time chat
/// ([`crate::agent_time_runtime::todays_chat`]), else the chat of the most
/// recent run that has one (the review may be of an earlier day).
pub(crate) fn rail_chat_id(planner: &PlannerState, today: NaiveDate) -> Option<String> {
    crate::agent_time_runtime::todays_chat(planner, None, today).or_else(|| {
        planner
            .agent_time_sessions
            .iter()
            .filter(|s| s.chat_session_id.is_some())
            .max_by_key(|s| s.started_at)
            .and_then(|s| s.chat_session_id.clone())
    })
}

/// The runtime's own opening messages to the chat; not a person talking.
fn is_run_seed(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("[Agent time:") || t.starts_with("[New agent-time run:")
}

/// The last `limit` plain-text messages from the user or Hobbes, oldest
/// first. Tool calls, permission prompts, errors, empty text and the
/// runtime's seed messages are skipped.
pub(crate) fn visible_bubbles(messages: &[Message], limit: usize) -> Vec<Bubble> {
    let mut out: Vec<Bubble> = messages
        .iter()
        .filter_map(|m| {
            let from_user = match m.author.as_str() {
                "User" => true,
                "Hobbes" => false,
                _ => return None,
            };
            let MessageContent::Text { content, .. } = &m.content else {
                return None;
            };
            let text = content.trim();
            if text.is_empty() || (from_user && is_run_seed(text)) {
                return None;
            }
            Some(Bubble { from_user, text: text.to_string() })
        })
        .collect();
    let skip = out.len().saturating_sub(limit);
    out.drain(..skip);
    out
}

/// The message queued to the chat: prefixed with the card it is about.
pub(crate) fn chat_message(on_title: Option<&str>, text: &str) -> String {
    match on_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("[On: {t}] {}", text.trim()),
        None => text.trim().to_string(),
    }
}

/// Make sure the chat is in memory; hydrate it from the store if not.
/// Returns false when it can't be found.
fn hydrate(mut session_state: Signal<SessionState>, chat_id: &str) -> bool {
    let present = session_state.peek().sessions.contains_key(chat_id);
    if present {
        return true;
    }
    match crate::session_store::load_session(chat_id) {
        Some(s) => {
            session_state.write().sessions.insert(chat_id.to_string(), s);
            true
        }
        None => false,
    }
}

// ── The rail ────────────────────────────────────────────────────────────────

#[component]
pub(crate) fn ChatRail(chat_ref: Signal<Option<ChatRef>>) -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut draft = use_signal(String::new);

    let chat_id = use_memo(move || rail_chat_id(&planner.read(), today()));
    // Bring the chat into memory so its messages can show.
    use_effect(move || {
        if let Some(id) = chat_id() {
            hydrate(session_state, &id);
        }
    });
    let bubbles = use_memo(move || {
        let id = chat_id()?;
        session_state
            .read()
            .sessions
            .get(&id)
            .map(|s| visible_bubbles(&s.messages, RAIL_MESSAGES))
    });
    let bubbles_now = bubbles.read().clone().unwrap_or_default();
    let chat = chat_id();
    let disabled = chat.is_none();
    let reference = chat_ref.read().clone();

    let mut send = move || {
        let text = draft.peek().trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(chat_id) = chat_id.peek().clone() else {
            return;
        };
        let reference = chat_ref.peek().clone();
        if let Some(r) = &reference {
            let saved = planner.write().add_result_note(&r.id, &text, Utc::now());
            match saved {
                Ok(p) => {
                    if let Err(e) = store::save_proposal(&p) {
                        tracing::error!("planner: failed to save proposal {}: {}", p.id, e);
                    }
                }
                Err(e) => tracing::warn!("planner: couldn't note result {}: {}", r.id, e),
            }
        }
        if !hydrate(session_state, &chat_id) {
            tracing::warn!("agent-time chat {} could not be loaded", chat_id);
            return;
        }
        let msg = chat_message(reference.as_ref().map(|r| r.title.as_str()), &text);
        queue_push(&mut CHAT_QUEUE.write(), &chat_id, QueuedMessage::new(msg, vec![]));
        request_drain();
        draft.set(String::new());
        chat_ref.set(None);
    };

    rsx! {
        div {
            class: "flex w-[360px] max-w-full shrink-0 flex-col gap-2.5 self-start rounded-lg border border-faint bg-section p-3.5",
            div { class: "flex items-center gap-2",
                span { class: "text-[11px] uppercase tracking-[0.08em] text-fg-muted", "Talk about this run" }
                div { class: "flex-1" }
                if let Some(id) = chat.clone() {
                    button {
                        class: "text-[11px] text-primary-300 hover:text-fg",
                        title: "Open the agent-time chat in its own tab",
                        onclick: move |_| chat_command.set(Some(ChatCommand::SwitchToSession(id.clone()))),
                        "Open as tab"
                    }
                }
            }
            if disabled {
                p { class: "text-xs leading-relaxed text-fg-muted", "The chat starts with the first agent time." }
            } else if bubbles_now.is_empty() {
                p { class: "text-xs leading-relaxed text-fg-muted", "Nothing said yet. Ask about a result, or tell Hobbes what to do differently next time." }
            } else {
                div { class: "flex max-h-[420px] flex-col gap-1.5 overflow-y-auto",
                    for (i, b) in bubbles_now.iter().enumerate() {
                        div {
                            key: "{i}",
                            class: if b.from_user {
                                "ml-auto max-w-[88%] whitespace-pre-line rounded-lg bg-btn-primary px-2.5 py-1.5 text-xs leading-4 text-fg"
                            } else {
                                "mr-auto max-w-[88%] whitespace-pre-line rounded-lg border border-faint bg-card px-2.5 py-1.5 text-xs leading-4 text-fg"
                            },
                            "{b.text}"
                        }
                    }
                }
            }
            if let Some(r) = reference {
                div { class: "flex min-w-0 items-center gap-1.5 self-start rounded-full border border-primary-400 px-2 py-0.5 text-[11px] text-fg",
                    span { class: "min-w-0 truncate", title: "{r.title}", "On: {r.title}" }
                    button {
                        class: "shrink-0 text-fg-muted hover:text-fg",
                        "aria-label": "Clear the reference",
                        onclick: move |_| chat_ref.set(None),
                        "\u{d7}"
                    }
                }
            }
            textarea {
                id: COMPOSER_ID,
                class: "min-h-[64px] w-full resize-y rounded border border-faint bg-input px-2 py-1.5 text-xs text-fg outline-none disabled:opacity-50",
                placeholder: "Say something about the run\u{2026}",
                disabled: disabled,
                value: "{draft}",
                oninput: move |e| draft.set(e.value()),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Enter && !e.modifiers().shift() {
                        e.prevent_default();
                        send();
                    }
                },
            }
            div { class: "flex items-center gap-2",
                span { class: "min-w-0 flex-1 text-[11px] leading-4 text-fg-muted",
                    "Replies use agent time only if you ask for more work."
                }
                button {
                    class: "shrink-0 rounded bg-btn-primary px-3 py-1 text-xs text-fg hover:bg-btn-primary-hover disabled:opacity-50",
                    disabled: disabled || draft.read().trim().is_empty(),
                    onclick: move |_| send(),
                    "Send"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::AgentTimeSession;

    fn msg(author: &str, content: MessageContent) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content,
            attachments: vec![],
            comments: vec![],
            created_at: Utc::now(),
            usage: None,
        }
    }

    fn text(author: &str, t: &str) -> Message {
        msg(
            author,
            MessageContent::Text {
                content: t.into(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            },
        )
    }

    fn run(id: &str, chat: Option<&str>, started: chrono::DateTime<Utc>) -> AgentTimeSession {
        AgentTimeSession {
            id: id.into(),
            intent: "x".into(),
            started_at: started,
            ended_at: Some(started + chrono::Duration::minutes(5)),
            cost_usd: 0.0,
            tool_calls: vec![],
            outputs: vec![],
            end_reason: None,
            summary: None,
            chat_session_id: chat.map(String::from),
            out_of_scope_attempts: 0,
            todo_id: None,
            reviewed_at: None,
        }
    }

    #[test]
    fn only_plain_user_and_hobbes_text_shows_and_the_seed_is_skipped() {
        let msgs = vec![
            text("User", "[Agent time: Hobbes opened this on the user's behalf]\n\nrules"),
            text("Hobbes", "Starting with the digest."),
            msg("Hobbes", MessageContent::Error { message: "boom".into() }),
            text("System", "ignored"),
            text("User", "   "),
            text("User", "Why did you skip the invoice?"),
        ];
        let b = visible_bubbles(&msgs, 12);
        assert_eq!(
            b,
            vec![
                Bubble { from_user: false, text: "Starting with the digest.".into() },
                Bubble { from_user: true, text: "Why did you skip the invoice?".into() },
            ]
        );
    }

    #[test]
    fn only_the_last_few_show() {
        let msgs: Vec<Message> = (0..20).map(|i| text("User", &format!("m{i}"))).collect();
        let b = visible_bubbles(&msgs, 12);
        assert_eq!(b.len(), 12);
        assert_eq!(b[0].text, "m8");
        assert_eq!(b[11].text, "m19");
    }

    #[test]
    fn the_message_carries_the_card_it_is_about() {
        assert_eq!(chat_message(Some("Q3 digest"), " too long "), "[On: Q3 digest] too long");
        assert_eq!(chat_message(None, "hello"), "hello");
        assert_eq!(chat_message(Some("  "), "hello"), "hello");
    }

    #[test]
    fn the_rail_talks_in_the_most_recent_chat() {
        let mut p = PlannerState::default();
        assert_eq!(rail_chat_id(&p, today()), None);
        let old = Utc::now() - chrono::Duration::days(3);
        p.agent_time_sessions.push(run("a", Some("chat-old"), old));
        p.agent_time_sessions.push(run("b", Some("chat-older"), old - chrono::Duration::days(1)));
        p.agent_time_sessions.push(run("c", None, old + chrono::Duration::days(1)));
        assert_eq!(rail_chat_id(&p, today()).as_deref(), Some("chat-old"));
        p.agent_time_sessions.push(run("d", Some("chat-today"), Utc::now() - chrono::Duration::minutes(10)));
        assert_eq!(rail_chat_id(&p, today()).as_deref(), Some("chat-today"));
    }
}
