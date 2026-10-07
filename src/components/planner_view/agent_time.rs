//! The Agent time panel: the human-granted budget in which the harness
//! chooses its own work, in the open (SYMBIOTIC_PLANNER.md §3).
//!
//! Off by default. The panel edits the grant, shows the live status with
//! prominent Pause / Stop (sent as `ChatCommand`s to the agent-time
//! runtime), any call the run is waiting on your approval for (answered
//! here through the chat's own approve / deny path), today's declared
//! intent, the full log, the proposals to accept / edit / dismiss, and the
//! trust ledger. Evidence only ever *suggests* a wider scope; allowing one
//! records the person's choice.

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::agent_time_runtime::{held_approvals, HeldApproval};
use crate::components::chat_input::ChatCommand;
use crate::session::SessionState;
use crate::settings::{short_model_label, AllowedModel, Settings};
use crate::todo::agent_time::{
    accept_start_route, proposal_starts_work, AcceptStartRoute, AgentTimeBudget,
};
use crate::todo::measures;
use crate::todo::model::{
    AgentTimeLogKind, AgentTimeSession, Proposal, ProposalKind, ProposalStatus, TrustLedger,
};
use crate::todo::store;
use crate::todo::PlannerState;

use super::today;

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The header status pill and its spend line. `waiting`: the run is held on
/// a call that needs the user's approval.
pub(crate) fn status_line(b: &AgentTimeBudget, running: bool, waiting: bool) -> (String, Option<String>) {
    if !b.enabled {
        return ("Off \u{b7} 0 min granted".to_string(), None);
    }
    if !b.granted_today {
        return ("Not a granted day".to_string(), None);
    }
    let state = if running && waiting {
        "Waiting on you"
    } else if running {
        "Running"
    } else {
        "Idle"
    };
    (
        format!("{state} \u{b7} {} of {} min today", b.minutes_used, b.minutes_granted),
        Some(format!("${:.2} of ${:.2} cap", b.spend_used_usd, b.spend_cap_usd)),
    )
}

/// The trust ledger strip: (value, caption) × 3. "—" where nothing is known.
pub(crate) fn trust_stats(l: &TrustLedger) -> [(String, String); 3] {
    let decided = l.proposals_accepted + l.proposals_edited + l.proposals_dismissed;
    let accepted = match l.acceptance_rate() {
        Some(r) => (
            format!("{:.0}%", r * 100.0),
            format!("accepted \u{b7} {} of {}", l.proposals_accepted + l.proposals_edited, decided),
        ),
        None => ("\u{2014}".to_string(), "accepted \u{b7} none decided".to_string()),
    };
    let in_scope = match l.in_scope_rate() {
        Some(r) => (
            format!("{:.0}%", r * 100.0),
            format!("in scope \u{b7} {} sessions", l.clean_sessions),
        ),
        None => ("\u{2014}".to_string(), "in scope \u{b7} no calls yet".to_string()),
    };
    let promises = l.promises_kept + l.promises_broken;
    let kept = if promises == 0 {
        ("\u{2014}".to_string(), "promises kept".to_string())
    } else {
        (format!("{} of {}", l.promises_kept, promises), "promises kept".to_string())
    };
    [accepted, in_scope, kept]
}

/// `"18m · $0.42 · 9 tool calls"` for the log header. Counts real calls
/// only: the model's notes and the runtime's own lines are not calls.
pub(crate) fn log_meta(s: &AgentTimeSession, now: DateTime<Utc>) -> String {
    let calls = s.call_count();
    format!(
        "{}m \u{b7} ${:.2} \u{b7} {} tool call{}",
        s.minutes_used(now),
        s.cost_usd,
        calls,
        if calls == 1 { "" } else { "s" }
    )
}

pub(crate) fn kind_label(k: ProposalKind) -> &'static str {
    match k {
        ProposalKind::Research => "Research digest",
        ProposalKind::Todo => "Suggested to-do",
        ProposalKind::Skill => "Draft skill",
        ProposalKind::Note => "Note",
        ProposalKind::Other => "Proposal",
        ProposalKind::Offer => "Offer to take",
    }
}

/// A proposal card's headline: an offer names the person's to-do it would
/// take; everything else shows its title.
pub(crate) fn proposal_headline(p: &Proposal) -> String {
    if p.kind == ProposalKind::Offer {
        format!("Hobbes offers to take: {}", p.title)
    } else {
        p.title.clone()
    }
}

/// The session the panel shows: the open one, else the most recent.
fn current_session(sessions: &[AgentTimeSession]) -> Option<&AgentTimeSession> {
    sessions
        .iter()
        .find(|s| s.is_open())
        .or_else(|| sessions.iter().max_by_key(|s| s.started_at))
}

fn end_reason_label(r: crate::todo::model::AgentTimeEndReason) -> String {
    r.as_str().replace('_', " ")
}

fn hhmmss(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()
}

fn hhmm(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// The live log's last line while a run is open.
pub(crate) fn live_tail(held: &[HeldApproval]) -> String {
    match held.first() {
        Some(h) => format!("waiting for your approval \u{b7} {}", h.tool),
        None => "working".to_string(),
    }
}

/// Answer a held agent-time call through the chat's own approve / deny
/// path (`tool_call_display::approve_held_call` / `deny_held_call`), so the
/// approval-resume effect in chat.rs runs it with `user_approved = true`
/// exactly as a click in the chat would. Also logs the trust decision the
/// chat's permission prompt logs (skill loads log none there either).
pub(crate) fn answer_held(session_state: Signal<SessionState>, h: &HeldApproval, allow: bool) -> bool {
    use crate::components::tool_call_display::{approve_held_call, deny_held_call, DENIED_BY_USER};
    let done = if allow {
        approve_held_call(session_state, &h.chat_id, &h.execution_id)
    } else {
        deny_held_call(session_state, &h.chat_id, &h.execution_id, DENIED_BY_USER)
    };
    if done && h.tool != "HOBBES_INVOKE_SKILL" {
        use crate::context::trust_store::{log_decision, DecisionKind, TrustDecision};
        log_decision(TrustDecision::new(
            if allow { DecisionKind::Approved } else { DecisionKind::Denied },
            &h.server,
            &h.tool,
            h.short_args.clone(),
            Some(h.chat_id.clone()),
            None,
            None,
        ));
    }
    done
}

/// The grant's model select value: `""` for "Same as your chats", else
/// `"<connector id>|<model>"` (the model may be empty: the connector's own
/// model).
pub(crate) fn grant_select_value(connector_id: Option<&str>, model: Option<&str>) -> String {
    match connector_id {
        Some(c) => format!("{c}|{}", model.unwrap_or("")),
        None => String::new(),
    }
}

/// The inverse of [`grant_select_value`]: (connector id, model).
pub(crate) fn parse_grant_select(v: &str) -> (Option<String>, Option<String>) {
    match v.split_once('|') {
        Some((c, m)) if !c.is_empty() => {
            (Some(c.to_string()), Some(m.to_string()).filter(|m| !m.is_empty()))
        }
        _ => (None, None),
    }
}

/// One select option: (value, label).
type ModelOption = (String, String);

/// The select's options, grouped by connector in the allowed list's (stable)
/// order: (connector name, options).
pub(crate) fn model_option_groups(allowed: &[AllowedModel]) -> Vec<(String, Vec<ModelOption>)> {
    // (connector id, connector name, options)
    let mut groups: Vec<(String, String, Vec<ModelOption>)> = Vec::new();
    for a in allowed {
        let opt = (
            grant_select_value(Some(&a.connector_id), Some(&a.model)),
            short_model_label(&a.model),
        );
        match groups.last_mut() {
            Some((id, _, opts)) if *id == a.connector_id => opts.push(opt),
            _ => groups.push((a.connector_id.clone(), a.connector_name.clone(), vec![opt])),
        }
    }
    groups.into_iter().map(|(_, name, opts)| (name, opts)).collect()
}

/// What an accepted proposal turned into, shown in place of its card.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AcceptedLine {
    pub proposal_id: String,
    pub title: String,
    /// "Started in a new chat · Haiku 5.5 · uses no agent time", "Queued…".
    pub text: String,
    /// The chat doing the work, to open.
    pub chat_id: Option<String>,
    /// The to-do it made or moved, to open.
    pub todo_id: Option<String>,
    pub error: bool,
}

