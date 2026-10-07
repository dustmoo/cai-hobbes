//! The Agent time panel: the human-granted budget in which the harness
//! chooses its own work, in the open (SYMBIOTIC_PLANNER.md §3).
//!
//! Off by default. The panel edits the grant, shows the live status with
//! prominent Pause / Stop (sent as `ChatCommand`s to the agent-time
//! runtime), today's declared intent, the full log, the proposals to
//! accept / edit / dismiss, and the trust ledger. Evidence only ever
//! *suggests* a wider scope; allowing one records the person's choice.

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::chat_input::ChatCommand;
use crate::todo::agent_time::{proposal_to_todo, AgentTimeBudget};
use crate::todo::measures;
use crate::todo::model::{AgentTimeSession, Proposal, ProposalKind, ProposalStatus, TrustLedger};
use crate::todo::store;
use crate::todo::PlannerState;

use super::today;

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The header status pill and its spend line.
pub(crate) fn status_line(b: &AgentTimeBudget, running: bool) -> (String, Option<String>) {
    if !b.enabled {
        return ("Off \u{b7} 0 min granted".to_string(), None);
    }
    if !b.granted_today {
        return ("Not a granted day".to_string(), None);
    }
    let state = if running { "Running" } else { "Idle" };
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

/// `"18m · $0.42 · 9 tool calls"` for the log header.
pub(crate) fn log_meta(s: &AgentTimeSession, now: DateTime<Utc>) -> String {
    let calls = s.tool_calls.len();
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
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut widening_dismissed = use_signal(|| false);
    let mut editing = use_signal(|| Option::<(String, String, String)>::None);
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
    let proposals: Vec<Proposal> = state.open_proposals().into_iter().cloned().collect();
    let ledger = state.trust_ledger;
    let projects: Vec<String> = grant
        .scope
        .project_ids
        .iter()
        .filter_map(|id| state.projects.iter().find(|p| &p.id == id).map(|p| p.title.clone()))
        .collect();
    drop(state);

    let (status, spend) = status_line(&budget, open_session.is_some());
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
                        class: if open_session.is_some() {
                            "inline-flex h-6 items-center gap-1.5 rounded-full border border-primary-400 px-2.5 text-xs text-fg"
                        } else {
                            "inline-flex h-6 items-center gap-1.5 rounded-full border border-faint px-2.5 text-xs text-fg-muted"
                        },
                        if open_session.is_some() {
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
                                span { class: "text-fg-muted", "Connector" }
                                span { class: "text-fg", "Same as your chats" }
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
                            if proposals.is_empty() {
                                p { class: "{EMPTY}", "No proposals. Whatever agent time makes lands here as a suggestion you accept, edit or dismiss." }
                            }
                            for p in proposals {
                                {
                                    let is_editing = editing.read().as_ref().is_some_and(|(id, _, _)| *id == p.id);
                                    let pid = p.id.clone();
                                    rsx! {
                                        div {
                                            key: "{p.id}",
                                            class: "flex flex-col gap-1.5 rounded-md border border-faint bg-card px-2.5 py-2",
                                            div { class: "flex items-center gap-2",
                                                span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "{kind_label(p.kind)}" }
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
                                                div { class: "text-[13px] leading-[18px] text-fg", "{p.title}" }
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
                                                                resolve(planner, &pid, ProposalStatus::Accepted, edited);
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
                                                    button {
                                                        class: "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover",
                                                        onclick: {
                                                            let pid = pid.clone();
                                                            move |_| resolve(planner, &pid, ProposalStatus::Accepted, None)
                                                        },
                                                        "Accept"
                                                    }
                                                    button {
                                                        class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: {
                                                            let seed = (p.id.clone(), p.title.clone(), p.body.clone());
                                                            move |_| editing.set(Some(seed.clone()))
                                                        },
                                                        "Edit"
                                                    }
                                                    button {
                                                        class: "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input",
                                                        onclick: {
                                                            let pid = pid.clone();
                                                            move |_| resolve(planner, &pid, ProposalStatus::Dismissed, None)
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
                        div {
                            class: "{PANEL}",
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
                                                span { class: "truncate font-mono text-[11px] text-fg", "{e.tool}" }
                                                span { class: "min-w-0 truncate text-fg-muted", title: "{e.summary}", "{e.summary}" }
                                            }
                                        }
                                        if s.tool_calls.is_empty() {
                                            p { class: "{EMPTY} py-1", "No tool calls yet." }
                                        }
                                        if s.is_open() {
                                            div {
                                                class: "grid grid-cols-[64px_minmax(0,200px)_minmax(0,1fr)] items-baseline gap-x-3 py-1.5 text-xs",
                                                span { class: "font-mono text-[11px] text-fg-muted", "{hhmmss(now)}" }
                                                span { class: "inline-flex items-center gap-1.5 font-mono text-[11px] text-fg",
                                                    span { class: "h-1.5 w-1.5 rounded-full bg-primary-400 animate-pulse" }
                                                    "working"
                                                }
                                                span {}
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
                }
            }
        }
    }
}

/// Accept / edit / dismiss a proposal, persist it and the ledger, and turn
/// an accepted `Todo` proposal into an Inbox to-do.
fn resolve(mut planner: Signal<PlannerState>, id: &str, status: ProposalStatus, edited: Option<(String, String)>) {
    let resolved = planner.write().resolve_proposal(id, status, edited, Utc::now());
    let Some(p) = resolved else { return };
    if let Err(e) = store::save_proposal(&p) {
        tracing::error!("planner: failed to save proposal {}: {}", p.id, e);
    }
    if let Err(e) = store::save_trust_ledger(&planner.peek().trust_ledger) {
        tracing::error!("planner: failed to save trust ledger: {}", e);
    }
    if p.kind == ProposalKind::Todo && matches!(p.status, ProposalStatus::Accepted | ProposalStatus::Edited) {
        let todo = proposal_to_todo(&p, planner.peek().next_sort_order());
        if let Err(e) = store::save_todo(&todo) {
            tracing::error!("planner: failed to save accepted proposal to-do: {}", e);
        }
        planner.write().upsert_todo(todo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(status_line(&budget(false), false), ("Off \u{b7} 0 min granted".to_string(), None));
        let (s, spend) = status_line(&budget(true), true);
        assert_eq!(s, "Running \u{b7} 18 of 45 min today");
        assert_eq!(spend.as_deref(), Some("$0.42 of $2.00 cap"));
        assert!(status_line(&budget(true), false).0.starts_with("Idle"));
        let mut not_today = budget(true);
        not_today.granted_today = false;
        assert_eq!(status_line(&not_today, false).0, "Not a granted day");
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
        };
        assert_eq!(log_meta(&s, now), "18m \u{b7} $0.42 \u{b7} 0 tool calls");
        assert_eq!(kind_label(ProposalKind::Research), "Research digest");
    }
}
