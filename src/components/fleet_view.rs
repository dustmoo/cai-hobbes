//! The Fleet view (Pro) — every observed Claude Code session on the machine,
//! rendered in the planner's centre column when the Fleet rail entry is
//! selected.
//!
//! Reads the UI mirror `Signal<fleet::FleetState>` (fed by the drain loop in
//! `main.rs`). Actions go the other way through `fleet::shared()`:
//! Approve/Deny resolve a held gate's oneshot, the auto-passthrough toggle
//! writes state + store directly. Status colors are computed values, so they
//! ride inline `style:` attributes (Tailwind purges computed class names).

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::fleet::{self, AttentionKind, FleetSession, FleetStatus};
#[allow(unused_imports)]
use crate::fleet::FleetOrigin;
use crate::todo::model::format_minutes;

/// "just now" / "3m ago" / "2h ago" — fleet rows churn too fast for dates.
fn age_label(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (now - t).num_minutes();
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{}m ago", mins)
    } else if mins < 24 * 60 {
        format!("{}h ago", mins / 60)
    } else {
        format!("{}d ago", mins / (24 * 60))
    }
}

/// (label, chip color) for a status.
fn status_chip(status: &FleetStatus) -> (&'static str, &'static str) {
    match status {
        FleetStatus::Working => ("working", "#34d399"),
        FleetStatus::WorkingBackground => ("background agents", "#2dd4bf"),
        FleetStatus::Idle => ("idle", "#94a3b8"),
        FleetStatus::NeedsAttention(AttentionKind::Gate) => ("waiting on you", "#f59e0b"),
        FleetStatus::NeedsAttention(AttentionKind::Notification { .. }) => {
            ("needs you", "#f59e0b")
        }
    }
}

/// Needs-attention first, then most recent activity.
fn sorted_sessions(state: &fleet::FleetState) -> Vec<FleetSession> {
    let mut sessions: Vec<FleetSession> = state.sessions.values().cloned().collect();
    sessions.sort_by(|a, b| {
        b.status
            .needs_attention()
            .cmp(&a.status.needs_attention())
            .then(b.last_event_at.cmp(&a.last_event_at))
    });
    sessions
}

