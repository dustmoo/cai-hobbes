//! The decision queue: a drawer over Today that clears what the agents need
//! from you — question, options, an estimate, the worker, and the brief
//! (SYMBIOTIC_PLANNER.md §4, "Decision queue").
//!
//! Answering a worker's question goes through `lanes::resolve_decision`,
//! then back to the worker over whatever channel exists (a Hobbes chat gets
//! it as a queued message; a Claude Code run can't receive input, so the
//! answer is recorded and the to-do returns to the queue). A permission
//! gate is answered through `FleetShared::resolve_gate` — never as a
//! decision the model could resolve.

use chrono::{NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::chat_input::ChatCommand;
use crate::context::trust_store::{self, DecisionKind, TrustDecision};
use crate::session::SessionState;
use crate::todo::lanes;
use crate::todo::model::{
    AgentState, DecisionAnswer, DecisionDelivery, DecisionSource, ResolvedDecision,
};
use crate::todo::store;
use crate::todo::PlannerState;

use super::agent_lane::{fmt_since, queue_minutes};
use super::{persist_todo, today};

// ── Pure helpers ────────────────────────────────────────────────────────────

/// How an answer can reach its worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryRoute {
    /// A Hobbes chat: queue the answer as a message to this session.
    Chat(String),
    /// A headless Claude Code run: no input channel.
    Headless,
    /// No worker linked.
    Unlinked,
}

pub(crate) fn delivery_route(linked: Option<&str>, is_chat: impl Fn(&str) -> bool) -> DeliveryRoute {
    match linked {
        Some(sid) if is_chat(sid) => DeliveryRoute::Chat(sid.to_string()),
        Some(_) => DeliveryRoute::Headless,
        None => DeliveryRoute::Unlinked,
    }
}

impl DeliveryRoute {
    fn delivery(&self) -> DecisionDelivery {
        match self {
            DeliveryRoute::Chat(sid) => DecisionDelivery::ChatQueued { session_id: sid.clone() },
            DeliveryRoute::Headless => DecisionDelivery::NotDelivered {
                reason: "the worker is a headless Claude Code run; the fleet has no channel to send it input"
                    .into(),
            },
            DeliveryRoute::Unlinked => {
                DecisionDelivery::NotDelivered { reason: "no worker is linked to this to-do".into() }
            }
        }
    }
}

/// The one-line result shown after an answer.
pub(crate) fn delivery_line(route: &DeliveryRoute) -> &'static str {
    match route {
        DeliveryRoute::Chat(_) => "Queued to the chat \u{2014} it sends when that tab is next active.",
        DeliveryRoute::Headless => {
            "Recorded; Claude Code runs can't receive answers. The to-do is back in the queue \u{2014} re-dispatch it to continue."
        }
        DeliveryRoute::Unlinked => "Recorded; no worker is linked \u{2014} dispatch it to continue with this answer.",
    }
}

/// `"4 decisions · about 9 min"`.
pub(crate) fn queue_header(count: usize, minutes: u32) -> String {
    match count {
        0 => "No decisions waiting".to_string(),
        1 => format!("1 decision \u{b7} about {minutes} min"),
        n => format!("{n} decisions \u{b7} about {minutes} min"),
    }
}

/// Today's trust-rule hits (routing calls answered without asking).
pub(crate) fn rule_hits_today(trust: &[TrustDecision], day: NaiveDate) -> Vec<TrustDecision> {
    let mut v: Vec<TrustDecision> = trust
        .iter()
        .filter(|t| t.decision == DecisionKind::RuleHit)
        .filter(|t| t.ts.with_timezone(&chrono::Local).date_naive() == day)
        .cloned()
        .collect();
    v.sort_by(|a, b| b.ts.cmp(&a.ts));
    v
}

// ── Answering ───────────────────────────────────────────────────────────────

