//! The agent lane on Today: the header's "Agents" readout, the lane column
//! of cards (Needs you / Working / Stuck / Queued / Done today), and the
//! fleet → `agent_state` sync (SYMBIOTIC_PLANNER.md §2, §4).
//!
//! Progress is reported, never judged: cards show the worker's own
//! `latest_progress` line and the explicit state the fleet implies. Cost is
//! shown only where Hobbes actually knows it (a hydrated Hobbes chat's
//! running total); Claude Code runs report no cost, so none is shown.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::chat_input::ChatCommand;
use crate::fleet::{FleetOrigin, FleetSession, FleetState};
use crate::session::SessionState;
use crate::todo::lanes::{self, AgentLane};
use crate::todo::model::{self, AgentState, DecisionSource, Lane, Todo};
use crate::todo::{handlers, PlannerState};

use super::{persist_todo, today, DecisionsDrawerContext};

// ── Pure helpers ────────────────────────────────────────────────────────────

/// Who is doing an agent to-do, as a card shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkerLabel {
    pub name: String,
    /// A Hobbes chat (speech icon) rather than a Claude Code run (terminal).
    pub chat: bool,
    /// Known only for a hydrated Hobbes chat (its running total).
    pub cost_usd: Option<f64>,
    /// The linked fleet session's active minutes today, when it reports any.
    pub active_minutes: Option<u32>,
}