#[component]
pub fn FleetView() -> Element {
    let fleet_state = use_context::<Signal<fleet::FleetState>>();
    let settings = use_context::<Signal<crate::settings::Settings>>();

    // Ages and live minute counts drift without events; nudge a re-render on
    // a slow clock.
    let mut tick = use_signal(|| 0u32);
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            tick += 1;
        }
    });
    let _ = tick();

    let now = Utc::now();
    let today = Local::now().date_naive();
    let state = fleet_state.read();
    let sessions = sorted_sessions(&state);
    let needs_you = state.attention_count();
    let active = state
        .sessions
        .values()
        .filter(|s| {
            matches!(
                s.status,
                FleetStatus::Working | FleetStatus::WorkingBackground
            )
        })
        .count();
    let agent_minutes = fleet::fleet_agent_minutes_on(&state, today, now);
    let ended_today: Vec<FleetSession> = fleet::store::sessions_active_on(today)
        .into_iter()
        .filter(|r| !state.sessions.contains_key(&r.id))
        .collect();
    let sessions_today = sessions.len() + ended_today.len();
    let enabled = settings.read().fleet_enabled;
    let connected = fleet::hooks_config::claude_settings_path()
        .and_then(|p| fleet::hooks_config::connected_port_file(&p))
        .is_some();
    drop(state);

    rsx! {
        div {
            class: "flex-1 min-w-0 flex flex-col overflow-y-auto",

            // Header: identity left, the day's four live numbers right. The
            // stat strip is the "how bad is it" glance — amber only when
            // something actually needs the user.
            div {
                class: "px-6 pt-6 pb-3 flex items-center gap-3 flex-wrap",
                Icon {
                    icon: crate::components::pixel_icons::HobbesInvader,
                    width: 22,
                    height: 16,
                    class: "text-fg",
                }
                h1 { class: "text-xl font-semibold", "Fleet" }
                // Listener state as a glanceable icon — the port itself is
                // debug info and lives in the log ("fleet: listening on …").
                if fleet::running_port().is_some() {
                    span {
                        title: "Observing — listener running",
                        Icon {
                            width: 14,
                            height: 14,
                            icon: fi_icons::FiRadio,
                            class: "text-green-500",
                        }
                    }
                } else {
                    span {
                        title: "Listener stopped — check Settings → Fleet",
                        Icon {
                            width: 14,
                            height: 14,
                            icon: fi_icons::FiRadio,
                            class: "text-fg-muted opacity-50",
                        }
                    }
                }
                div { class: "flex-1" }
                div {
                    class: "flex items-center gap-6",
                    Stat {
                        value: needs_you.to_string(),
                        label: "need you",
                        accent: if needs_you > 0 { Some("#f59e0b".to_string()) } else { None },
                    }
                    Stat {
                        value: active.to_string(),
                        label: "active",
                        accent: if active > 0 { Some("#34d399".to_string()) } else { None },
                    }
                    Stat { value: sessions_today.to_string(), label: "today", accent: None }
                    Stat {
                        value: crate::todo::model::format_minutes(agent_minutes),
                        label: "agent time",
                        accent: None,
                    }
                    // The exocortex multiplier: agent-hours per clock-hour
                    // since the day's first activity. The whole fleet thesis
                    // in one number — parallel agents are why it beats 1×.
                    if let Some((mult, elapsed)) = fleet::exocortex_multiplier(
                        agent_minutes,
                        fleet::store::first_event_on(today),
                        now,
                    ) {
                        div {
                            class: "flex items-baseline gap-1.5",
                            title: format!(
                                "{} of agent work in {} of clock time — your exocortex multiplier",
                                crate::todo::model::format_minutes(agent_minutes),
                                crate::todo::model::format_minutes(elapsed),
                            ),
                            Icon {
                                width: 14,
                                height: 14,
                                icon: fi_icons::FiZap,
                                class: "self-center",
                                style: "color: #a78bfa;",
                            }
                            span {
                                class: "text-lg font-semibold tabular-nums",
                                style: "color: #a78bfa;",
                                { format!("{:.1}×", mult) }
                            }
                            span { class: "text-[11px] uppercase tracking-wider text-fg-muted", "exocortex" }
                        }
                    }
                }
            }

            ReviewBand { now, today }

            if sessions.is_empty() {
                div {
                    class: "px-6 py-8 text-sm text-fg-muted space-y-2",
                    p { "No Claude Code sessions observed yet." }
                    if !enabled {
                        p { "Turn on the fleet in Settings → Behavior → Fleet, then Connect Claude Code." }
                    } else if !connected {
                        p { "Hooks aren't registered yet — open Settings → Behavior → Fleet and hit \"Connect Claude Code\"." }
                    } else {
                        p { "Hooks are connected. Sessions appear here the moment one starts (or fires its next event)." }
                    }
                }
            }

            // Live sessions grouped by source — terminal sessions first (the
            // fleet's raison d'être), Hobbes' own tabs below. Within each
            // group: attention first, then most recent activity.
            {
                let (external, hobbes): (Vec<_>, Vec<_>) = sessions
                    .iter()
                    .cloned()
                    .partition(|s| s.origin != fleet::FleetOrigin::Hobbes);
                rsx! {
                    SessionGroup { label: "Claude Code", rows: external, now, today }
                    SessionGroup { label: "Hobbes tabs", rows: hobbes, now, today }
                }
            }

            EarlierToday { rows: ended_today, now, today }

            div { class: "pb-16" }
        }
    }
}

/// One origin's live sessions: a quiet section label and a responsive card
/// grid. Hidden entirely when the group is empty.
#[component]
fn SessionGroup(
    label: &'static str,
    rows: Vec<FleetSession>,
    now: DateTime<Utc>,
    today: chrono::NaiveDate,
) -> Element {
    if rows.is_empty() {
        return rsx! {};
    }
    let count = rows.len();
    rsx! {
        div {
            class: "px-6 pt-3",
            p {
                class: "text-[11px] uppercase tracking-wider text-fg-muted mb-2",
                { format!("{label} — {count}") }
            }
            div {
                class: "grid gap-3",
                style: "grid-template-columns: repeat(auto-fill, minmax(340px, 1fr));",
                for session in rows {
                    FleetRow { key: "{session.id}", session: session.clone(), now, today }
                }
            }
        }
    }
}