/// Answer a worker's (or attention) decision and hand it back. Returns the
/// result line, or the refusal.
fn answer(
    mut planner: Signal<PlannerState>,
    session_state: Signal<SessionState>,
    todo_id: &str,
    choice: &str,
) -> Result<&'static str, String> {
    let now = Utc::now();
    let entry: ResolvedDecision = lanes::resolve_decision(&mut planner.write(), todo_id, choice, None, now)?;
    persist_todo(&planner, todo_id);
    if let Err(e) = store::save_resolved_decision(&entry) {
        tracing::error!("planner: failed to persist decision {}: {}", entry.id, e);
    }
    let linked = planner.peek().todo(todo_id).and_then(|t| t.linked_fleet_session.clone());
    let route = delivery_route(linked.as_deref(), |sid| {
        session_state.peek().sessions.contains_key(sid)
            || crate::session_store::contains(sid)
            || crate::fleet::shared()
                .snapshot()
                .sessions
                .get(sid)
                .is_some_and(|r| r.origin == crate::fleet::FleetOrigin::Hobbes)
    });
    if let DeliveryRoute::Chat(sid) = &route {
        use crate::components::chat_queue;
        chat_queue::queue_push(
            &mut chat_queue::CHAT_QUEUE.write(),
            sid,
            chat_queue::QueuedMessage::new(lanes::answer_message(&entry), vec![]),
        );
        chat_queue::request_drain();
    }
    let updated = lanes::record_delivery(&mut planner.write(), &entry.id, route.delivery(), now);
    if let Some(d) = updated {
        if let Err(e) = store::save_resolved_decision(&d) {
            tracing::error!("planner: failed to persist decision delivery {}: {}", d.id, e);
        }
    }
    persist_todo(&planner, todo_id);
    Ok(delivery_line(&route))
}

/// Approve or deny a held permission gate. The fleet server owns the
/// follow-up; the to-do records the answer so the wait isn't re-raised.
fn answer_gate(mut planner: Signal<PlannerState>, todo_id: &str, request_id: &str, allow: bool) -> &'static str {
    crate::fleet::shared().resolve_gate(request_id, allow);
    let now = Utc::now();
    {
        let mut state = planner.write();
        if let Some(t) = state.todo_mut(todo_id) {
            if let Some(d) = t.decision.as_mut().filter(|d| d.is_open()) {
                d.resolved = Some(DecisionAnswer {
                    choice: if allow { "Approve".into() } else { "Deny".into() },
                    note: None,
                    at: now,
                    delivery: None,
                });
            }
            if allow {
                t.agent_state = Some(AgentState::Working);
            }
            t.updated_at = now;
        }
    }
    persist_todo(&planner, todo_id);
    if allow {
        "Approved at the permission gate \u{2014} the worker carries on."
    } else {
        "Denied at the permission gate."
    }
}

// ── The drawer ──────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct DecisionCard {
    todo_id: String,
    context: String,
    question: String,
    options: Vec<String>,
    gate: Option<String>,
    est: String,
    worker: Option<String>,
    waiting: String,
    did: Option<String>,
    blocked: Option<String>,
    next: Option<String>,
}

