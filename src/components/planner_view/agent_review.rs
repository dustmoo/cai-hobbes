//! The review the person reads after agent time: "While you were away".
//!
//! Saved questions, four sections of results (Prepare, Finish, Notice,
//! Tend) with Keep / Needs work / Discard grading (Notice keeps Accept /
//! Dismiss), and the chat rail ([`super::agent_review_chat`]). The data is
//! [`crate::todo::agent_time::review_for`]; every action persists what it
//! changes.

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::components::chat_input::ChatCommand;
use crate::settings::Settings;
use crate::todo::agent_time::{proposal_starts_work, AgentTimeReview};
use crate::todo::model::{AgentQuestion, AgentTimeSession, Job, Proposal, ProposalKind, ProposalStatus, TrustLedger};
use crate::todo::store;
use crate::todo::PlannerState;

use super::agent_review_chat::{ChatRail, ChatRef, COMPOSER_ID};
use super::agent_time::{accept, dismiss, kind_label, persist_resolved, proposal_headline, AcceptedLine};

const EYEBROW: &str = "text-[11px] uppercase tracking-[0.08em] text-fg-muted";
const EMPTY: &str = "text-xs leading-relaxed text-fg-muted";
const CARD: &str = "relative flex flex-col gap-1.5 rounded-md border border-faint bg-card px-2.5 py-2";
const BTN_PRIMARY: &str = "rounded bg-btn-primary px-2.5 py-0.5 text-[11px] text-fg hover:bg-btn-primary-hover";
const BTN: &str = "rounded border border-faint px-2.5 py-0.5 text-[11px] text-fg hover:bg-input";

// ── Pure helpers ────────────────────────────────────────────────────────────