fn persist_grant(planner: &Signal<PlannerState>) {
    if let Err(e) = store::save_agent_time_grant(&planner.peek().agent_time_grant) {
        tracing::error!("planner: failed to save agent-time grant: {}", e);
    }
}

// ── The view ────────────────────────────────────────────────────────────────

const PANEL: &str = "flex min-w-0 flex-col gap-2.5 rounded-lg border border-faint bg-section p-3.5";
const EYEBROW: &str = "text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const EMPTY: &str = "text-xs leading-relaxed text-fg-muted";
const DAY_NAMES: [(&str, &str); 7] = [
    ("M", "Monday"),
    ("T", "Tuesday"),
    ("W", "Wednesday"),
    ("T", "Thursday"),
    ("F", "Friday"),
    ("S", "Saturday"),
    ("S", "Sunday"),
];

#[component]
pub(crate) fn AgentTimeView() -> Element {
    let mut planner = use_context::<Signal<PlannerState>>();
    let session_state = use_context::<Signal<SessionState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut todo_detail = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    // The dispatch path "Accept & start" shares with HOBBES_DISPATCH (P-015).
    let dispatch_ctx = crate::components::builtin_tools::builtin_ctx_from_tree();
    // Proposals accepted in this sitting, shown as one line each.
    let mut accepted = use_signal(Vec::<AcceptedLine>::new);
    // Calls the run is waiting on the user for. A memo, so a chunk streaming
    // into any chat re-runs only this scan, not the panel.
    let held = use_memo(move || held_approvals(&planner.read(), &session_state.read()));
    let mut widening_dismissed = use_signal(|| false);
    let mut editing = use_signal(|| Option::<(String, String, String)>::None);
    // Trust ledger → Review: the list is open; the change being made
    // (proposal id, new verdict or None = remove, the note).
    let mut ledger_open = use_signal(|| false);
    let mut revising = use_signal(|| Option::<(String, Option<ProposalStatus>, String)>::None);
    let mut cap_text = use_signal(|| format!("{:.2}", planner.peek().agent_time_grant.spend_cap_usd));

    // The running clock and spend move while a session is open.
    let mut tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            tick += 1;
        }
    });
    let _t = *tick.read();

    let now = Utc::now();
    let day = today();
    let state = planner.read();
    let grant = state.agent_time_grant.clone();
    let budget = state.agent_time_budget(day, now);
    let open_session = state.open_agent_time_session().cloned();
    let running = open_session.is_some() || state.agent_time_lease.is_some();
    let session = current_session(&state.agent_time_sessions).cloned();
    // "Working on: <to-do>" for a session started on an agent-lane to-do.
    let working_on: Option<(String, String)> = session
        .as_ref()
        .and_then(|s| s.todo_id.clone())
        .map(|id| {
            let title = state.todo(&id).map(|t| t.title.clone()).unwrap_or_else(|| "a deleted to-do".to_string());
            (id, title)
        });
    let proposals: Vec<Proposal> = state.open_proposals().into_iter().cloned().collect();
    let ledger = state.trust_ledger;
    let resolved = ledger_review_rows(&state.proposals);
    let projects: Vec<String> = grant
        .scope
        .project_ids
        .iter()
        .filter_map(|id| state.projects.iter().find(|p| &p.id == id).map(|p| p.title.clone()))
        .collect();
    drop(state);

    let held = held.read().clone();
    let waiting = running && !held.is_empty();
    let (status, spend) = status_line(&budget, open_session.is_some(), waiting);
    let pill_class = if waiting {
        "inline-flex h-6 items-center gap-1.5 rounded-full border border-yellow-600 px-2.5 text-xs text-yellow-200"
    } else if open_session.is_some() {
        "inline-flex h-6 items-center gap-1.5 rounded-full border border-primary-400 px-2.5 text-xs text-fg"
    } else {
        "inline-flex h-6 items-center gap-1.5 rounded-full border border-faint px-2.5 text-xs text-fg-muted"
    };
    let refusal = budget.refusal().map(|r| r.message());
    let stats = trust_stats(&ledger);
    let widening = measures::suggest_scope_widening(&ledger).filter(|_| !*widening_dismissed.read());
    let min_pct = (grant.daily_minutes.min(120) as f64 / 120.0 * 100.0).round();
    let kinds_label = if grant.scope.proposal_kinds.is_empty() {
        "notes, proposals, draft skills, research digests and suggested to-dos".to_string()
    } else {
        grant
            .scope
            .proposal_kinds
            .iter()
            .map(|k| kind_label(*k).to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let allowed = settings.read().allowed_models();
    let grant_value = grant_select_value(grant.connector_id.as_deref(), grant.model.as_deref());
    let groups = model_option_groups(&allowed);
    // A saved choice no longer allowed still shows (and Start refuses it).
    let stale_choice = (!grant_value.is_empty()
        && !groups.iter().any(|(_, opts)| opts.iter().any(|(v, _)| *v == grant_value)))
    .then(|| {
        let label = grant
            .model
            .as_deref()
            .map(short_model_label)
            .unwrap_or_else(|| "connector default".to_string());
        format!("{label} (no longer available)")
    });
    let accepted_lines = accepted.read().clone();
    let reads_label = if projects.is_empty() {
        "notes, the planner, and what you've shared".to_string()
    } else {
        format!("notes, the planner, and {}", projects.join(", "))
    };

    rsx! {
        div {
            class: "flex-1 min-w-0 min-h-0 overflow-y-auto bg-app",
            div {
                class: "flex flex-col gap-3 px-6 pt-6 pb-10",
                // ── Header: status + the controls ─────────────────────────
                div {
                    class: "flex flex-wrap items-center gap-3",
                    h1 { class: "text-xl font-semibold", "Agent time" }
                    span {
                        class: "{pill_class}",
                        if waiting {
                            span { class: "h-[7px] w-[7px] rounded-full bg-yellow-400" }
                        } else if open_session.is_some() {
                            span { class: "h-[7px] w-[7px] rounded-full bg-primary-400 animate-pulse" }
                        }
                        "{status}"
                    }
                    if let Some(s) = spend.clone() {
                        span { class: "font-mono text-[11px] text-fg-muted", "{s}" }
                    }
                    div { class: "flex-1" }
                    if grant.enabled {
                        if running {
                            button {
                                class: "inline-flex h-8 items-center gap-1.5 rounded-md bg-btn-primary px-3.5 text-[13px] font-medium text-fg hover:bg-btn-primary-hover",
                                title: "Pause now \u{2014} takes effect immediately",
                                onclick: move |_| chat_command.set(Some(ChatCommand::AgentTimePause)),
                                Icon { width: 13, height: 13, icon: fi_icons::FiPause }
                                "Pause"
                            }
                            button {
                                class: "inline-flex h-8 items-center gap-1.5 rounded-md border border-red-700 bg-red-900/40 px-3.5 text-[13px] font-medium text-red-200 hover:bg-red-900/70",
                                title: "Stop now \u{2014} ends the session on the record",
                                onclick: move |_| chat_command.set(Some(ChatCommand::AgentTimeStop)),
                                Icon { width: 12, height: 12, icon: fi_icons::FiSquare }
                                "Stop"
                            }
                        } else {
                            button {
                                class: "inline-flex h-8 items-center gap-1.5 rounded-md bg-btn-primary px-3.5 text-[13px] font-medium text-fg hover:bg-btn-primary-hover disabled:opacity-50",
                                disabled: refusal.is_some(),
                                title: refusal.clone().unwrap_or_else(|| "Start an agent-time session now".to_string()),
                                onclick: move |_| chat_command.set(Some(ChatCommand::AgentTimeStart)),
                                Icon { width: 13, height: 13, icon: fi_icons::FiPlay }
                                "Start now"
                            }
                        }
                    } else {
                        button {
                            class: "inline-flex h-8 items-center rounded-md bg-btn-primary px-3.5 text-[13px] font-medium text-fg hover:bg-btn-primary-hover",
                            onclick: move |_| {
                                planner.write().agent_time_grant.enabled = true;
                                persist_grant(&planner);
                            },
                            "Grant agent time"
                        }
                    }
                }
                // ── Waiting for your approval ────────────────────────────
                for h in held.iter().cloned() {
                    div {
                        key: "{h.execution_id}",
                        role: "alert",
                        class: "flex flex-wrap items-center gap-3 rounded-lg border border-yellow-700 bg-yellow-900/40 px-3.5 py-3",
                        span { class: "shrink-0 text-yellow-300", Icon { width: 16, height: 16, icon: fi_icons::FiShield } }
                        div { class: "flex min-w-[240px] flex-1 flex-col gap-0.5",
                            div { class: "flex min-w-0 items-baseline gap-2 text-sm text-yellow-100",
                                span { class: "shrink-0 font-medium", "Waiting for your approval:" }
                                span { class: "shrink-0 font-mono text-[13px]", "{h.tool}" }
                                if !h.short_args.is_empty() {
                                    span { class: "min-w-0 truncate font-mono text-xs text-yellow-200/80", title: "{h.short_args}", "{h.short_args}" }
                                }
                            }
                            span { class: "text-xs text-yellow-200/80",
                                "{h.reason} Asked {hhmm(h.asked_at)}; the run waits, and waiting uses its minutes."
                            }
                        }
                        button {
                            class: "rounded-md px-2.5 py-1 text-xs text-yellow-200/80 hover:text-yellow-100",
                            title: "Open the agent-time chat",
                            onclick: {
                                let id = h.chat_id.clone();
                                move |_| chat_command.set(Some(ChatCommand::SwitchToSession(id.clone())))
                            },
                            "Open chat"
                        }
                        button {
                            class: "rounded-md bg-input px-3 py-1.5 text-[13px] text-fg hover:bg-gray-500",
                            onclick: {
                                let h = h.clone();
                                move |_| { answer_held(session_state, &h, false); }
                            },
                            "Deny"
                        }
                        button {
                            class: "rounded-md bg-green-600 px-3 py-1.5 text-[13px] font-medium text-fg hover:bg-green-500",
                            onclick: {
                                let h = h.clone();
                                move |_| { answer_held(session_state, &h, true); }
                            },
                            "Approve"
                        }
                    }
                }
                if !grant.enabled {
                    div {
                        class: "{PANEL} max-w-[640px]",
                        span { class: "{EYEBROW}", "Off by default" }
                        p { class: "text-sm leading-relaxed text-fg",
                            "Agent time is a small budget you grant, in which Hobbes picks its own work \u{2014} in the open. It declares what it intends before it starts, logs every tool call, and everything it makes lands here as a proposal you accept, edit or dismiss."
                        }
                        ul { class: "flex list-disc flex-col gap-1 pl-5 text-xs text-fg-muted",
                            li { "It reads what you've shared, and writes only to Hobbes-owned space: notes, proposals, draft skills." }
                            li { "Anything with an outside effect \u{2014} sending, buying, publishing, changing a repo, running a command \u{2014} asks, through the normal approval gates." }
                            li { "Pause or stop it any time; a pause takes effect at once." }
                            li { "Evidence (accepted proposals, kept promises) can only suggest a wider grant. You decide." }
                        }
                    }
                } else if let Some(r) = refusal.clone().filter(|_| !running) {
                    p { class: "text-xs text-fg-muted", "{r}" }
                }
                div {
                    class: "flex flex-wrap gap-3",
                    // ── Left: the grant + proposals ───────────────────────
                    div {
                        class: "flex w-[390px] max-w-full shrink-0 flex-col gap-3",
                        div {
                            class: "{PANEL}",
                            div { class: "flex items-center gap-2",
                                span { class: "{EYEBROW}", "The grant" }
                                div { class: "flex-1" }
                                if grant.enabled {
                                    button {
                                        class: "font-mono text-[10.5px] text-fg-muted hover:text-red-300",
                                        title: "Turn agent time off",
                                        onclick: move |_| {
                                            planner.write().agent_time_grant.enabled = false;
                                            persist_grant(&planner);
                                            if running {
                                                chat_command.set(Some(ChatCommand::AgentTimeStop));
                                            }
                                        },
                                        "turn off"
                                    }
                                } else {
                                    span { class: "font-mono text-[10.5px] text-fg-muted", "off by default" }
                                }
                            }
                            div {
                                class: "grid grid-cols-[96px_minmax(0,1fr)] items-center gap-x-3 gap-y-2.5 text-xs",
                                span { class: "text-fg-muted", "Minutes a day" }
                                div { class: "flex items-center gap-2.5",
                                    input {
                                        r#type: "range",
                                        class: "min-w-0 flex-1 accent-primary-500",
                                        min: "0",
                                        max: "120",
                                        step: "5",
                                        value: "{grant.daily_minutes}",
                                        "aria-label": "Minutes a day",
                                        style: "--pct: {min_pct}%;",
                                        oninput: move |e| {
                                            if let Ok(m) = e.value().parse::<u32>() {
                                                planner.write().agent_time_grant.daily_minutes = m;
                                            }
                                        },
                                        onchange: move |_| persist_grant(&planner),
                                    }
                                    span { class: "w-14 text-right font-mono text-[13px] text-fg", "{grant.daily_minutes} min" }
                                }
                                span { class: "text-fg-muted", "Spend cap" }
                                div { class: "flex items-center gap-1.5",
                                    span { class: "font-mono text-fg-muted", "$" }
                                    input {
                                        class: "h-6 w-20 rounded border border-faint bg-input px-2 text-right font-mono text-xs text-fg outline-none",
                                        value: "{cap_text}",
                                        "aria-label": "Spend cap in dollars a day",
                                        oninput: move |e| cap_text.set(e.value()),
                                        onchange: move |e| {
                                            if let Ok(v) = e.value().trim().trim_start_matches('$').parse::<f64>() {
                                                if v >= 0.0 {
                                                    planner.write().agent_time_grant.spend_cap_usd = v;
                                                    persist_grant(&planner);
                                                    cap_text.set(format!("{v:.2}"));
                                                }
                                            }
                                        },
                                    }
                                    span { class: "text-fg-muted", "a day" }
                                }
                                span { class: "text-fg-muted", "Days" }
                                div { class: "flex gap-1",
                                    for (i, (l, name)) in DAY_NAMES.iter().enumerate() {
                                        button {
                                            key: "{i}",
                                            class: if grant.days[i] {
                                                "h-6 w-6 rounded bg-btn-primary text-[11px] font-medium text-fg"
                                            } else {
                                                "h-6 w-6 rounded border border-faint text-[11px] text-fg-muted hover:text-fg"
                                            },
                                            "aria-label": "{name}",
                                            "aria-pressed": if grant.days[i] { "true" } else { "false" },
                                            onclick: move |_| {
                                                let cur = planner.peek().agent_time_grant.days[i];
                                                planner.write().agent_time_grant.days[i] = !cur;
                                                persist_grant(&planner);
                                            },
                                            "{l}"
                                        }
                                    }
                                }
                                span { class: "text-fg-muted", "Model" }
                                select {
                                    class: "h-6 min-w-0 rounded border border-faint bg-input px-1.5 text-xs text-fg outline-none",
                                    "aria-label": "Model agent time runs on",
                                    title: "Agent time runs on this model; its spend follows the model's pricing.",
                                    onchange: move |e| {
                                        let (c, m) = parse_grant_select(&e.value());
                                        {
                                            let mut state = planner.write();
                                            state.agent_time_grant.connector_id = c;
                                            state.agent_time_grant.model = m;
                                        }
                                        persist_grant(&planner);
                                    },
                                    option { value: "", selected: grant_value.is_empty(), "Same as your chats" }
                                    if let Some(label) = stale_choice.clone() {
                                        option { value: "{grant_value}", selected: true, "{label}" }
                                    }
                                    for (name, opts) in groups.iter().cloned() {
                                        optgroup { key: "{name}", label: "{name}",
                                            for (v, label) in opts {
                                                option { key: "{v}", value: "{v}", selected: v == grant_value, "{label}" }
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "h-px bg-[var(--color-border-muted)]" }
                            p { class: "text-xs text-fg", span { class: "font-semibold", "Reads " } span { class: "text-fg-muted", "{reads_label}" } }
                            p { class: "text-xs text-fg", span { class: "font-semibold", "Writes " } span { class: "text-fg-muted", "only to Hobbes-owned space: {kinds_label}" } }
                            p { class: "text-xs text-fg", span { class: "font-semibold", "Outside effects " } span { class: "text-fg-muted", "always ask, through the normal approval gates" } }
                            if !grant.scope.note.trim().is_empty() {
                                p { class: "whitespace-pre-line text-[11px] text-fg-muted", "{grant.scope.note}" }
                            }
                        }
                        div {
                            class: "{PANEL}",
                            div { class: "flex items-center gap-2",
                                span { class: "{EYEBROW}", "Proposals" }
                                span { class: "font-mono text-[11px] text-fg-muted",
                                    if proposals.is_empty() { "none open" } else { "{proposals.len()} open" }
                                }
                            }
                            for line in accepted_lines.iter().cloned() {
                                div {
                                    key: "acc-{line.proposal_id}",
                                    class: "flex min-w-0 items-center gap-2 rounded-md border border-faint px-2.5 py-1.5 text-[11.5px]",
                                    if line.error {
                                        span { class: "shrink-0 text-yellow-300", Icon { width: 12, height: 12, icon: fi_icons::FiAlertTriangle } }
                                    } else {
                                        span { class: "shrink-0 text-primary-300", Icon { width: 12, height: 12, icon: fi_icons::FiCheck } }
                                    }
                                    span { class: "min-w-0 truncate text-fg", title: "{line.title}", "{line.title}" }
                                    span { class: "min-w-0 shrink truncate text-fg-muted", title: "{line.text}", "\u{b7} {line.text}" }
                                    div { class: "flex-1" }
                                    if let Some(chat) = line.chat_id.clone() {
                                        button {
                                            class: "shrink-0 text-primary-300 hover:text-fg",
                                            title: "Open the chat doing the work",
                                            onclick: move |_| chat_command.set(Some(ChatCommand::SwitchToSession(chat.clone()))),
                                            "Open chat"
                                        }
                                    } else if let Some(tid) = line.todo_id.clone() {
                                        button {
                                            class: "shrink-0 text-primary-300 hover:text-fg",
                                            title: "Open the to-do",
                                            onclick: move |_| todo_detail.set(Some(tid.clone())),
                                            "Open to-do"
                                        }
                                    }
                                }
                            }
                            if proposals.is_empty() && accepted_lines.is_empty() {
                                p { class: "{EMPTY}", "No proposals. Whatever agent time makes lands here as a suggestion you accept, edit or dismiss." }
                            }
                            for p in proposals {
                                {
                                    let is_editing = editing.read().as_ref().is_some_and(|(id, _, _)| *id == p.id);
                                    let pid = p.id.clone();
                                    let starts = proposal_starts_work(&p);
                                    rsx! {
                                        div {
                                            key: "{p.id}",
                                            class: "flex flex-col gap-1.5 rounded-md border border-faint bg-card px-2.5 py-2",
                                            div { class: "flex items-center gap-2",
                                                if p.kind == ProposalKind::Offer {
                                                    span { class: "rounded-full border border-primary-400 px-1.5 text-[10px] uppercase leading-4 tracking-[0.08em] text-primary-300", "{kind_label(p.kind)}" }
                                                } else {
                                                    span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "{kind_label(p.kind)}" }
                                                }
                                                div { class: "flex-1" }
                                                span { class: "font-mono text-[10.5px] text-fg-muted", "{hhmm(p.created_at)}" }
                                            }
                                            if is_editing {
                                                input {
                                                    class: "h-7 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                                                    value: "{editing.read().as_ref().map(|e| e.1.clone()).unwrap_or_default()}",
                                                    oninput: move |e| if let Some(ed) = editing.write().as_mut() { ed.1 = e.value() },
                                                }
                                                textarea {
                                                    class: "min-h-[64px] rounded border border-faint bg-input px-2 py-1 text-xs text-fg outline-none",
                                                    value: "{editing.read().as_ref().map(|e| e.2.clone()).unwrap_or_default()}",
                                                    oninput: move |e| if let Some(ed) = editing.write().as_mut() { ed.2 = e.value() },
                                                }
                                            } else {
                                                div { class: "text-[13px] leading-[18px] text-fg", "{proposal_headline(&p)}" }
                                                if !p.body.trim().is_empty() {
                                                    div { class: "line-clamp-3 whitespace-pre-line text-[11.5px] leading-4 text-fg-muted", "{p.body}" }
                                                }
                                            }
                                            div { class: "flex gap-1.5",
                                                if is_editing {
                                                    button {
                                                        class: "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                                                        onclick: {
                                                            let pid = pid.clone();
                                                            move |_| {
                                                                let edited = editing.peek().clone().map(|(_, t, b)| (t, b));
                                                                if let Some(line) = accept(planner, settings, dispatch_ctx, &pid, edited, false) {
                                                                    accepted.write().push(line);
                                                                }
                                                                editing.set(None);
                                                            }
                                                        },
                                                        "Save & accept"
                                                    }
                                                    button {
                                                        class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: move |_| editing.set(None),
                                                        "Cancel"
                                                    }
                                                } else {
                                                    if starts {
                                                        button {
                                                            class: "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                                                            title: if running { "Accept and hand it to the running agent-time session" } else { "Accept and start it in a new chat (uses no agent time)" },
                                                            onclick: {
                                                                let pid = pid.clone();
                                                                move |_| {
                                                                    if let Some(line) = accept(planner, settings, dispatch_ctx, &pid, None, true) {
                                                                        if let Some(chat) = line.chat_id.clone().filter(|_| !line.error) {
                                                                            chat_command.set(Some(ChatCommand::OpenTabInBackground(chat)));
                                                                        }
                                                                        accepted.write().push(line);
                                                                    }
                                                                }
                                                            },
                                                            "Accept & start"
                                                        }
                                                        button {
                                                            class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                            title: "Accept and leave it queued in the agent lane",
                                                            onclick: {
                                                                let pid = pid.clone();
                                                                move |_| {
                                                                    if let Some(line) = accept(planner, settings, dispatch_ctx, &pid, None, false) {
                                                                        accepted.write().push(line);
                                                                    }
                                                                }
                                                            },
                                                            "Accept only"
                                                        }
                                                    } else {
                                                        button {
                                                            class: "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                                                            onclick: {
                                                                let pid = pid.clone();
                                                                move |_| {
                                                                    if let Some(line) = accept(planner, settings, dispatch_ctx, &pid, None, false) {
                                                                        accepted.write().push(line);
                                                                    }
                                                                }
                                                            },
                                                            "Accept"
                                                        }
                                                    }
                                                    if p.kind != ProposalKind::Offer {
                                                        button {
                                                            class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                            onclick: {
                                                                let seed = (p.id.clone(), p.title.clone(), p.body.clone());
                                                                move |_| editing.set(Some(seed.clone()))
                                                            },
                                                            "Edit"
                                                        }
                                                    }
                                                    button {
                                                        class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: {
                                                            let pid = pid.clone();
                                                            move |_| dismiss(planner, &pid)
                                                        },
                                                        "Dismiss"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ── Right: intent + live log ─────────────────────────
                    div {
                        class: "flex min-w-[300px] flex-1 flex-col gap-3",
                        // The intent stays in view above the log while it scrolls.
                        div {
                            class: "{PANEL} sticky top-0 z-10 shadow-md",
                            div { class: "flex items-center gap-2",
                                span { class: "{EYEBROW}", if open_session.is_some() { "Today\u{2019}s intent" } else { "Last intent" } }
                                div { class: "flex-1" }
                                span { class: "font-mono text-[10.5px] text-fg-muted",
                                    match &session { Some(s) => format!("declared {}", hhmm(s.started_at)), None => "\u{2014}".to_string() }
                                }
                            }
                            match &session {
                                Some(s) => rsx! {
                                    div { class: "text-sm leading-[21px] text-fg", "\u{201c}{s.intent}\u{201d}" }
                                    match working_on.clone() {
                                        Some((id, title)) => rsx! {
                                            div { class: "flex min-w-0 items-baseline gap-1.5 text-xs",
                                                span { class: "shrink-0 text-fg-muted", "Working on:" }
                                                button {
                                                    class: "min-w-0 truncate text-left text-primary-300 hover:text-fg",
                                                    title: "Open the to-do",
                                                    onclick: move |_| todo_detail.set(Some(id.clone())),
                                                    "{title}"
                                                }
                                            }
                                        },
                                        None => rsx! {
                                            div { class: "text-xs text-fg-muted", "Working in Hobbes-owned space (no to-do)." }
                                        },
                                    }
                                    if let Some(sum) = s.summary.clone().filter(|x| !x.trim().is_empty()) {
                                        div { class: "text-xs text-fg-muted", "Summary: {sum}" }
                                    }
                                    if let Some(r) = s.end_reason {
                                        div { class: "font-mono text-[10.5px] text-fg-muted", "ended \u{b7} {end_reason_label(r)}" }
                                    }
                                },
                                None => rsx! {
                                    p { class: "{EMPTY}", "Nothing declared. Before each session, Hobbes states what it intends to do and why, here, in one or two lines." }
                                },
                            }
                        }
                        div {
                            class: "{PANEL} flex-1",
                            div { class: "flex items-center gap-2",
                                span { class: "{EYEBROW}", "Live log" }
                                div { class: "flex-1" }
                                span { class: "font-mono text-[10.5px] text-fg-muted",
                                    match &session { Some(s) => log_meta(s, now), None => "\u{2014}".to_string() }
                                }
                            }
                            match &session {
                                Some(s) => rsx! {
                                    div { class: "flex flex-col",
                                        for (i, e) in s.tool_calls.iter().enumerate() {
                                            div {
                                                key: "{i}",
                                                class: "grid grid-cols-[64px_minmax(0,200px)_minmax(0,1fr)] items-baseline gap-x-3 border-b border-faint py-1.5 text-xs",
                                                span { class: "font-mono text-[11px] text-fg-muted", "{hhmmss(e.at)}" }
                                                if e.kind.is_call() {
                                                    span { class: "truncate font-mono text-[11px] text-fg", "{e.tool}" }
                                                } else {
                                                    span { class: "truncate font-mono text-[11px] text-fg-muted", "{e.tool}" }
                                                }
                                                match (e.kind, e.note.clone()) {
                                                    (AgentTimeLogKind::Call, Some(n)) => rsx! {
                                                        span { class: "min-w-0 truncate", title: "{n}\n{e.summary}",
                                                            span { class: "text-fg", "{n}" }
                                                            span { class: "text-fg-muted", " \u{b7} {e.summary}" }
                                                        }
                                                    },
                                                    (AgentTimeLogKind::Call, None) => rsx! {
                                                        span { class: "min-w-0 truncate text-fg-muted", title: "{e.summary}", "{e.summary}" }
                                                    },
                                                    _ => rsx! {
                                                        span { class: "min-w-0 truncate italic text-fg-muted opacity-80", title: "{e.summary}", "{e.summary}" }
                                                    },
                                                }
                                            }
                                        }
                                        if s.tool_calls.is_empty() {
                                            p { class: "{EMPTY} py-1", "No tool calls yet." }
                                        }
                                        if s.is_open() {
                                            div {
                                                class: "grid grid-cols-[64px_minmax(0,200px)_minmax(0,1fr)] items-baseline gap-x-3 py-1.5 text-xs",
                                                span { class: "font-mono text-[11px] text-fg-muted", "{hhmmss(now)}" }
                                                if held.is_empty() {
                                                    span { class: "inline-flex items-center gap-1.5 font-mono text-[11px] text-fg",
                                                        span { class: "h-1.5 w-1.5 rounded-full bg-primary-400 animate-pulse" }
                                                        "{live_tail(&held)}"
                                                    }
                                                } else {
                                                    span { class: "col-span-2 inline-flex min-w-0 items-center gap-1.5 font-mono text-[11px] text-yellow-200",
                                                        span { class: "h-1.5 w-1.5 shrink-0 rounded-full bg-yellow-400" }
                                                        span { class: "truncate", "{live_tail(&held)}" }
                                                    }
                                                }
                                                if held.is_empty() {
                                                    span {}
                                                }
                                            }
                                        }
                                    }
                                },
                                None => rsx! {
                                    p { class: "{EMPTY}",
                                        if grant.enabled {
                                            "No sessions yet. Every session leaves a full record here: intent, tool calls, outputs and cost."
                                        } else {
                                            "Nothing runs until you grant time. Every session will leave a full record here: intent, tool calls, outputs and cost."
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
                // ── Trust ledger ─────────────────────────────────────────
                div {
                    class: "flex flex-wrap items-center gap-5 rounded-lg border border-faint bg-section px-4 py-3",
                    div { class: "flex w-[92px] shrink-0 flex-col gap-0.5",
                        span { class: "{EYEBROW}", "Trust ledger" }
                        span { class: "font-mono text-[10.5px] text-fg-muted", "all time" }
                    }
                    for (i, (v, cap)) in stats.iter().cloned().enumerate() {
                        div { key: "{i}", class: "flex w-[120px] flex-col gap-0.5",
                            span { class: "text-lg font-semibold tabular-nums text-fg", "{v}" }
                            span { class: "text-[11px] text-fg-muted", "{cap}" }
                        }
                    }
                    if let Some(text) = widening {
                        div {
                            class: "flex min-w-[280px] flex-1 items-center gap-3 rounded-lg border border-primary-400 bg-primary-500/10 px-3 py-2",
                            div { class: "flex min-w-0 flex-1 flex-col gap-0.5",
                                span { class: "text-[10px] uppercase tracking-[0.08em] text-primary-300", "Suggested widening \u{b7} needs you" }
                                span { class: "text-[12.5px] leading-[17px] text-fg", "{text}" }
                            }
                            button {
                                class: "rounded bg-btn-primary px-2.5 py-1 text-[11px] text-fg hover:bg-btn-primary-hover",
                                title: "Records your choice in the grant's scope",
                                onclick: move |_| {
                                    {
                                        let mut state = planner.write();
                                        let note = &mut state.agent_time_grant.scope.note;
                                        let line = format!(
                                            "Allowed {}: file accepted research digests straight into Notes.",
                                            today().format("%-d %b %Y")
                                        );
                                        if !note.trim().is_empty() {
                                            note.push('\n');
                                        }
                                        note.push_str(&line);
                                    }
                                    persist_grant(&planner);
                                    widening_dismissed.set(true);
                                },
                                "Allow"
                            }
                            button {
                                class: "rounded border border-faint px-2.5 py-1 text-[11px] text-fg hover:bg-input",
                                onclick: move |_| widening_dismissed.set(true),
                                "Not yet"
                            }
                        }
                    } else if ledger == TrustLedger::default() {
                        p { class: "{EMPTY} min-w-[240px] flex-1",
                            "No evidence yet. Accepted proposals, kept promises and in-scope sessions add up here; Hobbes can only suggest a wider grant, never take one."
                        }
                    }
                    if !resolved.is_empty() {
                        button {
                            class: "ml-auto rounded border border-faint px-2.5 py-1 text-[11px] text-fg hover:bg-input",
                            title: "Change or remove past verdicts. Hobbes sees what you change, with your note.",
                            onclick: move |_| {
                                let open = !*ledger_open.peek();
                                ledger_open.set(open);
                                revising.set(None);
                            },
                            if *ledger_open.read() { "Done" } else { "Review" }
                        }
                    }
                    if *ledger_open.read() && !resolved.is_empty() {
                        div { class: "flex basis-full flex-col gap-1.5 border-t border-faint pt-3",
                            span { class: "text-[11px] text-fg-muted",
                                "Changing a verdict changes the record only; any to-do it created stays where it is. Hobbes sees each change and your note at its next agent time."
                            }
                            for row in resolved.iter().cloned() {
                                {
                                    let id = row.id.clone();
                                    let dismissed = row.status == ProposalStatus::Dismissed;
                                    let flip_to = if dismissed { ProposalStatus::Accepted } else { ProposalStatus::Dismissed };
                                    let flip_label = if dismissed { "Count as accepted" } else { "Count as dismissed" };
                                    let this_revision = revising.read().as_ref().filter(|r| r.0 == id).map(|r| (r.1, r.2.clone()));
                                    let verdict_class = if dismissed {
                                        "shrink-0 rounded-full border border-faint px-2 text-[10.5px] text-fg-muted"
                                    } else {
                                        "shrink-0 rounded-full border border-primary-400 px-2 text-[10.5px] text-fg"
                                    };
                                    let verdict = match row.status {
                                        ProposalStatus::Edited => "accepted \u{b7} edited",
                                        ProposalStatus::Dismissed => "dismissed",
                                        _ => "accepted",
                                    };
                                    let when = row.resolved_at.map(|t| t.with_timezone(&chrono::Local).format("%b %-d").to_string()).unwrap_or_default();
                                    let last_note = row.revisions.last().map(|r| {
                                        let what = format!("you changed it from {}", if r.from == ProposalStatus::Dismissed { "dismissed" } else { "accepted" });
                                        if r.note.is_empty() { what } else { format!("{what}: \u{201c}{}\u{201d}", r.note) }
                                    });
                                    rsx! {
                                        div { key: "{id}", class: "flex flex-col gap-1 rounded border border-faint px-2.5 py-1.5",
                                            div { class: "flex items-center gap-2",
                                                span { class: "{verdict_class}", "{verdict}" }
                                                span { class: "min-w-0 flex-1 truncate text-[12.5px] text-fg", title: "{row.title}", "{proposal_headline(&row)}" }
                                                span { class: "shrink-0 font-mono text-[10.5px] text-fg-muted", "{kind_label(row.kind)} \u{b7} {when}" }
                                                if this_revision.is_none() {
                                                    button {
                                                        class: "shrink-0 rounded border border-faint px-2 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: {
                                                            let id = id.clone();
                                                            move |_| revising.set(Some((id.clone(), Some(flip_to), String::new())))
                                                        },
                                                        "{flip_label}"
                                                    }
                                                    button {
                                                        class: "shrink-0 rounded border border-faint px-2 py-0.5 text-[11px] text-fg-muted hover:bg-input",
                                                        title: "Take it off the ledger: it stops counting either way",
                                                        onclick: {
                                                            let id = id.clone();
                                                            move |_| revising.set(Some((id.clone(), None, String::new())))
                                                        },
                                                        "Remove"
                                                    }
                                                }
                                            }
                                            if let Some(note) = last_note {
                                                span { class: "text-[11px] text-fg-muted", "{note}" }
                                            }
                                            if let Some((to, note)) = this_revision {
                                                div { class: "flex items-center gap-1.5",
                                                    input {
                                                        class: "min-w-0 flex-1 rounded border border-faint bg-input px-2 py-0.5 text-xs text-fg outline-none",
                                                        placeholder: "Why? Hobbes sees this note (optional)",
                                                        value: "{note}",
                                                        oninput: move |e| if let Some(r) = revising.write().as_mut() { r.2 = e.value() },
                                                    }
                                                    button {
                                                        class: "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                                                        onclick: {
                                                            let id = id.clone();
                                                            move |_| {
                                                                let note = revising.peek().as_ref().map(|r| r.2.clone()).unwrap_or_default();
                                                                revise(planner, &id, to, &note);
                                                                revising.set(None);
                                                            }
                                                        },
                                                        if to.is_some() { "{flip_label}" } else { "Remove from ledger" }
                                                    }
                                                    button {
                                                        class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: move |_| revising.set(None),
                                                        "Cancel"
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

/// Resolved proposals still on the ledger, most recently decided first,
/// for Trust ledger → Review.
const LEDGER_REVIEW_ROWS: usize = 30;

fn ledger_review_rows(proposals: &[Proposal]) -> Vec<Proposal> {
    let mut rows: Vec<Proposal> = proposals
        .iter()
        .filter(|p| p.status != ProposalStatus::Open && !p.removed)
        .cloned()
        .collect();
    rows.sort_by(|a, b| b.resolved_at.cmp(&a.resolved_at));
    rows.truncate(LEDGER_REVIEW_ROWS);
    rows
}

/// Change a past verdict (`to`) or remove it from the ledger (`None`);
/// persist the proposal and the ledger.
fn revise(mut planner: Signal<PlannerState>, id: &str, to: Option<ProposalStatus>, note: &str) {
    let revised = planner.write().revise_proposal(id, to, note, Utc::now());
    match revised {
        Ok(p) => persist_resolved(&planner, &p),
        Err(e) => tracing::warn!("planner: couldn't revise proposal {}: {}", id, e),
    }
}

fn persist_resolved(planner: &Signal<PlannerState>, p: &Proposal) {
    if let Err(e) = store::save_proposal(p) {
        tracing::error!("planner: failed to save proposal {}: {}", p.id, e);
    }
    if let Err(e) = store::save_trust_ledger(&planner.peek().trust_ledger) {
        tracing::error!("planner: failed to save trust ledger: {}", e);
    }
}

/// Dismiss a proposal; persist it and the ledger.
fn dismiss(mut planner: Signal<PlannerState>, id: &str) {
    let resolved = planner.write().resolve_proposal(id, ProposalStatus::Dismissed, None, Utc::now());
    if let Some(p) = resolved {
        persist_resolved(&planner, &p);
    }
}

/// Accept a proposal (`PlannerState::accept_proposal`: an offer moves the
/// to-do to the agent lane, a suggested to-do is created) and persist it.
/// With `start`, also start the work ("Accept & start"): hand it to the
/// running agent-time session when one has budget left, else dispatch it
/// to a new chat worker on the grant's model, which is user-initiated and
/// uses no agent time. Returns the line shown in place of the card.
fn accept(
    mut planner: Signal<PlannerState>,
    settings: Signal<Settings>,
    ctx: crate::components::builtin_tools::BuiltinToolCtx,
    id: &str,
    edited: Option<(String, String)>,
    start: bool,
) -> Option<AcceptedLine> {
    let done = planner.write().accept_proposal(id, edited, Utc::now())?;
    persist_resolved(&planner, &done.proposal);
    if let Some(t) = &done.todo {
        if let Err(e) = store::save_todo(t) {
            tracing::error!("planner: failed to save accepted proposal's to-do {}: {}", t.id, e);
        }
    }
    let p = &done.proposal;
    let todo_id = done.todo.as_ref().map(|t| t.id.clone());
    let mut line = AcceptedLine {
        proposal_id: p.id.clone(),
        title: proposal_headline(p),
        text: String::new(),
        chat_id: None,
        todo_id: todo_id.clone(),
        error: false,
    };
    if !proposal_starts_work(p) {
        line.text = match p.kind {
            ProposalKind::Todo => "Added to your Inbox".to_string(),
            _ => "Accepted".to_string(),
        };
        return Some(line);
    }
    let Some(todo_id) = todo_id else {
        line.text = "Accepted; the to-do was already moved or closed, so nothing started".to_string();
        line.error = true;
        return Some(line);
    };
    if !start {
        line.text = "Queued in the agent lane".to_string();
        return Some(line);
    }
    // Bind the route first: a `peek()` in the match scrutinee would hold the
    // planner's read guard across both arms, and each arm writes the planner.
    let route = accept_start_route(&planner.peek(), today(), Utc::now());
    match route {
        AcceptStartRoute::HandToRun { .. } => {
            let offer = p.kind == ProposalKind::Offer;
            match crate::agent_time_runtime::hand_to_run(planner, settings, &todo_id, offer) {
                Some(chat) => {
                    line.text = "Started \u{b7} handed to the running agent-time session".to_string();
                    line.chat_id = Some(chat);
                }
                None => {
                    line.text = "Queued; agent time stopped before it could take it".to_string();
                    line.error = true;
                }
            }
        }
        AcceptStartRoute::NewChat => {
            let choice = crate::agent_time_runtime::grant_model_choice(
                &planner.peek().agent_time_grant,
                &settings.peek(),
            );
            let started = choice.and_then(|choice| {
                crate::todo::dispatch::dispatch_to_chat(
                    ctx,
                    &todo_id,
                    crate::todo::dispatch::ChatDispatchOpts { choice, focus: false, ..Default::default() },
                )
            });
            match started {
                Ok(d) => {
                    line.text = format!(
                        "Started in a new chat \u{b7} {} \u{b7} uses no agent time",
                        short_model_label(&d.model)
                    );
                    line.chat_id = Some(d.session_id);
                }
                Err(e) => {
                    line.text = format!("Queued; couldn't start: {e}");
                    line.error = true;
                }
            }
        }
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_and_start_on_an_offer_dispatches_without_a_borrow_panic() {
        // Regression: `match accept_start_route(&planner.peek(), ..)` held the
        // planner's read guard across the arms, and dispatch_to_chat's
        // `planner.write()` panicked (crash on "Accept & start", 2026-10-07).
        let mut dom = dioxus::prelude::VirtualDom::new(|| {
            use crate::todo::model::{AgentState, Lane, Proposal, ProposalKind, ProposalStatus, Todo};
            let mut state = PlannerState::default();
            let todo = Todo::new("Compare Threadripper Halo vs DGX Station", 1.0);
            let todo_id = todo.id.clone();
            state.todos.push(todo);
            state.proposals.push(Proposal {
                id: "pr-offer".into(),
                kind: ProposalKind::Offer,
                title: "Hobbes offers to take it".into(),
                body: String::new(),
                status: ProposalStatus::Open,
                created_at: Utc::now(),
                resolved_at: None,
                session_id: "ats-1".into(),
                todo_id: Some(todo_id.clone()),
                lane: None,
                revisions: Vec::new(),
                removed: false,
            });
            let ctx = crate::todo::dispatch::tests::test_ctx_with(state, Settings::default());
            let line = accept(ctx.planner, ctx.settings, ctx, "pr-offer", None, true).expect("accepted");
            assert!(!line.error, "{}", line.text);
            let chat = line.chat_id.expect("a chat started");
            let t = ctx.planner.peek().todo(&todo_id).cloned().unwrap();
            assert_eq!(t.lane, Lane::Agent);
            assert_eq!(t.agent_state, Some(AgentState::Working));
            assert_eq!(t.linked_fleet_session.as_deref(), Some(chat.as_str()));

            dioxus::prelude::rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }

    fn budget(enabled: bool) -> AgentTimeBudget {
        AgentTimeBudget {
            enabled,
            granted_today: true,
            minutes_granted: 45,
            minutes_used: 18,
            spend_cap_usd: 2.0,
            spend_used_usd: 0.42,
        }
    }

    #[test]
    fn status_reads_like_the_design() {
        assert_eq!(status_line(&budget(false), false, false), ("Off \u{b7} 0 min granted".to_string(), None));
        let (s, spend) = status_line(&budget(true), true, false);
        assert_eq!(s, "Running \u{b7} 18 of 45 min today");
        assert_eq!(spend.as_deref(), Some("$0.42 of $2.00 cap"));
        assert!(status_line(&budget(true), false, false).0.starts_with("Idle"));
        let mut not_today = budget(true);
        not_today.granted_today = false;
        assert_eq!(status_line(&not_today, false, false).0, "Not a granted day");
    }

    #[test]
    fn status_and_tail_say_waiting_on_you_while_a_call_is_held() {
        let (s, _) = status_line(&budget(true), true, true);
        assert_eq!(s, "Waiting on you \u{b7} 18 of 45 min today", "waiting still counts minutes");
        // A held call with nothing running (never happens once the lease
        // closes and expires it) doesn't claim a run.
        assert!(status_line(&budget(true), false, true).0.starts_with("Idle"));
        assert_eq!(live_tail(&[]), "working");
        let h = HeldApproval {
            chat_id: "c".into(),
            execution_id: "e".into(),
            server: "hobbes-core".into(),
            tool: "HOBBES_INVOKE_SKILL".into(),
            short_args: "github-security".into(),
            reason: "The skill's permission is set to Ask.".into(),
            asked_at: Utc::now(),
        };
        assert_eq!(live_tail(&[h]), "waiting for your approval \u{b7} HOBBES_INVOKE_SKILL");
    }

    #[test]
    fn the_model_select_round_trips_and_groups_by_connector() {
        assert_eq!(grant_select_value(None, None), "");
        assert_eq!(parse_grant_select(""), (None, None));
        let v = grant_select_value(Some("c1"), Some("claude-haiku-5-5"));
        assert_eq!(parse_grant_select(&v), (Some("c1".into()), Some("claude-haiku-5-5".into())));
        assert_eq!(parse_grant_select("c1|"), (Some("c1".into()), None));
        let s = crate::settings::test_delegation_settings();
        let groups = model_option_groups(&s.allowed_models());
        let names: Vec<&str> = groups.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Claude", "Local"]);
        let labels: Vec<&str> = groups[0].1.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["Fable 5.1", "Haiku 5.5", "Opus 5.5", "Sonnet 5.5"]);
    }

    #[test]
    fn offers_read_as_offers() {
        let p = Proposal {
            id: "p".into(),
            kind: ProposalKind::Offer,
            title: "Draft the release notes".into(),
            body: "pitch".into(),
            status: ProposalStatus::Open,
            created_at: Utc::now(),
            resolved_at: None,
            session_id: "s".into(),
            todo_id: Some("t".into()),
            lane: None,
            revisions: Vec::new(),
            removed: false,
        };
        assert_eq!(proposal_headline(&p), "Hobbes offers to take: Draft the release notes");
        assert_eq!(kind_label(ProposalKind::Offer), "Offer to take");
        let note = Proposal { kind: ProposalKind::Note, todo_id: None, ..p };
        assert_eq!(proposal_headline(&note), "Draft the release notes");
    }

    #[test]
    fn trust_stats_show_dashes_without_evidence() {
        let empty = trust_stats(&TrustLedger::default());
        assert!(empty.iter().all(|(v, _)| v == "\u{2014}"));
        let l = TrustLedger {
            proposals_accepted: 36,
            proposals_edited: 2,
            proposals_dismissed: 6,
            in_scope_attempts: 50,
            out_of_scope_attempts: 0,
            promises_kept: 9,
            promises_broken: 1,
            clean_sessions: 23,
        };
        let s = trust_stats(&l);
        assert_eq!(s[0], ("86%".to_string(), "accepted \u{b7} 38 of 44".to_string()));
        assert_eq!(s[1], ("100%".to_string(), "in scope \u{b7} 23 sessions".to_string()));
        assert_eq!(s[2].0, "9 of 10");
    }

    #[test]
    fn log_meta_counts_calls_and_cost() {
        let now = Utc::now();
        let s = AgentTimeSession {
            id: "s".into(),
            intent: "digest".into(),
            started_at: now - chrono::Duration::minutes(18),
            ended_at: None,
            cost_usd: 0.42,
            tool_calls: vec![],
            outputs: vec![],
            end_reason: None,
            summary: None,
            chat_session_id: None,
            out_of_scope_attempts: 0,
            todo_id: None,
        };
        assert_eq!(log_meta(&s, now), "18m \u{b7} $0.42 \u{b7} 0 tool calls");
        // Only real calls count: not the model's notes or the runtime's lines.
        use crate::todo::model::AgentTimeLogEntry;
        let line = |kind| AgentTimeLogEntry { at: now, tool: "t".into(), summary: "s".into(), kind, note: None };
        let mut s = s;
        s.tool_calls = vec![line(AgentTimeLogKind::Call), line(AgentTimeLogKind::Note), line(AgentTimeLogKind::Event), line(AgentTimeLogKind::Call)];
        assert_eq!(log_meta(&s, now), "18m \u{b7} $0.42 \u{b7} 2 tool calls");
        assert_eq!(kind_label(ProposalKind::Research), "Research digest");
    }
}