/// What the worker lookups found for one to-do.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerFacts<'a> {
    /// A hydrated Hobbes chat with this id: (title, total cost).
    pub chat: Option<(&'a str, f64)>,
    /// The live fleet row: (display name, is a Hobbes chat, active minutes today).
    pub fleet: Option<(&'a str, bool, u32)>,
    /// The worker named on the decision.
    pub decision_worker: Option<&'a str>,
}

/// Resolve the worker label: a Hobbes chat's title beats the fleet's name;
/// a decision's worker name is the last resort. `None` when nothing names
/// a worker (not dispatched yet).
pub(crate) fn resolve_worker(linked: bool, f: &WorkerFacts) -> Option<WorkerLabel> {
    let active = f.fleet.map(|(_, _, m)| m).filter(|m| *m > 0);
    if let Some((title, cost)) = f.chat {
        return Some(WorkerLabel {
            name: title.to_string(),
            chat: true,
            cost_usd: (cost > 0.0).then_some(cost),
            active_minutes: active,
        });
    }
    if let Some((name, is_chat, _)) = f.fleet {
        return Some(WorkerLabel {
            name: name.to_string(),
            chat: is_chat,
            cost_usd: None,
            active_minutes: active,
        });
    }
    let name = f.decision_worker.map(str::trim).filter(|w| !w.is_empty());
    match (name, linked) {
        (Some(n), _) => Some(WorkerLabel { name: n.to_string(), chat: false, cost_usd: None, active_minutes: None }),
        (None, true) => Some(WorkerLabel {
            name: "linked session".to_string(),
            chat: false,
            cost_usd: None,
            active_minutes: None,
        }),
        (None, false) => None,
    }
}

/// The card's model label ("Haiku 5.5") from the model dispatch recorded.
pub(crate) fn worker_model_label(t: &Todo) -> Option<String> {
    t.worker_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(crate::settings::short_model_label)
}

/// `"just now"` / `"9m"` / `"1h 05m"` between two instants.
pub(crate) fn fmt_since(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let m = (now - from).num_minutes().max(0) as u32;
    if m == 0 {
        "just now".to_string()
    } else {
        model::format_minutes(m)
    }
}

/// The card's right-hand meta: elapsed (state-appropriate) and cost if known.
pub(crate) fn card_meta(todo: &Todo, worker: Option<&WorkerLabel>, now: DateTime<Utc>) -> String {
    let mut parts: Vec<String> = Vec::new();
    match todo.agent_state {
        Some(AgentState::NeedsYou) => {
            if let Some(d) = todo.decision.as_ref().filter(|d| d.is_open()) {
                parts.push(format!("waiting {}", fmt_since(d.asked_at, now)));
            }
        }
        Some(AgentState::Stuck) => parts.push(format!("stuck {}", fmt_since(todo.updated_at, now))),
        _ => {
            if let Some(m) = worker.and_then(|w| w.active_minutes) {
                parts.push(model::format_minutes(m));
            }
        }
    }
    if let Some(c) = worker.and_then(|w| w.cost_usd) {
        parts.push(format!("${c:.2}"));
    }
    parts.join(" \u{b7} ")
}

/// Minutes to clear the decision queue (each open decision's estimate, a
/// minute when none was given).
pub(crate) fn queue_minutes(queue: &[&Todo]) -> u32 {
    queue
        .iter()
        .map(|t| t.decision.as_ref().and_then(|d| d.est_minutes).unwrap_or(1).max(1))
        .sum()
}

/// The fleet worker names `@word` capture may resolve to: live, unended
/// sessions' display names, deduplicated.
pub(crate) fn capture_workers(state: &FleetState) -> Vec<String> {
    let mut names: Vec<String> = state
        .sessions
        .values()
        .filter(|s| s.ended_at.is_none())
        .map(|s| s.display_name().to_string())
        .filter(|n| !n.trim().is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn worker_for(
    todo: &Todo,
    fleet: &FleetState,
    sessions: &SessionState,
    day: NaiveDate,
    now: DateTime<Utc>,
) -> Option<WorkerLabel> {
    let linked = todo.linked_fleet_session.as_deref();
    let chat = linked.and_then(|sid| sessions.sessions.get(sid)).map(|s| (s.name.as_str(), s.total_cost()));
    let fleet_row = linked.and_then(|sid| fleet.sessions.get(sid)).map(|r: &FleetSession| {
        (r.display_name(), r.origin == FleetOrigin::Hobbes, r.minutes_on(day, now))
    });
    let facts = WorkerFacts {
        chat,
        fleet: fleet_row,
        decision_worker: todo.decision.as_ref().and_then(|d| d.worker.as_deref()),
    };
    resolve_worker(linked.is_some(), &facts)
}

/// Where clicking an agent card goes. Navigation only: opening a worker
/// approves, denies or sends nothing — it takes the person to where the
/// question or permission prompt is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CardOpen {
    /// A Hobbes chat that is in memory or a Hobbes-origin fleet row: open
    /// its tab (`SwitchToSession` hydrates a closed one from History).
    Chat(String),
    /// A Claude Code run: go to its terminal (`terminal_focus`), falling
    /// back to the Fleet when there's no way there.
    Terminal(String),
    /// Linked, but neither in memory nor in the fleet mirror: decided on
    /// click (a stored chat opens as a tab, a stored Claude Code run goes to
    /// its terminal, anything else opens the Fleet).
    Unresolved(String),
    /// No worker yet: the to-do's details.
    Details,
}

/// **Pure.** `in_memory` = the linked id is a hydrated chat; `fleet_origin`
/// = the linked id's fleet row, `Some(true)` when Hobbes started it.
pub(crate) fn card_open(linked: Option<&str>, in_memory: bool, fleet_origin: Option<bool>) -> CardOpen {
    match (linked, in_memory, fleet_origin) {
        (None, _, _) => CardOpen::Details,
        (Some(id), true, _) | (Some(id), _, Some(true)) => CardOpen::Chat(id.to_string()),
        (Some(id), false, Some(false)) => CardOpen::Terminal(id.to_string()),
        (Some(id), false, None) => CardOpen::Unresolved(id.to_string()),
    }
}

// ── Header readout ──────────────────────────────────────────────────────────

/// The "Agents" header readout: working · needs you · done today, and the
/// time it takes to clear the decisions when any are waiting.
#[component]
pub(crate) fn AgentsReadout() -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut drawer = use_context::<DecisionsDrawerContext>().0;
    // Release gate: without the decision queue, "needs you" is a count only
    // (each card opens its worker instead).
    let decisions_on = use_context::<Signal<crate::settings::Settings>>().read().agent_time_enabled();
    let (working, needs, done, minutes) = {
        let state = planner.read();
        let lane = lanes::agent_lane(&state.todos, today());
        (lane.working.len(), lane.needs_you.len(), lane.done_today.len(), queue_minutes(&lane.needs_you))
    };
    rsx! {
        div {
            class: "flex w-[236px] max-w-full shrink-0 flex-col gap-1 rounded-lg border border-primary-700/80 bg-section px-3 py-1.5",
            div {
                class: "flex items-baseline gap-2",
                span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "Agents" }
            }
            div {
                class: "flex h-[18px] items-center gap-2.5 whitespace-nowrap text-xs text-fg",
                span { class: "inline-flex items-center gap-1",
                    span { class: "h-[7px] w-[7px] rounded-full bg-primary-400" }
                    "{working} working"
                }
                span { class: "inline-flex items-center gap-1",
                    if needs > 0 {
                        span { class: "rounded-full bg-yellow-800 px-1.5 text-[10px] font-bold leading-4 text-yellow-300", "{needs}" }
                    } else {
                        span { class: "text-fg-muted", "0" }
                    }
                    "needs you"
                }
                span { class: "text-fg-muted", "{done} done" }
            }
            if needs > 0 && decisions_on {
                button {
                    class: "self-start font-mono text-[10px] text-primary-300 hover:text-fg",
                    title: "Open the decision queue",
                    onclick: move |_| drawer.set(true),
                    "Open decisions \u{b7} about {minutes} min to clear"
                }
            } else if needs > 0 {
                span { class: "font-mono text-[10px] text-fg-muted", "about {minutes} min to clear" }
            } else if decisions_on {
                span { class: "font-mono text-[10px] text-fg-muted", "no decisions waiting" }
            } else {
                span { class: "font-mono text-[10px] text-fg-muted", "nothing waiting on you" }
            }
        }
    }
}