fn local_hm(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

fn local_day_hm(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%b %-d, %H:%M").to_string()
}

/// The window the review covers: first run's start to the last run's end
/// (local times), e.g. `"06:30 \u{2013} 07:12"`; with the day on each end
/// when it spans days. `None` without a run.
pub(crate) fn window_label(sessions: &[AgentTimeSession]) -> Option<String> {
    let first = sessions.iter().map(|s| s.started_at).min()?;
    let last = sessions
        .iter()
        .map(|s| s.ended_at.unwrap_or(s.started_at))
        .max()?;
    let same_day = first.with_timezone(&chrono::Local).date_naive() == last.with_timezone(&chrono::Local).date_naive();
    Some(if same_day {
        format!("{} \u{2013} {}", local_hm(first), local_hm(last))
    } else {
        format!("{} \u{2013} {}", local_day_hm(first), local_day_hm(last))
    })
}

/// The header's meta line: window, minutes of the daily grant, spend of the
/// cap, result count.
pub(crate) fn review_meta(r: &AgentTimeReview, granted_minutes: u32, cap_usd: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(w) = window_label(&r.sessions) {
        parts.push(w);
    }
    parts.push(format!("{} of {} min", r.minutes_used, granted_minutes));
    parts.push(format!("${:.2} of ${:.2}", r.spend_usd, cap_usd));
    let n = r.counts.results;
    parts.push(format!("{} result{}", n, if n == 1 { "" } else { "s" }));
    parts.join(" \u{b7} ")
}

/// The trust strip's two v2 stats: (value, caption) x 2.
pub(crate) fn result_stats(l: &TrustLedger) -> [(String, String); 2] {
    let graded = l.results_kept + l.results_needs_work + l.results_discarded;
    let kept = match l.keep_rate() {
        Some(r) => (format!("{:.0}%", r * 100.0), format!("kept \u{b7} {} of {}", l.results_kept, graded)),
        None => ("\u{2014}".to_string(), "kept \u{b7} none graded".to_string()),
    };
    let corrections = (l.corrections_delivered.to_string(), "corrections delivered".to_string());
    [kept, corrections]
}

/// Whether a card's body needs the expand control.
fn long_body(body: &str) -> bool {
    body.lines().count() > 3 || body.chars().count() > 240
}

// ── Actions ─────────────────────────────────────────────────────────────────

/// Mark the review's runs read and persist them.
pub(crate) fn mark_reviewed(mut planner: Signal<PlannerState>, session_ids: &[String]) {
    let done = planner.write().mark_reviewed(session_ids, Utc::now());
    for s in &done {
        if let Err(e) = store::save_agent_time_session(s) {
            tracing::error!("planner: failed to save agent-time session {}: {}", s.id, e);
        }
    }
}

fn grade(mut planner: Signal<PlannerState>, id: &str, verdict: ProposalStatus) {
    let graded = planner.write().grade_result(id, verdict, Utc::now());
    if let Some(g) = graded {
        persist_resolved(&planner, &g.proposal);
        if let Some(t) = &g.todo {
            if let Err(e) = store::save_todo(t) {
                tracing::error!("planner: failed to save graded result's to-do {}: {}", t.id, e);
            }
        }
    }
}

fn answer(mut planner: Signal<PlannerState>, id: &str, text: &str) {
    let saved = planner.write().answer_question(id, text, Utc::now());
    match saved {
        Ok(q) => {
            if let Err(e) = store::save_agent_question(&q) {
                tracing::error!("planner: failed to save question {}: {}", q.id, e);
            }
        }
        Err(e) => tracing::warn!("planner: couldn't answer question {}: {}", id, e),
    }
}

fn focus_composer() {
    let _ = document::eval(&format!(
        "const el = document.getElementById('{COMPOSER_ID}'); if (el) {{ el.focus(); }}"
    ));
}

// ── The body ────────────────────────────────────────────────────────────────

#[component]
pub(crate) fn ReviewBody(review: AgentTimeReview) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let chat_ref = use_signal(|| Option::<ChatRef>::None);
    let accepted = use_signal(Vec::<AcceptedLine>::new);

    let pending: Vec<AgentQuestion> = planner.read().pending_answers().into_iter().cloned().collect();
    let mut questions: Vec<AgentQuestion> = review.questions.clone();
    questions.extend(pending);

    let accepted_lines = accepted.read().clone();
    let accepted_ids: Vec<String> = accepted_lines.iter().map(|l| l.proposal_id.clone()).collect();

    let sections: [(Job, &str, &str, &str, &Vec<Proposal>); 4] = [
        (
            Job::Prepare,
            "Prepare",
            "ready for today",
            "Nothing flagged Prep this. Flag one of your to-dos with Prep this and Hobbes prepares it here.",
            &review.prepare,
        ),
        (
            Job::Finish,
            "Finish",
            "done from the agent queue",
            "Nothing finished from the agent queue yet.",
            &review.finish,
        ),
        (
            Job::Notice,
            "Notice",
            "would change your stuff",
            "Nothing waiting on your say-so.",
            &review.notice,
        ),
        (
            Job::Tend,
            "Tend",
            "Hobbes\u{2019}s own upkeep",
            "No upkeep to review.",
            &review.tend,
        ),
    ];

    rsx! {
        div { class: "flex flex-wrap items-start gap-3",
            div { class: "flex min-w-[320px] flex-1 flex-col gap-3",
                if !questions.is_empty() {
                    div {
                        class: "flex flex-col gap-2.5 rounded-lg border border-yellow-700 bg-yellow-900/40 px-3.5 py-3",
                        span { class: "text-[11px] uppercase tracking-[0.08em] text-yellow-300", "Saved for you" }
                        for q in questions {
                            QuestionRow { key: "{q.id}", q: q.clone() }
                        }
                    }
                }
                div { class: "grid grid-cols-[repeat(auto-fit,minmax(300px,1fr))] items-start gap-3",
                    for (job, name, tag, empty, items) in sections {
                        {
                            let is_notice = job == Job::Notice;
                            let shown: Vec<Proposal> = items
                                .iter()
                                .filter(|p| !(is_notice && accepted_ids.contains(&p.id)))
                                .cloned()
                                .collect();
                            let lines: Vec<AcceptedLine> = if is_notice { accepted_lines.clone() } else { Vec::new() };
                            let no_lines = lines.is_empty();
                            rsx! {
                                div {
                                    key: "{name}",
                                    class: "flex min-w-0 flex-col gap-2.5 rounded-lg border border-faint bg-section p-3.5",
                                    div { class: "flex items-baseline gap-2",
                                        span { class: "{EYEBROW}", "{name}" }
                                        span { class: "min-w-0 truncate text-[11px] text-fg-muted", "\u{b7} {tag}" }
                                        div { class: "flex-1" }
                                        span { class: "font-mono text-[11px] text-fg-muted", "{shown.len()}" }
                                    }
                                    for line in lines {
                                        AcceptedRow { key: "acc-{line.proposal_id}", line: line.clone() }
                                    }
                                    if shown.is_empty() && no_lines {
                                        p { class: "{EMPTY}", "{empty}" }
                                    }
                                    for p in shown {
                                        ResultCard { key: "{p.id}", p: p.clone(), chat_ref, accepted }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ChatRail { chat_ref }
        }
    }
}

// ── Pieces ──────────────────────────────────────────────────────────────────

#[component]
fn AcceptedRow(line: AcceptedLine) -> Element {
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut todo_detail = use_context::<crate::components::todo_detail::TodoDetailContext>().0;
    rsx! {
        div {
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
}

#[component]
fn QuestionRow(q: AgentQuestion) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let mut changing = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let answered = q.answer.clone().filter(|_| !*changing.read());
    let id = q.id.clone();
    let submit = {
        let id = id.clone();
        move || {
            let text = draft.peek().trim().to_string();
            if text.is_empty() {
                return;
            }
            answer(planner, &id, &text);
            draft.set(String::new());
            changing.set(false);
        }
    };
    rsx! {
        div { class: "flex flex-col gap-1.5",
            div { class: "text-[13px] leading-[18px] text-yellow-100", "{q.question}" }
            if let Some(a) = answered {
                div { class: "flex items-center gap-2 text-xs",
                    span { class: "text-yellow-200/80", "Your answer:" }
                    span { class: "min-w-0 truncate text-fg", title: "{a}", "{a}" }
                    div { class: "flex-1" }
                    button {
                        class: "shrink-0 text-yellow-200/80 hover:text-yellow-100",
                        title: "Change it before it reaches agent time",
                        onclick: {
                            let a = a.clone();
                            move |_| {
                                draft.set(a.clone());
                                changing.set(true);
                            }
                        },
                        "Change"
                    }
                }
            } else {
                if !q.options.is_empty() {
                    div { class: "flex flex-wrap gap-1.5",
                        for (i, opt) in q.options.iter().cloned().enumerate() {
                            button {
                                key: "{i}",
                                class: "rounded border border-yellow-700 px-2.5 py-0.5 text-xs text-yellow-100 hover:bg-yellow-900/70",
                                onclick: {
                                    let id = id.clone();
                                    let opt = opt.clone();
                                    move |_| {
                                        answer(planner, &id, &opt);
                                        changing.set(false);
                                    }
                                },
                                "{opt}"
                            }
                        }
                    }
                }
                div { class: "flex items-center gap-1.5",
                    input {
                        class: "h-6 min-w-0 flex-1 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                        placeholder: "Or answer in your own words",
                        "aria-label": "Answer",
                        value: "{draft}",
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: {
                            let mut submit = submit.clone();
                            move |e: KeyboardEvent| {
                                if e.key() == Key::Enter {
                                    e.prevent_default();
                                    submit();
                                }
                            }
                        },
                    }
                    button {
                        class: "{BTN} disabled:opacity-50",
                        disabled: draft.read().trim().is_empty(),
                        onclick: {
                            let mut submit = submit.clone();
                            move |_| submit()
                        },
                        "Answer"
                    }
                }
            }
        }
    }
}

#[component]
fn ResultCard(p: Proposal, chat_ref: Signal<Option<ChatRef>>, accepted: Signal<Vec<AcceptedLine>>) -> Element {
    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let mut chat_command = use_context::<Signal<Option<ChatCommand>>>();
    let mut expanded = use_signal(|| false);
    let mut editing = use_signal(|| Option::<(String, String)>::None);

    let is_open = p.status == ProposalStatus::Open;
    let graded_job = p.job.is_graded();
    let starts = proposal_starts_work(&p);
    let running = planner.read().open_agent_time_session().is_some();
    let notes = p.notes.len();
    let headline = proposal_headline(&p);
    let verdict = p.status.verdict_word(p.job);
    let long = long_body(&p.body);
    let body_class = if *expanded.read() {
        "whitespace-pre-line text-[11.5px] leading-4 text-fg-muted"
    } else {
        "line-clamp-3 whitespace-pre-line text-[11.5px] leading-4 text-fg-muted"
    };
    let is_editing = editing.read().is_some();
    let pid = p.id.clone();

    rsx! {
        div { class: "{CARD}",
            div { class: "flex items-center gap-2",
                if p.correction {
                    span { class: "rounded-full border border-yellow-600 px-1.5 text-[10px] uppercase leading-4 tracking-[0.08em] text-yellow-200", "Correction" }
                }
                if p.kind == ProposalKind::Offer {
                    span { class: "rounded-full border border-primary-400 px-1.5 text-[10px] uppercase leading-4 tracking-[0.08em] text-primary-300", "{kind_label(p.kind)}" }
                } else {
                    span { class: "text-[10px] uppercase tracking-[0.08em] text-fg-muted", "{kind_label(p.kind)}" }
                }
                div { class: "flex-1" }
                button {
                    class: "inline-flex shrink-0 items-center gap-1 text-fg-muted hover:text-fg",
                    title: "Chat about this",
                    "aria-label": "Chat about this",
                    onclick: {
                        let r = ChatRef { id: p.id.clone(), title: p.title.clone() };
                        move |_| {
                            chat_ref.set(Some(r.clone()));
                            focus_composer();
                        }
                    },
                    Icon { width: 13, height: 13, icon: fi_icons::FiMessageCircle }
                    if notes > 0 {
                        span { class: "font-mono text-[10.5px]", "{notes}" }
                    }
                }
            }
            if is_editing {
                input {
                    class: "h-7 rounded border border-faint bg-input px-2 text-xs text-fg outline-none",
                    value: "{editing.read().as_ref().map(|e| e.0.clone()).unwrap_or_default()}",
                    oninput: move |e| if let Some(ed) = editing.write().as_mut() { ed.0 = e.value() },
                }
                textarea {
                    class: "min-h-[64px] rounded border border-faint bg-input px-2 py-1 text-xs text-fg outline-none",
                    value: "{editing.read().as_ref().map(|e| e.1.clone()).unwrap_or_default()}",
                    oninput: move |e| if let Some(ed) = editing.write().as_mut() { ed.1 = e.value() },
                }
            } else {
                div { class: "text-[13px] leading-[18px] text-fg", "{headline}" }
                if !p.body.trim().is_empty() {
                    div { class: "{body_class}", "{p.body}" }
                    if long {
                        button {
                            class: "self-start text-[11px] text-primary-300 hover:text-fg",
                            onclick: move |_| {
                                let now = *expanded.peek();
                                expanded.set(!now);
                            },
                            if *expanded.read() { "Show less" } else { "Show more" }
                        }
                    }
                }
            }
            if !is_open {
                div { class: "flex items-center gap-2",
                    span { class: "rounded-full border border-faint px-2 text-[10.5px] text-fg-muted", "{verdict}" }
                }
            } else if graded_job {
                div { class: "flex flex-wrap gap-1.5",
                    button {
                        class: "{BTN_PRIMARY}",
                        onclick: {
                            let pid = pid.clone();
                            move |_| grade(planner, &pid, ProposalStatus::Accepted)
                        },
                        "Keep"
                    }
                    button {
                        class: "{BTN}",
                        onclick: {
                            let pid = pid.clone();
                            move |_| grade(planner, &pid, ProposalStatus::NeedsWork)
                        },
                        "Needs work"
                    }
                    button {
                        class: "{BTN}",
                        onclick: {
                            let pid = pid.clone();
                            move |_| grade(planner, &pid, ProposalStatus::Dismissed)
                        },
                        "Discard"
                    }
                }
            } else {
                div { class: "flex flex-wrap gap-1.5",
                    if is_editing {
                        button {
                            class: "{BTN_PRIMARY}",
                            onclick: {
                                let pid = pid.clone();
                                move |_| {
                                    let edited = editing.peek().clone();
                                    if let Some(line) = accept(planner, settings, &pid, edited, false) {
                                        accepted.write().push(line);
                                    }
                                    editing.set(None);
                                }
                            },
                            "Save & accept"
                        }
                        button { class: "{BTN}", onclick: move |_| editing.set(None), "Cancel" }
                    } else {
                        if starts {
                            button {
                                class: "{BTN_PRIMARY}",
                                title: if running { "Accept and hand it to the running agent-time session" } else { "Accept and queue it for the next agent time" },
                                onclick: {
                                    let pid = pid.clone();
                                    move |_| {
                                        if let Some(line) = accept(planner, settings, &pid, None, true) {
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
                                class: "{BTN}",
                                title: "Accept and leave it queued in the agent lane",
                                onclick: {
                                    let pid = pid.clone();
                                    move |_| {
                                        if let Some(line) = accept(planner, settings, &pid, None, false) {
                                            accepted.write().push(line);
                                        }
                                    }
                                },
                                "Accept only"
                            }
                        } else {
                            button {
                                class: "{BTN_PRIMARY}",
                                onclick: {
                                    let pid = pid.clone();
                                    move |_| {
                                        if let Some(line) = accept(planner, settings, &pid, None, false) {
                                            accepted.write().push(line);
                                        }
                                    }
                                },
                                "Accept"
                            }
                        }
                        if p.kind != ProposalKind::Offer {
                            button {
                                class: "{BTN}",
                                onclick: {
                                    let seed = (p.title.clone(), p.body.clone());
                                    move |_| editing.set(Some(seed.clone()))
                                },
                                "Edit"
                            }
                        }
                        button {
                            class: "{BTN}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn run(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> AgentTimeSession {
        AgentTimeSession {
            id: "s".into(),
            intent: "x".into(),
            started_at: start,
            ended_at: end,
            cost_usd: 0.0,
            tool_calls: vec![],
            outputs: vec![],
            end_reason: None,
            summary: None,
            chat_session_id: None,
            out_of_scope_attempts: 0,
            todo_id: None,
            reviewed_at: None,
        }
    }

    fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Local.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_window_runs_from_the_first_start_to_the_last_end() {
        assert_eq!(window_label(&[]), None);
        let a = run(at(8, 6, 30), Some(at(8, 6, 50)));
        let b = run(at(8, 7, 0), Some(at(8, 7, 12)));
        assert_eq!(window_label(&[b.clone(), a.clone()]).as_deref(), Some("06:30 \u{2013} 07:12"));
        let overnight = run(at(9, 5, 5), Some(at(9, 5, 20)));
        assert_eq!(
            window_label(&[a, overnight]).as_deref(),
            Some("Oct 8, 06:30 \u{2013} Oct 9, 05:20")
        );
    }

    #[test]
    fn the_meta_line_reads_window_minutes_spend_and_results() {
        let mut r = AgentTimeReview::default();
        r.sessions = vec![run(at(8, 6, 30), Some(at(8, 7, 12)))];
        r.minutes_used = 42;
        r.spend_usd = 0.4;
        r.counts.results = 1;
        assert_eq!(review_meta(&r, 45, 2.0), "06:30 \u{2013} 07:12 \u{b7} 42 of 45 min \u{b7} $0.40 of $2.00 \u{b7} 1 result");
        r.counts.results = 5;
        assert!(review_meta(&r, 45, 2.0).ends_with("5 results"));
    }

    #[test]
    fn result_stats_show_keep_rate_and_corrections() {
        let none = result_stats(&TrustLedger::default());
        assert_eq!(none[0].0, "\u{2014}");
        assert_eq!(none[1], ("0".to_string(), "corrections delivered".to_string()));
        let l = TrustLedger {
            results_kept: 9,
            results_needs_work: 1,
            results_discarded: 1,
            corrections_delivered: 3,
            ..Default::default()
        };
        let s = result_stats(&l);
        assert_eq!(s[0], ("82%".to_string(), "kept \u{b7} 9 of 11".to_string()));
        assert_eq!(s[1].0, "3");
    }

    #[test]
    fn only_long_bodies_get_the_expand_control() {
        assert!(!long_body("one\ntwo\nthree"));
        assert!(long_body("1\n2\n3\n4"));
        assert!(long_body(&"x".repeat(241)));
    }

    #[test]
    fn grading_a_card_saves_the_verdict_without_a_borrow_panic() {
        use crate::todo::model::Proposal;
        let mut dom = VirtualDom::new(|| {
            let mut state = PlannerState::default();
            state.proposals.push(Proposal {
                id: "r1".into(),
                kind: ProposalKind::Research,
                title: "Digest".into(),
                body: String::new(),
                status: ProposalStatus::Open,
                created_at: Utc::now(),
                resolved_at: None,
                session_id: "s".into(),
                todo_id: None,
                lane: None,
                revisions: Vec::new(),
                removed: false,
                job: Job::Prepare,
                correction: false,
                notes: Vec::new(),
            });
            let ctx = crate::todo::dispatch::tests::test_ctx_with(state, Settings::default());
            grade(ctx.planner, "r1", ProposalStatus::NeedsWork);
            let st = ctx.planner.peek().proposals[0].status;
            assert_eq!(st, ProposalStatus::NeedsWork);
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
}