/// The decision queue drawer, over Today.
#[component]
pub(crate) fn DecisionDrawer(open: Signal<bool>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut expanded = use_signal(|| Option::<String>::None);
    // Results of answers given while the drawer is open (newest first).
    let mut results = use_signal(Vec::<(String, String, bool)>::new);

    // Trust-rule hits are a SQLite read: sampled when the drawer opens (and
    // after each answer), not on every render.
    let mut rules_bump = use_signal(|| 0u64);
    let rules = use_memo(move || {
        let _ = *rules_bump.read();
        if !*open.read() {
            return Vec::new();
        }
        rule_hits_today(&trust_store::recent(500), today())
    });

    let now = Utc::now();
    let (cards, minutes) = {
        let state = planner.read();
        let queue = lanes::needs_you(&state.todos);
        let minutes = queue_minutes(&queue);
        let cards: Vec<DecisionCard> = queue
            .iter()
            .map(|t| {
                let d = t.decision.as_ref().filter(|d| d.is_open());
                let brief = d.and_then(|d| d.brief.clone());
                DecisionCard {
                    todo_id: t.id.clone(),
                    context: t.outcome.clone().filter(|o| !o.trim().is_empty()).unwrap_or_else(|| t.title.clone()),
                    question: d
                        .map(|d| d.question.clone())
                        .unwrap_or_else(|| "Waiting on you \u{2014} no question recorded; check its worker.".into()),
                    options: d.map(|d| d.options.clone()).unwrap_or_default(),
                    gate: d.and_then(|d| match &d.source {
                        DecisionSource::Gate { request_id } => Some(request_id.clone()),
                        _ => None,
                    }),
                    est: lanes::est_label(d.and_then(|d| d.est_minutes)),
                    worker: d.and_then(|d| d.worker.clone()),
                    waiting: d.map(|d| fmt_since(d.asked_at, now)).unwrap_or_default(),
                    did: brief.as_ref().map(|b| b.did.clone()).filter(|s| !s.trim().is_empty()),
                    blocked: brief.as_ref().and_then(|b| b.blocked.clone()),
                    next: brief.as_ref().and_then(|b| b.next.clone()),
                }
            })
            .collect();
        (cards, minutes)
    };
    let header = queue_header(cards.len(), minutes);
    let count = cards.len();
    let rule_rows = rules.read().clone();
    let n_rules = rule_rows.len();

    if !*open.read() {
        return rsx! {};
    }

    let mut on_result = move |(todo_id, line, ok): (String, String, bool)| {
        results.write().insert(0, (todo_id, line, ok));
        rules_bump += 1;
    };

    rsx! {
        div {
            class: "absolute inset-0 z-50 bg-black/60",
            onclick: move |_| open.set(false),
        }
        div {
            role: "dialog",
            "aria-label": "Decision queue",
            class: "absolute inset-y-0 right-0 z-50 flex w-[600px] max-w-full flex-col border-l border-primary-700/80 bg-app shadow-2xl",
            onkeydown: move |e| if e.key() == Key::Escape { open.set(false) },
            div {
                class: "flex items-center gap-2.5 px-5 pt-4",
                span { class: "text-lg font-semibold text-fg", "Decisions" }
                if count > 0 {
                    span { class: "rounded-full bg-yellow-800 px-1.5 text-[10px] font-bold leading-4 text-yellow-300", "{count}" }
                }
                span { class: "font-mono text-xs text-fg-muted", "{header}" }
                div { class: "flex-1" }
                button {
                    class: "rounded p-1 text-fg-muted hover:bg-input hover:text-fg",
                    title: "Close decisions",
                    onclick: move |_| open.set(false),
                    Icon { width: 14, height: 14, icon: fi_icons::FiX }
                }
            }
            div {
                class: "mx-5 mt-3 flex flex-col gap-1 rounded-md border border-faint px-2.5 py-2",
                div {
                    class: "flex items-center gap-2",
                    span { class: "shrink-0 text-primary-300", Icon { width: 12, height: 12, icon: fi_icons::FiShield } }
                    span { class: "text-xs text-fg", "{n_rules} handled by your rules today" }
                    span { class: "font-mono text-[10.5px] text-fg-muted", "routing \u{b7} not asked" }
                    div { class: "flex-1" }
                    button {
                        class: "text-[11.5px] text-primary-300 hover:text-fg",
                        onclick: move |_| chat_command.set(Some(ChatCommand::SwitchToSettingsTab(
                            crate::settings::SettingsTab::Permissions,
                            None,
                        ))),
                        "Trust rules \u{203a}"
                    }
                }
                for r in rule_rows.iter().take(3) {
                    div {
                        key: "{r.id}",
                        class: "flex items-center gap-2 pl-5 text-[11.5px] text-fg-muted",
                        span { class: "min-w-0 flex-1 truncate", "{r.tool} \u{b7} {r.arg_summary}" }
                        span { class: "shrink-0 font-mono text-[10.5px]", "{r.server}" }
                    }
                }
            }
            div {
                class: "flex min-h-0 flex-1 flex-col gap-2.5 overflow-y-auto px-5 pb-4 pt-3",
                for (tid, line, ok) in results.read().iter().take(4).cloned() {
                    div {
                        key: "res-{tid}-{line}",
                        class: if ok {
                            "flex items-start gap-2 rounded-md border border-faint bg-card px-2.5 py-1.5 text-[11.5px] text-fg-muted"
                        } else {
                            "flex items-start gap-2 rounded-md border border-red-800 bg-card px-2.5 py-1.5 text-[11.5px] text-red-300"
                        },
                        span { class: "shrink-0 pt-0.5 text-primary-300", Icon { width: 11, height: 11, icon: fi_icons::FiCheck } }
                        span { "{line}" }
                    }
                }
                if cards.is_empty() {
                    p { class: "py-6 text-center text-sm text-fg-muted", "Nothing needs you right now." }
                }
                for c in cards {
                    DecisionCardView {
                        key: "{c.todo_id}",
                        card: c.clone(),
                        expanded: expanded.read().as_deref() == Some(c.todo_id.as_str()),
                        on_toggle: move |id: String| {
                            let cur = expanded.peek().clone();
                            expanded.set(if cur.as_deref() == Some(id.as_str()) { None } else { Some(id) });
                        },
                        on_answer: move |(id, choice): (String, String)| {
                            let gate = planner.peek().todo(&id).and_then(|t| t.decision.as_ref()).and_then(|d| match &d.source {
                                DecisionSource::Gate { request_id } if d.is_open() => Some(request_id.clone()),
                                _ => None,
                            });
                            let outcome = match gate {
                                Some(rid) => Ok(answer_gate(planner, &id, &rid, choice.eq_ignore_ascii_case("approve"))),
                                None => answer(planner, session_state, &id, &choice),
                            };
                            match outcome {
                                Ok(line) => on_result((id, line.to_string(), true)),
                                Err(e) => on_result((id, e, false)),
                            }
                        },
                    }
                }
            }
            div {
                class: "flex items-center gap-2 border-t border-faint px-5 py-3",
                span { class: "font-mono text-[11px] text-fg-muted", "answers go straight back to each worker" }
                div { class: "flex-1" }
                button {
                    class: "rounded-md border border-faint px-2.5 py-1 text-[11px] text-fg hover:bg-input",
                    onclick: move |_| open.set(false),
                    "Later \u{b7} keep in queue"
                }
            }
        }
    }
}