// ── The lane column ─────────────────────────────────────────────────────────

const SEC: &str = "flex items-center gap-2 px-4 pt-3 pb-1.5 text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const CARD: &str = "mx-3 mb-2 flex cursor-pointer flex-col gap-1 rounded-lg border border-faint border-l-2 border-l-[#EA8A2E] bg-card px-2.5 py-2 text-left hover:bg-input";
const MINI: &str = "mx-3 flex h-7 items-center gap-2 rounded px-2 text-xs text-fg whitespace-nowrap";
const WK: &str = "inline-flex h-[18px] min-w-0 max-w-[60%] items-center gap-1 rounded-sm border border-faint px-1.5 font-mono text-[10px] text-fg";

/// The agent lane: cards grouped Needs you / Working / Stuck / Queued /
/// Done today. `embedded` renders it as a collapsible section inside the
/// right column (narrow windows) instead of its own column.
#[component]
pub(crate) fn AgentLaneColumn(#[props(default)] embedded: bool) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let fleet_state = use_context::<Signal<FleetState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let mut drawer = use_context::<DecisionsDrawerContext>().0;
    let decisions_on = use_context::<Signal<crate::settings::Settings>>().read().agent_time_enabled();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut detail = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    let mut collapsed = use_signal(|| None::<bool>);
    // Where the last go-to-terminal click ended up (fades after a few
    // seconds); `note_seq` keeps an older fade from clearing a newer note.
    let mut note = use_signal(|| None::<String>);
    let mut note_seq = use_signal(|| 0u64);

    // A once-a-minute tick keeps "waiting 9m" honest.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            tick += 1;
        }
    });
    let _t = *tick.read();

    let day = today();
    let now = Utc::now();
    let state = planner.read();
    let fleet = fleet_state.read();
    let sessions = session_state.read();
    let lane: AgentLane = lanes::agent_lane(&state.todos, day);
    let label = |t: &Todo| worker_for(t, &fleet, &sessions, day, now);
    let card = |t: &Todo| -> CardData {
        let w = label(t);
        let linked = t.linked_fleet_session.as_deref();
        let open = card_open(
            linked,
            linked.is_some_and(|sid| sessions.sessions.contains_key(sid)),
            linked.and_then(|sid| fleet.sessions.get(sid)).map(|r| r.origin == FleetOrigin::Hobbes),
        );
        CardData {
            id: t.id.clone(),
            open,
            outcome: t.outcome.clone().filter(|o| !o.trim().is_empty()).unwrap_or_else(|| t.title.clone()),
            question: t.decision.as_ref().filter(|d| d.is_open()).map(|d| d.question.clone()),
            gate: t
                .decision
                .as_ref()
                .is_some_and(|d| d.is_open() && matches!(d.source, DecisionSource::Gate { .. })),
            est: t
                .decision
                .as_ref()
                .filter(|d| d.is_open())
                .map(|d| lanes::est_label(d.est_minutes)),
            progress: t.latest_progress.clone().filter(|p| !p.trim().is_empty()),
            blocked: t
                .decision
                .as_ref()
                .and_then(|d| d.brief.as_ref())
                .and_then(|b| b.blocked.clone()),
            meta: card_meta(t, w.as_ref(), now),
            model: w.as_ref().and(worker_model_label(t)),
            worker: w,
        }
    };
    let needs: Vec<CardData> = lane.needs_you.iter().map(|t| card(t)).collect();
    let working: Vec<CardData> = lane.working.iter().map(|t| card(t)).collect();
    let stuck: Vec<CardData> = lane.stuck.iter().map(|t| card(t)).collect();
    let queued: Vec<(String, String)> = lane
        .to_dispatch
        .iter()
        .map(|t| (t.id.clone(), t.outcome.clone().filter(|o| !o.trim().is_empty()).unwrap_or_else(|| t.title.clone())))
        .collect();
    let done: Vec<(String, String, String)> = lane
        .done_today
        .iter()
        .map(|t| {
            let w = label(t);
            let meta = w
                .as_ref()
                .and_then(|w| w.active_minutes)
                .map(model::format_minutes)
                .unwrap_or_default();
            (t.id.clone(), t.outcome.clone().filter(|o| !o.trim().is_empty()).unwrap_or_else(|| t.title.clone()), meta)
        })
        .collect();
    let minutes = queue_minutes(&lane.needs_you);
    let counts = lane.counts_line();
    let empty = lane.is_empty();
    let n_needs = needs.len();
    drop(sessions);
    drop(fleet);
    drop(state);

    // Go to a Claude Code run's terminal; with no row to go by, or when
    // every rung fails, open the Fleet instead.
    let mut go_to_terminal = move |row: Option<FleetSession>| {
        let Some(row) = row.filter(|r| r.origin == FleetOrigin::External) else {
            chat_command.set(Some(ChatCommand::ToggleFleet));
            return;
        };
        spawn(async move {
            let name = row.display_name().to_string();
            let outcome = crate::fleet::terminal_focus::go_to_session(row).await;
            if matches!(outcome, crate::fleet::terminal_focus::FocusOutcome::Failed(_)) {
                chat_command.set(Some(ChatCommand::ToggleFleet));
            }
            let seq = *note_seq.peek() + 1;
            note_seq.set(seq);
            note.set(Some(outcome.note(&name)));
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            if *note_seq.peek() == seq {
                note.set(None);
            }
        });
    };
    // Card click: open the worker so the person can unblock it there.
    let open_card = move |(id, open): (String, CardOpen)| match open {
        CardOpen::Chat(sid) => chat_command.set(Some(ChatCommand::SwitchToSession(sid))),
        CardOpen::Terminal(sid) => {
            let row = crate::fleet::shared().snapshot().sessions.get(&sid).cloned();
            go_to_terminal(row.or_else(|| crate::fleet::store::load_session(&sid)));
        }
        CardOpen::Unresolved(sid) => {
            if crate::session_store::contains(&sid) {
                chat_command.set(Some(ChatCommand::SwitchToSession(sid)));
            } else {
                go_to_terminal(crate::fleet::store::load_session(&sid));
            }
        }
        CardOpen::Details => detail.set(Some(id)),
    };
    let is_collapsed = collapsed.read().unwrap_or(embedded && n_needs == 0);
    let root_class = if embedded {
        "flex shrink-0 flex-col rounded border border-faint bg-app pb-2"
    } else {
        "h-full w-full flex flex-col overflow-y-auto bg-section pb-4"
    };

    rsx! {
        div {
            class: "{root_class}",
            div {
                class: "{SEC}",
                if embedded {
                    button {
                        class: "inline-flex items-center gap-1 uppercase tracking-[0.08em] hover:text-fg",
                        onclick: move |_| collapsed.set(Some(!is_collapsed)),
                        span { class: "font-mono", if is_collapsed { "\u{25b8}" } else { "\u{25be}" } }
                        "Agent lane"
                    }
                    if is_collapsed && !counts.is_empty() {
                        span { class: "min-w-0 truncate normal-case tracking-normal", "\u{b7} {counts}" }
                    }
                } else {
                    span { "Agent lane" }
                }
                div { class: "flex-1" }
                button {
                    class: "text-[11px] normal-case tracking-normal text-primary-300 hover:text-fg",
                    title: "Open the Fleet",
                    onclick: move |_| chat_command.set(Some(ChatCommand::ToggleFleet)),
                    "Fleet \u{203a}"
                }
            }
            if let Some(n) = note.read().clone() {
                p { class: "px-4 pb-1 text-[11px] leading-relaxed text-fg-muted", "{n}" }
            }
            if !is_collapsed {
                if empty {
                    p { class: "px-4 py-2 text-[11px] leading-relaxed text-fg-muted",
                        "No agent work today. Hand a to-do over with @hobbes when you add it, or dispatch one from its row."
                    }
                }
                if !needs.is_empty() {
                    div {
                        class: "{SEC} pt-1",
                        span { class: "rounded-full bg-yellow-800 px-1.5 text-[10px] font-bold leading-4 text-yellow-300", "{n_needs}" }
                        span { class: "text-fg", "Needs you" }
                        span { class: "font-mono normal-case tracking-normal", "\u{b7} about {minutes} min" }
                        div { class: "flex-1" }
                        if decisions_on {
                            button {
                                class: "text-[11px] normal-case tracking-normal rounded px-2 py-0.5 bg-btn-primary text-fg hover:bg-btn-primary-hover",
                                onclick: move |_| drawer.set(true),
                                "Open decisions"
                            }
                        }
                    }
                    for c in needs {
                        LaneCard { key: "{c.id}", data: c.clone(), need: true, on_open: open_card, on_details: move |id: String| detail.set(Some(id)) }
                    }
                }
                if !working.is_empty() {
                    div {
                        class: "{SEC}",
                        span { class: "h-[7px] w-[7px] rounded-full bg-primary-400" }
                        span { class: "text-fg", "Working" }
                        span { "\u{b7} {working.len()}" }
                    }
                    for c in working {
                        LaneCard { key: "{c.id}", data: c.clone(), need: false, on_open: open_card, on_details: move |id: String| detail.set(Some(id)) }
                    }
                }
                if !stuck.is_empty() {
                    div {
                        class: "{SEC}",
                        span { class: "h-[7px] w-[7px] rounded-full bg-red-400" }
                        span { class: "text-fg", "Stuck" }
                        span { "\u{b7} {stuck.len()}" }
                    }
                    for c in stuck {
                        LaneCard { key: "{c.id}", data: c.clone(), need: false, on_open: open_card, on_details: move |id: String| detail.set(Some(id)) }
                    }
                }
                if !queued.is_empty() {
                    div {
                        class: "{SEC}",
                        span { class: "text-fg", "Queued" }
                        span { "\u{b7} {queued.len()}" }
                    }
                    div {
                        class: "flex flex-col gap-0.5",
                        for (id, title) in queued {
                            div {
                                key: "{id}",
                                class: "{MINI} border border-dashed border-faint",
                                button {
                                    class: "min-w-0 flex-1 truncate text-left hover:text-primary-300",
                                    onclick: {
                                        let id = id.clone();
                                        move |_| detail.set(Some(id.clone()))
                                    },
                                    "{title}"
                                }
                                button {
                                    class: "shrink-0 rounded border border-faint px-2 text-[11px] leading-5 text-fg hover:bg-input",
                                    title: "Dispatch to a background agent",
                                    onclick: {
                                        let id = id.clone();
                                        move |_| chat_command.set(Some(ChatCommand::DispatchTodoToAgent(id.clone())))
                                    },
                                    "Dispatch"
                                }
                            }
                        }
                    }
                }
                if !done.is_empty() {
                    div {
                        class: "{SEC}",
                        span { class: "text-fg", "Done today" }
                        span { "\u{b7} {done.len()}" }
                    }
                    div {
                        class: "flex flex-col gap-0.5",
                        for (id, title, meta) in done {
                            button {
                                key: "{id}",
                                class: "{MINI} text-fg-muted hover:bg-card",
                                onclick: {
                                    let id = id.clone();
                                    move |_| detail.set(Some(id.clone()))
                                },
                                span { class: "shrink-0 text-primary-300", Icon { width: 12, height: 12, icon: fi_icons::FiCheck } }
                                span { class: "min-w-0 flex-1 truncate text-left", "{title}" }
                                span { class: "shrink-0 font-mono text-[11px]", "{meta}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
struct CardData {
    id: String,
    open: CardOpen,
    outcome: String,
    question: Option<String>,
    gate: bool,
    est: Option<String>,
    progress: Option<String>,
    blocked: Option<String>,
    meta: String,
    worker: Option<WorkerLabel>,
    /// The worker's model ("Haiku 5.5"), when dispatch recorded one.
    model: Option<String>,
}

#[component]
fn LaneCard(
    data: CardData,
    need: bool,
    on_open: EventHandler<(String, CardOpen)>,
    on_details: EventHandler<String>,
) -> Element {
    let id = data.id.clone();
    let details_id = data.id.clone();
    let open = data.open.clone();
    let hint = match &data.open {
        CardOpen::Chat(_) => "Open its chat",
        CardOpen::Terminal(_) => "Go to its terminal",
        CardOpen::Unresolved(_) => "Open its worker",
        CardOpen::Details => "Show details",
    };
    rsx! {
        div {
            class: if need { "{CARD} border-subtle" } else { "{CARD}" },
            title: "{hint}",
            onclick: move |_| on_open.call((id.clone(), open.clone())),
            div { class: "flex items-start gap-2",
                div { class: "min-w-0 flex-1 text-[12.5px] font-medium leading-[17px] text-fg", "{data.outcome}" }
                if data.open != CardOpen::Details {
                    button {
                        class: "shrink-0 text-[10.5px] text-fg-muted hover:text-fg",
                        title: "The to-do's details",
                        onclick: move |evt: MouseEvent| {
                            evt.stop_propagation();
                            on_details.call(details_id.clone());
                        },
                        "Details"
                    }
                }
            }
            if need {
                if let Some(q) = data.question.clone() {
                    div { class: "text-xs leading-4 text-fg", "\u{201c}{q}\u{201d}" }
                } else {
                    div { class: "text-xs leading-4 text-fg-muted", "Waiting on you \u{2014} check its worker." }
                }
            } else if let Some(p) = data.progress.clone() {
                div { class: "truncate text-[11.5px] leading-[15px] text-fg-muted", title: "{p}", "{p}" }
            } else if let Some(b) = data.blocked.clone() {
                div { class: "truncate text-[11.5px] leading-[15px] text-fg-muted", "Blocked: {b}" }
            }
            div {
                class: "flex items-center gap-2",
                if let Some(w) = data.worker.clone() {
                    span {
                        class: "{WK}",
                        title: if w.chat { "Hobbes chat" } else { "Claude Code run" },
                        if w.chat {
                            Icon { width: 11, height: 11, icon: fi_icons::FiMessageCircle }
                        } else {
                            Icon { width: 11, height: 11, icon: fi_icons::FiTerminal }
                        }
                        span { class: "truncate", "{w.name}" }
                    }
                    if let Some(m) = data.model.clone() {
                        span { class: "min-w-0 shrink truncate font-mono text-[10px] text-fg-muted", title: "Model", "\u{b7} {m}" }
                    }
                } else {
                    span { class: "font-mono text-[10px] text-fg-muted", "no worker yet" }
                }
                if need {
                    if let Some(est) = data.est.clone() {
                        span { class: "font-mono text-[11px] text-yellow-300 shrink-0", "{est}" }
                    }
                    if data.gate {
                        span { class: "font-mono text-[10px] text-fg-muted shrink-0", "permission" }
                    }
                }
                span { class: "ml-auto shrink-0 font-mono text-[11px] text-fg-muted", "{data.meta}" }
            }
        }
    }
}

// ── Fleet → agent_state sync ────────────────────────────────────────────────

/// How long an ended fleet row read from the store is trusted before it's
/// re-read (late briefs can still land on an ended session).
const ENDED_ROW_TTL_SECS: i64 = 60;

/// Re-derive `agent_state` (and synthesized decisions) for linked agent
/// to-dos whenever the fleet mirror changes. Renders nothing. Mounted once
/// at the Planner root; the effect subscribes only to the fleet signal, so
/// it runs on fleet pokes — never on planner edits or every render — and it
/// writes the planner signal only when something actually changed.
#[component]
pub(crate) fn AgentStateSync() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let fleet_state = use_context::<Signal<FleetState>>();
    // Ended rows (they leave the live map on SessionEnd) cached by id.
    let mut ended_cache = use_signal(HashMap::<String, (DateTime<Utc>, Option<FleetSession>)>::new);

    use_effect(move || {
        let live = fleet_state.read().clone();
        let now = Utc::now();
        // Work on a scratch planner holding only the linked, open agent
        // to-dos: `sync_agent_states` needs `&mut PlannerState`, and taking
        // the real signal's write guard would re-render the Planner on every
        // fleet poke even when nothing changed.
        let linked: Vec<Todo> = planner
            .peek()
            .todos
            .iter()
            .filter(|t| t.lane == Lane::Agent && !t.status.is_closed() && t.linked_fleet_session.is_some())
            .cloned()
            .collect();
        if linked.is_empty() {
            return;
        }
        let mut ended: Vec<FleetSession> = Vec::new();
        {
            let mut cache = ended_cache.write();
            for sid in linked.iter().filter_map(|t| t.linked_fleet_session.as_deref()) {
                if live.sessions.contains_key(sid) {
                    continue;
                }
                let fresh = cache
                    .get(sid)
                    .is_some_and(|(at, _)| (now - *at).num_seconds() < ENDED_ROW_TTL_SECS);
                if !fresh {
                    cache.insert(sid.to_string(), (now, crate::fleet::store::load_session(sid)));
                }
                if let Some((_, Some(row))) = cache.get(sid) {
                    if !ended.iter().any(|r| r.id == row.id) {
                        ended.push(row.clone());
                    }
                }
            }
        }
        let mut scratch = PlannerState::default();
        scratch.todos = linked;
        let changed = handlers::sync_agent_states(&mut scratch, &live, &ended, now);
        if changed.is_empty() {
            return;
        }
        {
            let mut state = planner.write();
            for id in &changed {
                if let Some(t) = scratch.todos.iter().find(|t| &t.id == id) {
                    if let Some(slot) = state.todo_mut(id) {
                        *slot = t.clone();
                    }
                }
            }
        }
        for id in &changed {
            persist_todo(&planner, id);
        }
    });

    rsx! {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_opens_its_worker_or_falls_back_to_details() {
        assert_eq!(card_open(None, false, None), CardOpen::Details, "no worker yet");
        assert_eq!(card_open(Some("c1"), true, None), CardOpen::Chat("c1".into()), "an open chat tab");
        assert_eq!(card_open(Some("c1"), false, Some(true)), CardOpen::Chat("c1".into()), "a Hobbes chat whose tab is closed");
        assert_eq!(card_open(Some("cc"), false, Some(false)), CardOpen::Terminal("cc".into()), "a Claude Code run");
        assert_eq!(card_open(Some("x"), false, None), CardOpen::Unresolved("x".into()), "decided on click");
    }

    fn facts<'a>() -> WorkerFacts<'a> {
        WorkerFacts::default()
    }

    #[test]
    fn worker_prefers_chat_title_then_fleet_name_then_decision() {
        let chat = WorkerFacts { chat: Some(("Puget Agreement", 0.22)), fleet: Some(("puget", true, 14)), ..facts() };
        let w = resolve_worker(true, &chat).unwrap();
        assert_eq!(w.name, "Puget Agreement");
        assert!(w.chat);
        assert_eq!(w.cost_usd, Some(0.22));
        assert_eq!(w.active_minutes, Some(14));

        let run = WorkerFacts { fleet: Some(("cai-scott-marsh", false, 0)), ..facts() };
        let w = resolve_worker(true, &run).unwrap();
        assert_eq!(w.name, "cai-scott-marsh");
        assert!(!w.chat);
        assert_eq!(w.cost_usd, None, "Claude Code runs report no cost");
        assert_eq!(w.active_minutes, None);

        let named = WorkerFacts { decision_worker: Some("clearmirror-site"), ..facts() };
        assert_eq!(resolve_worker(false, &named).unwrap().name, "clearmirror-site");
        assert_eq!(resolve_worker(true, &facts()).unwrap().name, "linked session");
        assert!(resolve_worker(false, &facts()).is_none());
        // A free chat shows no cost rather than "$0.00".
        let free = WorkerFacts { chat: Some(("x", 0.0)), ..facts() };
        assert_eq!(resolve_worker(true, &free).unwrap().cost_usd, None);
    }

    #[test]
    fn meta_shows_waiting_for_decisions_and_active_time_otherwise() {
        let now = Utc::now();
        let mut t = Todo::new("Ship", 0.0);
        t.set_lane(Lane::Agent);
        lanes::ask_decision(&mut t, "Ship four now?", vec![], Some(2), None, None, now - chrono::Duration::minutes(9))
            .unwrap();
        assert_eq!(card_meta(&t, None, now), "waiting 9m");

        let mut w = Todo::new("Mockups", 0.0);
        w.set_lane(Lane::Agent);
        w.agent_state = Some(AgentState::Working);
        let label = WorkerLabel { name: "x".into(), chat: true, cost_usd: Some(1.12), active_minutes: Some(65) };
        assert_eq!(card_meta(&w, Some(&label), now), "1h 5m \u{b7} $1.12");
        assert_eq!(card_meta(&w, None, now), "");
    }

    #[test]
    fn the_card_names_the_workers_model() {
        let mut t = Todo::new("Digest", 0.0);
        assert_eq!(worker_model_label(&t), None);
        t.worker_model = Some("claude-haiku-5-5".into());
        assert_eq!(worker_model_label(&t).as_deref(), Some("Haiku 5.5"));
        t.worker_model = Some("sonnet".into());
        assert_eq!(worker_model_label(&t).as_deref(), Some("sonnet"));
    }

    #[test]
    fn queue_minutes_counts_a_minute_for_unestimated_decisions() {
        let now = Utc::now();
        let mut a = Todo::new("a", 0.0);
        a.set_lane(Lane::Agent);
        lanes::ask_decision(&mut a, "q", vec![], Some(3), None, None, now).unwrap();
        let mut b = Todo::new("b", 0.0);
        b.set_lane(Lane::Agent);
        lanes::ask_decision(&mut b, "q", vec![], None, None, None, now).unwrap();
        assert_eq!(queue_minutes(&[&a, &b]), 4);
        assert_eq!(fmt_since(now, now), "just now");
    }
}