/// One stat in the header strip: a number that carries meaning, a label that
/// stays out of the way.
#[component]
fn Stat(value: String, label: &'static str, accent: Option<String>) -> Element {
    let value_style = accent
        .map(|c| format!("color: {c};"))
        .unwrap_or_default();
    rsx! {
        div {
            class: "flex items-baseline gap-1.5",
            span { class: "text-lg font-semibold tabular-nums", style: "{value_style}", "{value}" }
            span { class: "text-[11px] uppercase tracking-wider text-fg-muted", "{label}" }
        }
    }
}

/// Ended sessions, out of the way: one collapsed line summarizing them,
/// expandable into a compact ledger. Replaces the old duplicate per-session
/// list in the header card.
#[component]
fn EarlierToday(rows: Vec<FleetSession>, now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
    let mut open = use_signal(|| false);
    if rows.is_empty() {
        return rsx! {};
    }
    let mut rows = rows;
    rows.sort_by(|a, b| {
        b.minutes_on(today, now)
            .cmp(&a.minutes_on(today, now))
            .then_with(|| a.name.cmp(&b.name))
    });
    let total: u32 = rows.iter().map(|s| s.minutes_on(today, now)).sum();
    let count = rows.len();
    rsx! {
        div {
            class: "px-6 pt-3",
            button {
                class: "flex items-center gap-2 text-xs text-fg-muted hover:text-fg transition-colors select-none",
                onclick: move |_| { let v = !*open.peek(); open.set(v); },
                span { if *open.read() { "▾" } else { "▸" } }
                span {
                    { format!("Earlier today — {} ended session{} · {}",
                        count,
                        if count == 1 { "" } else { "s" },
                        crate::todo::model::format_minutes(total)) }
                }
            }
            if *open.read() {
                div {
                    class: "mt-2 space-y-1",
                    for s in rows.iter() {
                        div {
                            key: "{s.id}",
                            class: "flex items-baseline gap-2 text-xs",
                            span {
                                class: "font-medium shrink-0 max-w-64 truncate",
                                { s.display_name().to_string() }
                            }
                            span { class: "text-fg-muted shrink-0",
                                { crate::todo::model::format_minutes(s.minutes_on(today, now)) } }
                            if let Some(b) = &s.brief {
                                span { class: "text-fg-muted truncate",
                                    { fleet::truncate_summary(&b.headline, 120) } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The day-in-review band: the LLM narrative with its date named honestly
/// (yesterday's rollup never masquerades as today), collapsed to one line
/// until wanted. The Roll-up button lives here, next to its subject.
#[component]
fn ReviewBand(now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
    let settings = use_context::<Signal<crate::settings::Settings>>();
    let mut open = use_signal(|| false);

    // Rollups cache per-date in meta ("fleet_rollup_YYYY-MM-DD") — nothing
    // is ever replaced; the band is a window onto one day at a time, with
    // ‹ › walking to other cached days.
    fn load_rollup(d: chrono::NaiveDate) -> Option<fleet::briefs::DayRollup> {
        crate::session_store::meta_get(&fleet::briefs::rollup_meta_key(d))
            .and_then(|s| serde_json::from_str(&s).ok())
    }
    /// Nearest cached day strictly before/after `from`, probing two weeks.
    fn adjacent_cached(from: chrono::NaiveDate, back: bool, today: chrono::NaiveDate)
        -> Option<chrono::NaiveDate>
    {
        let mut d = from;
        for _ in 0..14 {
            d = if back { d.pred_opt()? } else { d.succ_opt()? };
            if !back && d > today {
                return None;
            }
            if load_rollup(d).is_some() {
                return Some(d);
            }
        }
        None
    }

    let mut viewing = use_signal(|| {
        // Morning re-entry: no rollup yet today → open on yesterday's.
        if load_rollup(today).is_none() {
            if let Some(y) = today.pred_opt().filter(|y| load_rollup(*y).is_some()) {
                return y;
            }
        }
        today
    });
    let mut rollup = use_signal(|| Option::<fleet::briefs::DayRollup>::None);
    let mut rollup_running = use_signal(|| false);
    let mut rollup_error = use_signal(|| Option::<String>::None);
    use_effect(move || {
        let d = viewing();
        rollup.set(load_rollup(d));
    });

    let on_rollup = move |_| {
        if *rollup_running.peek() {
            return;
        }
        rollup_running.set(true);
        rollup_error.set(None);
        spawn(async move {
            let s = settings.peek().clone();
            let outcome = async {
                let instance = s
                    .active_connector()
                    .filter(|i| s.is_connector_configured(i))
                    .or_else(|| {
                        s.llm_connectors
                            .iter()
                            .find(|i| s.is_connector_configured(i))
                    })
                    .cloned()
                    .ok_or_else(|| "No configured LLM connector.".to_string())?;
                let now = Utc::now();
                let today = Local::now().date_naive();
                let state = fleet::shared().snapshot();
                let mut rows: Vec<FleetSession> = fleet::store::sessions_active_on(today)
                    .into_iter()
                    .filter(|r| !state.sessions.contains_key(&r.id))
                    .collect();
                rows.extend(state.sessions.values().cloned());
                if rows.is_empty() {
                    return Err("No sessions today to roll up.".to_string());
                }
                let total = fleet::fleet_agent_minutes_on(&state, today, now);
                let lines = fleet::briefs::rollup_lines(&rows, today, now, 12);
                let framed = fleet::briefs::rollup_framing(today, &lines, total);
                let connector = crate::llm::build_connector_for_instance(&instance, None);
                let narrative = tokio::time::timeout(
                    std::time::Duration::from_secs(90),
                    connector.generate_fleet_rollup(framed),
                )
                .await
                .map_err(|_| "Rollup timed out.".to_string())?
                .map_err(|e| e.to_string())?;
                let day_rollup = fleet::briefs::DayRollup {
                    date: today,
                    narrative,
                    generated_at: now,
                    session_count: rows.len(),
                    total_minutes: total,
                };
                if let Ok(json) = serde_json::to_string(&day_rollup) {
                    let _ = crate::session_store::meta_set(
                        &fleet::briefs::rollup_meta_key(today),
                        &json,
                    );
                }
                Ok(day_rollup)
            }
            .await;
            match outcome {
                Ok(r) => {
                    // A fresh rollup is today's — snap the window to it.
                    viewing.set(Local::now().date_naive());
                    rollup.set(Some(r));
                }
                Err(e) => rollup_error.set(Some(e)),
            }
            rollup_running.set(false);
        });
    };

    let current = rollup.read().clone();
    let viewing_day = viewing();
    let day_label = if viewing_day == today {
        "today".to_string()
    } else if today.pred_opt() == Some(viewing_day) {
        "yesterday".to_string()
    } else {
        viewing_day.format("%a, %-d %b").to_string()
    };
    let band_label = format!("Day in review — {day_label}");
    let prev_day = adjacent_cached(viewing_day, true, today);
    let next_day = adjacent_cached(viewing_day, false, today);
    let preview = current
        .as_ref()
        .map(|r| fleet::truncate_summary(&r.narrative, 110))
        .unwrap_or_else(|| "No rollup yet.".to_string());

    rsx! {
        div {
            class: "px-6 pb-1",
            div {
                class: "rounded border border-subtle bg-section px-3 py-2",
                div {
                    class: "flex items-center gap-2",
                    button {
                        class: "flex items-center gap-2 min-w-0 flex-1 text-left select-none",
                        onclick: move |_| { let v = !*open.peek(); open.set(v); },
                        span { class: "text-xs text-fg-muted", if *open.read() { "▾" } else { "▸" } }
                        span { class: "text-xs font-semibold shrink-0", "{band_label}" }
                        if !*open.read() {
                            span { class: "text-xs text-fg-muted truncate", "{preview}" }
                        }
                    }
                    if let Some(r) = &current {
                        span {
                            class: "text-[11px] text-fg-muted shrink-0",
                            { format!("rolled up {}", age_label(r.generated_at, now)) }
                        }
                    }
                    // Walk between cached days — nothing is ever replaced,
                    // each day keeps its own rollup.
                    if prev_day.is_some() || next_day.is_some() {
                        div {
                            class: "flex items-center shrink-0",
                            button {
                                class: "px-1.5 text-xs text-fg-muted hover:text-fg disabled:opacity-30 transition-colors select-none",
                                disabled: prev_day.is_none(),
                                title: "Earlier day",
                                onclick: move |_| {
                                    if let Some(d) = prev_day {
                                        viewing.set(d);
                                    }
                                },
                                "‹"
                            }
                            button {
                                class: "px-1.5 text-xs text-fg-muted hover:text-fg disabled:opacity-30 transition-colors select-none",
                                disabled: next_day.is_none(),
                                title: "Later day",
                                onclick: move |_| {
                                    if let Some(d) = next_day {
                                        viewing.set(d);
                                    }
                                },
                                "›"
                            }
                        }
                    }
                    button {
                        class: "px-3 py-1 text-xs font-bold rounded bg-btn-primary hover:bg-btn-primary-hover transition-colors disabled:opacity-50 shrink-0",
                        disabled: *rollup_running.read(),
                        onclick: on_rollup,
                        if *rollup_running.read() { "Rolling up…" } else { "Roll up my day" }
                    }
                }
                if *open.read() {
                    if let Some(r) = &current {
                        p { class: "mt-2 text-sm text-fg leading-relaxed", "{r.narrative}" }
                    } else {
                        p { class: "mt-2 text-xs text-fg-muted", "Roll up your day to get a short review of what every session got done, what's blocked, and what to pick up next." }
                    }
                }
                if let Some(err) = rollup_error.read().clone() {
                    p { class: "mt-2 text-xs text-red-400", "{err}" }
                }
            }
        }
    }
}

#[component]
fn FleetRow(session: FleetSession, now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let planner = use_context::<Signal<crate::todo::PlannerState>>();
    let session_state = use_context::<Signal<crate::session::SessionState>>();

    // Project attribution: terminal rows map by cwd against Project.path;
    // Hobbes rows carry their chat session's tag (hydrated tabs only).
    let project = {
        let p = planner.read();
        let id = match session.origin {
            fleet::FleetOrigin::External => {
                crate::services::project_tagger::project_for_cwd(&p.projects, &session.cwd)
            }
            fleet::FleetOrigin::Hobbes => session_state
                .read()
                .sessions
                .get(&session.id)
                .and_then(|s| s.project_id.clone()),
        };
        id.and_then(|id| {
            crate::services::project_tagger::project_title(&p.projects, &id)
                .map(str::to_string)
        })
    };
    let (chip_label, chip_color) = status_chip(&session.status);
    let attention = session.status.needs_attention();
    // Attention rows get a loud left edge; computed color → inline style.
    let row_style = if attention {
        format!("border-left: 3px solid {chip_color}; background: {chip_color}14;")
    } else {
        "border-left: 3px solid transparent;".to_string()
    };
    let minutes_today = session.minutes_on(today, now);
    let auto = session.auto_passthrough;
    let is_hobbes = session.origin == fleet::FleetOrigin::Hobbes;
    let session_id = session.id.clone();
    let toggle_id = session.id.clone();

    rsx! {
        div {
            class: if is_hobbes {
                "group rounded border border-subtle bg-section p-3 cursor-pointer hover:bg-card transition-colors flex flex-col"
            } else {
                "group rounded border border-subtle bg-section p-3 flex flex-col"
            },
            style: "{row_style}",
            title: if is_hobbes { "Open this chat tab" } else { "" },
            // A Hobbes row is a door back into its tab (SwitchToSession also
            // dismisses the fleet view).
            onclick: move |_| {
                if is_hobbes {
                    chat_command.set(Some(
                        crate::components::chat_input::ChatCommand::SwitchToSession(
                            session_id.clone(),
                        ),
                    ));
                }
            },
            div {
                class: "flex items-center gap-3",
                if session.origin == fleet::FleetOrigin::Hobbes {
                    Icon { width: 16, height: 16, icon: fi_icons::FiMessageSquare }
                } else {
                    Icon { width: 16, height: 16, icon: fi_icons::FiTerminal }
                }
                div {
                    class: "flex-1 min-w-0",
                    div {
                        class: "flex items-center gap-2",
                        span {
                            class: "text-sm font-semibold truncate",
                            { session.display_name().to_string() }
                        }
                        if session.dispatched_todo.is_some() {
                            span {
                                class: "shrink-0 px-1.5 rounded-full border border-faint text-[10px] text-fg-muted",
                                title: "Launched by Hobbes for a todo",
                                "dispatched"
                            }
                        }
                        // Status as a dot, not a pill — the full name of the
                        // state lives in the tooltip; attention pulses.
                        span {
                            class: if attention {
                                "w-2.5 h-2.5 rounded-full shrink-0 animate-pulse"
                            } else {
                                "w-2.5 h-2.5 rounded-full shrink-0"
                            },
                            style: "background-color: {chip_color};",
                            title: "{chip_label}",
                        }
                    }
                    // Project leads the subtitle so truncation eats the
                    // path, never the tag.
                    p {
                        class: "text-xs text-fg-muted truncate",
                        if let Some(proj) = &project {
                            span { class: "text-fg", "{proj}" }
                            span { " · " }
                        }
                        if session.origin == fleet::FleetOrigin::Hobbes {
                            "Hobbes chat tab"
                        } else {
                            "{session.cwd}"
                        }
                    }
                }
            }

            // The assignment this session carries, when a todo links to it.
            {
                let working_on = planner
                    .read()
                    .todos
                    .iter()
                    .find(|t| {
                        t.linked_fleet_session.as_deref() == Some(session.id.as_str())
                            && !matches!(
                                t.status,
                                crate::todo::model::TodoStatus::Completed
                                    | crate::todo::model::TodoStatus::Cancelled
                            )
                    })
                    .map(|t| t.title.clone());
                rsx! {
                    if let Some(title) = working_on {
                        p {
                            class: "mt-1 text-xs text-fg-muted truncate",
                            span { class: "text-fg-muted/70", "working on: " }
                            span { class: "text-fg", { fleet::truncate_summary(&title, 90) } }
                        }
                    }
                }
            }

            // Attention detail line (what the session wants).
            if let FleetStatus::NeedsAttention(AttentionKind::Notification { message, .. }) = &session.status {
                if !message.is_empty() {
                    p { class: "mt-2 text-xs text-fg", "{message}" }
                }
            }

            // Re-entry brief: what happened since you last looked. Status is
            // ms-fresh, brief text is minutes-stale — make the age legible
            // rather than letting the mix read as broken.
            if let Some(brief) = &session.brief {
                {
                    let brief_age_min = (session.last_event_at - brief.generated_at).num_minutes();
                    let turn_running = matches!(
                        session.status,
                        FleetStatus::Working | FleetStatus::WorkingBackground
                    );
                    rsx! {
                        p {
                            class: "mt-2 text-xs text-fg",
                            { fleet::truncate_summary(&brief.headline, 140) }
                            if brief_age_min > 3 {
                                span {
                                    class: "text-fg-muted/60",
                                    { format!("  · {}m old", brief_age_min) }
                                }
                            }
                        }
                        if let Some(blocked) = &brief.blocked_on {
                            // Dim while a turn is running: the blocker was
                            // written before this turn and is under re-test.
                            p {
                                class: "text-xs",
                                style: if turn_running {
                                    "color: #f59e0b; opacity: 0.55;"
                                } else {
                                    "color: #f59e0b;"
                                },
                                { format!("blocked: {}", fleet::truncate_summary(blocked, 100)) }
                            }
                        }
                    }
                }
            }

            // A held gate renders inline with Approve / Deny. Resolution goes
            // through the shared runtime; the server task does the rest and
            // the drain loop repaints this row.
            if let Some(gate) = session.pending_gate.clone() {
                div {
                    class: "mt-2 rounded bg-input p-2",
                    div {
                        class: "flex items-center gap-2",
                        span { class: "text-xs font-semibold", "{gate.tool_name}" }
                        span { class: "text-xs text-fg-muted", { age_label(gate.requested_at, now) } }
                    }
                    p { class: "mt-1 text-xs text-fg-muted break-all", "{gate.input_summary}" }
                    div {
                        class: "mt-2 flex gap-2",
                        button {
                            class: "px-3 py-1 text-xs font-bold rounded bg-btn-primary hover:bg-btn-primary-hover transition-colors",
                            onclick: {
                                let id = gate.request_id.clone();
                                move |_| fleet::shared().resolve_gate(&id, true)
                            },
                            "Approve"
                        }
                        button {
                            class: "px-3 py-1 text-xs font-bold rounded bg-red-900/60 hover:bg-red-800/60 transition-colors",
                            onclick: {
                                let id = gate.request_id.clone();
                                move |_| fleet::shared().resolve_gate(&id, false)
                            },
                            "Deny"
                        }
                    }
                }
            }

            // Per-session auto-passthrough: gates skip the in-app hold and go
            // straight to the terminal prompt. Hover-revealed (the tab-close
            // idiom) so it never competes with the brief; only the checkbox +
            // its text are clickable, and only external sessions show it —
            // Hobbes tabs approve tools in the tab itself.
            if session.origin != fleet::FleetOrigin::Hobbes {
                div {
                    class: "opacity-0 group-hover:opacity-100 transition-opacity",
                    label {
                        class: "mt-2 inline-flex w-fit items-center gap-2 text-xs text-fg-muted cursor-pointer select-none",
                        onclick: move |evt| evt.stop_propagation(),
                        input {
                            r#type: "checkbox",
                            checked: auto,
                            onchange: move |e| {
                                fleet::shared().set_auto_passthrough(&toggle_id, e.value() == "true");
                            },
                        }
                        "Answer permission prompts in the terminal (skip in-app approval)"
                    }
                }
            }

            // Footer, pinned bottom-right: recency and today's banked time.
            div {
                class: "mt-auto pt-2 flex items-baseline justify-end gap-2 text-[11px] text-fg-muted",
                span { { age_label(session.last_event_at, now) } }
                if minutes_today > 0 {
                    span { "·" }
                    span { { format!("{} today", format_minutes(minutes_today)) } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn age_labels_scale() {
        let now = Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap();
        assert_eq!(age_label(now, now), "just now");
        assert_eq!(age_label(now - chrono::Duration::minutes(3), now), "3m ago");
        assert_eq!(age_label(now - chrono::Duration::hours(5), now), "5h ago");
        assert_eq!(age_label(now - chrono::Duration::days(2), now), "2d ago");
    }

    #[test]
    fn attention_sorts_first_then_recency() {
        let mut state = fleet::FleetState::default();
        let t0 = utc("2026-08-25T10:00:00Z");
        for (id, minutes_ago, needs) in [
            ("old-idle", 50i64, false),
            ("fresh-working", 1, false),
            ("stale-gate", 40, true),
        ] {
            let ev = crate::fleet::events::FleetEvent::SessionStart {
                session_id: id.into(),
                cwd: format!("/x/{id}"),
                reason: "startup".into(),
            };
            fleet::reduce(&mut state, &ev, t0 - chrono::Duration::minutes(minutes_ago));
            if needs {
                let gate = crate::fleet::events::FleetEvent::PermissionRequest {
                    session_id: id.into(),
                    cwd: String::new(),
                    request_id: "g".into(),
                    tool_name: "Bash".into(),
                    tool_input: serde_json::json!({}),
                };
                fleet::reduce(&mut state, &gate, t0 - chrono::Duration::minutes(minutes_ago));
            }
        }
        let sorted = sorted_sessions(&state);
        let ids: Vec<&str> = sorted.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["stale-gate", "fresh-working", "old-idle"]);
    }

    #[test]
    fn status_chips_flag_attention_states() {
        assert_eq!(status_chip(&FleetStatus::Working).0, "working");
        assert_eq!(status_chip(&FleetStatus::Idle).0, "idle");
        assert_eq!(
            status_chip(&FleetStatus::NeedsAttention(AttentionKind::Gate)).0,
            "waiting on you"
        );
    }
}