#[component]
fn DecisionCardView(
    card: DecisionCard,
    expanded: bool,
    on_toggle: EventHandler<String>,
    on_answer: EventHandler<(String, String)>,
) -> Element {
    let mut other = use_signal(String::new);
    let mut other_open = use_signal(|| false);
    let gate = card.gate.is_some();
    let options: Vec<String> = if gate {
        vec!["Approve".into(), "Deny".into()]
    } else {
        card.options.clone()
    };
    let free_text = !gate && (options.is_empty() || *other_open.read());
    let id = card.todo_id.clone();
    let dash = |s: &Option<String>| s.clone().filter(|x| !x.trim().is_empty()).unwrap_or_else(|| "\u{2014}".into());

    rsx! {
        div {
            class: "flex flex-col gap-2 rounded-lg border border-faint bg-card px-3 py-2.5",
            div { class: "text-[11.5px] text-fg-muted", "{card.context}" }
            div { class: "text-sm font-medium leading-5 text-fg", "{card.question}" }
            if !options.is_empty() {
                div {
                    class: "flex flex-wrap gap-1.5",
                    for (j, o) in options.iter().cloned().enumerate() {
                        button {
                            key: "{j}",
                            class: if j == 0 {
                                "rounded-md bg-btn-primary px-2.5 py-1 text-xs text-fg hover:bg-btn-primary-hover"
                            } else {
                                "rounded-md border border-faint px-2.5 py-1 text-xs text-fg hover:bg-input"
                            },
                            onclick: {
                                let id = id.clone();
                                move |_| on_answer.call((id.clone(), o.clone()))
                            },
                            "{o}"
                        }
                    }
                    if !gate && !*other_open.read() {
                        button {
                            class: "rounded-md px-2 py-1 text-xs text-fg-muted hover:text-fg",
                            onclick: move |_| other_open.set(true),
                            "Something else\u{2026}"
                        }
                    }
                }
            }
            if free_text {
                div {
                    class: "flex items-center gap-1.5",
                    input {
                        class: "h-7 min-w-0 flex-1 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                        placeholder: "Your answer\u{2026}",
                        value: "{other}",
                        oninput: move |e| other.set(e.value()),
                        onkeydown: {
                            let id = id.clone();
                            move |e: KeyboardEvent| {
                                if e.key() == Key::Enter {
                                    let text = other.peek().trim().to_string();
                                    if !text.is_empty() {
                                        on_answer.call((id.clone(), text));
                                        other.set(String::new());
                                    }
                                }
                            }
                        },
                    }
                    button {
                        class: "rounded-md bg-btn-primary px-2.5 py-1 text-xs text-fg hover:bg-btn-primary-hover",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                let text = other.peek().trim().to_string();
                                if !text.is_empty() {
                                    on_answer.call((id.clone(), text));
                                    other.set(String::new());
                                }
                            }
                        },
                        "Send"
                    }
                }
            }
            div {
                class: "flex items-center gap-2",
                span { class: "shrink-0 font-mono text-[11px] text-yellow-300", "{card.est}" }
                if let Some(w) = card.worker.clone() {
                    span {
                        class: "inline-flex h-[18px] min-w-0 max-w-[45%] items-center gap-1 rounded-sm border border-faint px-1.5 font-mono text-[10px] text-fg",
                        Icon { width: 11, height: 11, icon: fi_icons::FiTerminal }
                        span { class: "truncate", "{w}" }
                    }
                }
                if gate {
                    span { class: "font-mono text-[10px] text-fg-muted", "permission gate" }
                }
                span { class: "font-mono text-[11px] text-fg-muted", "waiting {card.waiting}" }
                div { class: "flex-1" }
                button {
                    class: "shrink-0 text-[11px] text-primary-300 hover:text-fg",
                    "aria-expanded": if expanded { "true" } else { "false" },
                    onclick: {
                        let id = id.clone();
                        move |_| on_toggle.call(id.clone())
                    },
                    "Brief "
                    span { class: "font-mono", if expanded { "\u{25be}" } else { "\u{25b8}" } }
                }
            }
            if expanded {
                div {
                    class: "grid grid-cols-[64px_minmax(0,1fr)] gap-x-3 gap-y-1 border-t border-faint pt-2 text-xs text-fg",
                    span { class: "font-mono text-[10.5px] uppercase text-fg-muted", "Did" }
                    span { "{dash(&card.did)}" }
                    span { class: "font-mono text-[10.5px] uppercase text-fg-muted", "Blocked" }
                    span { "{dash(&card.blocked)}" }
                    span { class: "font-mono text-[10.5px] uppercase text-fg-muted", "Next" }
                    span { "{dash(&card.next)}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_follows_the_linked_worker() {
        assert_eq!(delivery_route(Some("chat1"), |s| s == "chat1"), DeliveryRoute::Chat("chat1".into()));
        assert_eq!(delivery_route(Some("cc-run"), |_| false), DeliveryRoute::Headless);
        assert_eq!(delivery_route(None, |_| true), DeliveryRoute::Unlinked);
        assert!(delivery_line(&DeliveryRoute::Chat("x".into())).starts_with("Queued to the chat"));
        assert!(delivery_line(&DeliveryRoute::Headless).contains("Claude Code runs can't receive answers"));
        assert!(matches!(DeliveryRoute::Headless.delivery(), DecisionDelivery::NotDelivered { .. }));
        assert!(matches!(DeliveryRoute::Chat("s".into()).delivery(), DecisionDelivery::ChatQueued { .. }));
    }

    #[test]
    fn header_counts_and_minutes() {
        assert_eq!(queue_header(0, 0), "No decisions waiting");
        assert_eq!(queue_header(1, 2), "1 decision \u{b7} about 2 min");
        assert_eq!(queue_header(4, 9), "4 decisions \u{b7} about 9 min");
    }

    #[test]
    fn rule_hits_are_today_only_and_rule_hits_only() {
        let d: NaiveDate = "2026-10-07".parse().unwrap();
        let at = crate::components::planner_timeline::local_minutes_to_utc(d, 10 * 60);
        let mk = |kind, ts| {
            let mut t = TrustDecision::new(kind, "fleet", "Bash", "cargo test".into(), None, None, None);
            t.ts = ts;
            t
        };
        let rows = vec![
            mk(DecisionKind::RuleHit, at),
            mk(DecisionKind::Approved, at),
            mk(DecisionKind::RuleHit, at - chrono::Duration::days(1)),
        ];
        assert_eq!(rule_hits_today(&rows, d).len(), 1);
    }
}
